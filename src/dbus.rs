//! Supervise the D-Bus filter proxies — one host-side `xdg-dbus-proxy` sidecar
//! per [`crate::spec::DbusProxy`] entry.
//!
//! This is **mechanism, not policy** (`docs/architecture.md`'s third principle):
//! the engine spawns the proxy, waits for it to signal ready, binds its filtered
//! socket into the sandbox, and tears it down with the run. The *rules* it
//! enforces travel in the resolved spec — the Nix producer layer is the policy
//! engine; here we only translate `DbusPolicy` to the proxy's own flags and
//! supervise the process. `--filter` is always passed: a proxy with no rules
//! still filters (connect + authenticate, talk to nothing), never a raw socket.
//!
//! **Why the proxy exists at all** (see `nix/probe/dbus.nix`): a naive host-bus
//! bind fails because D-Bus's SASL `EXTERNAL` auth keys on the connecting uid,
//! and the sandbox presents its mapped (container) uid, which the host daemon
//! rejects. `xdg-dbus-proxy` runs host-side as the *real* uid, authenticates, and
//! relays — so the sandbox reaches the bus through a socket it's allowed to open.
//!
//! **One process per entry.** Each `dbus[]` entry becomes its own proxy process
//! (rather than one process with N bus/socket pairs) for lifecycle isolation:
//! independent ready-sync and independent teardown, so a failing system-bus proxy
//! can't drag down the session one. They start concurrently host-side, off the
//! container's critical path.
//!
//! **Ready-sync.** The proxy is handed a pipe write end via `--fd=N`; it writes a
//! byte once its sockets are set up (which includes connecting + authenticating
//! upstream), then keeps running. We block on the read end before returning, so
//! the socket exists when crun resolves the sandbox's bind — and an upstream auth
//! failure surfaces here (EOF before the byte) rather than as a deep bind error.

use std::io::Read;
use std::os::fd::AsRawFd;
use std::path::Path;
use std::process::{Child, Command};

use anyhow::{bail, Context, Result};
use nix::fcntl::{fcntl, FcntlArg, FdFlag};
use nix::sys::signal::{kill, Signal};
use nix::unistd::{pipe, Pid};
use oci_spec::runtime::{Mount as OciMount, MountBuilder};

use crate::spec::{BusAddress, DbusProxy};

/// Absolute path to `xdg-dbus-proxy`, pinned at build time by Nix
/// (`XDG_DBUS_PROXY_BIN=${pkgs.xdg-dbus-proxy}/bin/xdg-dbus-proxy`) — same bake as
/// `crun` (`src/crun.rs`), which also makes it a runtime dependency via Nix's
/// reference scan. No PATH lookup, no drift.
const XDG_DBUS_PROXY: &str = env!("XDG_DBUS_PROXY_BIN");

/// The running filter proxies for a sandbox, plus the binds that route their
/// sockets in. Supervised for the container's lifetime; [`finish`](Self::finish)
/// (or drop) kills and reaps them. Empty when the spec carries no `dbus` — a
/// zero-cost no-op, no processes spawned.
pub struct DbusProxies {
    children: Vec<Child>,
    mounts: Vec<OciMount>,
}

impl DbusProxies {
    /// Spawn one `xdg-dbus-proxy` per entry, each connected to its host bus and
    /// filtered by its policy, blocking until each signals ready. `work` is the
    /// per-run dir; the filtered sockets land in `work/dbus/`. `uid` resolves the
    /// `session`/`system` sentinels' default host addresses (the invoking user).
    pub fn start(specs: &[DbusProxy], uid: u32, work: &Path) -> Result<DbusProxies> {
        if specs.is_empty() {
            return Ok(DbusProxies {
                children: Vec::new(),
                mounts: Vec::new(),
            });
        }
        let dir = work.join("dbus");
        std::fs::create_dir_all(&dir).with_context(|| format!("mkdir {}", dir.display()))?;

        let mut children = Vec::with_capacity(specs.len());
        let mut mounts = Vec::with_capacity(specs.len());
        for (i, proxy) in specs.iter().enumerate() {
            // The filtered socket the sandbox connects to, host-side; bound onto
            // the spec's in-sandbox destination (a baked mountpoint).
            let host_sock = dir.join(format!("{i}.sock"));
            // On any failure mid-loop, the proxies started so far are killed by
            // `DbusProxies`' Drop (we return the error and `self` unwinds).
            let child = spawn_proxy(proxy, uid, &host_sock)
                .with_context(|| format!("starting dbus proxy for {}", proxy.socket.display()))?;
            children.push(child);
            mounts.push(socket_bind(&host_sock, proxy)?);
            // Push the child *before* the bind so a `socket_bind` error still
            // leaves the spawned proxy tracked for teardown.
        }
        Ok(DbusProxies { children, mounts })
    }

    /// The sandbox binds that route each proxy's socket to its in-sandbox path.
    /// Appended to the spec's `mounts` before realization (they are verbatim OCI
    /// binds, same as any other placement).
    pub fn mounts(&self) -> Vec<OciMount> {
        self.mounts.clone()
    }

    /// Kill and reap the proxies. Consumes `self`; the real work is in `Drop`, so
    /// an early `?`-return before this call tears down just the same.
    pub fn finish(self) {}
}

impl Drop for DbusProxies {
    fn drop(&mut self) {
        for child in &mut self.children {
            // SIGTERM (xdg-dbus-proxy exits cleanly on it), then reap so no
            // zombie outlives the run. Best-effort: the process may already be
            // gone (it died on its own), in which case the kill/wait just noop.
            let _ = kill(Pid::from_raw(child.id() as i32), Signal::SIGTERM);
            let _ = child.wait();
        }
    }
}

/// Spawn one proxy and block until it signals ready.
fn spawn_proxy(proxy: &DbusProxy, uid: u32, host_sock: &Path) -> Result<Child> {
    let address = resolve_address(&proxy.bus, uid);

    // The ready pipe: xdg-dbus-proxy writes a byte to the write end once its
    // sockets are up. The write end must survive exec (no CLOEXEC — `pipe()`'s
    // default), so the child inherits it at its raw number; the read end is
    // CLOEXEC so it doesn't leak into the child.
    let (read_fd, write_fd) = pipe().context("creating dbus-proxy ready pipe")?;
    fcntl(&read_fd, FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC)).context("CLOEXEC on ready pipe")?;

    let mut cmd = Command::new(XDG_DBUS_PROXY);
    // `--fd` first (global), then the bus/socket pair, then this socket's policy.
    cmd.arg(format!("--fd={}", write_fd.as_raw_fd()));
    cmd.arg(&address);
    cmd.arg(host_sock);
    // Deny-by-default; without this the proxy is a transparent relay (a raw
    // socket with a uid fix), which defeats the whole point.
    cmd.arg("--filter");
    for flag in policy_flags(proxy) {
        cmd.arg(flag);
    }

    let child = cmd
        .spawn()
        .with_context(|| format!("spawning {XDG_DBUS_PROXY}"))?;

    // Close our copy of the write end so the read end sees EOF if the proxy dies
    // before signaling — only the child holds a write end now.
    drop(write_fd);
    wait_ready(read_fd, &address)?;
    Ok(child)
}

/// Translate a [`DbusPolicy`](crate::spec::DbusPolicy) to `xdg-dbus-proxy` flags,
/// one bucket per rule kind — a literal transcription of the CLI (the engine
/// mirrors the tool, holds no policy). `call`/`broadcast` emit one flag per rule.
fn policy_flags(proxy: &DbusProxy) -> Vec<String> {
    let p = &proxy.policy;
    let mut flags = Vec::new();
    flags.extend(p.see.iter().map(|n| format!("--see={n}")));
    flags.extend(p.talk.iter().map(|n| format!("--talk={n}")));
    flags.extend(p.own.iter().map(|n| format!("--own={n}")));
    for (name, rules) in &p.call {
        flags.extend(rules.iter().map(|r| format!("--call={name}={r}")));
    }
    for (name, rules) in &p.broadcast {
        flags.extend(rules.iter().map(|r| format!("--broadcast={name}={r}")));
    }
    flags
}

/// Resolve a [`BusAddress`] to a concrete D-Bus address string. The
/// `session`/`system` sentinels resolve at launch (never baked): the environment
/// address if set, else the well-known path for the invoking uid.
fn resolve_address(bus: &BusAddress, uid: u32) -> String {
    match bus {
        BusAddress::Session => std::env::var("DBUS_SESSION_BUS_ADDRESS")
            .unwrap_or_else(|_| format!("unix:path=/run/user/{uid}/bus")),
        BusAddress::System => std::env::var("DBUS_SYSTEM_BUS_ADDRESS")
            .unwrap_or_else(|_| "unix:path=/run/dbus/system_bus_socket".to_string()),
        BusAddress::Address(addr) => addr.clone(),
    }
}

/// The bind routing a proxy's host socket onto its in-sandbox path — a verbatim
/// OCI bind (`rw`, `nosuid`, `nodev`). The destination is a baked mountpoint the
/// producer froze into the sealed root (a `touch`ed socket file), so the bind
/// lands on an existing target; the host socket path never leaks in.
fn socket_bind(host_sock: &Path, proxy: &DbusProxy) -> Result<OciMount> {
    Ok(MountBuilder::default()
        .destination(proxy.socket.clone())
        .source(host_sock.to_path_buf())
        .typ("bind".to_string())
        .options(vec![
            "bind".into(),
            "rw".into(),
            "nosuid".into(),
            "nodev".into(),
        ])
        .build()?)
}

/// Block until the proxy writes its ready byte, or fail if it exits first.
fn wait_ready(read_fd: std::os::fd::OwnedFd, address: &str) -> Result<()> {
    let mut file = std::fs::File::from(read_fd);
    let mut byte = [0u8; 1];
    loop {
        match file.read(&mut byte) {
            Ok(1) => return Ok(()),
            // EOF with no byte: the proxy exited before signaling ready — almost
            // always an upstream connect/auth failure (e.g. the uid wall, or no
            // bus at `address`). Surface it here, not as a deep bind failure.
            Ok(_) => bail!(
                "xdg-dbus-proxy exited before ready — could not connect/authenticate to {address}"
            ),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e).context("waiting on dbus-proxy ready fd"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::{BusAddress, DbusPolicy, DbusProxy};
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    fn proxy(bus: BusAddress, policy: DbusPolicy) -> DbusProxy {
        DbusProxy {
            bus,
            socket: PathBuf::from("/run/user/1000/bus"),
            policy,
        }
    }

    #[test]
    fn empty_policy_yields_only_filter() {
        // No rules ⇒ no policy flags; the caller still passes `--filter`, so the
        // result is deny-all (connect + authenticate, talk to nothing).
        assert!(policy_flags(&proxy(BusAddress::Session, DbusPolicy::default())).is_empty());
    }

    #[test]
    fn policy_flags_transcribe_every_rule_kind() {
        let policy = DbusPolicy {
            see: vec!["org.a".into()],
            talk: vec!["org.freedesktop.Notifications".into()],
            own: vec!["org.mpris.MediaPlayer2.app".into()],
            call: BTreeMap::from([(
                "org.freedesktop.Notifications".to_string(),
                vec!["Notify".to_string(), "GetCapabilities".to_string()],
            )]),
            broadcast: BTreeMap::from([(
                "org.freedesktop.Notifications".to_string(),
                vec!["*".to_string()],
            )]),
        };
        let flags = policy_flags(&proxy(BusAddress::Session, policy));
        assert_eq!(
            flags,
            vec![
                "--see=org.a",
                "--talk=org.freedesktop.Notifications",
                "--own=org.mpris.MediaPlayer2.app",
                "--call=org.freedesktop.Notifications=Notify",
                "--call=org.freedesktop.Notifications=GetCapabilities",
                "--broadcast=org.freedesktop.Notifications=*",
            ]
        );
    }

    #[test]
    fn address_sentinels_resolve_at_launch() {
        // Explicit address is verbatim.
        assert_eq!(
            resolve_address(&BusAddress::Address("unix:path=/tmp/a11y".into()), 1000),
            "unix:path=/tmp/a11y"
        );
        // `session` falls back to the uid's well-known path when the env is unset.
        std::env::remove_var("DBUS_SESSION_BUS_ADDRESS");
        assert_eq!(
            resolve_address(&BusAddress::Session, 1234),
            "unix:path=/run/user/1234/bus"
        );
    }
}
