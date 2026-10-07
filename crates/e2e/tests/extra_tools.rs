//! End-to-end check of the read-only RPCs exposed beyond the BDK set.
//!
//! ```text
//!   bitcoincore_rpc::Client ──MCP/Nostr──▶ nak serve (relay) ──▶ contextbtc-server ──JSON-RPC──▶ bitcoind (regtest)
//! ```
//!
//! These tools forward their arguments to bitcoind as JSON-RPC named
//! parameters, so this also covers omitted optionals (bitcoind defaults) and
//! skipped interior arguments.

#![allow(clippy::print_stdout, clippy::print_stderr)]

use bitcoincore_rpc::RpcApi;
use serde_json::{Value, json};

mod harness;

use harness::Stack;

/// Regtest genesis block hash; a fresh node's tip.
const REGTEST_GENESIS: &str = "0f9188f13cb7b2c71f2a335e3a4fc328bf5beb436012afca590b1a11466e2206";

#[test]
fn extra_tools_over_nostr() -> anyhow::Result<()> {
    let stack = Stack::start(&[])?;
    let client =
        bitcoincore_rpc::Client::new(vec![stack.relay_url.clone()], stack.server_pubkey.clone())?;

    let call = |cmd: &str, args: &[Value]| -> anyhow::Result<Value> {
        let res = client.call::<Value>(cmd, args)?;
        println!("{cmd} -> {res}");
        Ok(res)
    };

    // Blockchain
    assert_eq!(call("getbestblockhash", &[])?, json!(REGTEST_GENESIS));
    assert!(call("getdifficulty", &[])?.is_number());
    assert!(call("getchaintips", &[])?.is_array());
    // No args at all: named params serialize to `{}`.
    assert!(call("getdeploymentinfo", &[])?.get("deployments").is_some());

    // Mempool
    assert_eq!(call("getmempoolinfo", &[])?["size"], json!(0));

    // Fees: a fresh regtest node has no estimates, but still answers.
    assert!(call("estimatesmartfee", &[json!(6)])?.is_object());

    // Transactions & PSBTs
    let script = call("decodescript", &[json!("51")])?;
    assert!(script.get("type").is_some(), "decodescript: {script}");

    // Util
    let addr = call(
        "validateaddress",
        &[json!("bcrt1qw508d6qejxtdg4y5r3zarvary0c5xw7kygt080")],
    )?;
    assert!(addr["isvalid"].is_boolean(), "validateaddress: {addr}");
    assert!(call("getindexinfo", &[])?.is_object());

    // Mining
    assert_eq!(call("getmininginfo", &[])?["chain"], json!("regtest"));
    // Interior optional skipped (`nblocks`), later one set (`height`).
    assert!(call("getnetworkhashps", &[Value::Null, json!(0)])?.is_number());

    // Every new tool is advertised.
    let tools = client.list_tool_names()?;
    for name in [
        "getblockstats",
        "gettxout",
        "gettxoutproof",
        "gettxspendingprevout",
        "getmempoolentry",
        "decodepsbt",
        "createpsbt",
        "utxoupdatepsbt",
        "deriveaddresses",
        "createmultisig",
    ] {
        assert!(tools.iter().any(|t| t == name), "{name} not advertised");
    }

    Ok(())
}
