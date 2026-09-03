//! `pierogi net` — pinned network namespaces inside the pause process's registry.
//!
//! A network here is a **name bound to a network namespace**, and nothing more:
//! no addresses, no routes, no policy. Those arrive later; this module owns the
//! entity's lifecycle, which has to be right first.
//!
//! **Two processes, and why.** The user-facing command runs in the base context.
//! The work runs in a helper that has joined the pin's user + mount namespace,
//! because that is the only place with authority over namespaces the pin owns —
//! authority is capability in the *owning* user namespace, and a bind mount in the
//! registry needs `CAP_SYS_ADMIN` over the mount namespace holding it. The helper
//! joins first thing in `main` and never returns; see [`crate::unshare::enter_pin`]
//! for why that is one-way and why it must precede any thread.
//!
//! **Observe → decide → act, from the first operation.** `create` reads the
//! current state before doing anything, so it is idempotent from the start. That
//! is deliberately the *shape* of reconciliation without any of its machinery —
//! no plan, no diff, no ordering. Those need several interacting entity types to
//! be designed against anything real: turnip's expensive retrofit was not adding
//! idempotence, it was discovering that per-network staging collided on
//! container-global resources once containers were multi-homed, which no amount
//! of early design would have predicted. The cheap half is cheap now; the
//! expensive half is not yet knowable.
//!
//! **Why replacing is refused rather than converged.** A name that already exists
//! is left alone. Re-minting would strand every sandbox already attached to the
//! old namespace — the exact outcome the pin exists to prevent. Once networks
//! carry configuration, "exists but differs" will need converge-in-place; it can
//! never mean replace.
//!
//! **The stale case is real, not defensive.** A teardown that unmounts and then
//! dies before unlinking leaves an empty regular file where a namespace was. It
//! is indistinguishable from a live pin by name alone, so every read checks for an
//! `nsfs` inode. (Podman pins onto the host runtime directory, where such
//! leftovers survive until logout; the registry tmpfs narrows that to the pin's
//! lifetime but does not remove the check.)

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use nix::mount::{mount, MsFlags};
use nix::sched::{unshare, CloneFlags};
use nix::sys::statfs::{fstatfs, NSFS_MAGIC};

use crate::unshare::{ensure_pin, enter_pin, net_dir};

/// What a name currently is. The three cases the helper must tell apart before it
/// decides anything.
#[derive(Debug, PartialEq, Eq)]
enum State {
    /// No such name.
    Absent,
    /// A live network namespace.
    Live(u64),
    /// An empty file where a namespace used to be — a teardown that unmounted and
    /// then died before unlinking.
    Stale,
}

/// `pierogi net create <name>` — ensure a network namespace is pinned at `name`.
///
/// Idempotent: an existing live network is reported and left untouched.
pub fn create(name: &str) -> Result<()> {
    validate_name(name)?;
    let pin = ensure_pin()?;
    let path = net_dir().join(name);

    // The helper re-execs so it can join the pin at the top of `main` while still
    // single-threaded. `/proc/self/exe` is the running image regardless of argv[0].
    let status = Command::new("/proc/self/exe")
        .arg("__net-create")
        .arg(pin.pid.to_string())
        .arg(&path)
        .status()
        .context("spawning the in-pin helper")?;
    if !status.success() {
        bail!("creating network {name:?} failed ({status})");
    }
    Ok(())
}

/// The `__net-create` helper: joins the pin, then observe → decide → act.
///
/// Runs as its own process precisely so the join can happen before anything else.
pub fn helper_create(pin_pid: i32, path: PathBuf) -> Result<()> {
    enter_pin(pin_pid)?;

    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("<unnamed>");

    match observe(&path)? {
        State::Live(ino) => {
            eprintln!("pierogi: network {name:?} exists (net:[{ino}])");
        }
        State::Stale => {
            // Not a mount — a stale pin is by definition an unmounted leftover —
            // so unlinking is the whole cleanup.
            std::fs::remove_file(&path)
                .with_context(|| format!("removing stale pin {}", path.display()))?;
            let ino = mint(&path)?;
            eprintln!("pierogi: network {name:?} created (net:[{ino}], replaced a stale pin)");
        }
        State::Absent => {
            let ino = mint(&path)?;
            eprintln!("pierogi: network {name:?} created (net:[{ino}])");
        }
    }
    Ok(())
}

/// Read what is actually at `path`, distinguishing a live namespace from an empty
/// leftover. Name and existence are not enough — only the filesystem type is.
fn observe(path: &Path) -> Result<State> {
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(State::Absent),
        Err(e) => return Err(e).with_context(|| format!("opening {}", path.display())),
    };
    let st = fstatfs(&file).with_context(|| format!("statfs {}", path.display()))?;
    if st.filesystem_type() == NSFS_MAGIC {
        let ino = file
            .metadata()
            .with_context(|| format!("stat {}", path.display()))?
            .ino();
        Ok(State::Live(ino))
    } else {
        Ok(State::Stale)
    }
}

/// Mint a fresh network namespace and pin it at `path`, returning its inode.
///
/// The bind must happen **while still inside the new namespace**: `/proc/self/ns/net`
/// is the only handle to it, and once this process leaves there is no way back —
/// the namespace is owned by the pin's user namespace, and leaving costs the
/// authority needed to return. So: create the target, unshare, bind, and let the
/// mount hold the namespace open after we exit.
fn mint(path: &Path) -> Result<u64> {
    std::fs::File::create_new(path)
        .with_context(|| format!("creating mount target {}", path.display()))?;

    let pinned = || -> Result<u64> {
        unshare(CloneFlags::CLONE_NEWNET).context("unsharing a network namespace")?;
        mount(
            Some(Path::new("/proc/self/ns/net")),
            path,
            None::<&Path>,
            MsFlags::MS_BIND,
            None::<&Path>,
        )
        .with_context(|| format!("pinning the namespace at {}", path.display()))?;
        Ok(std::fs::metadata("/proc/self/ns/net")
            .context("stat /proc/self/ns/net")?
            .ino())
    }();

    // Don't leave the empty target behind if we failed to pin anything to it —
    // that is exactly the stale-placeholder state this module has to detect.
    if pinned.is_err() {
        let _ = std::fs::remove_file(path);
    }
    pinned
}

/// A name becomes a path component in the registry, so it must be exactly that:
/// no separators, no traversal, nothing that resolves anywhere else.
fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("network name is empty");
    }
    if name == "." || name == ".." {
        bail!("network name {name:?} is a path traversal");
    }
    if let Some(bad) = name.chars().find(|c| *c == '/' || *c == '\0') {
        bail!("network name {name:?} contains {bad:?}");
    }
    Ok(())
}

use std::os::unix::fs::MetadataExt;

#[cfg(test)]
mod tests {
    use super::validate_name;

    #[test]
    fn accepts_ordinary_names() {
        for ok in ["homelab", "net0", "a-b_c.1"] {
            assert!(validate_name(ok).is_ok(), "{ok:?} should be accepted");
        }
    }

    #[test]
    fn rejects_traversal_and_separators() {
        // Each of these would resolve outside the registry, or over something else
        // inside it, if it reached the mount call as a path component.
        for bad in ["", ".", "..", "a/b", "../escape", "/abs", "nul\0byte"] {
            assert!(validate_name(bad).is_err(), "{bad:?} should be rejected");
        }
    }
}
