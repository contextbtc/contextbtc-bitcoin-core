use contextvm_sdk::signer;
use contextvm_sdk::transport::client::{NostrClientTransport, NostrClientTransportConfig};
use rmcp::{ClientHandler, ServiceExt, model::CallToolRequestParams};

#[derive(Clone, Default)]
struct BitcoinRpcNostrClient;
impl ClientHandler for BitcoinRpcNostrClient {}

/// Nostr relay URLs to connect to, read from the comma-separated
/// `NOSTR_RELAY_URLS` env var. Falls back to a local relay when unset/empty.
fn relay_urls_from_env() -> Vec<String> {
    let urls: Vec<String> = std::env::var("NOSTR_RELAY_URLS")
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect();

    if urls.is_empty() {
        vec!["ws://localhost:10547".to_string()]
    } else {
        urls
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Load variables from a local `.env` file if present. Real environment
    // variables always take precedence and a missing file is not an error.
    dotenvy::dotenv().ok();

    tracing_subscriber::fmt::init();

    let server_pubkey = match std::env::args().nth(1) {
        Some(pubkey) => pubkey,
        None => {
            eprintln!("Usage: contextbtc-client <server_pubkey>");
            std::process::exit(1);
        }
    };

    run(server_pubkey).await
}

async fn run(server_pubkey: String) -> anyhow::Result<()> {
    let signer = match std::env::var("CLIENT_NOSTR_SECRET_KEY") {
        Ok(sk) => signer::from_sk(&sk)?,
        Err(_) => {
            eprintln!(
                "WARNING: CLIENT_NOSTR_SECRET_KEY not set; generating an ephemeral key \
                 (identity will change on every restart)."
            );
            signer::generate()
        }
    };
    let client_pubkey = signer.public_key().to_hex();
    println!("Client starting with public key: {client_pubkey}. Target Server: {server_pubkey}");

    let transport = NostrClientTransport::new(
        signer,
        NostrClientTransportConfig::default()
            .with_relay_urls(relay_urls_from_env())
            .with_server_pubkey(server_pubkey),
    )
    .await?;

    let client = BitcoinRpcNostrClient.serve(transport).await?;

    let tools = client.list_all_tools().await?;
    println!("Discovered {} tool(s).", tools.len());

    let result = client
        .call_tool(CallToolRequestParams::new("getblockchaininfo"))
        .await?;

    if let Some(content) = result.content.first()
        && let rmcp::model::RawContent::Text(text) = &content.raw
    {
        println!("Blockchain info: {}", text.text);
    }

    let result = client
        .call_tool(CallToolRequestParams::new("getblockcount"))
        .await?;

    if let Some(content) = result.content.first()
        && let rmcp::model::RawContent::Text(text) = &content.raw
    {
        println!("Block count: {}", text.text);
    }

    let arguments = serde_json::from_value(serde_json::json!({
        "verbosity": 0,
    }))?;

    let result = client
        .call_tool(CallToolRequestParams::new("get_network_info").with_arguments(arguments))
        .await?;

    if let Some(content) = result.content.first()
        && let rmcp::model::RawContent::Text(text) = &content.raw
    {
        println!("Network info: {}", text.text);
    }

    client.cancel().await?;
    Ok(())
}
