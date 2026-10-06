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

This is a Cargo workspace with two binary crates:

- `crates/server` — the ContextBTC MCP server (`contextbtc-server`).
- `crates/client` — an example client (`contextbtc-client`).

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

## Running client

## Client .env

```bash
CLIENT_NOSTR_SECRET_KEY=
```

```bash
cargo run -p contextbtc-client -- <server-pub-key-hex>
```

## Web client (manual testing)

`web/` is a single-page browser client built on the TypeScript
[ContextVM SDK](https://github.com/ContextVM/sdk). It lists the server's tools and
lets you call any of them with JSON arguments. It is meant for manual testing
from a browser. [Bun](https://bun.sh), which the dev shell provides, serves and
bundles it:

```bash
cd web
bun install
bun run dev   # open the URL bun prints (http://localhost:3000)
```

With a relay (`nak serve`) and the server running, paste the server's public key
and click **Connect**. If the client secret key field is left blank, a key is
generated and kept in the browser's `localStorage`. That keeps the client pubkey
(shown after connecting) stable, so you can add it to `ALLOWED_CLIENT_PUBKEYS`.

`bun run check` type-checks the client and `bun run build` writes a static bundle
to `web/dist/`.

## Testing

```bash
cargo test --workspace
```

Most tests are pure unit tests. The end-to-end tests in `crates/e2e` exercise the
full path. Both start the same stack (`crates/e2e/tests/harness/`): a local Nostr
relay (`nak serve`), a regtest `bitcoind` (managed by
[`corepc-node`](https://github.com/rust-bitcoin/corepc)), and the real server
against that node.

- `tests/e2e.rs` runs the real client and checks it receives live regtest data.
- `tests/filter_iter.rs` syncs a descriptor with BDK's compact block filter
  (BIP157/158) `FilterIter`, over the patched `bitcoincore-rpc` — so the whole
  sync travels over MCP/Nostr. It mines a block paying a descriptor address and
  asserts that block was matched by its filter and its output reached the graph.
  Needs `bitcoind` started with `-blockfilterindex=1`, which the harness does.

Those tests need `bitcoind` and `nak` available. The dev shell provides both, so
the simplest way to run the whole suite is:

```bash
nix develop --command cargo test --workspace
```

Outside the dev shell, make `nak` available on `PATH` and point `corepc-node` at a
bitcoind binary via `BITCOIND_EXE`. The e2e test **fails** if either is missing —
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
nix run github:karliatto/contextbtc

# Run the client (note the `--` before program arguments)
nix run github:karliatto/contextbtc#client -- <server-pub-key-hex>
```

Configuration works the same way as a local run: pass the environment variables
from the [Configuration](#configuration) table inline, e.g.

```bash
SERVER_NOSTR_SECRET_KEY=<secret-key-hex> \
NOSTR_RELAY_URLS=wss://relay.contextvm.org \
BITCOIN_RPC_URL=http://127.0.0.1:8332 \
BITCOIN_RPC_USER=myuser \
BITCOIN_RPC_PASSWORD=mypass \
nix run github:karliatto/contextbtc
```

To build without running, or to install into your profile:

```bash
nix build github:karliatto/contextbtc   # -> ./result/bin/{contextbtc-server,contextbtc-client}
nix profile install github:karliatto/contextbtc
```

### As a NixOS service

For a NixOS host, the flake also provides a module (`nixosModules.default`) that
runs the server as a hardened systemd service. Add it to the target machine's
flake:

```nix
{
  inputs.contextbtc.url = "github:karliatto/contextbtc";

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
