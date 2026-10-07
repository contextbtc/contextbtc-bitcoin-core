# ContextBTC Rust

ContextBTC Rust provides a [Model Context Protocol (MCP)](https://modelcontextprotocol.io) interface to a Bitcoin Core node, using [ContextVM](https://github.com/contextvm) to transport MCP messages over Nostr. Nostr's cryptographic keypairs and signed events provide built-in verification and authorization.

## Generating a Nostr key

The server needs a stable Nostr identity. Generate a secret key with [nak](https://github.com/fiatjaf/nak) (included in the dev shell):

```bash
nak key generate
# -> 7b94e287...bc6148d  (64-char hex secret key)
```

Derive the public key (what clients target) from a secret key with:

```bash
nak key public <secret-key-hex>
```

## Configuration

The server is configured via environment variables. For local development, copy
the provided template and fill in your values:

```bash
cp .env.example .env
# edit .env
```

On startup the server automatically loads a `.env` file if present. Real
environment variables always take precedence over `.env`, and a missing file is
not an error (useful for systemd/Docker where variables are injected directly).
`.env` is gitignored, so your secrets are never committed.

| Variable | Required | Default | Description |
| --- | --- | --- | --- |
| `SERVER_NOSTR_SECRET_KEY` | No | ephemeral | 64-char hex or `nsec...` key. If unset, a temporary key is generated on each start (testing only, not for production). |
| `NOSTR_RELAY_URLS` | No | `ws://localhost:10547` | Comma-separated relay websocket URLs, used by both server and client. |
| `BITCOIN_RPC_URL` | No | `http://127.0.0.1:8332` | Bitcoin Core JSON-RPC endpoint. |
| `BITCOIN_RPC_USER` | Yes | — | JSON-RPC username. |
| `BITCOIN_RPC_PASSWORD` | Yes | — | JSON-RPC password. |
| `BITCOIN_RPC_TIMEOUT_SECS` | No | `30` | Overall HTTP request timeout for RPC calls, in seconds. |

## Project layout

This is a Cargo workspace with these crates:

- `crates/server`: the ContextBTC MCP server for Bitcoin Core (`contextbtc-server`).
- `crates/electrs-server`: the same for an Electrum server such as electrs
  (`contextbtc-electrs-server`). See [Electrum server](#electrum-server-electrs).
- `crates/common`: Nostr startup code shared by both servers.
- `crates/electrum-client-nostr`: `contextbtc-electrum-client`, an
  `electrum_client::ElectrumApi` that talks to `contextbtc-electrs-server`
  over Nostr. It works with `bdk_electrum`.
- `crates/electrum-client-cli`: a command-line client for the Electrum server
  (`contextbtc-electrum-client-cli`).
- `crates/client`: an example client (`contextbtc-client`).
- `crates/e2e`: end-to-end tests.

Alongside them sit two library crates that are not workspace members. They are
pulled in through `[patch.crates-io]` in the root `Cargo.toml`, which swaps them
in wherever a dependency asks for `bitcoincore-rpc`:

- `crates/bitcoincore-rpc-client` — publishes the crate name `bitcoincore-rpc`
  and keeps the upstream `RpcApi` surface, but sends each call as an MCP
  `tools/call` over Nostr instead of HTTP JSON-RPC.
- `crates/bitcoincore-rpc-json` — the matching `bitcoincore-rpc-json` types.

The point of the patch is that unmodified crates.io libraries built on
`bitcoincore-rpc` — `bdk_bitcoind_rpc`, say — end up talking to a ContextBTC
server without any awareness of the transport. They stay out of `members` so
`cargo fmt`/`cargo clippy --workspace` don't lint vendored upstream code.

## Running server

With a `.env` file in place:

```bash
cargo run -p contextbtc-server
```

Alternatively, set variables inline (these override any `.env` values):

```bash
SERVER_NOSTR_SECRET_KEY=<secret-key-hex> \
BITCOIN_RPC_URL=http://127.0.0.1:18443 \
BITCOIN_RPC_USER=myuser \
BITCOIN_RPC_PASSWORD=mypass \
cargo run -p contextbtc-server
```

## Electrum server (electrs)

`contextbtc-electrs-server` exposes an Electrum protocol server, such as
[electrs](https://github.com/romanz/electrs), over the same MCP-over-Nostr
transport. Electrum servers index the chain by script, so a light wallet can
fetch its own history and balance without a wallet on the node.

```text
wallet ──MCP/Nostr──▶ contextbtc-electrs-server ──Electrum TCP──▶ electrs ──▶ bitcoind
```

Each tool is named after the Electrum method it proxies:

| Tool | Arguments |
| --- | --- |
| `blockchain.headers.subscribe` | none (returns the current tip) |
| `blockchain.block.header` | `height`, `cp_height?` |
| `blockchain.block.headers` | `start_height`, `count` (at most 2016) |
| `blockchain.estimatefee` | `number` |
| `blockchain.relayfee` | none |
| `blockchain.scripthash.get_balance`, `.get_history`, `.get_mempool`, `.listunspent`, `.subscribe` | `scripthash` |
| `blockchain.transaction.get` | `tx_hash` |
| `blockchain.transaction.get_merkle` | `tx_hash`, `height` |
| `blockchain.transaction.id_from_pos` | `height`, `tx_pos`, `merkle?` |
| `mempool.get_fee_histogram` | none |
| `server.features` | none (`hosts` and `server_version` are blanked) |
| `server.ping` | none |

Notes on the tool set:

- **Read-only.** There is no `blockchain.transaction.broadcast`.
- **No banner, donation address or peer list.** `server.banner`,
  `server.donation_address` and `server.peers.subscribe` are not exposed,
  because they can identify the host.
- **No notifications.** The `subscribe` tools return the current state only.
  `blockchain.scripthash.subscribe` computes the status hash from the
  script's history instead of subscribing on electrs.

It reads the shared Nostr variables (`SERVER_NOSTR_SECRET_KEY`,
`NOSTR_RELAY_URLS`, `ALLOWED_CLIENT_PUBKEYS`) plus:

| Variable | Required | Default | Description |
| --- | --- | --- | --- |
| `ELECTRS_ADDR` | No | `127.0.0.1:50001` | Electrum server TCP address (plain TCP, no TLS). electrs listens on `60401` on regtest. |
| `ELECTRS_TIMEOUT_SECS` | No | `30` | Time limit for each Electrum call, in seconds. |

```bash
SERVER_NOSTR_SECRET_KEY=<secret-key-hex> \
ELECTRS_ADDR=127.0.0.1:50001 \
cargo run -p contextbtc-electrs-server
```

Give each server its own `SERVER_NOSTR_SECRET_KEY`, so clients address the
Bitcoin Core and Electrum servers separately.

**Privacy:** as with any Electrum server, whoever runs it sees every script
hash a client asks about, so they can link those addresses to that client's
Nostr key. Only use a server you trust with that.

From Rust, `contextbtc-electrum-client` plugs into `bdk_electrum`:

```rust
let client = contextbtc_electrum_client::NostrElectrumClient::new(relay_urls, server_pubkey)?;
let bdk_client = bdk_electrum::BdkElectrumClient::new(client);
let update = bdk_client.full_scan(request, stop_gap, batch_size, false)?;
```

Use `NostrElectrumClient::with_keys` instead of `new` when the server has an
`ALLOWED_CLIENT_PUBKEYS` allowlist. Each call times out after 30s by default
(`set_call_timeout`), which is how a wrong server public key shows up.

### Electrum CLI

`contextbtc-electrum-client-cli` talks to the server through that library:

| Command | What it shows |
| --- | --- |
| `tip` | chain tip height, hash and time |
| `header <HEIGHT>` | a block header |
| `balance <ADDRESS>` | confirmed and unconfirmed balance |
| `history <ADDRESS>` | the address's transactions |
| `utxos <ADDRESS>` | the address's unspent outputs |
| `tx <TXID> [--decode]` | a transaction, as hex or decoded |
| `merkle <TXID> <HEIGHT>` | a transaction's merkle proof |
| `fee [--blocks N]` | fee estimate and relay fee, in sat/vB |
| `features` | the server's network and protocol versions |
| `ping` | round-trip time |
| `scan <DESCRIPTOR> [<CHANGE_DESCRIPTOR>]` | a full `bdk_electrum` wallet scan: balance, UTXOs, transactions |

It reads the server from `--server` or `ELECTRUM_SERVER_PUBKEY`, the relays
from `--relays` or `NOSTR_RELAY_URLS`, and its own key from
`CLIENT_NOSTR_SECRET_KEY` (ephemeral if unset). It detects the network from the
server's genesis hash, so addresses are checked against the right chain.
`RUST_LOG=contextbtc_electrum_client=debug` prints every MCP call.

### Try it locally (regtest)

Each long-running step goes in its own terminal, inside `nix develop`:

```bash
# 1. Relay
nak serve

# 2. bitcoind (P2P is on by default; electrs downloads blocks through it)
mkdir -p /tmp/rt/btc
bitcoind -regtest -datadir=/tmp/rt/btc

# 3. Mine past IBD, paying an address you want to look at
bitcoin-cli -regtest -datadir=/tmp/rt/btc generatetoaddress 101 bcrt1qw508d6qejxtdg4y5r3zarvary0c5xw7kygt080

# 4. electrs (Electrum port 60401 on regtest)
electrs --network regtest --daemon-dir /tmp/rt/btc --db-dir /tmp/rt/electrs

# 5. The MCP server; note the "Public key: ..." it prints
ELECTRS_ADDR=127.0.0.1:60401 cargo run -p contextbtc-electrs-server

# 6. Query it
export ELECTRUM_SERVER_PUBKEY=<public key from step 5>
cargo run -p contextbtc-electrum-client-cli -- tip
cargo run -p contextbtc-electrum-client-cli -- balance bcrt1qw508d6qejxtdg4y5r3zarvary0c5xw7kygt080
cargo run -p contextbtc-electrum-client-cli -- scan "<public descriptor>"
```

If your `.env` sets `ALLOWED_CLIENT_PUBKEYS` for the server, also set
`CLIENT_NOSTR_SECRET_KEY` for the CLI to a key on that list.

## Running client

## Client .env

```bash
CLIENT_NOSTR_SECRET_KEY=
```

```bash
cargo run -p contextbtc-client -- <server-pub-key-hex>
```

## Testing

```bash
cargo test --workspace
```

Most tests are pure unit tests. The end-to-end tests in `crates/e2e` exercise the
full path. They start a stack from `crates/e2e/tests/harness/`: a local Nostr
relay (`nak serve`), a regtest `bitcoind` (managed by
[`corepc-node`](https://github.com/rust-bitcoin/corepc)), and the real server
against that node. The Electrum test also runs `electrs` between the node and
`contextbtc-electrs-server`.

- `tests/e2e.rs` runs the real client and checks it receives live regtest data.
- `tests/filter_iter.rs` syncs a descriptor with BDK's compact block filter
  (BIP157/158) `FilterIter`, over the patched `bitcoincore-rpc` — so the whole
  sync travels over MCP/Nostr. It mines a block paying a descriptor address and
  asserts that block was matched by its filter and its output reached the graph.
  Needs `bitcoind` started with `-blockfilterindex=1`, which the harness does.
- `tests/electrum_sync.rs` runs `bdk_electrum`'s `full_scan` through
  `contextbtc-electrum-client`, so the whole Electrum sync travels over
  MCP/Nostr. It mines a block paying a descriptor address and asserts that the
  output reached the graph at the right height. A second test runs the
  `contextbtc-electrum-client-cli` binary (`tip`, `balance`, `scan`) against
  the same setup.

Those tests need `bitcoind`, `nak` and, for the Electrum test, `electrs`. The dev shell provides all three, so
the simplest way to run the whole suite is:

```bash
nix develop --command cargo test --workspace
```

Outside the dev shell, make `nak` available on `PATH`, point `corepc-node` at a
bitcoind binary via `BITCOIND_EXE`, and point the harness at electrs via
`ELECTRS_EXE` (or put it on `PATH`). The e2e test **fails** if either is missing —
it never silently skips — so `cargo test` needs both present. This is the same
command CI runs (see `.github/workflows/ci.yml`).

Running e2e tests only with logs:

```bash
cargo test -p contextbtc-e2e -- --nocapture
```

## Running with Nix (from another machine)

The flake exposes prebuilt packages, so any machine with [Nix](https://nixos.org/download)
(flakes enabled) can run the server or client straight from GitHub — no clone,
no toolchain setup:

```bash
# Run the server
nix run github:contextbtc/contextbtc

# Run the Electrum server, and the CLI against it
nix run github:contextbtc/contextbtc#electrs-server
nix run github:contextbtc/contextbtc#electrum-cli -- --server <server-pub-key-hex> tip

# Run the client (note the `--` before program arguments)
nix run github:contextbtc/contextbtc#client -- <server-pub-key-hex>
```

Configuration works the same way as a local run: pass the environment variables
from the [Configuration](#configuration) table inline, e.g.

```bash
SERVER_NOSTR_SECRET_KEY=<secret-key-hex> \
NOSTR_RELAY_URLS=wss://relay.contextvm.org \
BITCOIN_RPC_URL=http://127.0.0.1:8332 \
BITCOIN_RPC_USER=myuser \
BITCOIN_RPC_PASSWORD=mypass \
nix run github:contextbtc/contextbtc
```

To build without running, or to install into your profile:

```bash
nix build github:contextbtc/contextbtc   # -> ./result/bin/{contextbtc-server,contextbtc-electrs-server,contextbtc-client,contextbtc-electrum-client-cli}
nix profile install github:contextbtc/contextbtc
```

### As a NixOS service

For a NixOS host, the flake also provides a module (`nixosModules.default`) that
runs the server as a hardened systemd service. Add it to the target machine's
flake:

```nix
{
  inputs.contextbtc.url = "github:contextbtc/contextbtc";

  outputs = { nixpkgs, contextbtc, ... }: {
    nixosConfigurations.myhost = nixpkgs.lib.nixosSystem {
      system = "x86_64-linux";
      modules = [
        contextbtc.nixosModules.default
        {
          services.contextbtc = {
            enable = true;
            relayUrls = [ "wss://relay.contextvm.org" ];
            # Non-secret settings:
            extraEnvironment.BITCOIN_RPC_URL = "http://127.0.0.1:8332";
            # Secrets (SERVER_NOSTR_SECRET_KEY, BITCOIN_RPC_USER/PASSWORD, ...)
            # live in a file read at runtime, never in the Nix store:
            environmentFile = "/run/secrets/contextbtc.env";
          };
        }
      ];
    };
  };
}
```

Then `sudo nixos-rebuild switch`. The service runs as an isolated `DynamicUser`
with automatic restart.

## Architecture

This project bridges two distinct protocol layers:

- **Client ⟷ ContexVM MCP server:** MCP over Nostr.
- **ContexVM MCP server ⟷ bitcoind:** JSON-RPC over HTTP.
- **ContexVM Electrum MCP server ⟷ electrs:** Electrum protocol (newline-delimited
  JSON-RPC) over TCP.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in this project by you, as defined in the Apache-2.0 license,
shall be dual licensed as above, without any additional terms or conditions.

`crates/bitcoincore-rpc-client` and `crates/bitcoincore-rpc-json` are vendored
from [rust-bitcoincore-rpc](https://github.com/rust-bitcoin/rust-bitcoincore-rpc)
v0.19.0 and remain under its original CC0-1.0 dedication. Their per-file
headers are kept as-is.
