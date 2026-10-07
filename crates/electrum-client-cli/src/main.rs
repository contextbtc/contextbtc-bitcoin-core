//! Command-line client for `contextbtc-electrs-server`.
//!
//! ```text
//!   contextbtc-electrum-client-cli ──MCP/Nostr──▶ contextbtc-electrs-server ──▶ electrs ──▶ bitcoind
//! ```
//!
//! Each subcommand is one Electrum call made through `NostrElectrumClient`,
//! except `scan`, which runs a full `bdk_electrum` wallet scan.

mod scan;

use std::time::{Duration, Instant};

use anyhow::Context;
use bdk_chain::bitcoin::address::NetworkUnchecked;
use bdk_chain::bitcoin::constants::ChainHash;
use bdk_chain::bitcoin::hex::DisplayHex;
use bdk_chain::bitcoin::{Address, Network, ScriptBuf, Txid, consensus};
use clap::{Parser, Subcommand};
use contextbtc_electrum_client::electrum_client::ElectrumApi;
use contextbtc_electrum_client::{Keys, NostrElectrumClient};

#[derive(Parser)]
#[command(version, about = "Query a contextbtc-electrs-server over Nostr")]
struct Cli {
    /// Public key of the contextbtc-electrs-server (hex, npub or nprofile)
    #[arg(long, env = "ELECTRUM_SERVER_PUBKEY", global = true)]
    server: Option<String>,

    /// Comma-separated Nostr relay URLs
    #[arg(
        long,
        env = "NOSTR_RELAY_URLS",
        value_delimiter = ',',
        default_value = "ws://localhost:10547",
        global = true
    )]
    relays: Vec<String>,

    /// Seconds to wait for the server to answer each call
    #[arg(long, default_value_t = 30, global = true)]
    timeout: u64,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Show the chain tip
    Tip,
    /// Show the block header at a height
    Header { height: usize },
    /// Show an address's confirmed and unconfirmed balance
    Balance { address: String },
    /// List an address's transactions
    History { address: String },
    /// List an address's unspent outputs
    Utxos { address: String },
    /// Print a transaction as raw hex
    Tx {
        txid: Txid,
        /// Show inputs and outputs instead of hex
        #[arg(long)]
        decode: bool,
    },
    /// Show the merkle proof of a confirmed transaction
    Merkle { txid: Txid, height: usize },
    /// Show fee estimates
    Fee {
        /// Confirmation target, in blocks
        #[arg(long, default_value_t = 6)]
        blocks: usize,
    },
    /// Show the server's features and the network it serves
    Features,
    /// Check the server answers, and time the round trip
    Ping,
    /// Full-scan a wallet descriptor with bdk_electrum and show its balance
    Scan {
        /// Public descriptor of the receiving keychain
        descriptor: String,
        /// Public descriptor of the change keychain
        change_descriptor: Option<String>,
        /// Stop after this many consecutive unused addresses
        #[arg(long, default_value_t = 20)]
        stop_gap: usize,
        /// Script hashes requested per batch
        #[arg(long, default_value_t = 10)]
        batch_size: usize,
    },
}

fn main() -> anyhow::Result<()> {
    // Load variables from a local `.env` file if present. Real environment
    // variables always take precedence and a missing file is not an error.
    dotenvy::dotenv().ok();
    env_logger::init();

    let cli = Cli::parse();
    let server = cli
        .server
        .context("no server public key: pass --server or set ELECTRUM_SERVER_PUBKEY")?;
    let relays: Vec<String> = cli
        .relays
        .iter()
        .map(|r| r.trim().to_string())
        .filter(|r| !r.is_empty())
        .collect();

    let mut client = NostrElectrumClient::with_keys(client_keys()?, relays, server)?;
    client.set_call_timeout(Duration::from_secs(cli.timeout));

    run(cli.command, client)
}

/// The client's Nostr keys from `CLIENT_NOSTR_SECRET_KEY`, or an ephemeral key
/// (with a warning) when unset.
fn client_keys() -> anyhow::Result<Keys> {
    // An empty value (as left by the `.env.example` template) means unset.
    match std::env::var("CLIENT_NOSTR_SECRET_KEY")
        .ok()
        .filter(|sk| !sk.trim().is_empty())
    {
        Some(sk) => Keys::parse(sk.trim()).context("invalid CLIENT_NOSTR_SECRET_KEY"),
        None => {
            eprintln!(
                "WARNING: CLIENT_NOSTR_SECRET_KEY not set; using an ephemeral key \
                 (servers with ALLOWED_CLIENT_PUBKEYS will not answer)."
            );
            Ok(Keys::generate())
        }
    }
}

fn run(command: Command, client: NostrElectrumClient) -> anyhow::Result<()> {
    match command {
        Command::Tip => {
            let tip = client.block_headers_subscribe()?;
            println!("height: {}", tip.height);
            println!("hash:   {}", tip.header.block_hash());
            println!("time:   {}", tip.header.time);
        }
        Command::Header { height } => {
            let header = client.block_header(height)?;
            println!("height:    {height}");
            println!("hash:      {}", header.block_hash());
            println!("prev hash: {}", header.prev_blockhash);
            println!("time:      {}", header.time);
            println!(
                "hex:       {}",
                consensus::serialize(&header).to_lower_hex_string()
            );
        }
        Command::Balance { address } => {
            let script = address_script(&client, &address)?;
            let balance = client.script_get_balance(&script)?;
            println!("confirmed:   {} sat", balance.confirmed);
            println!("unconfirmed: {} sat", balance.unconfirmed);
        }
        Command::History { address } => {
            let script = address_script(&client, &address)?;
            let history = client.script_get_history(&script)?;
            if history.is_empty() {
                println!("no transactions");
            }
            for entry in history {
                println!("{}  {}", entry.tx_hash, height_label(entry.height));
            }
        }
        Command::Utxos { address } => {
            let script = address_script(&client, &address)?;
            let utxos = client.script_list_unspent(&script)?;
            if utxos.is_empty() {
                println!("no unspent outputs");
            }
            for utxo in utxos {
                println!(
                    "{}:{}  {} sat  {}",
                    utxo.tx_hash,
                    utxo.tx_pos,
                    utxo.value,
                    height_label(utxo.height as i32)
                );
            }
        }
        Command::Tx { txid, decode } => {
            if !decode {
                let raw = client.transaction_get_raw(&txid)?;
                println!("{}", raw.to_lower_hex_string());
                return Ok(());
            }
            let network = server_network(&client)?;
            let tx = client.transaction_get(&txid)?;
            println!("txid:     {}", tx.compute_txid());
            println!("version:  {}", tx.version.0);
            println!("locktime: {}", tx.lock_time);
            println!("vsize:    {} vB", tx.vsize());
            println!("inputs:");
            for input in &tx.input {
                if tx.is_coinbase() {
                    println!("  coinbase");
                } else {
                    println!("  {}", input.previous_output);
                }
            }
            println!("outputs:");
            for (vout, output) in tx.output.iter().enumerate() {
                let dest = Address::from_script(&output.script_pubkey, network)
                    .map(|a| a.to_string())
                    .unwrap_or_else(|_| format!("script {}", output.script_pubkey));
                println!("  {vout}: {:.8} BTC  {dest}", output.value.to_btc());
            }
        }
        Command::Merkle { txid, height } => {
            let proof = client.transaction_get_merkle(&txid, height)?;
            println!("block height: {}", proof.block_height);
            println!("position:     {}", proof.pos);
            println!("merkle path:");
            for hash in proof.merkle {
                // Electrum sends each node in display (reversed) order.
                println!("  {}", hash.to_lower_hex_string());
            }
        }
        Command::Fee { blocks } => {
            let estimate = client.estimate_fee(blocks, None)?;
            let relay = client.relay_fee()?;
            if estimate < 0.0 {
                println!("estimate ({blocks} blocks): unavailable");
            } else {
                println!(
                    "estimate ({blocks} blocks): {:.2} sat/vB",
                    btc_per_kvb_to_sat_per_vb(estimate)
                );
            }
            println!(
                "relay fee:            {:.2} sat/vB",
                btc_per_kvb_to_sat_per_vb(relay)
            );
        }
        Command::Features => {
            let features = client.server_features()?;
            let network = network_from_genesis(features.genesis_hash);
            println!(
                "network:       {}",
                network.map_or("unknown".to_string(), |n| n.to_string())
            );
            println!(
                "genesis hash:  {}",
                features.genesis_hash.to_lower_hex_string()
            );
            println!(
                "protocol:      {} - {}",
                features.protocol_min, features.protocol_max
            );
            println!(
                "hash function: {}",
                features.hash_function.as_deref().unwrap_or("-")
            );
        }
        Command::Ping => {
            let start = Instant::now();
            client.ping()?;
            println!("pong in {} ms", start.elapsed().as_millis());
        }
        Command::Scan {
            descriptor,
            change_descriptor,
            stop_gap,
            batch_size,
        } => {
            let network = server_network(&client)?;
            scan::run(
                client,
                network,
                &descriptor,
                change_descriptor.as_deref(),
                stop_gap,
                batch_size,
            )?;
        }
    }
    Ok(())
}

/// The network the server serves, from the genesis hash it reports.
fn server_network(client: &NostrElectrumClient) -> anyhow::Result<Network> {
    let features = client.server_features()?;
    network_from_genesis(features.genesis_hash).with_context(|| {
        format!(
            "server reports an unknown genesis hash {}",
            features.genesis_hash.to_lower_hex_string()
        )
    })
}

/// Electrum reports the genesis hash in display order; `ChainHash` holds it
/// in internal (reversed) byte order.
fn network_from_genesis(mut genesis_hash: [u8; 32]) -> Option<Network> {
    genesis_hash.reverse();
    Network::from_chain_hash(ChainHash::from(genesis_hash))
}

/// Parse `address` for the server's network and return its scriptPubKey.
fn address_script(client: &NostrElectrumClient, address: &str) -> anyhow::Result<ScriptBuf> {
    let network = server_network(client)?;
    let address = address
        .parse::<Address<NetworkUnchecked>>()
        .with_context(|| format!("invalid address {address}"))?
        .require_network(network)
        .with_context(|| format!("{address} is not a {network} address"))?;
    Ok(address.script_pubkey())
}

/// Electrum heights: positive is confirmed, 0 is mempool, -1 is mempool with
/// unconfirmed parents.
fn height_label(height: i32) -> String {
    match height {
        h if h > 0 => format!("confirmed at {h}"),
        0 => "mempool".to_string(),
        _ => "mempool (unconfirmed parents)".to_string(),
    }
}

fn btc_per_kvb_to_sat_per_vb(btc_per_kvb: f64) -> f64 {
    btc_per_kvb * 100_000_000.0 / 1000.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use bdk_chain::bitcoin::constants::genesis_block;

    #[test]
    fn detects_network_from_electrum_genesis_hash() {
        for network in [
            Network::Bitcoin,
            Network::Testnet,
            Network::Signet,
            Network::Regtest,
        ] {
            // What electrs puts in `server.features`: the display-order hash.
            let hex = genesis_block(network).block_hash().to_string();
            let bytes: [u8; 32] =
                <[u8; 32] as bdk_chain::bitcoin::hex::FromHex>::from_hex(&hex).unwrap();
            assert_eq!(network_from_genesis(bytes), Some(network));
        }
    }

    #[test]
    fn converts_fee_units() {
        // 0.00001 BTC/kvB is 1 sat/vB.
        assert!((btc_per_kvb_to_sat_per_vb(0.00001) - 1.0).abs() < 1e-9);
    }
}
