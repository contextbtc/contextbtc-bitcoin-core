mod params;
mod rpc;
mod tools;

use rmcp::ServiceExt;

use tools::BitcoinRpcNostrServer;

/// Run the ContextBTC MCP server until it is shut down.
pub async fn run() -> anyhow::Result<()> {
    // Build the server first so config errors (e.g. a typo in ENABLED_TOOLS)
    // surface before connecting to any relay.
    let server = BitcoinRpcNostrServer::from_env()?;
    let transport = contextbtc_common::server_transport_from_env().await?;

    let service = server.serve(transport).await?;
    println!("Server ready. Press Ctrl+C to stop.");
    service.waiting().await?;
    Ok(())
}
