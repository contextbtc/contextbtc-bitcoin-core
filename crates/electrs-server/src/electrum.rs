use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use rmcp::model::ErrorData;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::Mutex;

/// Client name and protocol version sent in the `server.version` handshake.
/// electrs 0.10/0.11 speak protocol 1.4.
const CLIENT_NAME: &str = "contextbtc";
const PROTOCOL_VERSION: &str = "1.4";

/// A client for an Electrum protocol server (electrs): newline-delimited
/// JSON-RPC 2.0 over a plain TCP connection.
///
/// A single connection is opened lazily and shared; calls are serialized over
/// it. Any transport failure drops the connection so the next attempt opens a
/// fresh one.
pub struct ElectrumClient {
    addr: String, // e.g. "127.0.0.1:50001"
    connect_timeout: Duration,
    timeout: Duration,
    conn: Mutex<Option<Conn>>,
    next_id: AtomicU64,
}

struct Conn {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
}

impl ElectrumClient {
    /// Build the client from environment variables, applying sensible
    /// defaults for the address and timeout.
    pub fn from_env() -> Self {
        let timeout_secs = std::env::var("ELECTRS_TIMEOUT_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(30);
        Self {
            addr: std::env::var("ELECTRS_ADDR").unwrap_or_else(|_| "127.0.0.1:50001".to_string()),
            connect_timeout: Duration::from_secs(5),
            timeout: Duration::from_secs(timeout_secs),
            conn: Mutex::new(None),
            next_id: AtomicU64::new(0),
        }
    }

    /// Perform an Electrum call, retrying transient failures (connect, I/O and
    /// timeout errors) with exponential backoff. Errors returned by the
    /// Electrum server itself fail immediately.
    pub async fn call(&self, method: &str, params: Value) -> Result<Value, ElectrumCallError> {
        use backon::{ExponentialBuilder, Retryable};

        let policy = ExponentialBuilder::default()
            .with_max_times(3)
            .with_jitter();

        (|| async { self.call_once(method, &params).await })
            .retry(policy)
            .when(|e: &ElectrumCallError| e.retryable)
            .notify(|e: &ElectrumCallError, dur| {
                tracing::warn!(
                    method = %method,
                    error = %e.source,
                    retry_in = ?dur,
                    "electrs call failed; retrying"
                );
            })
            .await
    }

    async fn call_once(&self, method: &str, params: &Value) -> Result<Value, ElectrumCallError> {
        let mut guard = self.conn.lock().await;

        if guard.is_none() {
            *guard = Some(self.connect().await?);
        }
        let conn = guard.as_mut().expect("connection was just opened");

        let result = self.exchange(conn, method, params).await;
        // A transport failure leaves the stream in an unknown state (a reply
        // may still be in flight), so it can't be reused. Errors returned by
        // electrs are complete replies and keep the connection.
        if matches!(&result, Err(e) if matches!(e.kind, ErrorKind::Unavailable)) {
            *guard = None;
        }
        result
    }

    /// Open a connection and negotiate the protocol version.
    async fn connect(&self) -> Result<Conn, ElectrumCallError> {
        let stream = tokio::time::timeout(self.connect_timeout, TcpStream::connect(&self.addr))
            .await
            .map_err(|_| {
                ElectrumCallError::unavailable(
                    true,
                    anyhow::anyhow!("timed out connecting to electrs at {}", self.addr),
                )
            })?
            .map_err(|e| {
                ElectrumCallError::unavailable(
                    true,
                    anyhow::anyhow!("failed to connect to electrs at {}: {e}", self.addr),
                )
            })?;
        let (read, writer) = stream.into_split();
        let mut conn = Conn {
            reader: BufReader::new(read),
            writer,
        };

        let version = self
            .exchange(
                &mut conn,
                "server.version",
                &json!([CLIENT_NAME, PROTOCOL_VERSION]),
            )
            .await?;
        tracing::debug!(addr = %self.addr, version = %version, "connected to electrs");
        Ok(conn)
    }

    /// Send one request on `conn` and wait for its response, bounded by the
    /// call timeout.
    async fn exchange(
        &self,
        conn: &mut Conn,
        method: &str,
        params: &Value,
    ) -> Result<Value, ElectrumCallError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        tokio::time::timeout(self.timeout, exchange(conn, id, method, params))
            .await
            .map_err(|_| {
                ElectrumCallError::unavailable(
                    true,
                    anyhow::anyhow!("electrs did not answer {method} within {:?}", self.timeout),
                )
            })?
    }
}

async fn exchange(
    conn: &mut Conn,
    id: u64,
    method: &str,
    params: &Value,
) -> Result<Value, ElectrumCallError> {
    let request = json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    });
    let mut line = request.to_string();
    line.push('\n');
    conn.writer
        .write_all(line.as_bytes())
        .await
        .map_err(|e| ElectrumCallError::unavailable(true, e.into()))?;

    loop {
        let mut line = String::new();
        let n = conn
            .reader
            .read_line(&mut line)
            .await
            .map_err(|e| ElectrumCallError::unavailable(true, e.into()))?;
        if n == 0 {
            return Err(ElectrumCallError::unavailable(
                true,
                anyhow::anyhow!("electrs closed the connection"),
            ));
        }

        let msg: Value = serde_json::from_str(&line).map_err(|_| {
            let snippet: String = line.trim().chars().take(200).collect();
            ElectrumCallError::unavailable(
                false,
                anyhow::anyhow!("electrs returned a non-JSON message: {snippet}"),
            )
        })?;

        // Subscription notifications carry no id; we never forward them.
        if msg.get("id").and_then(Value::as_u64) != Some(id) {
            tracing::trace!(message = %msg, "skipping unrelated electrs message");
            continue;
        }

        if let Some(err) = msg.get("error").filter(|e| !e.is_null()) {
            return Err(ElectrumCallError::electrum(err));
        }
        return Ok(msg.get("result").cloned().unwrap_or(Value::Null));
    }
}

/// An error from a single Electrum call attempt, tagged with whether it is
/// worth retrying.
///
/// The full `source` is only ever logged server-side. Clients receive a fixed
/// message chosen from `kind`, so nothing electrs puts in an error message, and
/// no backend address, is relayed verbatim.
pub struct ElectrumCallError {
    retryable: bool,
    kind: ErrorKind,
    source: anyhow::Error,
}

enum ErrorKind {
    /// electrs could not be reached or did not return a usable response.
    Unavailable,
    /// electrs answered with a JSON-RPC error; only its numeric code is kept
    /// for the client.
    Electrum { code: Option<i64> },
}

impl ElectrumCallError {
    /// electrs was unreachable or returned an unusable response.
    fn unavailable(retryable: bool, source: anyhow::Error) -> Self {
        Self {
            retryable,
            kind: ErrorKind::Unavailable,
            source,
        }
    }

    /// An application-level JSON-RPC error object returned by electrs.
    fn electrum(err: &Value) -> Self {
        Self {
            retryable: false,
            kind: ErrorKind::Electrum {
                code: err.get("code").and_then(Value::as_i64),
            },
            source: anyhow::anyhow!("electrs error: {err}"),
        }
    }

    /// Convert into a client-facing MCP error. The detailed error is logged
    /// server-side; the client only sees a generic message (plus the Electrum
    /// error code, when there is one).
    pub fn into_error_data(self) -> ErrorData {
        match self.kind {
            ErrorKind::Unavailable => {
                tracing::error!(error = %self.source, "electrs request failed");
                ErrorData::internal_error(
                    "failed to query the Electrum server (see server logs)".to_string(),
                    None,
                )
            }
            ErrorKind::Electrum { code } => {
                tracing::warn!(error = %self.source, "electrs returned an error");
                electrum_error_data(code)
            }
        }
    }
}

/// Map an electrs error code to a fixed, client-safe MCP error.
fn electrum_error_data(code: Option<i64>) -> ErrorData {
    let data = code.map(|code| json!({ "electrum_code": code }));
    match code {
        // JSON-RPC invalid request / invalid params
        Some(-32600 | -32602) => ErrorData::invalid_params("malformed parameter", data),
        // JSON-RPC method not found
        Some(-32601) => {
            ErrorData::internal_error("method not available on the Electrum server", data)
        }
        // electrs RpcError::BadRequest
        Some(1) => ErrorData::invalid_params("the Electrum server rejected the request", data),
        // electrs RpcError::DaemonError (e.g. unknown transaction)
        Some(2) => ErrorData::invalid_params("the Bitcoin node rejected the request", data),
        _ => ErrorData::internal_error("the Electrum server rejected the request", data),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::ErrorCode;

    fn message(err: ElectrumCallError) -> String {
        serde_json::to_string(&err.into_error_data()).unwrap()
    }

    #[test]
    fn transport_error_is_not_relayed() {
        let err = ElectrumCallError::unavailable(
            true,
            anyhow::anyhow!("failed to connect to electrs at electrs.internal:50001"),
        );
        let msg = message(err);
        assert!(!msg.contains("electrs.internal"), "{msg}");
    }

    #[test]
    fn electrum_error_message_is_not_relayed() {
        let err = ElectrumCallError::electrum(&json!({
            "code": 2,
            "message": "daemon error: DaemonError { code: -5, message: \"No such mempool or blockchain transaction\" }",
        }));
        let data = err.into_error_data();
        assert_eq!(data.code, ErrorCode::INVALID_PARAMS);
        assert_eq!(data.message, "the Bitcoin node rejected the request");
        assert_eq!(data.data, Some(json!({ "electrum_code": 2 })));
        let msg = serde_json::to_string(&data).unwrap();
        assert!(!msg.contains("mempool"), "{msg}");
    }

    #[test]
    fn electrum_error_without_code() {
        let data =
            ElectrumCallError::electrum(&json!({ "message": "secret detail" })).into_error_data();
        assert_eq!(data.code, ErrorCode::INTERNAL_ERROR);
        assert_eq!(data.data, None);
    }

    /// Serve canned Electrum replies on a local socket: the handshake, an
    /// unrelated notification, then the answer to the first real request.
    #[tokio::test]
    async fn skips_notifications_and_matches_ids() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();

        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read, mut write) = stream.into_split();
            let mut lines = BufReader::new(read).lines();
            while let Some(line) = lines.next_line().await.unwrap() {
                let req: Value = serde_json::from_str(&line).unwrap();
                let id = req["id"].clone();
                let reply = match req["method"].as_str().unwrap() {
                    "server.version" => {
                        json!({ "jsonrpc": "2.0", "id": id, "result": ["electrs/0.11.0", "1.4"] })
                    }
                    _ => {
                        let note = json!({ "jsonrpc": "2.0", "method": "blockchain.headers.subscribe", "params": [{ "height": 7, "hex": "00" }] });
                        write
                            .write_all(format!("{note}\n").as_bytes())
                            .await
                            .unwrap();
                        json!({ "jsonrpc": "2.0", "id": id, "result": { "height": 6, "hex": "ff" } })
                    }
                };
                write
                    .write_all(format!("{reply}\n").as_bytes())
                    .await
                    .unwrap();
            }
        });

        let client = ElectrumClient {
            addr,
            connect_timeout: Duration::from_secs(5),
            timeout: Duration::from_secs(5),
            conn: Mutex::new(None),
            next_id: AtomicU64::new(0),
        };
        let result = client
            .call("blockchain.headers.subscribe", json!([]))
            .await
            .unwrap_or_else(|e| panic!("{}", e.source));
        assert_eq!(result, json!({ "height": 6, "hex": "ff" }));
    }
}
