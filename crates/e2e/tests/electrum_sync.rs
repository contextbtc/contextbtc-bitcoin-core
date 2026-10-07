//! End-to-end Electrum wallet sync over MCP-over-Nostr.
//!
//! ```text
//!   bdk_electrum ──▶ NostrElectrumClient ──MCP/Nostr──▶ nak serve (relay) ──▶ contextbtc-electrs-server ──Electrum──▶ electrs ──▶ bitcoind (regtest)
//! ```
//!
//! `bdk_electrum` is the unmodified crates.io release. Its `BdkElectrumClient`
//! is generic over `electrum_client::ElectrumApi`, which
//! `contextbtc-electrum-client` implements by turning each Electrum call into
//! an MCP `tools/call` over Nostr.
//!
//! The test drives a regtest chain to a known shape: filler blocks up to
//! [`START_HEIGHT`], one block whose coinbase pays a descriptor address, then a
//! few more filler blocks. It then runs a full scan of the descriptors and
//! asserts the funded output landed in the graph, anchored at the right height.

#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::io::Read;
use std::process::Stdio;
use std::time::{Duration, Instant};

use bdk_chain::bitcoin::{
    Address, Amount, BlockHash, CompressedPublicKey, Network, PrivateKey, constants::genesis_block,
    secp256k1::Secp256k1,
};
use bdk_chain::indexer::keychain_txout::{FullScanRequestBuilderExt, KeychainTxOutIndex};
use bdk_chain::local_chain::LocalChain;
use bdk_chain::miniscript::Descriptor;
use bdk_chain::spk_client::FullScanRequest;
use bdk_chain::{ConfirmationBlockTime, IndexedTxGraph};
use bdk_electrum::BdkElectrumClient;
use bdk_electrum::electrum_client::ElectrumApi;
use contextbtc_electrum_client::NostrElectrumClient;
use wait_timeout::ChildExt;

mod harness;

use harness::ElectrsStack;

const EXTERNAL: &str = "tr([83737d5e/86'/1'/0']tpubDDR5GgtoxS8fJyjjvdahN4VzV5DV6jtbcyvVXhEKq2XtpxjxBXmxH3r8QrNbQqHg4bJM1EGkxi7Pjfkgnui9jQWqS7kxHvX6rhUeriLDKxz/0/*)";
const INTERNAL: &str = "tr([83737d5e/86'/1'/0']tpubDDR5GgtoxS8fJyjjvdahN4VzV5DV6jtbcyvVXhEKq2XtpxjxBXmxH3r8QrNbQqHg4bJM1EGkxi7Pjfkgnui9jQWqS7kxHvX6rhUeriLDKxz/1/*)";
const NETWORK: Network = Network::Regtest;

/// Height of the last filler block before the funded one.
const START_HEIGHT: u32 = 20;
/// Derivation index (on the external keychain) of the address the funded
/// block's coinbase pays to. Must be below `STOP_GAP` to be found.
const FUNDED_INDEX: u32 = 2;
/// Filler blocks mined after the funded one.
const BLOCKS_AFTER_FUNDING: usize = 5;
/// Full-scan stop gap and number of script hashes per batch.
const STOP_GAP: usize = 10;
const BATCH_SIZE: usize = 5;

#[test]
fn bdk_electrum_full_scan_over_nostr() -> anyhow::Result<()> {
    // Show each MCP tools/call under RUST_LOG (only printed for failing tests).
    let _ = env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("contextbtc_electrum_client=debug"),
    )
    .is_test(true)
    .try_init();

    let stack = ElectrsStack::start()?;

    // --- Setup receiving chain and graph structures ---------------------------

    let secp = Secp256k1::new();
    let (descriptor, _) = Descriptor::parse_descriptor(&secp, EXTERNAL)?;
    let (change_descriptor, _) = Descriptor::parse_descriptor(&secp, INTERNAL)?;
    let (mut chain, _) = LocalChain::from_genesis_hash(genesis_block(NETWORK).block_hash());

    let mut graph = IndexedTxGraph::<ConfirmationBlockTime, KeychainTxOutIndex<&str>>::new({
        let mut index = KeychainTxOutIndex::default();
        index.insert_descriptor("external", descriptor.clone())?;
        index.insert_descriptor("internal", change_descriptor.clone())?;
        index
    });

    let ChainShape {
        funded_height,
        tip_height,
        tip_hash,
        ..
    } = shape_chain(&stack)?;

    // --- Electrum client (MCP over Nostr) -------------------------------------

    let electrum =
        NostrElectrumClient::new(vec![stack.relay_url.clone()], stack.server_pubkey.clone())?;

    // electrs indexes new blocks asynchronously.
    wait_for_electrs_tip(&electrum, tip_height, Duration::from_secs(60))?;

    let features = electrum.server_features()?;
    assert_eq!(
        features.server_version, "",
        "server version should be redacted"
    );

    // --- Full scan ------------------------------------------------------------

    let bdk_client = BdkElectrumClient::new(electrum);
    let request = FullScanRequest::builder()
        .chain_tip(chain.tip())
        .spks_from_indexer(&graph.index);

    let start = Instant::now();
    let update = bdk_client.full_scan(request, STOP_GAP, BATCH_SIZE, false)?;
    println!("full scan took {}s", start.elapsed().as_secs_f32());

    if let Some(chain_update) = update.chain_update {
        let _ = chain.apply_update(chain_update)?;
    }
    let _ = graph
        .index
        .reveal_to_target_multi(&update.last_active_indices);
    let _ = graph.apply_update(update.tx_update);

    // --- Assertions -----------------------------------------------------------

    assert_eq!(
        chain.tip().height(),
        tip_height,
        "local chain should reach the node's tip"
    );
    assert_eq!(chain.tip().hash(), tip_hash);
    assert_eq!(
        update.last_active_indices.get("external"),
        Some(&FUNDED_INDEX),
        "the funded index should be the last active external index"
    );

    let unspent: Vec<_> = graph
        .graph()
        .filter_chain_unspents(
            &chain,
            chain.tip().block_id(),
            Default::default(),
            graph.index.outpoints().clone(),
        )
        .collect();
    println!("unspent: {unspent:?}");

    assert_eq!(
        unspent.len(),
        1,
        "exactly one output (the funded coinbase) should be unspent: {unspent:?}"
    );
    let (keychain_index, utxo) = &unspent[0];
    assert_eq!(*keychain_index, ("external", FUNDED_INDEX));
    assert!(utxo.is_on_coinbase);
    // Regtest halves every 150 blocks, so the subsidy at this height is 50 BTC
    // and there are no fees to collect.
    assert_eq!(utxo.txout.value, Amount::from_int_btc(50));
    assert_eq!(
        utxo.chain_position.confirmation_height_upper_bound(),
        Some(funded_height)
    );

    Ok(())
}

/// Drives the CLI binary through the same stack: `tip`, `balance` and `scan`
/// must report what the node mined.
#[test]
fn cli_queries_over_nostr() -> anyhow::Result<()> {
    let stack = ElectrsStack::start()?;
    let shape = shape_chain(&stack)?;

    let electrum =
        NostrElectrumClient::new(vec![stack.relay_url.clone()], stack.server_pubkey.clone())?;
    wait_for_electrs_tip(&electrum, shape.tip_height, Duration::from_secs(60))?;

    let tip = run_cli(&stack, &["tip"])?;
    assert!(
        tip.contains(&format!("height: {}", shape.tip_height)),
        "{tip}"
    );
    assert!(tip.contains(&shape.tip_hash.to_string()), "{tip}");

    let balance = run_cli(&stack, &["balance", &shape.funded_address.to_string()])?;
    assert!(balance.contains("confirmed:   5000000000 sat"), "{balance}");

    let scan = run_cli(&stack, &["scan", EXTERNAL, INTERNAL])?;
    assert!(
        scan.contains(&format!("external: {FUNDED_INDEX}")),
        "{scan}"
    );
    // A coinbase needs 100 confirmations to mature, so the 50 BTC is immature.
    assert!(
        scan.contains("immature:          50.00000000 BTC"),
        "{scan}"
    );
    assert!(
        scan.contains(&format!("confirmed at {}  coinbase", shape.funded_height)),
        "{scan}"
    );

    Ok(())
}

/// Run `contextbtc-electrum-client-cli` against the stack and return its
/// stdout, failing if it errors or takes longer than a minute.
fn run_cli(stack: &ElectrsStack, args: &[&str]) -> anyhow::Result<String> {
    let cli = escargot::CargoBuild::new()
        .package("contextbtc-electrum-client-cli")
        .bin("contextbtc-electrum-client-cli")
        .run()?;
    let mut child = cli
        .command()
        .args([
            "--server",
            &stack.server_pubkey,
            "--relays",
            &stack.relay_url,
        ])
        .args(args)
        .env_remove("CLIENT_NOSTR_SECRET_KEY")
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()?;

    let Some(status) = child.wait_timeout(Duration::from_secs(60))? else {
        let _ = child.kill();
        anyhow::bail!("CLI {args:?} did not finish within 60s");
    };
    let mut stdout = String::new();
    child
        .stdout
        .take()
        .expect("stdout piped")
        .read_to_string(&mut stdout)?;
    println!("$ cli {}\n{stdout}", args.join(" "));
    anyhow::ensure!(status.success(), "CLI {args:?} failed with {status}");
    Ok(stdout)
}

/// The regtest chain after [`shape_chain`].
struct ChainShape {
    funded_address: Address,
    funded_height: u32,
    tip_height: u32,
    tip_hash: BlockHash,
}

/// Drive the regtest chain into a known shape: filler blocks up to
/// [`START_HEIGHT`], one block paying the external descriptor at
/// [`FUNDED_INDEX`], then [`BLOCKS_AFTER_FUNDING`] filler blocks.
fn shape_chain(stack: &ElectrsStack) -> anyhow::Result<ChainShape> {
    let secp = Secp256k1::new();
    let (descriptor, _) = Descriptor::parse_descriptor(&secp, EXTERNAL)?;

    // Filler blocks pay a throwaway key unrelated to either descriptor.
    let filler_address = throwaway_address(&secp)?;
    let funded_address = descriptor
        .at_derivation_index(FUNDED_INDEX)?
        .address(NETWORK)?;

    let client = &stack.node.client;
    // The harness already mined a block so electrs could leave IBD.
    let premined = client.get_block_count()?.0 as u32;
    client.generate_to_address((START_HEIGHT - premined) as usize, &filler_address)?;
    client.generate_to_address(1, &funded_address)?;
    let funded_height = START_HEIGHT + 1;
    client.generate_to_address(BLOCKS_AFTER_FUNDING, &filler_address)?;
    let tip_height = funded_height + BLOCKS_AFTER_FUNDING as u32;

    Ok(ChainShape {
        funded_address,
        funded_height,
        tip_height,
        tip_hash: block_hash_at(stack, tip_height)?,
    })
}

/// A regtest address for a deterministic throwaway key, used to mine blocks
/// that must not match the watched descriptors.
fn throwaway_address(
    secp: &Secp256k1<bdk_chain::bitcoin::secp256k1::All>,
) -> anyhow::Result<Address> {
    let sk = PrivateKey::from_slice(&[0x42; 32], NETWORK)?;
    let pk = CompressedPublicKey::from_private_key(secp, &sk)?;
    Ok(Address::p2wpkh(&pk, NETWORK))
}

/// Read a block hash straight from the node (not through the Nostr transport).
fn block_hash_at(stack: &ElectrsStack, height: u32) -> anyhow::Result<BlockHash> {
    Ok(stack
        .node
        .client
        .get_block_hash(height as u64)?
        .block_hash()?)
}

/// Poll the tip electrs reports (over Nostr) until it reaches `height`.
fn wait_for_electrs_tip(
    electrum: &NostrElectrumClient,
    height: u32,
    timeout: Duration,
) -> anyhow::Result<()> {
    let deadline = Instant::now() + timeout;
    let mut last = None;
    while Instant::now() < deadline {
        let tip = electrum.block_headers_subscribe()?;
        if tip.height >= height as usize {
            return Ok(());
        }
        last = Some(tip.height);
        std::thread::sleep(Duration::from_millis(250));
    }
    anyhow::bail!("electrs did not reach height {height} within {timeout:?} (last seen: {last:?})")
}
