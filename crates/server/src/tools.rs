use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use rmcp::{
    ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::*,
    tool, tool_handler, tool_router,
};
use serde::Serialize;

use crate::params::*;
use crate::rpc::{BitcoinRpc, RpcCallError};

#[derive(Clone)]
pub struct BitcoinRpcNostrServer {
    tool_router: ToolRouter<Self>,
    rpc: Arc<BitcoinRpc>,
}

impl BitcoinRpcNostrServer {
    /// Build the server from environment variables: the bitcoind connection
    /// (see [`BitcoinRpc::from_env`]) and the `ENABLED_TOOLS` allowlist.
    pub fn from_env() -> anyhow::Result<Self> {
        let mut tool_router = Self::tool_router();

        let spec = contextbtc_common::list_from_env("ENABLED_TOOLS");
        match resolve_enabled_tools(&spec, tool_router.map.keys().map(|k| k.as_ref()))? {
            Some(enabled) => {
                restrict_tools(&mut tool_router, &enabled);
                let mut names: Vec<_> = tool_router.map.keys().map(|k| k.to_string()).collect();
                names.sort();
                tracing::info!(tools = ?names, "ENABLED_TOOLS set; exposing only these tools");
            }
            None => tracing::info!(
                count = tool_router.map.len(),
                "ENABLED_TOOLS unset; exposing all tools"
            ),
        }

        Ok(Self {
            tool_router,
            rpc: Arc::new(BitcoinRpc::from_env()),
        })
    }
}

/// Named sets of tools accepted in `ENABLED_TOOLS`, in addition to individual
/// tool names and `all`.
const TOOL_GROUPS: &[(&str, &[&str])] = &[
    // What BDK's `bdk_bitcoind_rpc` needs to sync a wallet.
    (
        "bdk",
        &[
            "getblockchaininfo",
            "getnetworkinfo",
            "getblock",
            "getblock_verbose",
            "getblockcount",
            "getblockhash",
            "getblockheader",
            "getblockheader_hex",
            "getblockfilter",
            "getrawmempool",
            "getrawtransaction",
        ],
    ),
    (
        "blockchain",
        &[
            "getbestblockhash",
            "getdifficulty",
            "getchaintips",
            "getchaintxstats",
            "getblockstats",
            "getdeploymentinfo",
            "gettxout",
            "gettxoutproof",
            "verifytxoutproof",
            "gettxspendingprevout",
        ],
    ),
    (
        "mempool",
        &[
            "getmempoolinfo",
            "getmempoolentry",
            "getmempoolancestors",
            "getmempooldescendants",
        ],
    ),
    ("fees", &["estimatesmartfee"]),
    (
        "transactions",
        &[
            "decoderawtransaction",
            "decodescript",
            "decodepsbt",
            "analyzepsbt",
            "createrawtransaction",
            "createpsbt",
            "combinepsbt",
            "joinpsbts",
            "finalizepsbt",
            "converttopsbt",
            "utxoupdatepsbt",
        ],
    ),
    (
        "util",
        &[
            "validateaddress",
            "getdescriptorinfo",
            "deriveaddresses",
            "verifymessage",
            "createmultisig",
            "getindexinfo",
        ],
    ),
    ("mining", &["getmininginfo", "getnetworkhashps"]),
];

/// Resolve an `ENABLED_TOOLS` spec (tool and group names) against the
/// `available` tool names.
///
/// Returns `None` for an empty spec, meaning every tool is enabled. Names are
/// matched by [`normalize_tool_name`], so `get_block_hash` selects
/// `getblockhash`. Unknown names are an error so a typo can't silently hide a
/// tool.
fn resolve_enabled_tools<'a>(
    spec: &[String],
    available: impl Iterator<Item = &'a str>,
) -> anyhow::Result<Option<HashSet<String>>> {
    if spec.is_empty() {
        return Ok(None);
    }

    let by_normalized: HashMap<String, &str> = available
        .map(|name| (normalize_tool_name(name), name))
        .collect();
    let resolve = |name: &str| by_normalized.get(&normalize_tool_name(name)).copied();

    let mut enabled = HashSet::new();
    let mut unknown = Vec::new();
    for entry in spec {
        let wanted = normalize_tool_name(entry);
        if wanted == "all" {
            enabled.extend(by_normalized.values().map(|n| n.to_string()));
        } else if let Some((_, members)) = TOOL_GROUPS.iter().find(|(g, _)| *g == wanted) {
            for member in *members {
                let name = resolve(member)
                    .ok_or_else(|| anyhow::anyhow!("tool group lists unknown tool {member}"))?;
                enabled.insert(name.to_string());
            }
        } else if let Some(name) = resolve(entry) {
            enabled.insert(name.to_string());
        } else {
            unknown.push(entry.as_str());
        }
    }

    if !unknown.is_empty() {
        let mut tools: Vec<_> = by_normalized.values().copied().collect();
        tools.sort();
        let groups: Vec<_> = TOOL_GROUPS.iter().map(|(g, _)| *g).collect();
        anyhow::bail!(
            "ENABLED_TOOLS has unknown entries: {}. Valid groups: all, {}. Valid tools: {}",
            unknown.join(", "),
            groups.join(", "),
            tools.join(", "),
        );
    }

    Ok(Some(enabled))
}

/// Remove every route not in `enabled`, so disabled tools are neither listed
/// nor callable (not even through a name alias).
fn restrict_tools(router: &mut ToolRouter<BitcoinRpcNostrServer>, enabled: &HashSet<String>) {
    let disabled: Vec<String> = router
        .map
        .keys()
        .filter(|name| !enabled.contains(name.as_ref()))
        .map(|name| name.to_string())
        .collect();
    for name in disabled {
        router.remove_route(&name);
    }
}

impl BitcoinRpcNostrServer {
    /// Call `method` on bitcoind and wrap the JSON result as tool output.
    async fn forward(&self, method: &str, params: Value) -> Result<CallToolResult, ErrorData> {
        let result = self
            .rpc
            .call(method, params)
            .await
            .map_err(RpcCallError::into_error_data)?;

        Ok(CallToolResult::success(vec![Content::text(
            result.to_string(),
        )]))
    }

    /// Call `method` on bitcoind with `params` serialized as JSON-RPC named
    /// parameters, so omitted optionals fall back to bitcoind's defaults.
    async fn forward_named<P: Serialize>(
        &self,
        method: &str,
        params: &P,
    ) -> Result<CallToolResult, ErrorData> {
        let params = serde_json::to_value(params).map_err(|e| {
            ErrorData::internal_error(format!("failed to encode params: {e}"), None)
        })?;
        self.forward(method, params).await
    }
}

#[tool_router]
impl BitcoinRpcNostrServer {
    #[tool(
        name = "getblockchaininfo",
        description = "Get current blockchain state (height, chain, difficulty, ...)"
    )]
    async fn get_blockchain_info(&self) -> Result<CallToolResult, ErrorData> {
        self.forward("getblockchaininfo", json!([])).await
    }

    #[tool(
        name = "getnetworkinfo",
        description = "Get network state (version, relay fee, ...); node addresses, proxies, user agent and peer counts are redacted"
    )]
    async fn get_network_info(&self) -> Result<CallToolResult, ErrorData> {
        let result = self
            .rpc
            .call("getnetworkinfo", json!([]))
            .await
            .map_err(RpcCallError::into_error_data)?;

        Ok(CallToolResult::success(vec![Content::text(
            redact_network_info(result).to_string(),
        )]))
    }

    #[tool(name = "getblock", description = "Get a block by hash")]
    async fn get_block(
        &self,
        Parameters(GetBlockParams {
            blockhash,
            verbosity,
        }): Parameters<GetBlockParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.forward("getblock", json!([blockhash, verbosity.unwrap_or(1)]))
            .await
    }

    #[tool(
        name = "getblockcount",
        description = "Get the height of the most-work fully-validated chain"
    )]
    async fn get_block_count(&self) -> Result<CallToolResult, ErrorData> {
        self.forward("getblockcount", json!([])).await
    }

    #[tool(
        name = "getblockhash",
        description = "Get the block hash at a given height"
    )]
    async fn get_block_hash(
        &self,
        Parameters(GetBlockHashParams { height }): Parameters<GetBlockHashParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.forward("getblockhash", json!([height])).await
    }

    #[tool(
        name = "getrawmempool",
        description = "Get the transaction ids in the mempool (or detailed info when verbose)"
    )]
    async fn get_raw_mempool(
        &self,
        Parameters(GetRawMempoolParams { verbose }): Parameters<GetRawMempoolParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.forward("getrawmempool", json!([verbose.unwrap_or(false)]))
            .await
    }

    #[tool(
        name = "getrawtransaction",
        description = "Get a raw transaction by txid"
    )]
    async fn get_raw_transaction(
        &self,
        Parameters(GetRawTransactionParams {
            txid,
            verbosity,
            blockhash,
        }): Parameters<GetRawTransactionParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let mut p = vec![json!(txid), json!(verbosity.unwrap_or(1))];
        if let Some(bh) = blockhash {
            p.push(json!(bh));
        }
        self.forward("getrawtransaction", Value::Array(p)).await
    }

    #[tool(
        name = "getblock_verbose",
        description = "Get a block by hash (decoded); variant of getblock"
    )]
    async fn get_block_info(
        &self,
        Parameters(GetBlockParams {
            blockhash,
            verbosity,
        }): Parameters<GetBlockParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.forward("getblock", json!([blockhash, verbosity.unwrap_or(1)]))
            .await
    }

    #[tool(
        name = "getblockheader",
        description = "Get a block header by hash (decoded JSON or raw hex)"
    )]
    async fn get_block_header_info(
        &self,
        Parameters(GetBlockHeaderParams { blockhash, verbose }): Parameters<GetBlockHeaderParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.forward(
            "getblockheader",
            json!([blockhash, verbose.unwrap_or(true)]),
        )
        .await
    }

    #[tool(
        name = "getblockheader_hex",
        description = "Get the raw hex-encoded block header by hash"
    )]
    async fn get_block_header(
        &self,
        Parameters(GetBlockHeaderRawParams { blockhash }): Parameters<GetBlockHeaderRawParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.forward("getblockheader", json!([blockhash, false]))
            .await
    }

    #[tool(
        name = "getblockfilter",
        description = "Get the BIP157 content filter for a block by hash"
    )]
    async fn get_block_filter(
        &self,
        Parameters(GetBlockFilterParams {
            blockhash,
            filtertype,
        }): Parameters<GetBlockFilterParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let filtertype = filtertype.unwrap_or_else(|| "basic".to_string());
        self.forward("getblockfilter", json!([blockhash, filtertype]))
            .await
    }

    // --- Blockchain -----------------------------------------------------------

    #[tool(
        name = "getbestblockhash",
        description = "Get the hash of the best (tip) block"
    )]
    async fn get_best_block_hash(&self) -> Result<CallToolResult, ErrorData> {
        self.forward("getbestblockhash", json!([])).await
    }

    #[tool(
        name = "getdifficulty",
        description = "Get the proof-of-work difficulty as a multiple of the minimum"
    )]
    async fn get_difficulty(&self) -> Result<CallToolResult, ErrorData> {
        self.forward("getdifficulty", json!([])).await
    }

    #[tool(
        name = "getchaintips",
        description = "Get all known tips in the block tree (main chain and orphaned branches)"
    )]
    async fn get_chain_tips(&self) -> Result<CallToolResult, ErrorData> {
        self.forward("getchaintips", json!([])).await
    }

    #[tool(
        name = "getchaintxstats",
        description = "Get statistics about the total number and rate of transactions in the chain"
    )]
    async fn get_chain_tx_stats(
        &self,
        Parameters(params): Parameters<GetChainTxStatsParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.forward_named("getchaintxstats", &params).await
    }

    #[tool(
        name = "getblockstats",
        description = "Get per-block statistics (fees, feerates, sizes, tx counts, ...) by hash or height"
    )]
    async fn get_block_stats(
        &self,
        Parameters(params): Parameters<GetBlockStatsParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.forward_named("getblockstats", &params).await
    }

    #[tool(
        name = "getdeploymentinfo",
        description = "Get the state of soft-fork deployments"
    )]
    async fn get_deployment_info(
        &self,
        Parameters(params): Parameters<GetDeploymentInfoParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.forward_named("getdeploymentinfo", &params).await
    }

    #[tool(
        name = "gettxout",
        description = "Get details about an unspent transaction output (null if spent or unknown)"
    )]
    async fn get_tx_out(
        &self,
        Parameters(params): Parameters<GetTxOutParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.forward_named("gettxout", &params).await
    }

    #[tool(
        name = "gettxoutproof",
        description = "Get a hex-encoded merkle proof that transactions are included in a block"
    )]
    async fn get_tx_out_proof(
        &self,
        Parameters(params): Parameters<GetTxOutProofParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.forward_named("gettxoutproof", &params).await
    }

    #[tool(
        name = "verifytxoutproof",
        description = "Verify a merkle proof and return the txids it commits to"
    )]
    async fn verify_tx_out_proof(
        &self,
        Parameters(params): Parameters<VerifyTxOutProofParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.forward_named("verifytxoutproof", &params).await
    }

    #[tool(
        name = "gettxspendingprevout",
        description = "Find mempool transactions spending the given outpoints"
    )]
    async fn get_tx_spending_prevout(
        &self,
        Parameters(params): Parameters<GetTxSpendingPrevoutParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.forward_named("gettxspendingprevout", &params).await
    }

    // --- Mempool --------------------------------------------------------------

    #[tool(
        name = "getmempoolinfo",
        description = "Get mempool state (size, bytes, usage, min fee, ...)"
    )]
    async fn get_mempool_info(&self) -> Result<CallToolResult, ErrorData> {
        self.forward("getmempoolinfo", json!([])).await
    }

    #[tool(
        name = "getmempoolentry",
        description = "Get mempool data for a transaction"
    )]
    async fn get_mempool_entry(
        &self,
        Parameters(params): Parameters<TxidParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.forward_named("getmempoolentry", &params).await
    }

    #[tool(
        name = "getmempoolancestors",
        description = "Get all in-mempool ancestors of a mempool transaction"
    )]
    async fn get_mempool_ancestors(
        &self,
        Parameters(params): Parameters<MempoolRelativesParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.forward_named("getmempoolancestors", &params).await
    }

    #[tool(
        name = "getmempooldescendants",
        description = "Get all in-mempool descendants of a mempool transaction"
    )]
    async fn get_mempool_descendants(
        &self,
        Parameters(params): Parameters<MempoolRelativesParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.forward_named("getmempooldescendants", &params).await
    }

    // --- Fees -----------------------------------------------------------------

    #[tool(
        name = "estimatesmartfee",
        description = "Estimate the feerate (BTC/kvB) needed to confirm within a number of blocks"
    )]
    async fn estimate_smart_fee(
        &self,
        Parameters(params): Parameters<EstimateSmartFeeParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.forward_named("estimatesmartfee", &params).await
    }

    // --- Transactions & PSBTs -------------------------------------------------

    #[tool(
        name = "decoderawtransaction",
        description = "Decode a hex-encoded transaction into JSON"
    )]
    async fn decode_raw_transaction(
        &self,
        Parameters(params): Parameters<DecodeRawTransactionParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.forward_named("decoderawtransaction", &params).await
    }

    #[tool(name = "decodescript", description = "Decode a hex-encoded script")]
    async fn decode_script(
        &self,
        Parameters(params): Parameters<DecodeScriptParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.forward_named("decodescript", &params).await
    }

    #[tool(name = "decodepsbt", description = "Decode a base64-encoded PSBT")]
    async fn decode_psbt(
        &self,
        Parameters(params): Parameters<PsbtParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.forward_named("decodepsbt", &params).await
    }

    #[tool(
        name = "analyzepsbt",
        description = "Analyze a PSBT: next role, missing data, estimated fee and size"
    )]
    async fn analyze_psbt(
        &self,
        Parameters(params): Parameters<PsbtParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.forward_named("analyzepsbt", &params).await
    }

    #[tool(
        name = "createrawtransaction",
        description = "Create an unsigned hex-encoded transaction from inputs and outputs"
    )]
    async fn create_raw_transaction(
        &self,
        Parameters(params): Parameters<CreateTransactionParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.forward_named("createrawtransaction", &params).await
    }

    #[tool(
        name = "createpsbt",
        description = "Create an unsigned PSBT from inputs and outputs"
    )]
    async fn create_psbt(
        &self,
        Parameters(params): Parameters<CreateTransactionParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.forward_named("createpsbt", &params).await
    }

    #[tool(
        name = "combinepsbt",
        description = "Combine several PSBTs for the same transaction into one"
    )]
    async fn combine_psbt(
        &self,
        Parameters(params): Parameters<PsbtListParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.forward_named("combinepsbt", &params).await
    }

    #[tool(
        name = "joinpsbts",
        description = "Join the inputs and outputs of several PSBTs into one"
    )]
    async fn join_psbts(
        &self,
        Parameters(params): Parameters<PsbtListParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.forward_named("joinpsbts", &params).await
    }

    #[tool(
        name = "finalizepsbt",
        description = "Finalize a PSBT and optionally extract the network-serialized transaction"
    )]
    async fn finalize_psbt(
        &self,
        Parameters(params): Parameters<FinalizePsbtParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.forward_named("finalizepsbt", &params).await
    }

    #[tool(
        name = "converttopsbt",
        description = "Convert a hex-encoded raw transaction into a PSBT"
    )]
    async fn convert_to_psbt(
        &self,
        Parameters(params): Parameters<ConvertToPsbtParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.forward_named("converttopsbt", &params).await
    }

    #[tool(
        name = "utxoupdatepsbt",
        description = "Add UTXO data (from the UTXO set and mempool) and descriptor info to a PSBT"
    )]
    async fn utxo_update_psbt(
        &self,
        Parameters(params): Parameters<UtxoUpdatePsbtParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.forward_named("utxoupdatepsbt", &params).await
    }

    // --- Util -----------------------------------------------------------------

    #[tool(
        name = "validateaddress",
        description = "Validate a Bitcoin address and return its script and type"
    )]
    async fn validate_address(
        &self,
        Parameters(params): Parameters<AddressParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.forward_named("validateaddress", &params).await
    }

    #[tool(
        name = "getdescriptorinfo",
        description = "Analyze a descriptor (checksum, ranged, solvable, ...)"
    )]
    async fn get_descriptor_info(
        &self,
        Parameters(params): Parameters<DescriptorParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.forward_named("getdescriptorinfo", &params).await
    }

    #[tool(
        name = "deriveaddresses",
        description = "Derive addresses from an output descriptor"
    )]
    async fn derive_addresses(
        &self,
        Parameters(params): Parameters<DeriveAddressesParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.forward_named("deriveaddresses", &params).await
    }

    #[tool(
        name = "verifymessage",
        description = "Verify a message signed with an address's key"
    )]
    async fn verify_message(
        &self,
        Parameters(params): Parameters<VerifyMessageParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.forward_named("verifymessage", &params).await
    }

    #[tool(
        name = "createmultisig",
        description = "Create an n-of-m multisig address and redeem script from public keys"
    )]
    async fn create_multisig(
        &self,
        Parameters(params): Parameters<CreateMultisigParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.forward_named("createmultisig", &params).await
    }

    #[tool(
        name = "getindexinfo",
        description = "Get the status of the node's optional indexes (txindex, blockfilterindex, ...)"
    )]
    async fn get_index_info(
        &self,
        Parameters(params): Parameters<GetIndexInfoParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.forward_named("getindexinfo", &params).await
    }

    // --- Mining ---------------------------------------------------------------

    #[tool(
        name = "getmininginfo",
        description = "Get mining-related state (difficulty, network hashrate, chain, ...)"
    )]
    async fn get_mining_info(&self) -> Result<CallToolResult, ErrorData> {
        self.forward("getmininginfo", json!([])).await
    }

    #[tool(
        name = "getnetworkhashps",
        description = "Estimate the network hash rate (hashes per second)"
    )]
    async fn get_network_hash_ps(
        &self,
        Parameters(params): Parameters<GetNetworkHashPsParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.forward_named("getnetworkhashps", &params).await
    }
}

/// Normalize a tool name for lenient matching: lowercase and drop underscores.
/// This lets clients call tools using either Bitcoin Core style
/// (`getblockhash`) or snake_case (`get_block_hash`).
fn normalize_tool_name(name: &str) -> String {
    name.chars()
        .filter(|c| *c != '_')
        .flat_map(|c| c.to_lowercase())
        .collect()
}

/// Strip host-identifying data from a `getnetworkinfo` response.
///
/// The server is only reachable over Nostr, so exposing the node's public /
/// onion addresses, proxy endpoints, user agent or peer counts would
/// deanonymize the host.
/// Values are blanked rather than removed so the response keeps the shape
/// Bitcoin Core clients expect (e.g. `GetNetworkInfoResult`).
fn redact_network_info(mut info: Value) -> Value {
    let Some(map) = info.as_object_mut() else {
        return info;
    };

    if let Some(addrs) = map.get_mut("localaddresses") {
        *addrs = json!([]);
    }

    if let Some(Value::Array(networks)) = map.get_mut("networks") {
        for network in networks.iter_mut().filter_map(Value::as_object_mut) {
            if let Some(proxy) = network.get_mut("proxy") {
                *proxy = json!("");
            }
            if let Some(randomize) = network.get_mut("proxy_randomize_credentials") {
                *randomize = json!(false);
            }
        }
    }

    for key in [
        "connections",
        "connections_in",
        "connections_out",
        "timeoffset",
    ] {
        if let Some(v) = map.get_mut(key) {
            *v = json!(0);
        }
    }

    // The user agent can carry operator-chosen `-uacomment`s that fingerprint
    // the node; keep it a string so clients still deserialize it.
    if let Some(subversion) = map.get_mut("subversion") {
        *subversion = json!("");
    }

    info
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for BitcoinRpcNostrServer {
    async fn call_tool(
        &self,
        mut request: rmcp::model::CallToolRequestParams,
        context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let requested = request.name.to_string();

        // Resolve the requested tool name to a canonical route. Bitcoin Core
        // style names (`getblockhash`) are canonical, but we also accept
        // snake_case / mixed-case variants (`get_block_hash`, `getBlockHash`)
        // by matching on a normalized form.
        let canonical = if self.tool_router.has_route(&requested) {
            Some(requested.clone())
        } else {
            let wanted = normalize_tool_name(&requested);
            self.tool_router
                .map
                .keys()
                .find(|name| normalize_tool_name(name) == wanted)
                .map(|name| name.to_string())
        };

        let Some(canonical) = canonical else {
            tracing::warn!(tool = %requested, "tool not found");
            return Err(ErrorData::invalid_params(
                format!("tool not found: {requested}"),
                Some(json!({ "tool": requested })),
            ));
        };

        if canonical != requested {
            tracing::debug!(requested = %requested, canonical = %canonical, "resolved tool alias");
            request.name = std::borrow::Cow::Owned(canonical);
        }

        let tcc = rmcp::handler::server::tool::ToolCallContext::new(self, request, context);
        self.tool_router.call(tcc).await
    }

    fn get_info(&self) -> rmcp::model::ServerInfo {
        InitializeResult::new(ServerCapabilities::builder().enable_tools().build())
            .with_protocol_version(ProtocolVersion::LATEST)
            .with_server_info(
                Implementation::new(env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"))
                    .with_title("ContextBTC — Bitcoin Core over MCP/Nostr"),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::{
        BitcoinRpcNostrServer, TOOL_GROUPS, normalize_tool_name, redact_network_info,
        resolve_enabled_tools, restrict_tools,
    };
    use crate::params::{GetBlockStatsParams, GetTxOutProofParams, HashOrHeight};
    use serde_json::json;
    use std::collections::{HashMap, HashSet};

    const EXPECTED_TOOLS: &[&str] = &[
        // Original BDK set
        "getblockchaininfo",
        "getnetworkinfo",
        "getblock",
        "getblock_verbose",
        "getblockcount",
        "getblockhash",
        "getblockheader",
        "getblockheader_hex",
        "getblockfilter",
        "getrawmempool",
        "getrawtransaction",
        // Blockchain
        "getbestblockhash",
        "getdifficulty",
        "getchaintips",
        "getchaintxstats",
        "getblockstats",
        "getdeploymentinfo",
        "gettxout",
        "gettxoutproof",
        "verifytxoutproof",
        "gettxspendingprevout",
        // Mempool
        "getmempoolinfo",
        "getmempoolentry",
        "getmempoolancestors",
        "getmempooldescendants",
        // Fees
        "estimatesmartfee",
        // Transactions & PSBTs
        "decoderawtransaction",
        "decodescript",
        "decodepsbt",
        "analyzepsbt",
        "createrawtransaction",
        "createpsbt",
        "combinepsbt",
        "joinpsbts",
        "finalizepsbt",
        "converttopsbt",
        "utxoupdatepsbt",
        // Util
        "validateaddress",
        "getdescriptorinfo",
        "deriveaddresses",
        "verifymessage",
        "createmultisig",
        "getindexinfo",
        // Mining
        "getmininginfo",
        "getnetworkhashps",
    ];

    #[test]
    fn registers_expected_tools() {
        let router = BitcoinRpcNostrServer::tool_router();
        for name in EXPECTED_TOOLS {
            assert!(router.has_route(name), "tool {name} not registered");
        }
        assert_eq!(
            router.map.len(),
            EXPECTED_TOOLS.len(),
            "unexpected extra tools"
        );
    }

    #[test]
    fn tool_names_stay_distinct_after_normalization() {
        // `call_tool` resolves aliases by normalized name, so two tools that
        // normalize to the same string would make one unreachable.
        let router = BitcoinRpcNostrServer::tool_router();
        let mut seen = HashMap::new();
        for name in router.map.keys() {
            if let Some(other) = seen.insert(normalize_tool_name(name), name.to_string()) {
                panic!("{name} and {other} normalize to the same name");
            }
        }
    }

    fn resolve(spec: &[&str]) -> anyhow::Result<Option<HashSet<String>>> {
        let spec: Vec<String> = spec.iter().map(|s| s.to_string()).collect();
        resolve_enabled_tools(&spec, EXPECTED_TOOLS.iter().copied())
    }

    fn set(names: &[&str]) -> HashSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn empty_allowlist_enables_everything() {
        assert_eq!(resolve(&[]).unwrap(), None);
    }

    #[test]
    fn allowlist_expands_groups_and_names() {
        let enabled = resolve(&["fees", "mining", "decodepsbt"]).unwrap().unwrap();
        assert_eq!(
            enabled,
            set(&[
                "estimatesmartfee",
                "getmininginfo",
                "getnetworkhashps",
                "decodepsbt"
            ])
        );

        let all = resolve(&["all"]).unwrap().unwrap();
        assert_eq!(all, set(EXPECTED_TOOLS));
    }

    #[test]
    fn allowlist_accepts_name_variants() {
        let enabled = resolve(&["get_block_hash", "GetMempoolInfo", "BDK"])
            .unwrap()
            .unwrap();
        assert!(enabled.contains("getblockhash"));
        assert!(enabled.contains("getmempoolinfo"));
        assert!(enabled.contains("getblockheader_hex"));
    }

    #[test]
    fn allowlist_rejects_unknown_names() {
        let err = resolve(&["bdk", "getblockhashh", "sendrawtransaction"]).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("getblockhashh"), "{msg}");
        assert!(msg.contains("sendrawtransaction"), "{msg}");
    }

    #[test]
    fn tool_groups_match_registered_tools() {
        let router = BitcoinRpcNostrServer::tool_router();
        let registered: HashSet<String> = router.map.keys().map(|k| k.to_string()).collect();
        let tool_names: HashSet<String> =
            registered.iter().map(|n| normalize_tool_name(n)).collect();

        let mut grouped = HashSet::new();
        for (group, members) in TOOL_GROUPS {
            assert!(
                !tool_names.contains(&normalize_tool_name(group)) && *group != "all",
                "group {group} shadows a tool name"
            );
            assert_eq!(
                *group,
                normalize_tool_name(group),
                "group {group} not normalized"
            );
            for member in *members {
                assert!(
                    registered.contains(*member),
                    "group {group} lists unknown {member}"
                );
                grouped.insert(member.to_string());
            }
        }
        assert_eq!(grouped, registered, "every tool must belong to a group");
    }

    #[test]
    fn restrict_tools_removes_disabled_routes() {
        let mut router = BitcoinRpcNostrServer::tool_router();
        restrict_tools(&mut router, &set(&["getblockcount", "estimatesmartfee"]));
        let mut left: Vec<_> = router.map.keys().map(|k| k.to_string()).collect();
        left.sort();
        assert_eq!(left, ["estimatesmartfee", "getblockcount"]);
        assert!(!router.has_route("getmininginfo"));
    }

    #[test]
    fn named_params_skip_unset_optionals() {
        let params = GetTxOutProofParams {
            txids: vec!["aa".into()],
            blockhash: None,
        };
        assert_eq!(
            serde_json::to_value(&params).unwrap(),
            json!({ "txids": ["aa"] })
        );
    }

    #[test]
    fn block_stats_accepts_hash_or_height() {
        let by_height: GetBlockStatsParams =
            serde_json::from_value(json!({ "hash_or_height": 840000 })).unwrap();
        assert!(matches!(
            by_height.hash_or_height,
            HashOrHeight::Height(840000)
        ));
        assert_eq!(
            serde_json::to_value(&by_height).unwrap(),
            json!({ "hash_or_height": 840000 })
        );

        let by_hash: GetBlockStatsParams =
            serde_json::from_value(json!({ "hash_or_height": "00ff", "stats": ["txs"] })).unwrap();
        assert!(matches!(by_hash.hash_or_height, HashOrHeight::Hash(ref h) if h == "00ff"));
        assert_eq!(
            serde_json::to_value(&by_hash).unwrap(),
            json!({ "hash_or_height": "00ff", "stats": ["txs"] })
        );
    }

    #[test]
    fn normalizes_case_and_underscores() {
        // Bitcoin Core style, snake_case, and mixed case all collapse to the
        // same canonical form.
        assert_eq!(normalize_tool_name("getblockhash"), "getblockhash");
        assert_eq!(normalize_tool_name("get_block_hash"), "getblockhash");
        assert_eq!(normalize_tool_name("getBlockHash"), "getblockhash");
        assert_eq!(normalize_tool_name("GET_BLOCK_HASH"), "getblockhash");
    }

    #[test]
    fn distinct_names_stay_distinct() {
        assert_ne!(
            normalize_tool_name("getblock"),
            normalize_tool_name("getblockheader")
        );
    }

    #[test]
    fn redacts_identifying_network_info() {
        let raw = json!({
            "version": 270000,
            "subversion": "/Satoshi:27.0.0/",
            "protocolversion": 70016,
            "localservices": "0000000000000c09",
            "localservicesnames": ["NETWORK", "WITNESS", "NETWORK_LIMITED", "P2P_V2"],
            "localrelay": true,
            "timeoffset": -2,
            "networkactive": true,
            "connections": 42,
            "connections_in": 32,
            "connections_out": 10,
            "networks": [
                { "name": "ipv4", "limited": false, "reachable": true, "proxy": "127.0.0.1:9050", "proxy_randomize_credentials": true },
                { "name": "onion", "limited": false, "reachable": true, "proxy": "127.0.0.1:9050", "proxy_randomize_credentials": true }
            ],
            "relayfee": 0.00001,
            "incrementalfee": 0.00001,
            "localaddresses": [
                { "address": "203.0.113.7", "port": 8333, "score": 4 },
                { "address": "exampleonionaddressexampleonionaddressexampleonionaddr.onion", "port": 8333, "score": 4 }
            ],
            "warnings": []
        });

        let redacted = redact_network_info(raw.clone());

        assert_eq!(redacted["localaddresses"], json!([]));
        for network in redacted["networks"].as_array().unwrap() {
            assert_eq!(network["proxy"], json!(""));
            assert_eq!(network["proxy_randomize_credentials"], json!(false));
        }
        for key in [
            "connections",
            "connections_in",
            "connections_out",
            "timeoffset",
        ] {
            assert_eq!(redacted[key], json!(0), "{key} not redacted");
        }
        assert_eq!(redacted["subversion"], json!(""));
        for key in ["version", "relayfee", "incrementalfee", "localrelay"] {
            assert_eq!(redacted[key], raw[key], "{key} changed");
        }

        let raw_keys: Vec<_> = raw.as_object().unwrap().keys().collect();
        let redacted_keys: Vec<_> = redacted.as_object().unwrap().keys().collect();
        assert_eq!(raw_keys, redacted_keys);
    }

    #[test]
    fn redaction_does_not_add_missing_keys() {
        // Pre-0.21 nodes don't report connections_in / connections_out.
        let redacted = redact_network_info(json!({ "version": 200000, "connections": 8 }));
        assert_eq!(redacted, json!({ "version": 200000, "connections": 0 }));
    }
}
