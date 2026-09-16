use serde_json::{Value, json};

use rmcp::model::ErrorData;

pub struct BitcoinRpc {
    http: reqwest::Client,
    url: String, // e.g. "http://127.0.0.1:8332"
    user: String,
    password: String,
}

impl BitcoinRpc {
    /// Build the RPC client from environment variables, applying sensible
    /// defaults for the URL and timeout.
    pub fn from_env() -> Self {
        let timeout_secs = std::env::var("BITCOIN_RPC_TIMEOUT_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(30);
        Self {
            http: reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(5))
                .timeout(std::time::Duration::from_secs(timeout_secs))
                .build()
                .expect("failed to build HTTP client"),
            url: std::env::var("BITCOIN_RPC_URL")
                .unwrap_or_else(|_| "http://127.0.0.1:8332".to_string()),
            user: std::env::var("BITCOIN_RPC_USER").unwrap_or_default(),
            password: std::env::var("BITCOIN_RPC_PASSWORD").unwrap_or_default(),
        }
    }

    /// Perform an RPC call, retrying transient failures (connect/timeout errors
    /// and 5xx/429 responses) with exponential backoff. Permanent failures
    /// (auth errors, malformed responses, application-level RPC errors) fail
    /// immediately.
    pub async fn call(&self, method: &str, params: Value) -> Result<Value, RpcCallError> {
        use backon::{ExponentialBuilder, Retryable};

        let policy = ExponentialBuilder::default()
            .with_max_times(3)
            .with_jitter();

        (|| async { self.call_once(method, &params).await })
            .retry(policy)
            .when(|e: &RpcCallError| e.retryable)
            .notify(|e: &RpcCallError, dur| {
                tracing::warn!(
                    method = %method,
                    error = %e.source,
                    retry_in = ?dur,
                    "bitcoind RPC call failed; retrying"
                );
            })
            .await
    }

    async fn call_once(&self, method: &str, params: &Value) -> Result<Value, RpcCallError> {
        let body = json!({
            "jsonrpc": "2.0",
            "id": "contextbtc",
            "method": method,
            "params": params,
        });

        let resp = self
            .http
            .post(&self.url)
            .basic_auth(&self.user, Some(&self.password))
            .json(&body)
            .send()
            .await
            .map_err(|e| {
                // Connection and timeout failures are typically transient.
                let retryable = e.is_timeout() || e.is_connect();
                RpcCallError::transport(retryable, e.into())
            })?;

        // Read the status and body once. bitcoind returns non-JSON bodies
        // (often plain text or HTML) on transport-level errors, so we must not
        // blindly parse as JSON.
        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| RpcCallError::transport(true, e.into()))?;

        if !status.is_success() {
            let hint = match status {
                reqwest::StatusCode::UNAUTHORIZED => {
                    " (check BITCOIN_RPC_USER / BITCOIN_RPC_PASSWORD)"
                }
                reqwest::StatusCode::FORBIDDEN => {
                    " (client not allowed; check bitcoind rpcallowip / rpcbind)"
                }
                _ => "",
            };
            let snippet: String = text.trim().chars().take(200).collect();
            // Server errors and rate limiting are transient; 4xx are not.
            let retryable =
                status.is_server_error() || status == reqwest::StatusCode::TOO_MANY_REQUESTS;
            let err = anyhow::anyhow!("bitcoind HTTP {status}{hint}: {snippet}");
            return Err(RpcCallError::unavailable(retryable, err));
        }

        let resp: Value = serde_json::from_str(&text).map_err(|_| {
            let snippet: String = text.trim().chars().take(200).collect();
            RpcCallError::unavailable(
                false,
                anyhow::anyhow!("bitcoind returned a non-JSON response: {snippet}"),
            )
        })?;

        if let Some(err) = resp.get("error").filter(|e| !e.is_null()) {
            return Err(RpcCallError::rpc(err));
        }
        Ok(resp.get("result").cloned().unwrap_or(Value::Null))
    }
}

/// An error from a single RPC attempt, tagged with whether it is worth retrying.
///
/// The full `source` is only ever logged server-side. Clients receive a fixed
/// message chosen from `kind`, so nothing bitcoind (or a proxy in front of it)
/// puts in a response body, and no node URL or configuration detail, is
/// relayed verbatim.
pub struct RpcCallError {
    retryable: bool,
    kind: ErrorKind,
    source: anyhow::Error,
}

enum ErrorKind {
    /// The node could not be reached or did not return a usable response.
    Unavailable,
    /// bitcoind answered with a JSON-RPC error; only its numeric code is kept
    /// for the client.
    Rpc { code: Option<i64> },
}

impl RpcCallError {
    /// A transport-level error derived from `reqwest`. May contain the node
    /// URL.
    fn transport(retryable: bool, source: anyhow::Error) -> Self {
        Self::unavailable(retryable, source)
    }

    /// The node was unreachable or returned an unusable response.
    fn unavailable(retryable: bool, source: anyhow::Error) -> Self {
        Self {
            retryable,
            kind: ErrorKind::Unavailable,
            source,
        }
    }

    /// An application-level JSON-RPC error object returned by bitcoind.
    fn rpc(err: &Value) -> Self {
        Self {
            retryable: false,
            kind: ErrorKind::Rpc {
                code: err.get("code").and_then(Value::as_i64),
            },
            source: anyhow::anyhow!("bitcoind RPC error: {err}"),
        }
    }

    /// Convert into a client-facing MCP error. The detailed error is logged
    /// server-side; the client only sees a generic message (plus the RPC error
    /// code, when there is one).
    pub fn into_error_data(self) -> ErrorData {
        match self.kind {
            ErrorKind::Unavailable => {
                tracing::error!(error = %self.source, "bitcoind request failed");
                ErrorData::internal_error(
                    "failed to query the Bitcoin node (see server logs)".to_string(),
                    None,
                )
            }
            ErrorKind::Rpc { code } => {
                tracing::warn!(error = %self.source, "bitcoind returned an RPC error");
                rpc_error_data(code)
            }
        }
    }
}

/// Map a Bitcoin Core RPC error code (see `src/rpc/protocol.h`) to a fixed,
/// client-safe MCP error.
fn rpc_error_data(code: Option<i64>) -> ErrorData {
    let data = code.map(|code| json!({ "rpc_code": code }));
    match code {
        // RPC_INVALID_ADDRESS_OR_KEY
        Some(-5) => ErrorData::invalid_params("block or transaction not found", data),
        // RPC_INVALID_PARAMETER
        Some(-8) => ErrorData::invalid_params("invalid parameter", data),
        // RPC_TYPE_ERROR, RPC_DESERIALIZATION_ERROR, RPC_INVALID_PARAMS
        Some(-3 | -22 | -32602) => ErrorData::invalid_params("malformed parameter", data),
        // RPC_IN_WARMUP
        Some(-28) => {
            ErrorData::internal_error("Bitcoin node is starting up; try again later", data)
        }
        // RPC_CLIENT_NOT_CONNECTED, RPC_CLIENT_IN_INITIAL_DOWNLOAD
        Some(-9 | -10) => ErrorData::internal_error("Bitcoin node is not ready", data),
        // RPC_METHOD_NOT_FOUND
        Some(-32601) => ErrorData::internal_error("method not available on the Bitcoin node", data),
        _ => ErrorData::internal_error("the Bitcoin node rejected the request", data),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::ErrorCode;

    fn message(err: RpcCallError) -> String {
        let data = err.into_error_data();
        serde_json::to_string(&data).unwrap()
    }

    #[test]
    fn http_error_body_is_not_relayed() {
        let err = RpcCallError::unavailable(
            false,
            anyhow::anyhow!("bitcoind HTTP 502: <html>nginx/1.25 at node.internal:8332</html>"),
        );
        let msg = message(err);
        assert!(!msg.contains("nginx"), "{msg}");
        assert!(!msg.contains("node.internal"), "{msg}");
    }

    #[test]
    fn rpc_error_message_is_not_relayed() {
        let err = RpcCallError::rpc(&json!({
            "code": -1,
            "message": "Block not available (pruned data)",
        }));
        let msg = message(err);
        assert!(!msg.contains("pruned"), "{msg}");
        assert!(msg.contains("-1"), "{msg}");
    }

    #[test]
    fn known_rpc_codes_map_to_fixed_messages() {
        let data = RpcCallError::rpc(&json!({
            "code": -5,
            "message": "No such mempool or blockchain transaction. Use gettransaction for wallet transactions.",
        }))
        .into_error_data();
        assert_eq!(data.code, ErrorCode::INVALID_PARAMS);
        assert_eq!(data.message, "block or transaction not found");
        assert_eq!(data.data, Some(json!({ "rpc_code": -5 })));
    }

    #[test]
    fn rpc_error_without_code() {
        let data = RpcCallError::rpc(&json!({ "message": "secret detail" })).into_error_data();
        assert_eq!(data.code, ErrorCode::INTERNAL_ERROR);
        assert_eq!(data.data, None);
    }
}
