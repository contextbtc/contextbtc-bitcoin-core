//! `scan`: a full `bdk_electrum` wallet scan through `NostrElectrumClient`.

use std::time::Instant;

use anyhow::{Context, bail};
use bdk_chain::bitcoin::constants::genesis_block;
use bdk_chain::bitcoin::secp256k1::Secp256k1;
use bdk_chain::bitcoin::{Amount, Network};
use bdk_chain::indexer::keychain_txout::{FullScanRequestBuilderExt, KeychainTxOutIndex};
use bdk_chain::local_chain::LocalChain;
use bdk_chain::miniscript::{Descriptor, DescriptorPublicKey};
use bdk_chain::spk_client::FullScanRequest;
use bdk_chain::{CanonicalizationParams, ChainPosition, ConfirmationBlockTime, IndexedTxGraph};
use bdk_electrum::BdkElectrumClient;
use contextbtc_electrum_client::NostrElectrumClient;
use contextbtc_electrum_client::electrum_client::ElectrumApi;

const EXTERNAL: &str = "external";
const INTERNAL: &str = "internal";

pub fn run(
    client: NostrElectrumClient,
    network: Network,
    descriptor: &str,
    change_descriptor: Option<&str>,
    stop_gap: usize,
    batch_size: usize,
) -> anyhow::Result<()> {
    let mut index = KeychainTxOutIndex::<&str>::default();
    index.insert_descriptor(EXTERNAL, parse_public(descriptor)?)?;
    if let Some(change) = change_descriptor {
        index.insert_descriptor(INTERNAL, parse_public(change)?)?;
    }
    let mut graph = IndexedTxGraph::<ConfirmationBlockTime, _>::new(index);
    let (mut chain, _) = LocalChain::from_genesis_hash(genesis_block(network).block_hash());

    let bdk_client = BdkElectrumClient::new(client);
    let request = FullScanRequest::builder()
        .chain_tip(chain.tip())
        .spks_from_indexer(&graph.index);

    let start = Instant::now();
    let update = bdk_client.full_scan(request, stop_gap, batch_size, false)?;
    let elapsed = start.elapsed();

    if let Some(chain_update) = update.chain_update {
        chain
            .apply_update(chain_update)
            .context("chain update does not connect")?;
    }
    let _ = graph
        .index
        .reveal_to_target_multi(&update.last_active_indices);
    let _ = graph.apply_update(update.tx_update);

    let tip = chain.tip().block_id();
    let outpoints = graph.index.outpoints().clone();
    let tx_graph = graph.graph();
    let params = CanonicalizationParams::default;

    println!(
        "scanned in {:.2}s ({} calls)",
        elapsed.as_secs_f64(),
        bdk_client.inner.calls_made()?
    );
    println!("network: {network}");
    println!("tip:     {} {}", tip.height, tip.hash);

    println!("last used index:");
    for keychain in graph.index.keychains().map(|(k, _)| k) {
        match update.last_active_indices.get(keychain) {
            Some(i) => println!("  {keychain}: {i}"),
            None => println!("  {keychain}: none"),
        }
    }

    // Change outputs are ours, so unconfirmed change counts as trusted.
    let balance = tx_graph.balance(&chain, tip, params(), outpoints.iter().cloned(), |k, _| {
        k.0 == INTERNAL
    });
    println!("balance:");
    println!("  confirmed:         {}", btc(balance.confirmed));
    println!("  immature:          {}", btc(balance.immature));
    println!("  trusted pending:   {}", btc(balance.trusted_pending));
    println!("  untrusted pending: {}", btc(balance.untrusted_pending));
    println!("  total:             {}", btc(balance.total()));

    println!("utxos:");
    let mut any = false;
    for ((keychain, i), utxo) in tx_graph.filter_chain_unspents(&chain, tip, params(), outpoints) {
        any = true;
        println!(
            "  {keychain}/{i}  {}  {}  {}{}",
            utxo.outpoint,
            btc(utxo.txout.value),
            position(&utxo.chain_position),
            if utxo.is_on_coinbase {
                "  coinbase"
            } else {
                ""
            }
        );
    }
    if !any {
        println!("  none");
    }

    println!("transactions:");
    let mut any = false;
    for tx in tx_graph.list_canonical_txs(&chain, tip, params()) {
        any = true;
        println!("  {}  {}", tx.tx_node.txid, position(&tx.chain_position));
    }
    if !any {
        println!("  none");
    }

    Ok(())
}

/// Parse a descriptor, refusing private keys: a scan only needs public ones,
/// and secrets shouldn't end up in shell history.
fn parse_public(descriptor: &str) -> anyhow::Result<Descriptor<DescriptorPublicKey>> {
    let secp = Secp256k1::signing_only();
    let (descriptor, keys) = Descriptor::parse_descriptor(&secp, descriptor)
        .with_context(|| format!("invalid descriptor {descriptor}"))?;
    if !keys.is_empty() {
        bail!("descriptor contains private keys; pass its public form instead");
    }
    Ok(descriptor)
}

fn position(pos: &ChainPosition<ConfirmationBlockTime>) -> String {
    match pos {
        ChainPosition::Confirmed { anchor, .. } => {
            format!("confirmed at {}", anchor.block_id.height)
        }
        ChainPosition::Unconfirmed { .. } => "unconfirmed".to_string(),
    }
}

fn btc(amount: Amount) -> String {
    format!("{:.8} BTC", amount.to_btc())
}
