# Architecture

This document describes how ContextBTC is put together and why. The
[README](README.md) covers setup and usage. This file covers the design: what
each crate is for, how a request travels, and the decisions that aren't obvious
from the code. Keep it at that level; the code is the reference for details.

## What it does

ContextBTC lets a client query a Bitcoin Core node or an Electrum server
(electrs) over [Nostr](https://nostr.com), using [MCP](https://modelcontextprotocol.io)
as the message format via [ContextVM](https://github.com/ContextVM). The node
never opens a port to the client; both sides only connect out to Nostr relays.
Clients address the server by its Nostr public key.

```text
                         ┌───────── Nostr relays ─────────┐
 client ── MCP tools/call ─▶  (encrypted events, by pubkey)  ─▶ contextbtc-server ── JSON-RPC/HTTP ──▶ bitcoind
                                                              ─▶ contextbtc-electrs-server ── Electrum/TCP ──▶ electrs
```

There are three protocol hops:

| Hop | Protocol | Code |
| --- | --- | --- |
| Client ⟷ server | MCP over Nostr (ContextVM) | `contextvm-sdk`, set up in `crates/common` |
| Server ⟷ bitcoind | JSON-RPC 2.0 over HTTP, basic auth | `crates/server/src/rpc.rs` |
| Server ⟷ electrs | Electrum protocol (newline-delimited JSON-RPC) over plain TCP | `crates/electrs-server/src/electrum.rs` |

## Crate map

```text
            servers                                    clients
 ┌───────────────────────────┐        ┌──────────────────────────────────────────┐
 │ server ──────┐            │        │ client (demo binary)                     │
 │              ├─▶ common   │        │ bitcoincore-rpc-client ─▶ bitcoincore-rpc-json
 │ electrs-server┘           │        │ electrum-client-nostr ◀── electrum-client-cli
 └───────────────────────────┘        │ web/ (TypeScript, browser)               │
                                      └──────────────────────────────────────────┘
                     e2e (tests only) ── spawns the servers, uses the client libraries
```

| Crate | Kind | Role |
| --- | --- | --- |
| `common` | lib | Shared server startup: Nostr keys, relay list, client allowlist, transport, rustls provider. |
| `server` | bin + lib | `contextbtc-server`: Bitcoin Core RPCs as MCP tools. |
| `electrs-server` | bin + lib | `contextbtc-electrs-server`: Electrum methods as MCP tools. |
| `bitcoincore-rpc-client` | lib, **not a member** | Vendored `bitcoincore-rpc` 0.19 with its transport replaced by MCP over Nostr. |
| `bitcoincore-rpc-json` | lib, **not a member** | Vendored response types matching the above. |
| `electrum-client-nostr` | lib | `NostrElectrumClient`, an `electrum_client::ElectrumApi` over MCP over Nostr. |
| `electrum-client-cli` | bin | CLI on top of `electrum-client-nostr`, including a full `bdk_electrum` wallet scan. |
| `client` | bin | Minimal example client that calls a fixed set of tools. |
| `e2e` | tests only | End-to-end tests against real `nak`, `bitcoind` and `electrs` processes. |
| `web/` | TypeScript app | Browser client for manual testing, built on the TypeScript ContextVM SDK. |

The two servers are deliberately parallel: same startup (`common`), same shape
(a backend client with retries plus a `tools.rs` of MCP tools), same error
policy. A change to one usually needs a matching change in the other.

## Life of a request

Taking `getblockhash` through `contextbtc-server`:

1. **Client.** Builds an MCP `tools/call` with name `getblockhash` and arguments
   `{"height": 100}`. The ContextVM transport signs and encrypts it as a Nostr
   event addressed to the server's pubkey and publishes it to the relays.
   Clients run the transport in stateless mode (no MCP `initialize` round trip).
2. **Relay.** Forwards the event. It sees metadata (sender and recipient
   pubkeys, timing) but not the content.
3. **Transport (server side).** `NostrServerTransport` decrypts the event and
   drops it unless the sender is in `ALLOWED_CLIENT_PUBKEYS` (an empty list
   means anyone may call).
4. **Name resolution.** `call_tool` in `crates/server/src/tools.rs` maps the
   requested name to a registered route, accepting Bitcoin Core style
   (`getblockhash`) and snake_case or mixed case (`get_block_hash`,
   `getBlockHash`) through `normalize_tool_name`. Unknown names fail with
   `invalid_params`.
5. **Tool.** The `#[tool]` method deserializes its typed params (`params.rs`)
   and calls `forward`/`forward_named`. Older tools send positional params; the
   newer ones send the params struct as JSON-RPC *named* params, so unset
   optionals are omitted and bitcoind applies its own defaults.
6. **Backend.** `BitcoinRpc::call` posts JSON-RPC to bitcoind, retrying
   transient failures (connect errors, timeouts, 5xx, 429) with jittered
   exponential backoff, up to 3 retries.
7. **Response.** The JSON result is stringified into a single MCP text content
   block and sent back the same way. Errors go through the sanitizer described
   below.

The electrs path is the same apart from step 6: `ElectrumClient` keeps **one**
lazily opened TCP connection, serializes calls over it behind a mutex, performs
the `server.version` handshake (protocol 1.4) on connect, and discards the
connection after any transport error, since a reply may still be in flight.
Electrum tools are named exactly after the Electrum method they proxy
(`blockchain.scripthash.get_history`, …), so clients map them one to one.

## The transport swap

This is the central trick of the client side, and the least obvious part of the
repo.

BDK and many other Rust libraries talk to Bitcoin Core through the crates.io
`bitcoincore-rpc` crate. Rather than fork those libraries, the workspace vendors
`bitcoincore-rpc` 0.19 into `crates/bitcoincore-rpc-client`, keeps its crate
name, version and `RpcApi` surface, and replaces only the HTTP transport in
`src/client.rs`. The root `Cargo.toml` then patches it in:

```toml
[patch.crates-io]
bitcoincore-rpc = { path = "crates/bitcoincore-rpc-client" }
bitcoincore-rpc-json = { path = "crates/bitcoincore-rpc-json" }
```

Every crate in the build that depends on `bitcoincore-rpc` — notably an
unmodified `bdk_bitcoind_rpc` — now reaches the node over MCP over Nostr without
knowing it. `crates/e2e/tests/filter_iter.rs` checks this with BDK's
`FilterIter`.

How a call is translated (`RpcApi::call` in `client.rs`):

- The JSON-RPC method name is the MCP tool name, unchanged.
- Positional args are zipped with the tool's parameter names from the
  `rpc_params` table, and `null`s are dropped so the server's defaults apply.
- The first text content block of the result is parsed as JSON into the
  expected return type.

Consequences to keep in mind:

- **Two tables must agree.** `rpc_params` (client) must match the parameter
  names and order in `crates/server/src/params.rs` and `tools.rs` (server).
  Adding a tool means updating both. `Client::dump_tool_schemas` helps check
  them against a live server.
- **The `RpcApi` surface is trimmed** to the methods BDK needs. Methods that
  were removed don't exist at compile time, rather than failing at runtime.
- **The API is synchronous.** Each client owns a Tokio runtime and `block_on`s
  every call, so it must not be used from inside an existing async runtime.
- **It always uses a random Nostr key** (`Client::new` calls
  `signer::generate()`), so it can't be used against a server with a non-empty
  `ALLOWED_CLIENT_PUBKEYS`. `NostrElectrumClient::with_keys` is the pattern to
  follow if that's needed.
- The vendored crates stay outside `members` so workspace-wide `fmt` and
  `clippy` don't touch upstream code, and they keep upstream's CC0 licence.

The Electrum side doesn't need a patch: `bdk_electrum::BdkElectrumClient` is
generic over `electrum_client::ElectrumApi`, so `electrum-client-nostr` just
implements that trait. Electrum batches become concurrent `tools/call`s (16 in
flight at most).

## Security and privacy model

The goal is that operating a server reveals as little as possible about the
host, and only intended clients can use it.

- **Reachability.** The servers are not announced on relays, and they don't
  publish a relay list (kind 10002), which would publicly tie the server's key
  to its relays. A client must already know the pubkey.
- **Authorization.** `ALLOWED_CLIENT_PUBKEYS` is enforced by the transport
  before any tool code runs. The transport also handles encryption.
- **Read-only surface.** No wallet RPCs, and no broadcasting on either server
  (`sendrawtransaction`, `blockchain.transaction.broadcast`). PSBT tools only
  construct or decode data; nothing is signed or relayed.
- **Tool allowlist.** `ENABLED_TOOLS` (Bitcoin Core server only) narrows the
  exposed tools to individual names and/or groups (`bdk`, `blockchain`,
  `mempool`, …; see `TOOL_GROUPS`). Disabled tools are removed from the router
  itself, so they are neither listed nor callable through an alias. Unknown
  names are a startup error, so a typo can't silently expose or hide a tool.
- **Error sanitization.** `RpcCallError` and `ElectrumCallError` log the full
  error on the server, but the client only gets a fixed message chosen by error
  kind, plus the numeric backend error code where there is one. Backend
  messages, HTTP bodies, proxy banners and node URLs are never relayed. Unit
  tests in `rpc.rs` and `electrum.rs` guard this.
- **Response redaction.** Responses that would fingerprint the host are blanked:
  `getnetworkinfo` (local addresses, proxies, user agent, peer counts, time
  offset) and `server.features` (hosts, server version). Values are blanked
  rather than removed so typed clients still deserialize them. `server.banner`,
  `server.donation_address` and `server.peers.subscribe` aren't exposed.
- **No subscriptions.** `blockchain.scripthash.subscribe` returns a status hash
  computed from `get_history` instead of subscribing on the shared electrs
  connection, so the server doesn't accumulate per-client state there, and no
  notifications are forwarded.

Relays still see who talks to whom and when. The threat model is not written up
beyond this section yet.

## Cross-cutting constraints

- **One rustls crypto provider.** The Nostr websocket stack uses rustls with
  `ring`. If any dependency enables `aws-lc-rs` too, Cargo's feature unification
  gives rustls two providers and it panics on the first TLS connection.
  `common::install_crypto_provider` selects `ring` explicitly, and crates that
  pull in TLS-capable dependencies (`electrum-client`, `bdk_electrum`) disable
  their TLS features. Check this when adding a dependency.
- **Configuration is environment variables** (optionally from `.env`), read at
  startup in several places. The README has the full list.
- **The e2e harness parses stdout.** Servers print `Public key: <hex>`, which
  `crates/e2e/tests/harness` reads to find the server identity. Don't change
  that line without updating the harness.

## Testing

- **Unit tests** sit next to the code: tool registration and name
  normalization, `ENABLED_TOOLS` resolution, redaction, error sanitization, and
  the Electrum framing (id matching, skipping notifications).
- **End-to-end tests** (`crates/e2e`) start a real relay (`nak serve`), a
  regtest `bitcoind` (and `electrs` where needed), and the real server binaries,
  then drive them with the real clients, including unmodified BDK. They skip
  cleanly when the external binaries are missing. `nix develop` provides them,
  and CI runs everything inside it.
