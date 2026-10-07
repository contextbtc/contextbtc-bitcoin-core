//! Startup plumbing shared by the ContextBTC servers: the Nostr identity, the
//! relays to connect to, the client allowlist, and the server transport built
//! from them.

use contextvm_sdk::signer;
use contextvm_sdk::transport::server::{NostrServerTransport, NostrServerTransportConfig};

/// Read a comma-separated env var into a list, dropping blanks.
fn list_from_env(var: &str) -> Vec<String> {
    std::env::var(var)
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect()
}

/// Nostr relay URLs to connect to, read from the comma-separated
/// `NOSTR_RELAY_URLS` env var. Falls back to a local relay when unset/empty.
pub fn relay_urls_from_env() -> Vec<String> {
    let urls = list_from_env("NOSTR_RELAY_URLS");
    if urls.is_empty() {
        vec!["ws://localhost:10547".to_string()]
    } else {
        urls
    }
}

/// Client public keys allowed to call the server, read from the comma-separated
/// `ALLOWED_CLIENT_PUBKEYS` env var. Empty means allow all clients.
pub fn allowed_pubkeys_from_env() -> Vec<String> {
    list_from_env("ALLOWED_CLIENT_PUBKEYS")
}

/// The server's Nostr keys from `SERVER_NOSTR_SECRET_KEY`, or an ephemeral key
/// (with a warning) when unset.
pub fn server_keys_from_env() -> anyhow::Result<signer::Keys> {
    match std::env::var("SERVER_NOSTR_SECRET_KEY") {
        Ok(sk) => Ok(signer::from_sk(&sk)?),
        Err(_) => {
            eprintln!(
                "WARNING: SERVER_NOSTR_SECRET_KEY not set; generating an ephemeral key \
                 (identity will change on every restart)."
            );
            Ok(signer::generate())
        }
    }
}

/// Build the server's Nostr transport from the environment and print its
/// public key (`Public key: <hex>`; the e2e harness parses this line).
///
/// The server is not announced on relays, so only clients that already know
/// its public key can reach it, and only those on the allowlist are served.
pub async fn server_transport_from_env() -> anyhow::Result<NostrServerTransport> {
    let keys = server_keys_from_env()?;
    println!("Public key: {}", keys.public_key().to_hex());

    let transport = NostrServerTransport::new(
        keys,
        NostrServerTransportConfig::default()
            .with_relay_urls(relay_urls_from_env())
            .with_announced_server(false)
            .with_allowed_public_keys(allowed_pubkeys_from_env()),
    )
    .await?;
    Ok(transport)
}
