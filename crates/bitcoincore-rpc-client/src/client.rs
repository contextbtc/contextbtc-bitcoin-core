// To the extent possible under law, the author(s) have dedicated all
// copyright and related and neighboring rights to this software to
// the public domain worldwide. This software is distributed without
// any warranty.
//
// You should have received a copy of the CC0 Public Domain Dedication
// along with this software.
// If not, see <http://creativecommons.org/publicdomain/zero/1.0/>.
//

use std::{fmt, result};

use crate::bitcoin;
use crate::bitcoin::consensus::encode;
use bitcoin::hex::DisplayHex;
use serde;
use serde_json;

use contextvm_sdk::rmcp::ServiceExt;
use contextvm_sdk::rmcp::model::{CallToolRequestParams, RawContent};
use contextvm_sdk::rmcp::service::{RoleClient, RunningService};
use contextvm_sdk::signer;
use contextvm_sdk::transport::client::{NostrClientTransport, NostrClientTransportConfig};
use tokio::runtime::Runtime;

use crate::bitcoin::{Block, Transaction};
use log::Level::Debug;

use crate::error::*;
use crate::json;
use crate::queryable;

/// Crate-specific Result type, shorthand for `std::result::Result` with our
/// crate-specific Error type;
pub type Result<T> = result::Result<T, Error>;

/// Shorthand for converting a variable into a serde_json::Value.
fn into_json<T>(val: T) -> Result<serde_json::Value>
where
    T: serde::ser::Serialize,
{
    Ok(serde_json::to_value(val)?)
}

/// Shorthand for converting an Option into an Option<serde_json::Value>.
fn opt_into_json<T>(opt: Option<T>) -> Result<serde_json::Value>
where
    T: serde::ser::Serialize,
{
    match opt {
        Some(val) => Ok(into_json(val)?),
        None => Ok(serde_json::Value::Null),
    }
}

/// Shorthand for `serde_json::Value::Null`.
fn null() -> serde_json::Value {
    serde_json::Value::Null
}

/// Handle default values in the argument list
///
/// Substitute `Value::Null`s with corresponding values from `defaults` table,
/// except when they are trailing, in which case just skip them altogether
/// in returned list.
///
/// Note, that `defaults` corresponds to the last elements of `args`.
///
/// ```norust
/// arg1 arg2 arg3 arg4
///           def1 def2
/// ```
///
/// Elements of `args` without corresponding `defaults` value, won't
/// be substituted, because they are required.
fn handle_defaults<'a, 'b>(
    args: &'a mut [serde_json::Value],
    defaults: &'b [serde_json::Value],
) -> &'a [serde_json::Value] {
    assert!(args.len() >= defaults.len());

    // Pass over the optional arguments in backwards order, filling in defaults after the first
    // non-null optional argument has been observed.
    let mut first_non_null_optional_idx = None;
    for i in 0..defaults.len() {
        let args_i = args.len() - 1 - i;
        let defaults_i = defaults.len() - 1 - i;
        if args[args_i] == serde_json::Value::Null {
            if first_non_null_optional_idx.is_some() {
                if defaults[defaults_i] == serde_json::Value::Null {
                    panic!("Missing `default` for argument idx {}", args_i);
                }
                args[args_i] = defaults[defaults_i].clone();
            }
        } else if first_non_null_optional_idx.is_none() {
            first_non_null_optional_idx = Some(args_i);
        }
    }

    let required_num = args.len() - defaults.len();

    if let Some(i) = first_non_null_optional_idx {
        &args[..i + 1]
    } else {
        &args[..required_num]
    }
}

/// Used to pass raw txs into the API.
pub trait RawTx: Sized + Clone {
    fn raw_hex(self) -> String;
}

impl<'a> RawTx for &'a Transaction {
    fn raw_hex(self) -> String {
        encode::serialize_hex(self)
    }
}

impl<'a> RawTx for &'a [u8] {
    fn raw_hex(self) -> String {
        self.to_lower_hex_string()
    }
}

impl<'a> RawTx for &'a Vec<u8> {
    fn raw_hex(self) -> String {
        self.to_lower_hex_string()
    }
}

impl<'a> RawTx for &'a str {
    fn raw_hex(self) -> String {
        self.to_owned()
    }
}

impl RawTx for String {
    fn raw_hex(self) -> String {
        self
    }
}

pub trait RpcApi: Sized {
    /// Call a `cmd` rpc with given `args` list
    fn call<T: for<'a> serde::de::Deserialize<'a>>(
        &self,
        cmd: &str,
        args: &[serde_json::Value],
    ) -> Result<T>;

    /// Query an object implementing `Querable` type
    fn get_by_id<T: queryable::Queryable<Self>>(
        &self,
        id: &<T as queryable::Queryable<Self>>::Id,
    ) -> Result<T> {
        T::query(&self, &id)
    }

    fn version(&self) -> Result<usize> {
        #[derive(Deserialize)]
        struct Response {
            pub version: usize,
        }
        let res: Response = self.call("getnetworkinfo", &[])?;
        Ok(res.version)
    }

    fn get_block(&self, hash: &bitcoin::BlockHash) -> Result<Block> {
        let hex: String = self.call("getblock", &[into_json(hash)?, 0.into()])?;
        Ok(encode::deserialize_hex(&hex)?)
    }

    fn get_block_hex(&self, hash: &bitcoin::BlockHash) -> Result<String> {
        self.call("getblock", &[into_json(hash)?, 0.into()])
    }

    fn get_block_info(&self, hash: &bitcoin::BlockHash) -> Result<json::GetBlockResult> {
        self.call("getblock_verbose", &[into_json(hash)?, 1.into()])
    }
    //TODO(stevenroose) add getblock_txs

    fn get_block_header_info(
        &self,
        hash: &bitcoin::BlockHash,
    ) -> Result<json::GetBlockHeaderResult> {
        self.call("getblockheader", &[into_json(hash)?, true.into()])
    }

    /// Returns a data structure containing various state info regarding
    /// blockchain processing.
    fn get_blockchain_info(&self) -> Result<json::GetBlockchainInfoResult> {
        let mut raw: serde_json::Value = self.call("getblockchaininfo", &[])?;
        // The softfork fields are not backwards compatible:
        // - 0.18.x returns a "softforks" array and a "bip9_softforks" map.
        // - 0.19.x returns a "softforks" map.
        Ok(if self.version()? < 190000 {
            use crate::Error::UnexpectedStructure as err;

            // First, remove both incompatible softfork fields.
            // We need to scope the mutable ref here for v1.29 borrowck.
            let (bip9_softforks, old_softforks) = {
                let map = raw.as_object_mut().ok_or(err)?;
                let bip9_softforks = map.remove("bip9_softforks").ok_or(err)?;
                let old_softforks = map.remove("softforks").ok_or(err)?;
                // Put back an empty "softforks" field.
                map.insert("softforks".into(), serde_json::Map::new().into());
                (bip9_softforks, old_softforks)
            };
            let mut ret: json::GetBlockchainInfoResult = serde_json::from_value(raw)?;

            // Then convert both softfork types and add them.
            for sf in old_softforks.as_array().ok_or(err)?.iter() {
                let json = sf.as_object().ok_or(err)?;
                let id = json.get("id").ok_or(err)?.as_str().ok_or(err)?;
                let reject = json.get("reject").ok_or(err)?.as_object().ok_or(err)?;
                let active = reject.get("status").ok_or(err)?.as_bool().ok_or(err)?;
                ret.softforks.insert(
                    id.into(),
                    json::Softfork {
                        type_: json::SoftforkType::Buried,
                        bip9: None,
                        height: None,
                        active: active,
                    },
                );
            }
            for (id, sf) in bip9_softforks.as_object().ok_or(err)?.iter() {
                #[derive(Deserialize)]
                struct OldBip9SoftFork {
                    pub status: json::Bip9SoftforkStatus,
                    pub bit: Option<u8>,
                    #[serde(rename = "startTime")]
                    pub start_time: i64,
                    pub timeout: u64,
                    pub since: u32,
                    pub statistics: Option<json::Bip9SoftforkStatistics>,
                }
                let sf: OldBip9SoftFork = serde_json::from_value(sf.clone())?;
                ret.softforks.insert(
                    id.clone(),
                    json::Softfork {
                        type_: json::SoftforkType::Bip9,
                        bip9: Some(json::Bip9SoftforkInfo {
                            status: sf.status,
                            bit: sf.bit,
                            start_time: sf.start_time,
                            timeout: sf.timeout,
                            since: sf.since,
                            statistics: sf.statistics,
                        }),
                        height: None,
                        active: sf.status == json::Bip9SoftforkStatus::Active,
                    },
                );
            }
            ret
        } else {
            serde_json::from_value(raw)?
        })
    }

    /// Returns the numbers of block in the longest chain.
    fn get_block_count(&self) -> Result<u64> {
        self.call("getblockcount", &[])
    }

    /// Get block hash at a given height
    fn get_block_hash(&self, height: u64) -> Result<bitcoin::BlockHash> {
        self.call("getblockhash", &[height.into()])
    }

    fn get_raw_transaction(
        &self,
        txid: &bitcoin::Txid,
        block_hash: Option<&bitcoin::BlockHash>,
    ) -> Result<Transaction> {
        let mut args = [
            into_json(txid)?,
            into_json(false)?,
            opt_into_json(block_hash)?,
        ];
        let hex: String = self.call("getrawtransaction", handle_defaults(&mut args, &[null()]))?;
        Ok(encode::deserialize_hex(&hex)?)
    }

    fn get_block_filter(
        &self,
        block_hash: &bitcoin::BlockHash,
    ) -> Result<json::GetBlockFilterResult> {
        self.call("getblockfilter", &[into_json(block_hash)?])
    }

    /// Get txids of all transactions in a memory pool
    fn get_raw_mempool(&self) -> Result<Vec<bitcoin::Txid>> {
        self.call("getrawmempool", &[])
    }
}

/// MCP client handler used for the Nostr transport. It uses the default
/// [`ClientHandler`](contextvm_sdk::rmcp::ClientHandler) behaviour.
#[derive(Clone, Default)]
struct BitcoinRpcNostrClient;

impl contextvm_sdk::rmcp::ClientHandler for BitcoinRpcNostrClient {}

/// Client implements a Bitcoin Core RPC client that talks to a ContextVM
/// MCP server over the Nostr network.
///
/// The synchronous [`RpcApi`] surface is preserved: each call is translated
/// into an MCP `tools/call` request and executed by blocking on an internal
/// Tokio runtime. Because of this, methods on this client must not be invoked
/// from within an existing async runtime, or the internal `block_on` will
/// panic.
pub struct Client {
    runtime: Runtime,
    service: RunningService<RoleClient, BitcoinRpcNostrClient>,
    server_pubkey: String,
}

impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "bitcoincore_rpc::Client(server_pubkey={})",
            self.server_pubkey
        )
    }
}

impl Client {
    /// Creates a client connected to a ContextVM MCP server over Nostr.
    ///
    /// A fresh random signer is generated for this client. The client connects
    /// to the given `relay_urls` and targets the server identified by
    /// `server_pubkey` (hex, npub, or nprofile).
    pub fn new(relay_urls: Vec<String>, server_pubkey: String) -> Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(Error::from)?;

        let pubkey = server_pubkey.clone();
        let stateless = true;
        let service = runtime.block_on(async move {
            let signer = signer::generate();
            let transport = NostrClientTransport::new(
                signer,
                NostrClientTransportConfig::default()
                    .with_relay_urls(relay_urls)
                    .with_stateless(stateless)
                    .with_server_pubkey(pubkey),
            )
            .await
            .map_err(|e| Error::Mcp(e.to_string()))?;

            BitcoinRpcNostrClient
                .serve(transport)
                .await
                .map_err(|e| Error::Mcp(e.to_string()))
        })?;

        Ok(Client {
            runtime,
            service,
            server_pubkey,
        })
    }

    /// Returns the list of tool names advertised by the connected MCP server.
    ///
    /// This is a debugging aid to verify that the tool names and schemas match
    /// the mapping used by this client (see [`rpc_params`]).
    pub fn list_tool_names(&self) -> Result<Vec<String>> {
        let tools = self
            .runtime
            .block_on(self.service.list_all_tools())
            .map_err(|e| Error::Mcp(e.to_string()))?;
        Ok(tools.into_iter().map(|t| t.name.to_string()).collect())
    }

    /// Debugging aid: prints every tool advertised by the connected MCP server
    /// together with its description and input schema (parameter names/types).
    ///
    /// Use this to verify that the server's actual tool names and parameters
    /// match the mapping used by this client (see [`rpc_params`]).
    pub fn dump_tool_schemas(&self) -> Result<()> {
        let tools = self
            .runtime
            .block_on(self.service.list_all_tools())
            .map_err(|e| Error::Mcp(e.to_string()))?;
        println!("MCP server advertises {} tool(s):", tools.len());
        for tool in &tools {
            let schema = serde_json::to_string_pretty(&*tool.input_schema)
                .unwrap_or_else(|_| "<unserializable schema>".to_string());
            println!("- {}", tool.name);
            if let Some(desc) = &tool.description {
                println!("  description: {}", desc);
            }
            println!("  input_schema: {}", schema);
        }
        Ok(())
    }
}

impl RpcApi for Client {
    /// Call an `cmd` rpc with given `args` list.
    ///
    /// The JSON-RPC `cmd` name and its positional `args` are translated into an
    /// MCP `tools/call` request: the tool name matches the `cmd` name and the
    /// positional arguments are zipped with the tool's named parameters.
    fn call<T: for<'a> serde::de::Deserialize<'a>>(
        &self,
        cmd: &str,
        args: &[serde_json::Value],
    ) -> Result<T> {
        let tool = cmd;
        let params = rpc_params(cmd)
            .ok_or_else(|| Error::ReturnedError(format!("unknown RPC command: {}", cmd)))?;

        // Zip positional args with the tool's named parameters, skipping any
        // trailing/interior `null`s so the server applies its own defaults.
        let mut arguments = serde_json::Map::new();
        for (name, value) in params.iter().zip(args.iter()) {
            if !value.is_null() {
                arguments.insert((*name).to_string(), value.clone());
            }
        }

        if log_enabled!(Debug) {
            debug!(target: "bitcoincore_rpc", "MCP tools/call: {} {}", tool, serde_json::Value::Object(arguments.clone()));
        }

        let mut request = CallToolRequestParams::new(tool.to_string());
        if !arguments.is_empty() {
            request = request.with_arguments(arguments);
        }

        let result = self
            .runtime
            .block_on(self.service.call_tool(request))
            .map_err(|e| Error::Mcp(e.to_string()))?;

        // Extract the first textual content block as the JSON-encoded result.
        let text = result
            .content
            .iter()
            .find_map(|c| match &c.raw {
                RawContent::Text(t) => Some(t.text.clone()),
                _ => None,
            })
            .unwrap_or_else(|| "null".to_string());

        if log_enabled!(Debug) {
            debug!(target: "bitcoincore_rpc", "MCP tools/call result for {}: is_error={:?} text={}", tool, result.is_error, text);
        }

        if result.is_error == Some(true) {
            return Err(Error::ReturnedError(text));
        }

        let value: serde_json::Value = serde_json::from_str(&text)?;
        Ok(serde_json::from_value(value)?)
    }
}

/// Return the ordered list of named parameters for a Bitcoin Core JSON-RPC
/// command.
///
/// The MCP tool name matches the JSON-RPC `cmd` name, so no name translation is
/// needed. The parameter names follow Bitcoin Core's canonical argument names,
/// and their order matches the order in which the corresponding `RpcApi` method
/// pushes positional arguments into `call`.
fn rpc_params(cmd: &str) -> Option<&'static [&'static str]> {
    let params: &'static [&'static str] = match cmd {
        "getblockchaininfo" => &[],
        "getnetworkinfo" => &[],
        "getblockcount" => &[],
        "getblockhash" => &["height"],
        "getblock" => &["blockhash", "verbosity"],
        "getblock_verbose" => &["blockhash", "verbosity"],
        "getblockheader" => &["blockhash", "verbose"],
        "getblockfilter" => &["blockhash", "filtertype"],
        "getrawmempool" => &["verbose"],
        "getrawtransaction" => &["txid", "verbosity", "blockhash"],
        // Blockchain
        "getbestblockhash" => &[],
        "getdifficulty" => &[],
        "getchaintips" => &[],
        "getchaintxstats" => &["nblocks", "blockhash"],
        "getblockstats" => &["hash_or_height", "stats"],
        "getdeploymentinfo" => &["blockhash"],
        "gettxout" => &["txid", "n", "include_mempool"],
        "gettxoutproof" => &["txids", "blockhash"],
        "verifytxoutproof" => &["proof"],
        "gettxspendingprevout" => &["outputs"],
        // Mempool
        "getmempoolinfo" => &[],
        "getmempoolentry" => &["txid"],
        "getmempoolancestors" => &["txid", "verbose"],
        "getmempooldescendants" => &["txid", "verbose"],
        // Fees
        "estimatesmartfee" => &["conf_target", "estimate_mode"],
        // Transactions & PSBTs
        "decoderawtransaction" => &["hexstring", "iswitness"],
        "decodescript" => &["hexstring"],
        "decodepsbt" => &["psbt"],
        "analyzepsbt" => &["psbt"],
        "createrawtransaction" => &["inputs", "outputs", "locktime", "replaceable"],
        "createpsbt" => &["inputs", "outputs", "locktime", "replaceable"],
        "combinepsbt" => &["txs"],
        "joinpsbts" => &["txs"],
        "finalizepsbt" => &["psbt", "extract"],
        "converttopsbt" => &["hexstring", "permitsigdata", "iswitness"],
        "utxoupdatepsbt" => &["psbt", "descriptors"],
        // Util
        "validateaddress" => &["address"],
        "getdescriptorinfo" => &["descriptor"],
        "deriveaddresses" => &["descriptor", "range"],
        "verifymessage" => &["address", "signature", "message"],
        "createmultisig" => &["nrequired", "keys", "address_type"],
        "getindexinfo" => &["index_name"],
        // Mining
        "getmininginfo" => &[],
        "getnetworkhashps" => &["nblocks", "height"],
        _ => return None,
    };
    Some(params)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json;

    fn test_handle_defaults_inner() -> Result<()> {
        {
            let mut args = [into_json(0)?, null(), null()];
            let defaults = [into_json(1)?, into_json(2)?];
            let res = [into_json(0)?];
            assert_eq!(handle_defaults(&mut args, &defaults), &res);
        }
        {
            let mut args = [into_json(0)?, into_json(1)?, null()];
            let defaults = [into_json(2)?];
            let res = [into_json(0)?, into_json(1)?];
            assert_eq!(handle_defaults(&mut args, &defaults), &res);
        }
        {
            let mut args = [into_json(0)?, null(), into_json(5)?];
            let defaults = [into_json(2)?, into_json(3)?];
            let res = [into_json(0)?, into_json(2)?, into_json(5)?];
            assert_eq!(handle_defaults(&mut args, &defaults), &res);
        }
        {
            let mut args = [into_json(0)?, null(), into_json(5)?, null()];
            let defaults = [into_json(2)?, into_json(3)?, into_json(4)?];
            let res = [into_json(0)?, into_json(2)?, into_json(5)?];
            assert_eq!(handle_defaults(&mut args, &defaults), &res);
        }
        {
            let mut args = [null(), null()];
            let defaults = [into_json(2)?, into_json(3)?];
            let res: [serde_json::Value; 0] = [];
            assert_eq!(handle_defaults(&mut args, &defaults), &res);
        }
        {
            let mut args = [null(), into_json(1)?];
            let defaults = [];
            let res = [null(), into_json(1)?];
            assert_eq!(handle_defaults(&mut args, &defaults), &res);
        }
        {
            let mut args = [];
            let defaults = [];
            let res: [serde_json::Value; 0] = [];
            assert_eq!(handle_defaults(&mut args, &defaults), &res);
        }
        {
            let mut args = [into_json(0)?];
            let defaults = [into_json(2)?];
            let res = [into_json(0)?];
            assert_eq!(handle_defaults(&mut args, &defaults), &res);
        }
        Ok(())
    }

    #[test]
    fn test_handle_defaults() {
        test_handle_defaults_inner().unwrap();
    }
}
