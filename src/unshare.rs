//! `pierogi unshare` — hold a **user + mount namespace** open in a pause process,
//! and run commands inside it.
//!
//! **Step one, deliberately unmapped.** This builds only the namespace mechanics:
//! the pause process, the handshake that creates it, and the join. It writes *no*
//! id map, so the namespace maps nothing at all. That is a real, observable state
//! rather than a stub — and the thing it makes obvious is why the map is needed:
//!
//! * A process inside an unmapped user namespace has no valid uid. It reads as
//!   `nobody` (the kernel's overflow uid), and every file it touches is owned by
//!   somebody else.
//! * `setns` into a user namespace grants a full capability set — but `execve`
//!   takes it straight back unless the process's euid is 0. With nothing mapped,
//!   euid is the overflow uid, so the capabilities evaporate at the first exec.
//!   Entering works; doing anything does not.
//!
//! Mapping is a separate problem with separate failure modes (`/etc/subuid`, the
//! setuid helpers, the write-once map), so it is a separate step. See
//! [`docs/network.md`] for where this is going.
//!
//! **Why a process at all.** A rootless user namespace has no filesystem identity
//! and no way to acquire one: publishing a namespace bind-mount into the *host's*
//! mount namespace needs privilege we do not have, and propagation out of an
//! unprivileged mount namespace is inbound-only, so a rootless bind is invisible
//! outside. The only thing that can hold a user namespace open is a process that
//! is in it. The pause process is that and nothing else — it holds two namespaces
//! and blocks. No event loop, no policy, no IPC.
//!
//! **Why the mount namespace comes along now.** It is what will later let network
//! namespaces be *named*: a netns bind-mounted inside it outlives the process that
//! created it, so one pause process can hold many named networks with no process
//! running in any of them. Unsharing it here costs nothing and avoids rebuilding
//! the pin later.
//!
//! **Why a fork, and why the join is in `pre_exec`.** `unshare(CLONE_NEWUSER)` and
//! `setns(CLONE_NEWUSER)` both require a single-threaded caller. The pause child is
//! forked before any thread exists, and the join happens in a `pre_exec` — the
//! post-fork child, single-threaded by construction. This is the constraint that
//! forces Go implementations to re-exec through a helper; Rust needs no such
//! boundary.

use std::os::fd::OwnedFd;
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;

use anyhow::{bail, Context, Result};
use nix::errno::Errno;
use nix::fcntl::{open, OFlag};
use nix::sched::{setns, unshare, CloneFlags};
use nix::sys::stat::Mode;
use nix::unistd::{
    dup2_stderr, dup2_stdin, dup2_stdout, fork, pipe, read, setsid, write, ForkResult, Pid,
};
use serde::{Deserialize, Serialize};

/// What the pause process holds. `CLONE_NEWUSER` is the authority everything else
/// will rest on; `CLONE_NEWNS` is what will let namespaces be named.
const PIN_NAMESPACES: CloneFlags = CloneFlags::CLONE_NEWUSER.union(CloneFlags::CLONE_NEWNS);

/// A live pause process. Both namespace inodes are recorded because a pid on its
/// own is not a stable handle: pids are recycled, and a recycled one would
/// otherwise resolve to an unrelated process's namespaces — a failure that
/// *succeeds*, which is the dangerous kind.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
struct Pin {
    pid: i32,
    user_ns: u64,
    mnt_ns: u64,
}

/// `pierogi unshare [cmd…]` — join the pause process's user + mount namespace and
/// exec `cmd` there (default: `$SHELL`), starting the pause process if none is live.
///
/// Expect to land as `nobody` with no capabilities: nothing is mapped yet.
pub fn unshare_cmd(argv: Vec<String>) -> Result<()> {
    let pin = ensure_pin()?;

    let argv = if argv.is_empty() {
        vec![std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into())]
    } else {
        argv
    };

    // Open both namespace fds *before* joining anything: once inside the user
    // namespace, `/proc/<pin>/…` is no longer openable from a sibling.
    let user_ns = std::fs::File::open(format!("/proc/{}/ns/user", pin.pid)).with_context(|| {
        format!(
            "opening the pause process's user namespace (pid {})",
            pin.pid
        )
    })?;
    let mnt_ns = std::fs::File::open(format!("/proc/{}/ns/mnt", pin.pid)).with_context(|| {
        format!(
            "opening the pause process's mount namespace (pid {})",
            pin.pid
        )
    })?;

    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    // SAFETY: runs post-fork (single-threaded, as setns(CLONE_NEWUSER) demands)
    // and pre-exec, so only async-signal-safe work is legal — two setns calls on
    // fds opened above, no allocation. Order matters: entering the user namespace
    // is what grants the CAP_SYS_ADMIN that makes the mount-namespace join legal.
    unsafe {
        cmd.pre_exec(move || {
            setns(&user_ns, CloneFlags::CLONE_NEWUSER).map_err(errno_io)?;
            setns(&mnt_ns, CloneFlags::CLONE_NEWNS).map_err(errno_io)?;
            Ok(())
        });
    }

    let status = cmd
        .status()
        .with_context(|| format!("spawning {:?} in the pause namespace", argv[0]))?;
    match status.code() {
        Some(0) => Ok(()),
        Some(code) => std::process::exit(code),
        None => bail!("command terminated by signal: {status}"),
    }
}

/// Return a live pause process, starting one if the record is missing, dead, or
/// has been recycled onto a different process.
fn ensure_pin() -> Result<Pin> {
    if let Some(pin) = load_pin() {
        if pin_is_live(&pin) {
            return Ok(pin);
        }
        eprintln!(
            "pierogi: recorded pause process {} is stale; starting a new one",
            pin.pid
        );
    }
    let pin = start_pause()?;
    save_pin(&pin)?;
    eprintln!("pierogi: started pause process {} (unmapped)", pin.pid);
    Ok(pin)
}

/// Live only if *both* namespace inodes still match. The pid alone proves nothing.
fn pin_is_live(pin: &Pin) -> bool {
    ns_ino(pin.pid, "user").is_ok_and(|i| i == pin.user_ns)
        && ns_ino(pin.pid, "mnt").is_ok_and(|i| i == pin.mnt_ns)
}

fn ns_ino(pid: i32, which: &str) -> Result<u64> {
    let path = format!("/proc/{pid}/ns/{which}");
    Ok(std::fs::metadata(&path)
        .with_context(|| format!("stat {path}"))?
        .ino())
}

/// Fork the pause process and record the namespaces it created.
///
/// One pipe, one direction: only the child can unshare its own namespaces, and
/// the parent cannot stat them until it has. The child announces, the parent
/// records. (When id mapping lands it needs the reverse direction too — the child
/// must wait to be told its map exists — but with nothing to configure there is
/// nothing to wait for.)
fn start_pause() -> Result<Pin> {
    let (ready_r, ready_w) = pipe().context("creating pause ready pipe")?;

    // SAFETY: called before any thread is spawned, so the child inherits a
    // single-threaded process — required by unshare(CLONE_NEWUSER). The child
    // touches only syscalls before it blocks.
    match unsafe { fork() }.context("forking the pause process")? {
        ForkResult::Child => {
            drop(ready_r);
            let code = i32::from(pause_child(ready_w).is_err());
            unsafe { libc::_exit(code) }
        }
        ForkResult::Parent { child } => {
            drop(ready_w);
            match finish_pause(child, &ready_r) {
                Ok(pin) => Ok(pin),
                Err(e) => {
                    // Don't leave a half-built pause process behind.
                    let _ = nix::sys::signal::kill(child, nix::sys::signal::Signal::SIGKILL);
                    let _ = nix::sys::wait::waitpid(child, None);
                    Err(e)
                }
            }
        }
    }
}

/// The pause process: detach, unshare, announce, then hold forever.
/// Post-fork, so syscalls only — no allocation, no destructors.
fn pause_child(ready: OwnedFd) -> Result<(), Errno> {
    // Leave the caller's session so the pin outlives the terminal that started it:
    // a pause process killed by Ctrl-C takes everything it holds with it.
    setsid()?;
    unshare(PIN_NAMESPACES)?;
    write(&ready, &[1u8])?;
    drop(ready);

    // A pin outlives its creator, so every fd it keeps is held *forever*.
    // Inherited stdio is the one that bites: holding the creator's stdout leaves
    // any reader — a pipeline, a command substitution — waiting on an EOF that
    // never comes.
    detach_stdio()?;

    loop {
        nix::unistd::pause();
    }
}

/// Parent half: wait for the child to unshare, then record what it made.
fn finish_pause(child: Pid, ready: &OwnedFd) -> Result<Pin> {
    let mut byte = [0u8; 1];
    loop {
        match read(ready, &mut byte) {
            Ok(1) => break,
            // EOF without the byte: the child died before unsharing. Usually that
            // means unprivileged user namespaces are unavailable.
            Ok(_) => bail!(
                "pause process exited before unsharing — are unprivileged user namespaces enabled?"
            ),
            Err(Errno::EINTR) => continue,
            Err(e) => return Err(e).context("waiting for the pause process to unshare"),
        }
    }
    Ok(Pin {
        pid: child.as_raw(),
        user_ns: ns_ino(child.as_raw(), "user")?,
        mnt_ns: ns_ino(child.as_raw(), "mnt")?,
    })
}

/// Point stdin/stdout/stderr at `/dev/null`. Redirect rather than close, so a
/// stray write lands in the void instead of on whatever later claims fd 1.
///
/// A `CStr` literal keeps this allocation-free: `NixPath` for `Path` builds a
/// `CString`. Our fork is from a single-threaded parent, so an allocation would in
/// fact be safe — but staying allocation-free keeps the child correct if a thread
/// is ever spawned earlier in the verb.
fn detach_stdio() -> Result<(), Errno> {
    let null = open(c"/dev/null", OFlag::O_RDWR, Mode::empty())?;
    dup2_stdin(&null)?;
    dup2_stdout(&null)?;
    dup2_stderr(&null)?;
    // `null` is >2 (0-2 are still the inherited stdio when we open it), so
    // dropping it closes only the extra descriptor.
    Ok(())
}

fn state_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("pierogi")
}

fn pin_path() -> PathBuf {
    state_dir().join("pause.json")
}

fn load_pin() -> Option<Pin> {
    serde_json::from_slice(&std::fs::read(pin_path()).ok()?).ok()
}

fn save_pin(pin: &Pin) -> Result<()> {
    let dir = state_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("mkdir {}", dir.display()))?;
    let path = pin_path();
    std::fs::write(
        &path,
        serde_json::to_vec(pin).context("serializing pin record")?,
    )
    .with_context(|| format!("writing {}", path.display()))
}

fn errno_io(e: Errno) -> std::io::Error {
    std::io::Error::from_raw_os_error(e as i32)
}

#[cfg(test)]
mod tests {
    use super::Pin;

    /// The record is the only on-disk format here, and a silent change to it would
    /// make every existing pin look stale (or, worse, look live at a wrong pid).
    #[test]
    fn pin_record_round_trips() {
        let pin = Pin {
            pid: 4242,
            user_ns: 4026533682,
            mnt_ns: 4026533683,
        };
        let json = serde_json::to_vec(&pin).unwrap();
        assert_eq!(serde_json::from_slice::<Pin>(&json).unwrap(), pin);
        assert_eq!(
            String::from_utf8(json).unwrap(),
            r#"{"pid":4242,"user_ns":4026533682,"mnt_ns":4026533683}"#
        );
    }
}
