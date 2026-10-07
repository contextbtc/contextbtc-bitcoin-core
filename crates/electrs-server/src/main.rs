#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Load variables from a local `.env` file if present. Real environment
    // variables always take precedence and a missing file is not an error.
    dotenvy::dotenv().ok();

    tracing_subscriber::fmt::init();

    contextbtc_electrs_server::run().await
}
