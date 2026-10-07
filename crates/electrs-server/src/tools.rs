use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::sync::Arc;

use rmcp::{
    ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::*,
    schemars, tool, tool_handler, tool_router,
};

use crate::electrum::{ElectrumCallError, ElectrumClient};

/// Most headers `blockchain.block.headers` returns in one call (one
/// difficulty period; also electrs' own limit).
const MAX_HEADERS: u32 = 2016;

// Tools are named after the Electrum protocol method they proxy, so a client
// maps each call 1:1. Not exposed:
// - `blockchain.transaction.broadcast`: the server is read-only.
// - `server.banner`, `server.donation_address`, `server.peers.subscribe`:
//   operator-chosen text and peer lists that would fingerprint the host
//   (see `redact_server_features`).
#[derive(Clone)]
pub struct ElectrumNostrServer {
    tool_router: ToolRouter<Self>,
    electrum: Arc<ElectrumClient>,
}

impl ElectrumNostrServer {
    pub fn new() -> Self {
        Self {
            tool_router: Self::tool_router(),
            electrum: Arc::new(ElectrumClient::from_env()),
        }
    }

    /// Call electrs and return the JSON result as a single text block.
    async fn call_text(&self, method: &str, params: Value) -> Result<CallToolResult, ErrorData> {
        let result = self.call(method, params).await?;
        Ok(text_result(&result))
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value, ErrorData> {
        self.electrum
            .call(method, params)
            .await
            .map_err(ElectrumCallError::into_error_data)
    }
}

impl Default for ElectrumNostrServer {
    fn default() -> Self {
        Self::new()
    }
}

fn text_result(value: &Value) -> CallToolResult {
    CallToolResult::success(vec![Content::text(value.to_string())])
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct BlockHeaderParams {
    /// Block height
    height: u32,
    /// Optional checkpoint height; when non-zero, a merkle proof to that
    /// checkpoint is returned too
    #[serde(default)]
    cp_height: Option<u32>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct BlockHeadersParams {
    /// Height of the first header
    start_height: u32,
    /// Number of headers (at most 2016)
    count: u32,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct EstimateFeeParams {
    /// Confirmation target, in blocks
    number: u32,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct ScripthashParams {
    /// Electrum script hash: sha256 of the scriptPubKey, byte-reversed, as hex
    scripthash: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct TransactionGetParams {
    /// Transaction id (hex)
    tx_hash: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct TransactionGetMerkleParams {
    /// Transaction id (hex)
    tx_hash: String,
    /// Height of the block containing the transaction
    height: u32,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct IdFromPosParams {
    /// Block height
    height: u32,
    /// Position of the transaction in the block
    tx_pos: u32,
    /// If true, also return the merkle path
    #[serde(default)]
    merkle: Option<bool>,
}

/// Reject anything that isn't a 32-byte hex string before it reaches electrs.
fn check_hash32(field: &str, value: &str) -> Result<(), ErrorData> {
    if value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err(ErrorData::invalid_params(
            format!("{field} must be 64 hex characters"),
            None,
        ))
    }
}

#[tool_router]
impl ElectrumNostrServer {
    #[tool(
        name = "blockchain.headers.subscribe",
        description = "Get the current chain tip: {height, hex} (notifications are not forwarded)"
    )]
    async fn headers_subscribe(&self) -> Result<CallToolResult, ErrorData> {
        self.call_text("blockchain.headers.subscribe", json!([]))
            .await
    }

    #[tool(
        name = "blockchain.block.header",
        description = "Get the raw hex block header at a height"
    )]
    async fn block_header(
        &self,
        Parameters(BlockHeaderParams { height, cp_height }): Parameters<BlockHeaderParams>,
    ) -> Result<CallToolResult, ErrorData> {
        // electrs only accepts `cp_height` when it supports checkpoints, so
        // leave it out unless the client asked for one.
        let params = match cp_height {
            Some(cp_height) => json!([height, cp_height]),
            None => json!([height]),
        };
        self.call_text("blockchain.block.header", params).await
    }

    #[tool(
        name = "blockchain.block.headers",
        description = "Get a run of consecutive raw block headers: {count, hex, max}"
    )]
    async fn block_headers(
        &self,
        Parameters(BlockHeadersParams {
            start_height,
            count,
        }): Parameters<BlockHeadersParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.call_text(
            "blockchain.block.headers",
            json!([start_height, count.min(MAX_HEADERS)]),
        )
        .await
    }

    #[tool(
        name = "blockchain.estimatefee",
        description = "Estimate the fee rate (BTC/kvB) to confirm within a number of blocks; -1 if unknown"
    )]
    async fn estimate_fee(
        &self,
        Parameters(EstimateFeeParams { number }): Parameters<EstimateFeeParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.call_text("blockchain.estimatefee", json!([number]))
            .await
    }

    #[tool(
        name = "blockchain.relayfee",
        description = "Get the node's minimum relay fee (BTC/kvB)"
    )]
    async fn relay_fee(&self) -> Result<CallToolResult, ErrorData> {
        self.call_text("blockchain.relayfee", json!([])).await
    }

    #[tool(
        name = "blockchain.scripthash.get_balance",
        description = "Get the confirmed and unconfirmed balance of a script hash"
    )]
    async fn scripthash_get_balance(
        &self,
        Parameters(ScripthashParams { scripthash }): Parameters<ScripthashParams>,
    ) -> Result<CallToolResult, ErrorData> {
        check_hash32("scripthash", &scripthash)?;
        self.call_text("blockchain.scripthash.get_balance", json!([scripthash]))
            .await
    }

    #[tool(
        name = "blockchain.scripthash.get_history",
        description = "Get the confirmed and mempool transaction history of a script hash"
    )]
    async fn scripthash_get_history(
        &self,
        Parameters(ScripthashParams { scripthash }): Parameters<ScripthashParams>,
    ) -> Result<CallToolResult, ErrorData> {
        check_hash32("scripthash", &scripthash)?;
        self.call_text("blockchain.scripthash.get_history", json!([scripthash]))
            .await
    }

    #[tool(
        name = "blockchain.scripthash.get_mempool",
        description = "Get the mempool transactions of a script hash"
    )]
    async fn scripthash_get_mempool(
        &self,
        Parameters(ScripthashParams { scripthash }): Parameters<ScripthashParams>,
    ) -> Result<CallToolResult, ErrorData> {
        check_hash32("scripthash", &scripthash)?;
        self.call_text("blockchain.scripthash.get_mempool", json!([scripthash]))
            .await
    }

    #[tool(
        name = "blockchain.scripthash.listunspent",
        description = "Get the unspent outputs of a script hash"
    )]
    async fn scripthash_listunspent(
        &self,
        Parameters(ScripthashParams { scripthash }): Parameters<ScripthashParams>,
    ) -> Result<CallToolResult, ErrorData> {
        check_hash32("scripthash", &scripthash)?;
        self.call_text("blockchain.scripthash.listunspent", json!([scripthash]))
            .await
    }

    #[tool(
        name = "blockchain.scripthash.subscribe",
        description = "Get the current status hash of a script hash, or null if it has no history (notifications are not forwarded)"
    )]
    async fn scripthash_subscribe(
        &self,
        Parameters(ScripthashParams { scripthash }): Parameters<ScripthashParams>,
    ) -> Result<CallToolResult, ErrorData> {
        check_hash32("scripthash", &scripthash)?;
        // Derived from the history rather than proxied: a real subscription
        // would pile up on the shared electrs connection for every script
        // hash any client ever asked about.
        let history = self
            .call("blockchain.scripthash.get_history", json!([scripthash]))
            .await?;
        let status = scripthash_status(&history).ok_or_else(|| {
            tracing::error!(history = %history, "unexpected get_history response from electrs");
            ErrorData::internal_error(
                "failed to query the Electrum server (see server logs)".to_string(),
                None,
            )
        })?;
        Ok(text_result(&status))
    }

    #[tool(
        name = "blockchain.transaction.get",
        description = "Get a raw transaction (hex) by txid"
    )]
    async fn transaction_get(
        &self,
        Parameters(TransactionGetParams { tx_hash }): Parameters<TransactionGetParams>,
    ) -> Result<CallToolResult, ErrorData> {
        check_hash32("tx_hash", &tx_hash)?;
        self.call_text("blockchain.transaction.get", json!([tx_hash, false]))
            .await
    }

    #[tool(
        name = "blockchain.transaction.get_merkle",
        description = "Get the merkle proof of a confirmed transaction: {block_height, pos, merkle}"
    )]
    async fn transaction_get_merkle(
        &self,
        Parameters(TransactionGetMerkleParams { tx_hash, height }): Parameters<
            TransactionGetMerkleParams,
        >,
    ) -> Result<CallToolResult, ErrorData> {
        check_hash32("tx_hash", &tx_hash)?;
        self.call_text(
            "blockchain.transaction.get_merkle",
            json!([tx_hash, height]),
        )
        .await
    }

    #[tool(
        name = "blockchain.transaction.id_from_pos",
        description = "Get the txid at a position in a block, optionally with its merkle path"
    )]
    async fn transaction_id_from_pos(
        &self,
        Parameters(IdFromPosParams {
            height,
            tx_pos,
            merkle,
        }): Parameters<IdFromPosParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.call_text(
            "blockchain.transaction.id_from_pos",
            json!([height, tx_pos, merkle.unwrap_or(false)]),
        )
        .await
    }

    #[tool(
        name = "mempool.get_fee_histogram",
        description = "Get the mempool fee histogram: [[fee_rate, vsize], ...]"
    )]
    async fn mempool_get_fee_histogram(&self) -> Result<CallToolResult, ErrorData> {
        self.call_text("mempool.get_fee_histogram", json!([])).await
    }

    #[tool(
        name = "server.features",
        description = "Get the Electrum server's features (genesis hash, protocol versions, hash function); hosts and server version are redacted"
    )]
    async fn server_features(&self) -> Result<CallToolResult, ErrorData> {
        let result = self.call("server.features", json!([])).await?;
        Ok(text_result(&redact_server_features(result)))
    }

    #[tool(name = "server.ping", description = "Check that the server is alive")]
    async fn server_ping(&self) -> Result<CallToolResult, ErrorData> {
        // Answered locally: it only proves the MCP server is reachable.
        Ok(text_result(&Value::Null))
    }
}

/// Electrum status of a script hash from its `get_history` response: the hex
/// sha256 of the concatenated `tx_hash:height:` entries, or `null` when the
/// history is empty. Returns `None` if `history` isn't a well-formed history.
fn scripthash_status(history: &Value) -> Option<Value> {
    let entries = history.as_array()?;
    if entries.is_empty() {
        return Some(Value::Null);
    }
    let mut hasher = Sha256::new();
    for entry in entries {
        let tx_hash = entry.get("tx_hash")?.as_str()?;
        let height = entry.get("height")?.as_i64()?;
        hasher.update(format!("{tx_hash}:{height}:"));
    }
    let digest: String = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    Some(json!(digest))
}

/// Strip host-identifying data from a `server.features` response.
///
/// The server is only reachable over Nostr, so exposing the electrs host names
/// and ports, or its exact version, would deanonymize the host. Values are
/// blanked rather than removed so the response keeps the shape Electrum
/// clients expect.
fn redact_server_features(mut features: Value) -> Value {
    let Some(map) = features.as_object_mut() else {
        return features;
    };
    if let Some(hosts) = map.get_mut("hosts") {
        *hosts = json!({});
    }
    if let Some(version) = map.get_mut("server_version") {
        *version = json!("");
    }
    features
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for ElectrumNostrServer {
    fn get_info(&self) -> rmcp::model::ServerInfo {
        InitializeResult::new(ServerCapabilities::builder().enable_tools().build())
            .with_protocol_version(ProtocolVersion::LATEST)
            .with_server_info(
                Implementation::new(env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"))
                    .with_title("ContextBTC — electrs over MCP/Nostr"),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::{check_hash32, redact_server_features, scripthash_status};
    use serde_json::json;

    #[test]
    fn redacts_identifying_server_features() {
        let raw = json!({
            "genesis_hash": "0f9188f13cb7b2c71f2a335e3a4fc328bf5beb436012afca590b1a11466e2206",
            "hash_function": "sha256",
            "hosts": { "electrs.example.org": { "tcp_port": 50001, "ssl_port": null } },
            "protocol_max": "1.4",
            "protocol_min": "1.4",
            "pruning": null,
            "server_version": "electrs/0.11.0"
        });

        let redacted = redact_server_features(raw.clone());

        assert_eq!(redacted["hosts"], json!({}));
        assert_eq!(redacted["server_version"], json!(""));
        for key in [
            "genesis_hash",
            "hash_function",
            "protocol_max",
            "protocol_min",
            "pruning",
        ] {
            assert_eq!(redacted[key], raw[key], "{key} changed");
        }
    }

    #[test]
    fn redaction_does_not_add_missing_keys() {
        let redacted = redact_server_features(json!({ "protocol_max": "1.4" }));
        assert_eq!(redacted, json!({ "protocol_max": "1.4" }));
    }

    #[test]
    fn empty_history_has_null_status() {
        assert_eq!(scripthash_status(&json!([])), Some(json!(null)));
    }

    #[test]
    fn status_hashes_history_entries() {
        // sha256("aa…aa:5:bb…bb:0:")
        let history = json!([
            { "tx_hash": "a".repeat(64), "height": 5 },
            { "tx_hash": "b".repeat(64), "height": 0, "fee": 141 },
        ]);
        assert_eq!(
            scripthash_status(&history),
            Some(json!(
                "79a127da650a5d2524323656cf496efeb49ecf05f6f338e2c64b8e27789ac553"
            ))
        );
    }

    #[test]
    fn malformed_history_has_no_status() {
        assert_eq!(scripthash_status(&json!({ "error": "x" })), None);
        assert_eq!(scripthash_status(&json!([{ "height": 1 }])), None);
    }

    #[test]
    fn validates_32_byte_hex() {
        assert!(check_hash32("tx_hash", &"0f".repeat(32)).is_ok());
        assert!(check_hash32("tx_hash", &"0f".repeat(31)).is_err());
        assert!(check_hash32("tx_hash", &"zz".repeat(32)).is_err());
    }
}
