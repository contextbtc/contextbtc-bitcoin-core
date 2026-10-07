//! End-to-end check of the `ENABLED_TOOLS` allowlist: only the selected tools
//! are advertised and callable over MCP-over-Nostr.

#![allow(clippy::print_stdout, clippy::print_stderr)]

use bitcoincore_rpc::RpcApi;
use serde_json::Value;

mod harness;

use harness::Stack;

#[test]
fn enabled_tools_allowlist_over_nostr() -> anyhow::Result<()> {
    let stack = Stack::start_with_server_env(&[], &[("ENABLED_TOOLS", "bdk, estimatesmartfee")])?;
    let client =
        bitcoincore_rpc::Client::new(vec![stack.relay_url.clone()], stack.server_pubkey.clone())?;

    let mut tools = client.list_tool_names()?;
    tools.sort();
    assert_eq!(
        tools,
        [
            "estimatesmartfee",
            "getblock",
            "getblock_verbose",
            "getblockchaininfo",
            "getblockcount",
            "getblockfilter",
            "getblockhash",
            "getblockheader",
            "getblockheader_hex",
            "getnetworkinfo",
            "getrawmempool",
            "getrawtransaction",
        ]
    );

    assert_eq!(client.get_block_count()?, 0);
    assert!(
        client
            .call::<Value>("estimatesmartfee", &[6.into()])
            .is_ok()
    );

    let err = client
        .call::<Value>("getmininginfo", &[])
        .expect_err("getmininginfo should not be exposed");
    println!("getmininginfo -> {err}");

    Ok(())
}
