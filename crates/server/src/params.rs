//! Input parameters for the MCP tools.
//!
//! Field names match Bitcoin Core's own argument names. The newer tools are
//! forwarded to bitcoind as JSON-RPC *named* parameters (the struct serialized
//! as an object), so optional fields that are `None` are skipped and bitcoind
//! applies its own defaults.

use rmcp::schemars;
use serde::{Deserialize, Serialize};
use serde_json::Value;

// --- Parameters for the original (positional) tools -------------------------

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(crate) struct GetBlockParams {
    /// Block hash (hex)
    pub(crate) blockhash: String,
    /// Verbosity: 0=hex, 1=json, 2=json with tx details
    #[serde(default)]
    pub(crate) verbosity: Option<u8>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(crate) struct GetBlockHashParams {
    /// Block height
    pub(crate) height: u64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(crate) struct GetRawMempoolParams {
    /// If true, return detailed info for each tx; otherwise just an array of txids
    #[serde(default)]
    pub(crate) verbose: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(crate) struct GetRawTransactionParams {
    /// Transaction id (hex)
    pub(crate) txid: String,
    /// Verbosity: 0=hex, 1=json, 2=json with fee/prevout details
    #[serde(default)]
    pub(crate) verbosity: Option<u8>,
    /// Optional block hash (hex) the transaction is contained in
    #[serde(default)]
    pub(crate) blockhash: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(crate) struct GetBlockHeaderParams {
    /// Block hash (hex)
    pub(crate) blockhash: String,
    /// If true, return decoded JSON; otherwise the raw hex header
    #[serde(default)]
    pub(crate) verbose: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(crate) struct GetBlockHeaderRawParams {
    /// Block hash (hex)
    pub(crate) blockhash: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(crate) struct GetBlockFilterParams {
    /// Block hash (hex)
    pub(crate) blockhash: String,
    /// Filter type (e.g. "basic")
    #[serde(default)]
    pub(crate) filtertype: Option<String>,
}

// --- Blockchain ---------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct GetChainTxStatsParams {
    /// Size of the window in number of blocks (default: one month)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) nblocks: Option<u64>,
    /// Hash (hex) of the block that ends the window (default: chain tip)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) blockhash: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct GetBlockStatsParams {
    /// Block hash (hex string) or block height (number)
    pub(crate) hash_or_height: HashOrHeight,
    /// Only return these stats (e.g. ["avgfee", "txs"]); default: all
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) stats: Option<Vec<String>>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(untagged)]
pub(crate) enum HashOrHeight {
    Height(u64),
    Hash(String),
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct GetDeploymentInfoParams {
    /// Block hash (hex) to evaluate deployments at (default: chain tip)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) blockhash: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct GetTxOutParams {
    /// Transaction id (hex)
    pub(crate) txid: String,
    /// Output index
    pub(crate) n: u32,
    /// Whether to include the mempool (default: true)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) include_mempool: Option<bool>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct GetTxOutProofParams {
    /// Transaction ids (hex) to prove; all must be in the same block
    pub(crate) txids: Vec<String>,
    /// Hash (hex) of the block containing the transactions
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) blockhash: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct VerifyTxOutProofParams {
    /// Hex-encoded proof, as returned by gettxoutproof
    pub(crate) proof: String,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct Outpoint {
    /// Transaction id (hex)
    pub(crate) txid: String,
    /// Output index
    pub(crate) vout: u32,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct GetTxSpendingPrevoutParams {
    /// Outpoints to look up spenders for in the mempool
    pub(crate) outputs: Vec<Outpoint>,
}

// --- Mempool ------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct TxidParams {
    /// Transaction id (hex)
    pub(crate) txid: String,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct MempoolRelativesParams {
    /// Transaction id (hex) of a mempool transaction
    pub(crate) txid: String,
    /// If true, return detailed info for each tx; otherwise just txids
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) verbose: Option<bool>,
}

// --- Fees ---------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct EstimateSmartFeeParams {
    /// Confirmation target in blocks (1 - 1008)
    pub(crate) conf_target: u32,
    /// Estimate mode: "economical" or "conservative"
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) estimate_mode: Option<String>,
}

// --- Transactions & PSBTs -----------------------------------------------------

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct DecodeRawTransactionParams {
    /// Serialized transaction (hex)
    pub(crate) hexstring: String,
    /// Whether the transaction is segwit-serialized (default: try both)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) iswitness: Option<bool>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct DecodeScriptParams {
    /// Script (hex)
    pub(crate) hexstring: String,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct PsbtParams {
    /// Base64-encoded PSBT
    pub(crate) psbt: String,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct TxInput {
    /// Transaction id (hex) of the output being spent
    pub(crate) txid: String,
    /// Output index being spent
    pub(crate) vout: u32,
    /// Sequence number (default depends on `replaceable` and `locktime`)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) sequence: Option<u32>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct CreateTransactionParams {
    /// Inputs to spend
    pub(crate) inputs: Vec<TxInput>,
    /// Outputs: array of objects, each either {"<address>": <amount in BTC>} or {"data": "<hex>"}
    pub(crate) outputs: Value,
    /// Raw locktime (default: 0)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) locktime: Option<u32>,
    /// Signal BIP125 replaceability (default: true)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) replaceable: Option<bool>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct PsbtListParams {
    /// Base64-encoded PSBTs
    pub(crate) txs: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct FinalizePsbtParams {
    /// Base64-encoded PSBT
    pub(crate) psbt: String,
    /// If complete, also extract the network-serialized transaction (default: true)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) extract: Option<bool>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct ConvertToPsbtParams {
    /// Serialized raw transaction (hex)
    pub(crate) hexstring: String,
    /// Drop existing signatures instead of failing on them (default: false)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) permitsigdata: Option<bool>,
    /// Whether the transaction is segwit-serialized (default: try both)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) iswitness: Option<bool>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct UtxoUpdatePsbtParams {
    /// Base64-encoded PSBT
    pub(crate) psbt: String,
    /// Descriptors to add input/output info from: strings or {"desc": "...", "range": n | [begin, end]}
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) descriptors: Option<Value>,
}

// --- Util ---------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct AddressParams {
    /// Bitcoin address
    pub(crate) address: String,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct DescriptorParams {
    /// Output descriptor
    pub(crate) descriptor: String,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct DeriveAddressesParams {
    /// Output descriptor
    pub(crate) descriptor: String,
    /// For ranged descriptors: end index (number) or [begin, end] range
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) range: Option<Value>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct VerifyMessageParams {
    /// Address that signed the message
    pub(crate) address: String,
    /// Base64-encoded signature
    pub(crate) signature: String,
    /// The signed message
    pub(crate) message: String,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct CreateMultisigParams {
    /// Number of required signatures
    pub(crate) nrequired: u32,
    /// Hex-encoded public keys
    pub(crate) keys: Vec<String>,
    /// Address type: "legacy", "p2sh-segwit" or "bech32" (default: "legacy")
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) address_type: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct GetIndexInfoParams {
    /// Only report on this index (e.g. "txindex"); default: all
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) index_name: Option<String>,
}

// --- Mining -------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct GetNetworkHashPsParams {
    /// Number of blocks to average over; -1 = since last difficulty change (default: 120)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) nblocks: Option<i64>,
    /// Estimate at this height; -1 = chain tip (default: -1)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) height: Option<i64>,
}
