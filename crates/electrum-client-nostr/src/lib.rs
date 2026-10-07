//! An Electrum client that talks to a `contextbtc-electrs-server` over
//! MCP-over-Nostr (ContextVM) instead of the Electrum TCP protocol.
//!
//! [`NostrElectrumClient`] implements [`electrum_client::ElectrumApi`], so it
//! drops into anything generic over that trait — notably `bdk_electrum`'s
//! `BdkElectrumClient`:
//!
//! ```text
//!   BdkElectrumClient ──▶ NostrElectrumClient ──MCP/Nostr──▶ contextbtc-electrs-server ──▶ electrs
//! ```
//!
//! Each Electrum method is an MCP tool of the same name. Batches become
//! concurrent `tools/call`s. The server is read-only and does not forward
//! notifications, so broadcasting is unsupported and notification queues are
//! always empty.

use std::borrow::Borrow;
use std::fmt;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use contextvm_sdk::rmcp::ServiceExt;
use contextvm_sdk::rmcp::model::{CallToolRequestParams, RawContent};
use contextvm_sdk::rmcp::service::{RoleClient, RunningService};
use contextvm_sdk::signer;
use contextvm_sdk::transport::client::{NostrClientTransport, NostrClientTransportConfig};
use electrum_client::bitcoin::hex::FromHex;
use electrum_client::bitcoin::{Script, Txid};
use electrum_client::{
    Batch, BroadcastPackageRes, ElectrumApi, Error, EstimationMode, GetBalanceRes, GetHeadersRes,
    GetHistoryRes, GetMerkleRes, ListUnspentRes, MempoolInfoRes, Param, RawHeaderNotification,
    ScriptStatus, ServerFeaturesRes, ToElectrumScriptHash, TxidFromPosRes,
};
use futures::{StreamExt, TryStreamExt};
use log::Level::Debug;
use log::{debug, log_enabled};
use serde_json::{Map, Value, json};
use tokio::runtime::Runtime;

/// The Nostr keys a client signs with; see [`NostrElectrumClient::with_keys`].
pub use contextvm_sdk::signer::Keys;
/// The `electrum-client` crate whose [`ElectrumApi`] this client implements.
pub use electrum_client;

/// How many `tools/call`s of one batch are in flight at once.
const BATCH_CONCURRENCY: usize = 16;

/// Default time to wait for the server to answer one call.
const DEFAULT_CALL_TIMEOUT: Duration = Duration::from_secs(30);

/// MCP client handler used for the Nostr transport. It uses the default
/// [`ClientHandler`](contextvm_sdk::rmcp::ClientHandler) behaviour.
#[derive(Clone, Default)]
struct ElectrumNostrHandler;

impl contextvm_sdk::rmcp::ClientHandler for ElectrumNostrHandler {}

/// An [`ElectrumApi`] client for a `contextbtc-electrs-server`, reached over
/// Nostr.
///
/// The synchronous [`ElectrumApi`] surface is kept by blocking on an internal
/// Tokio runtime, so its methods must not be called from within an existing
/// async runtime, or the internal `block_on` will panic.
pub struct NostrElectrumClient {
    runtime: Runtime,
    service: RunningService<RoleClient, ElectrumNostrHandler>,
    server_pubkey: String,
    calls: AtomicUsize,
    call_timeout: Duration,
}

impl fmt::Debug for NostrElectrumClient {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "NostrElectrumClient(server_pubkey={})",
            self.server_pubkey
        )
    }
}

impl NostrElectrumClient {
    /// Creates a client connected to a `contextbtc-electrs-server` over Nostr,
    /// with a fresh random Nostr identity.
    ///
    /// The client connects to the given `relay_urls` and targets the server
    /// identified by `server_pubkey` (hex, npub, or nprofile).
    pub fn new(relay_urls: Vec<String>, server_pubkey: String) -> Result<Self, Error> {
        Self::with_keys(signer::generate(), relay_urls, server_pubkey)
    }

    /// Like [`new`](Self::new), but signs as `keys`. Use this when the server
    /// only serves an allowlist of client public keys.
    pub fn with_keys(
        keys: Keys,
        relay_urls: Vec<String>,
        server_pubkey: String,
    ) -> Result<Self, Error> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(Error::IOError)?;

        let pubkey = server_pubkey.clone();
        let service = runtime.block_on(async move {
            let transport = NostrClientTransport::new(
                keys,
                NostrClientTransportConfig::default()
                    .with_relay_urls(relay_urls)
                    .with_stateless(true)
                    .with_server_pubkey(pubkey),
            )
            .await
            .map_err(mcp_error)?;

            ElectrumNostrHandler
                .serve(transport)
                .await
                .map_err(mcp_error)
        })?;

        Ok(Self {
            runtime,
            service,
            server_pubkey,
            calls: AtomicUsize::new(0),
            call_timeout: DEFAULT_CALL_TIMEOUT,
        })
    }

    /// How long to wait for the server to answer one call (default 30s).
    ///
    /// Nothing on the Nostr side reports an unreachable server, so without a
    /// reply this is what turns a wrong server public key or a stopped server
    /// into an error.
    pub fn set_call_timeout(&mut self, timeout: Duration) {
        self.call_timeout = timeout;
    }

    /// Call the tool `method` with named `args` and parse its JSON result.
    async fn call_async(&self, method: &str, args: Value) -> Result<Value, Error> {
        self.calls.fetch_add(1, Ordering::Relaxed);

        if log_enabled!(Debug) {
            debug!(target: "contextbtc_electrum_client", "MCP tools/call: {method} {args}");
        }

        let mut request = CallToolRequestParams::new(method.to_string());
        if let Value::Object(arguments) = args
            && !arguments.is_empty()
        {
            request = request.with_arguments(arguments);
        }

        let result = tokio::time::timeout(self.call_timeout, self.service.call_tool(request))
            .await
            .map_err(|_| {
                Error::Message(format!(
                    "no reply to {method} within {:?}; check the server public key and that \
                     the server is running and connected to the same relays",
                    self.call_timeout
                ))
            })?
            .map_err(mcp_error)?;

        // The first textual content block holds the JSON-encoded result.
        let text = result
            .content
            .iter()
            .find_map(|c| match &c.raw {
                RawContent::Text(t) => Some(t.text.clone()),
                _ => None,
            })
            .unwrap_or_else(|| "null".to_string());

        if log_enabled!(Debug) {
            debug!(target: "contextbtc_electrum_client", "MCP tools/call result for {method}: is_error={:?} text={text}", result.is_error);
        }

        if result.is_error == Some(true) {
            return Err(Error::Protocol(Value::String(text)));
        }
        Ok(serde_json::from_str(&text)?)
    }

    fn call(&self, method: &str, args: Value) -> Result<Value, Error> {
        self.runtime.block_on(self.call_async(method, args))
    }

    /// Call `method` once per item of `args`, concurrently, keeping the order.
    fn batch(&self, method: &str, args: Vec<Value>) -> Result<Vec<Value>, Error> {
        self.runtime.block_on(
            futures::stream::iter(args)
                .map(|args| self.call_async(method, args))
                .buffered(BATCH_CONCURRENCY)
                .try_collect(),
        )
    }

    fn batch_scripts<'s, I>(&self, method: &str, scripts: I) -> Result<Vec<Value>, Error>
    where
        I: IntoIterator,
        I::Item: Borrow<&'s Script>,
    {
        let args = scripts
            .into_iter()
            .map(|s| scripthash_args(s.borrow()))
            .collect();
        self.batch(method, args)
    }
}

fn mcp_error(e: impl fmt::Display) -> Error {
    Error::Message(format!("MCP: {e}"))
}

fn unsupported(what: &str) -> Error {
    Error::Message(format!("{what} is not supported over contextbtc"))
}

fn scripthash_args(script: &Script) -> Value {
    // `ScriptHash` serializes as the hex form Electrum expects.
    json!({ "scripthash": script.to_electrum_scripthash() })
}

fn parse<T: serde::de::DeserializeOwned>(value: Value) -> Result<T, Error> {
    Ok(serde_json::from_value(value)?)
}

fn parse_all<T: serde::de::DeserializeOwned>(values: Vec<Value>) -> Result<Vec<T>, Error> {
    values.into_iter().map(parse).collect()
}

/// Decode a JSON hex string into bytes.
fn hex_bytes(value: Value) -> Result<Vec<u8>, Error> {
    let hex = value
        .as_str()
        .ok_or_else(|| Error::InvalidResponse(value.clone()))?;
    Ok(Vec::<u8>::from_hex(hex)?)
}

/// Named parameters, in Electrum's positional order, of each tool the server
/// exposes. Used to turn positional [`Param`]s into MCP arguments.
fn tool_params(method: &str) -> Option<&'static [&'static str]> {
    Some(match method {
        "blockchain.headers.subscribe"
        | "blockchain.relayfee"
        | "mempool.get_fee_histogram"
        | "server.features"
        | "server.ping" => &[],
        "blockchain.block.header" => &["height", "cp_height"],
        "blockchain.block.headers" => &["start_height", "count"],
        "blockchain.estimatefee" => &["number"],
        "blockchain.scripthash.get_balance"
        | "blockchain.scripthash.get_history"
        | "blockchain.scripthash.get_mempool"
        | "blockchain.scripthash.listunspent"
        | "blockchain.scripthash.subscribe" => &["scripthash"],
        "blockchain.transaction.get" => &["tx_hash"],
        "blockchain.transaction.get_merkle" => &["tx_hash", "height"],
        "blockchain.transaction.id_from_pos" => &["height", "tx_pos", "merkle"],
        _ => return None,
    })
}

/// Zip positional Electrum params with the tool's parameter names.
fn named_args(method: &str, params: impl IntoIterator<Item = Param>) -> Result<Value, Error> {
    let names = tool_params(method).ok_or_else(|| unsupported(method))?;
    let params: Vec<Param> = params.into_iter().collect();
    if params.len() > names.len() {
        return Err(Error::Message(format!(
            "{method} takes at most {} parameter(s) over contextbtc",
            names.len()
        )));
    }
    let mut args = Map::new();
    for (name, param) in names.iter().zip(params) {
        args.insert((*name).to_string(), serde_json::to_value(param)?);
    }
    Ok(Value::Object(args))
}

/// Build a [`GetHeadersRes`] from a `blockchain.block.headers` result in either
/// the protocol 1.4 (`hex`, concatenated) or 1.6 (`headers`, array) format.
fn parse_headers(value: Value) -> Result<GetHeadersRes, Error> {
    let raw: Vec<Vec<u8>> = match (value.get("hex"), value.get("headers")) {
        (Some(hex), _) => hex_bytes(hex.clone())?
            .chunks(80)
            .map(<[u8]>::to_vec)
            .collect(),
        (None, Some(Value::Array(headers))) => headers
            .iter()
            .cloned()
            .map(hex_bytes)
            .collect::<Result<_, _>>()?,
        _ => return Err(Error::InvalidResponse(value)),
    };
    // `GetHeadersRes` can't be built from outside its crate; its `headers`
    // field is skipped by serde and filled in below.
    let mut res: GetHeadersRes = serde_json::from_value(json!({
        "max": value.get("max").cloned().unwrap_or(json!(0)),
        "count": value.get("count").cloned().unwrap_or(json!(raw.len())),
    }))?;
    res.headers = raw
        .iter()
        .map(|h| electrum_client::bitcoin::consensus::deserialize(h))
        .collect::<Result<_, _>>()?;
    Ok(res)
}

impl ElectrumApi for NostrElectrumClient {
    fn raw_call(
        &self,
        method_name: &str,
        params: impl IntoIterator<Item = Param>,
    ) -> Result<Value, Error> {
        let args = named_args(method_name, params)?;
        self.call(method_name, args)
    }

    fn batch_call(&self, batch: &Batch) -> Result<Vec<Value>, Error> {
        let calls = batch
            .iter()
            .map(|(method, params)| Ok((method.clone(), named_args(method, params.clone())?)))
            .collect::<Result<Vec<_>, Error>>()?;
        self.runtime.block_on(
            futures::stream::iter(calls)
                .map(|(method, args)| async move { self.call_async(&method, args).await })
                .buffered(BATCH_CONCURRENCY)
                .try_collect(),
        )
    }

    fn block_headers_subscribe_raw(&self) -> Result<RawHeaderNotification, Error> {
        parse(self.call("blockchain.headers.subscribe", json!({}))?)
    }

    fn block_headers_pop_raw(&self) -> Result<Option<RawHeaderNotification>, Error> {
        // The server does not forward notifications.
        Ok(None)
    }

    fn block_header_raw(&self, height: usize) -> Result<Vec<u8>, Error> {
        hex_bytes(self.call("blockchain.block.header", json!({ "height": height }))?)
    }

    fn block_headers(&self, start_height: usize, count: usize) -> Result<GetHeadersRes, Error> {
        parse_headers(self.call(
            "blockchain.block.headers",
            json!({ "start_height": start_height, "count": count }),
        )?)
    }

    fn estimate_fee(&self, number: usize, _mode: Option<EstimationMode>) -> Result<f64, Error> {
        // The estimation mode needs protocol 1.6; electrs speaks 1.4.
        parse(self.call("blockchain.estimatefee", json!({ "number": number }))?)
    }

    fn relay_fee(&self) -> Result<f64, Error> {
        parse(self.call("blockchain.relayfee", json!({}))?)
    }

    fn script_subscribe(&self, script: &Script) -> Result<Option<ScriptStatus>, Error> {
        parse(self.call("blockchain.scripthash.subscribe", scripthash_args(script))?)
    }

    fn batch_script_subscribe<'s, I>(&self, scripts: I) -> Result<Vec<Option<ScriptStatus>>, Error>
    where
        I: IntoIterator + Clone,
        I::Item: Borrow<&'s Script>,
    {
        parse_all(self.batch_scripts("blockchain.scripthash.subscribe", scripts)?)
    }

    fn script_unsubscribe(&self, _script: &Script) -> Result<bool, Error> {
        // Subscribing only reads the current status; there is nothing to undo.
        Ok(true)
    }

    fn script_pop(&self, _script: &Script) -> Result<Option<ScriptStatus>, Error> {
        // The server does not forward notifications.
        Ok(None)
    }

    fn script_get_balance(&self, script: &Script) -> Result<GetBalanceRes, Error> {
        parse(self.call("blockchain.scripthash.get_balance", scripthash_args(script))?)
    }

    fn batch_script_get_balance<'s, I>(&self, scripts: I) -> Result<Vec<GetBalanceRes>, Error>
    where
        I: IntoIterator + Clone,
        I::Item: Borrow<&'s Script>,
    {
        parse_all(self.batch_scripts("blockchain.scripthash.get_balance", scripts)?)
    }

    fn script_get_history(&self, script: &Script) -> Result<Vec<GetHistoryRes>, Error> {
        parse(self.call("blockchain.scripthash.get_history", scripthash_args(script))?)
    }

    fn batch_script_get_history<'s, I>(&self, scripts: I) -> Result<Vec<Vec<GetHistoryRes>>, Error>
    where
        I: IntoIterator + Clone,
        I::Item: Borrow<&'s Script>,
    {
        parse_all(self.batch_scripts("blockchain.scripthash.get_history", scripts)?)
    }

    fn script_list_unspent(&self, script: &Script) -> Result<Vec<ListUnspentRes>, Error> {
        parse(self.call("blockchain.scripthash.listunspent", scripthash_args(script))?)
    }

    fn batch_script_list_unspent<'s, I>(
        &self,
        scripts: I,
    ) -> Result<Vec<Vec<ListUnspentRes>>, Error>
    where
        I: IntoIterator + Clone,
        I::Item: Borrow<&'s Script>,
    {
        parse_all(self.batch_scripts("blockchain.scripthash.listunspent", scripts)?)
    }

    fn transaction_get_raw(&self, txid: &Txid) -> Result<Vec<u8>, Error> {
        hex_bytes(self.call(
            "blockchain.transaction.get",
            json!({ "tx_hash": txid.to_string() }),
        )?)
    }

    fn batch_transaction_get_raw<'t, I>(&self, txids: I) -> Result<Vec<Vec<u8>>, Error>
    where
        I: IntoIterator + Clone,
        I::Item: Borrow<&'t Txid>,
    {
        let args = txids
            .into_iter()
            .map(|txid| json!({ "tx_hash": txid.borrow().to_string() }))
            .collect();
        self.batch("blockchain.transaction.get", args)?
            .into_iter()
            .map(hex_bytes)
            .collect()
    }

    fn batch_block_header_raw<I>(&self, heights: I) -> Result<Vec<Vec<u8>>, Error>
    where
        I: IntoIterator + Clone,
        I::Item: Borrow<u32>,
    {
        let args = heights
            .into_iter()
            .map(|height| json!({ "height": *height.borrow() }))
            .collect();
        self.batch("blockchain.block.header", args)?
            .into_iter()
            .map(hex_bytes)
            .collect()
    }

    fn batch_estimate_fee<I>(&self, numbers: I) -> Result<Vec<f64>, Error>
    where
        I: IntoIterator + Clone,
        I::Item: Borrow<usize>,
    {
        let args = numbers
            .into_iter()
            .map(|number| json!({ "number": *number.borrow() }))
            .collect();
        parse_all(self.batch("blockchain.estimatefee", args)?)
    }

    fn transaction_broadcast_raw(&self, _raw_tx: &[u8]) -> Result<Txid, Error> {
        Err(unsupported("transaction broadcast"))
    }

    fn transaction_broadcast_package_raw<T: AsRef<[u8]>>(
        &self,
        _raw_txs: &[T],
    ) -> Result<BroadcastPackageRes, Error> {
        Err(unsupported("package broadcast"))
    }

    fn transaction_get_merkle(&self, txid: &Txid, height: usize) -> Result<GetMerkleRes, Error> {
        parse(self.call(
            "blockchain.transaction.get_merkle",
            json!({ "tx_hash": txid.to_string(), "height": height }),
        )?)
    }

    fn batch_transaction_get_merkle<I>(
        &self,
        txids_and_heights: I,
    ) -> Result<Vec<GetMerkleRes>, Error>
    where
        I: IntoIterator + Clone,
        I::Item: Borrow<(Txid, usize)>,
    {
        let args = txids_and_heights
            .into_iter()
            .map(|item| {
                let (txid, height) = item.borrow();
                json!({ "tx_hash": txid.to_string(), "height": height })
            })
            .collect();
        parse_all(self.batch("blockchain.transaction.get_merkle", args)?)
    }

    fn txid_from_pos(&self, height: usize, tx_pos: usize) -> Result<Txid, Error> {
        parse(self.call(
            "blockchain.transaction.id_from_pos",
            json!({ "height": height, "tx_pos": tx_pos }),
        )?)
    }

    fn txid_from_pos_with_merkle(
        &self,
        height: usize,
        tx_pos: usize,
    ) -> Result<TxidFromPosRes, Error> {
        parse(self.call(
            "blockchain.transaction.id_from_pos",
            json!({ "height": height, "tx_pos": tx_pos, "merkle": true }),
        )?)
    }

    fn server_features(&self) -> Result<ServerFeaturesRes, Error> {
        parse(self.call("server.features", json!({}))?)
    }

    fn mempool_get_info(&self) -> Result<MempoolInfoRes, Error> {
        // `mempool.get_info` is protocol 1.6; electrs speaks 1.4.
        Err(unsupported("mempool.get_info"))
    }

    fn ping(&self) -> Result<(), Error> {
        self.call("server.ping", json!({}))?;
        Ok(())
    }

    fn calls_made(&self) -> Result<usize, Error> {
        Ok(self.calls.load(Ordering::Relaxed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use electrum_client::bitcoin::consensus::serialize;
    use electrum_client::bitcoin::constants::genesis_block;
    use electrum_client::bitcoin::hex::DisplayHex;
    use electrum_client::bitcoin::{Network, ScriptBuf};

    #[test]
    fn positional_params_become_named_args() {
        let args = named_args(
            "blockchain.transaction.get_merkle",
            [Param::String("ab".repeat(32)), Param::Usize(7)],
        )
        .unwrap();
        assert_eq!(args, json!({ "tx_hash": "ab".repeat(32), "height": 7 }));
    }

    #[test]
    fn unknown_methods_and_extra_params_are_rejected() {
        assert!(
            named_args(
                "blockchain.transaction.broadcast",
                [Param::String("00".into())]
            )
            .is_err()
        );
        assert!(
            named_args(
                "blockchain.transaction.get",
                [Param::String("ab".repeat(32)), Param::Bool(true)]
            )
            .is_err()
        );
    }

    #[test]
    fn scripthash_is_electrum_hex() {
        // Electrum's documented example: P2PKH for 1A1zP1eP5QGefi2DMPTfTL5SLmv7DivfNa.
        let script =
            ScriptBuf::from_hex("76a91462e907b15cbf27d5425399ebf6f0fb50ebb88f1888ac").unwrap();
        assert_eq!(
            scripthash_args(&script),
            json!({ "scripthash": "8b01df4e368ea28f8dc0423bcf7a4923e3a12d307c875e47a0cfbf90b5c39161" })
        );
    }

    #[test]
    fn parses_legacy_and_v16_headers() {
        let genesis = serialize(&genesis_block(Network::Regtest).header);
        let hex = genesis.to_lower_hex_string();

        let legacy =
            parse_headers(json!({ "count": 2, "hex": format!("{hex}{hex}"), "max": 2016 }))
                .unwrap();
        assert_eq!(
            (legacy.count, legacy.max, legacy.headers.len()),
            (2, 2016, 2)
        );

        let v16 = parse_headers(json!({ "count": 1, "headers": [hex], "max": 2016 })).unwrap();
        assert_eq!(v16.headers[0], genesis_block(Network::Regtest).header);
    }
}
