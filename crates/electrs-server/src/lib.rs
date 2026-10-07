mod electrum;
mod tools;

use rmcp::ServiceExt;

use tools::ElectrumNostrServer;

/// Run the ContextBTC electrs MCP server until it is shut down.
pub async fn run() -> anyhow::Result<()> {
    let transport = contextbtc_common::server_transport_from_env().await?;

    let service = ElectrumNostrServer::new().serve(transport).await?;
    println!("Server ready. Press Ctrl+C to stop.");
    service.waiting().await?;
    Ok(())
}
