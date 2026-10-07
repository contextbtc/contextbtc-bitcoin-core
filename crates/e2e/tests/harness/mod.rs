//! Shared plumbing for the end-to-end tests.
//!
//! [`Stack`] brings up the three moving parts the Bitcoin Core tests need — a
//! local Nostr relay (`nak serve`), a regtest `bitcoind` (via `corepc-node`),
//! and the real `contextbtc-server` binary wired to both — and tears them all
//! down when it is dropped. [`ElectrsStack`] does the same with `electrs`
//! between the node and `contextbtc-electrs-server`.
//!
//! Tests using it require a `bitcoind` binary (located via `BITCOIND_EXE` or
//! `PATH`) and `nak` on `PATH`; `ElectrsStack` also needs `electrs` (via
//! `ELECTRS_EXE` or `PATH`). The Nix devShell provides all three, so run them
//! with `nix develop --command cargo test`.

// Each test binary compiles its own copy of this module, and none of them use
// all of it.
#![allow(dead_code)]

use std::io::{BufRead, BufReader};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Fixed server identity so the run is deterministic and warning-free (no
/// ephemeral key). This is a throwaway test key, not a secret.
const SERVER_SECRET_KEY: &str = "1111111111111111111111111111111111111111111111111111111111111111";

/// Kills a spawned child when it goes out of scope so a failed assertion never
/// leaves `nak` or the server running.
struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Grab a free TCP port by binding to :0 and immediately releasing it. There's
/// an inherent race before the port is reused, but it's fine for a local test.
fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("bind ephemeral port")
        .local_addr()
        .expect("local_addr")
        .port()
}

/// Poll until something is listening on `port`, or the deadline passes.
fn wait_for_tcp(port: u16, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

/// Whether `nak` is runnable.
fn nak_available() -> bool {
    Command::new("nak")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// The `electrs` binary to run (`ELECTRS_EXE`, else `electrs` on `PATH`).
/// Panics if it is not runnable.
fn electrs_exe() -> String {
    let exe = std::env::var("ELECTRS_EXE").unwrap_or_else(|_| "electrs".to_string());
    let ok = Command::new(&exe)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    assert!(
        ok,
        "electrs not found: set ELECTRS_EXE or add electrs to PATH (`{exe}` is not runnable)"
    );
    exe
}

/// Start a local Nostr relay (`nak serve`) and return its URL.
fn start_relay() -> anyhow::Result<(String, ChildGuard)> {
    assert!(
        nak_available(),
        "`nak` not found on PATH (needed to run the local relay via `nak serve`)"
    );

    let relay_port = free_port();
    let relay_url = format!("ws://localhost:{relay_port}");
    let nak = Command::new("nak")
        .args(["serve", "--quiet", "--port", &relay_port.to_string()])
        // When stdin is not a terminal, `nak` reads it before serving. An
        // inherited pipe that never closes (as under some test runners) would
        // keep the relay from ever listening.
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let guard = ChildGuard(nak);
    assert!(
        wait_for_tcp(relay_port, Duration::from_secs(10)),
        "relay did not start listening on {relay_port}"
    );
    Ok((relay_url, guard))
}

/// Start a regtest bitcoind. `extra_args` are appended to the default regtest
/// arguments (e.g. `-blockfilterindex=1`).
fn start_node(extra_args: &[&str], p2p: corepc_node::P2P) -> anyhow::Result<corepc_node::Node> {
    let bitcoind_exe = corepc_node::exe_path()
        .expect("bitcoind not found: set BITCOIND_EXE or add bitcoind to PATH");

    // No wallet: the proxied tools are read-only chain/mempool queries, and
    // creating the default wallet fails on recent Bitcoin Core versions.
    let mut conf = corepc_node::Conf::default();
    conf.wallet = None;
    conf.p2p = p2p;
    conf.args.extend_from_slice(extra_args);
    corepc_node::Node::with_conf(&bitcoind_exe, &conf)
}

/// Build and start one of the workspace's server binaries with `envs` plus the
/// shared Nostr settings, and return the public key it prints at startup.
fn start_server(
    package: &str,
    relay_url: &str,
    envs: &[(&str, &str)],
) -> anyhow::Result<(String, ChildGuard)> {
    let server_bin = escargot::CargoBuild::new()
        .package(package)
        .bin(package)
        .run()?;
    let mut server = server_bin
        .command()
        .env("SERVER_NOSTR_SECRET_KEY", SERVER_SECRET_KEY)
        .env("NOSTR_RELAY_URLS", relay_url)
        .env("RUST_LOG", "warn")
        .envs(envs.iter().copied())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()?;

    // Stream the server's stdout on a thread so we can watch for the pubkey it
    // prints at startup. Keep draining after that so the server's later log
    // lines don't hit a closed pipe.
    let stdout = server.stdout.take().expect("server stdout piped");
    let (tx, rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            let _ = tx.send(line);
        }
    });
    let guard = ChildGuard(server);

    // We only wait for the pubkey line here. The server's own "Server ready"
    // log comes from `serve()`, which completes the MCP initialize handshake —
    // and that only happens once a client connects. Blocking on it here would
    // deadlock against the client the test starts next.
    let mut server_pubkey: Option<String> = None;
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline && server_pubkey.is_none() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(remaining) {
            Ok(line) => {
                if let Some(pk) = line.strip_prefix("Public key: ") {
                    server_pubkey = Some(pk.trim().to_string());
                }
            }
            Err(_) => break,
        }
    }
    let server_pubkey = server_pubkey.expect("server should print its public key");
    println!("Server pub key: {server_pubkey}");

    // Give the server a moment to finish subscribing on the relay before the
    // first client request goes out.
    std::thread::sleep(Duration::from_secs(1));

    Ok((server_pubkey, guard))
}

/// A running `relay + bitcoind + contextbtc-server` stack.
///
/// ```text
///   <test> ──MCP/Nostr──▶ nak serve (relay) ──▶ contextbtc-server ──JSON-RPC──▶ bitcoind (regtest)
/// ```
pub struct Stack {
    /// `ws://` URL of the local relay, for clients to connect to.
    pub relay_url: String,
    /// Hex public key the server announced on startup; clients address it.
    pub server_pubkey: String,
    /// The regtest node, for driving the chain directly (mining, etc.).
    pub node: corepc_node::Node,
    // Declared last so the subprocesses outlive anything above that talks to
    // them; fields drop in declaration order.
    _server: ChildGuard,
    _relay: ChildGuard,
}

impl Stack {
    /// Start the whole stack. `extra_bitcoind_args` are appended to the default
    /// regtest arguments (e.g. `-blockfilterindex=1`).
    pub fn start(extra_bitcoind_args: &[&str]) -> anyhow::Result<Self> {
        let (relay_url, relay_guard) = start_relay()?;
        let node = start_node(extra_bitcoind_args, corepc_node::P2P::No)?;

        let rpc_url = node.rpc_url();
        let cookie = node
            .params
            .get_cookie_values()?
            .expect("regtest node should expose cookie credentials");
        let (server_pubkey, server_guard) = start_server(
            "contextbtc-server",
            &relay_url,
            &[
                ("BITCOIN_RPC_URL", &rpc_url),
                ("BITCOIN_RPC_USER", &cookie.user),
                ("BITCOIN_RPC_PASSWORD", &cookie.password),
            ],
        )?;

        Ok(Self {
            relay_url,
            server_pubkey,
            node,
            _server: server_guard,
            _relay: relay_guard,
        })
    }
}

/// A running `relay + bitcoind + electrs + contextbtc-electrs-server` stack.
///
/// ```text
///   <test> ──MCP/Nostr──▶ nak serve (relay) ──▶ contextbtc-electrs-server ──Electrum──▶ electrs ──P2P/RPC──▶ bitcoind (regtest)
/// ```
///
/// Needs an `electrs` binary (located via `ELECTRS_EXE` or `PATH`) on top of
/// what [`Stack`] needs.
pub struct ElectrsStack {
    /// `ws://` URL of the local relay, for clients to connect to.
    pub relay_url: String,
    /// Hex public key the server announced on startup; clients address it.
    pub server_pubkey: String,
    // Fields drop in declaration order: the server and electrs stop before the
    // node they talk to, and the relay goes last.
    _server: ChildGuard,
    _electrs: ChildGuard,
    /// The regtest node, for driving the chain directly (mining, etc.).
    pub node: corepc_node::Node,
    _relay: ChildGuard,
}

/// Regtest address that blocks are mined to before the test takes over
/// (BIP173's regtest P2WPKH example; nothing in the tests watches it).
const PREMINE_ADDRESS: &str = "bcrt1qw508d6qejxtdg4y5r3zarvary0c5xw7kygt080";

impl ElectrsStack {
    pub fn start() -> anyhow::Result<Self> {
        let electrs_exe = electrs_exe();
        let (relay_url, relay_guard) = start_relay()?;

        // electrs downloads blocks over P2P, so the node must listen.
        let node = start_node(&[], corepc_node::P2P::Yes)?;
        // A regtest node with only the genesis block reports it is in initial
        // block download, and electrs waits for that to end before indexing.
        node.client
            .call::<serde_json::Value>("generatetoaddress", &[1.into(), PREMINE_ADDRESS.into()])?;

        // --- electrs --------------------------------------------------------------
        let electrum_port = free_port();
        let electrum_addr = format!("127.0.0.1:{electrum_port}");
        let p2p = node
            .params
            .p2p_socket
            .expect("node was started with P2P enabled");
        let db_dir = node.workdir().join("electrs-db");
        let electrs = Command::new(&electrs_exe)
            .arg("--skip-default-conf-files")
            .args(["--network", "regtest"])
            .arg("--db-dir")
            .arg(&db_dir)
            .arg("--daemon-dir")
            .arg(node.workdir())
            .arg("--cookie-file")
            .arg(&node.params.cookie_file)
            .args(["--daemon-rpc-addr", &node.params.rpc_socket.to_string()])
            .args(["--daemon-p2p-addr", &p2p.to_string()])
            .args(["--electrum-rpc-addr", &electrum_addr])
            .args(["--monitoring-addr", &format!("127.0.0.1:{}", free_port())])
            .args(["--log-filters", "warn"])
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()?;
        let electrs_guard = ChildGuard(electrs);
        // electrs only opens its Electrum port once the initial sync is done.
        assert!(
            wait_for_tcp(electrum_port, Duration::from_secs(60)),
            "electrs did not start listening on {electrum_port}"
        );

        let (server_pubkey, server_guard) = start_server(
            "contextbtc-electrs-server",
            &relay_url,
            &[("ELECTRS_ADDR", &electrum_addr)],
        )?;

        Ok(Self {
            relay_url,
            server_pubkey,
            _server: server_guard,
            _electrs: electrs_guard,
            node,
            _relay: relay_guard,
        })
    }
}
