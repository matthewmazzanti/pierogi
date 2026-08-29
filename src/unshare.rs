//! `pierogi unshare` — run a command inside a persistent, user-owned **user +
//! mount namespace**, plus the pause process that keeps that namespace alive
//! between invocations.
//!
//! This is the rootless half of the *registry* in [`docs/network.md`]: a user
//! namespace carrying a real subordinate-id range, and a mount namespace to hold
//! bind-mounted netns names. It mirrors `podman unshare` and its pause process
//! deliberately — that design is forced by the kernel, not chosen:
//!
//! * **A rootless user namespace needs a live holder.** Nothing else can pin one.
//!   A rootless process cannot publish a namespace bind-mount into the *host's*
//!   mount namespace — propagation into an unprivileged mount namespace is
//!   inbound-only, so the bind succeeds locally and the host sees nothing.
//!   `ip netns`-style path pinning is a root-only privilege. The pause process
//!   *is* the persistence mechanism.
//! * **The mount namespace is what makes *names* possible.** A netns bind-mounted
//!   inside it outlives the process that created it, so one pause process can hold
//!   many named networks with no process running in any of them.
//! * **The map must include the invoking uid.** `0 → <uid>` first, then the
//!   `/etc/subuid` range. That first entry is not cosmetic: it gives anything that
//!   later joins this namespace a *mapped* identity, and therefore capabilities
//!   that survive `execve` (an unmapped joiner lands as `nobody` and is stripped
//!   at the next exec). podman's rootless map has exactly this shape.
//!
//! **Why a fork, and why the join is in `pre_exec`.** Both `unshare(CLONE_NEWUSER)`
//! and `setns(CLONE_NEWUSER)` require a single-threaded caller. The pause child is
//! forked before anything spawns a thread, and the join happens in a `pre_exec` —
//! the post-fork child, which has exactly one thread by construction. This is the
//! wall that forces Go implementations to re-exec through `podman unshare`; Rust
//! needs no such boundary.
//!
//! **Why `newuidmap`/`newgidmap` are not pinned store paths.** Every other binary
//! the engine drives is baked by Nix (`CRUN_BIN`, `XDG_DBUS_PROXY_BIN`). These two
//! cannot be: they only function through their setuid wrappers (`/run/wrappers/bin`
//! on NixOS) and the store copies carry no setuid bit. So they are resolved on
//! `PATH` — the one lookup the engine does — and their absence is a loud error.

use std::os::fd::OwnedFd;
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;

use anyhow::{bail, Context, Result};
use nix::errno::Errno;
use nix::sched::{setns, unshare, CloneFlags};
use nix::unistd::{fork, getgid, getuid, pipe, read, setsid, write, ForkResult, Pid, User};
use serde::{Deserialize, Serialize};

/// The namespaces the pause process holds. `CLONE_NEWUSER` is the authority the
/// whole design rests on; `CLONE_NEWNS` is what lets netns be *named* rather than
/// addressed by pid.
const PIN_NAMESPACES: CloneFlags = CloneFlags::CLONE_NEWUSER.union(CloneFlags::CLONE_NEWNS);

/// What we record about a live pause process. The inodes are the anti-pid-reuse
/// check: a pid alone is not a stable handle, and a recycled one would otherwise
/// resolve to some unrelated process's namespaces.
#[derive(Serialize, Deserialize, Debug, Clone, Copy)]
struct Pin {
    pid: i32,
    user_ns: u64,
    mnt_ns: u64,
}

/// `pierogi unshare [cmd…]` — join the pause process's user + mount namespace and
/// exec `cmd` there (default: `$SHELL`). Starts the pause process if there isn't
/// a live one.
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
    // The join runs post-fork (single-threaded, as setns(CLONE_NEWUSER) demands)
    // and pre-exec. Order matters: entering the user namespace is what grants the
    // CAP_SYS_ADMIN that makes the mount-namespace join legal.
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

/// Return a live pause process, starting one if the recorded pin is missing,
/// dead, or has been recycled onto a different process.
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
    eprintln!("pierogi: started pause process {}", pin.pid);
    Ok(pin)
}

/// A pin is live only if *both* namespace inodes still match what we recorded —
/// the pid on its own proves nothing once it can be recycled.
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

/// Fork the pause process, write its id maps, and hand back the recorded pin.
///
/// The child cannot write its own maps (that needs privilege it does not have),
/// and the parent cannot write them until the child has unshared — so the two
/// halves rendezvous over a pipe pair: child signals "unshared", parent writes the
/// maps via the setuid helpers, then releases the child to hold forever.
fn start_pause() -> Result<Pin> {
    let (ready_r, ready_w) = pipe().context("creating pause ready pipe")?;
    let (go_r, go_w) = pipe().context("creating pause release pipe")?;

    // SAFETY: called before any thread is spawned, so the child inherits a
    // single-threaded process — required by unshare(CLONE_NEWUSER). The child
    // touches only syscalls before it blocks.
    match unsafe { fork() }.context("forking the pause process")? {
        ForkResult::Child => {
            drop(ready_r);
            drop(go_w);
            let code = i32::from(pause_child(&ready_w, &go_r).is_err());
            unsafe { libc::_exit(code) }
        }
        ForkResult::Parent { child } => {
            drop(ready_w);
            drop(go_r);
            match finish_pause(child, &ready_r, &go_w) {
                Ok(pin) => Ok(pin),
                Err(e) => {
                    // Don't leave a half-built pause process behind: it holds a
                    // namespace with no usable map.
                    let _ = nix::sys::signal::kill(child, nix::sys::signal::Signal::SIGKILL);
                    let _ = nix::sys::wait::waitpid(child, None);
                    Err(e)
                }
            }
        }
    }
}

/// The pause process itself: unshare, announce, wait to be released, then hold the
/// namespaces open forever. Post-fork, so syscalls only.
fn pause_child(ready: &OwnedFd, go: &OwnedFd) -> Result<(), Errno> {
    // Detach from the caller's session so the pin outlives the terminal that
    // started it — a pause process killed by Ctrl-C would take every network
    // bind-mounted inside it.
    setsid()?;
    unshare(PIN_NAMESPACES)?;
    write(ready, &[1u8])?;
    // Block until the parent has written the id maps. EOF here means the parent
    // failed or died; either way there is nothing to hold open for.
    let mut byte = [0u8; 1];
    loop {
        match read(go, &mut byte) {
            Ok(1) => break,
            Ok(_) => return Err(Errno::EPIPE),
            Err(Errno::EINTR) => continue,
            Err(e) => return Err(e),
        }
    }
    loop {
        unsafe { libc::pause() };
    }
}

/// Parent half of the rendezvous: wait for the child to unshare, map it, release it.
fn finish_pause(child: Pid, ready: &OwnedFd, go: &OwnedFd) -> Result<Pin> {
    let mut byte = [0u8; 1];
    loop {
        match read(ready, &mut byte) {
            Ok(1) => break,
            // EOF without the byte: the child failed to unshare. On most systems
            // that means unprivileged user namespaces are unavailable.
            Ok(_) => bail!(
                "pause process exited before unsharing — are unprivileged user namespaces enabled?"
            ),
            Err(Errno::EINTR) => continue,
            Err(e) => return Err(e).context("waiting for the pause process to unshare"),
        }
    }

    write_id_maps(child)?;

    let pin = Pin {
        pid: child.as_raw(),
        user_ns: ns_ino(child.as_raw(), "user")?,
        mnt_ns: ns_ino(child.as_raw(), "mnt")?,
    };

    write(go, &[1u8]).context("releasing the pause process")?;
    Ok(pin)
}

/// Write the pause process's uid/gid maps through the setuid helpers.
///
/// Two entries, in this order: the invoking user at container id 0, then their
/// whole subordinate range. The map is **write-once** — this is the only chance to
/// size it, and it bounds every user namespace later nested inside.
fn write_id_maps(child: Pid) -> Result<()> {
    let uid = getuid();
    let gid = getgid();
    let user = User::from_uid(uid)
        .context("looking up the invoking user")?
        .ok_or_else(|| anyhow::anyhow!("no passwd entry for uid {uid}"))?;

    let (sub_uid, uid_count) = subid_range("/etc/subuid", &user.name)?;
    let (sub_gid, gid_count) = subid_range("/etc/subgid", &user.name)?;

    helper(
        "newuidmap",
        &[
            child.to_string(),
            "0".into(),
            uid.to_string(),
            "1".into(),
            "1".into(),
            sub_uid.to_string(),
            uid_count.to_string(),
        ],
    )?;
    helper(
        "newgidmap",
        &[
            child.to_string(),
            "0".into(),
            gid.to_string(),
            "1".into(),
            "1".into(),
            sub_gid.to_string(),
            gid_count.to_string(),
        ],
    )
}

/// Read the subordinate id range for `user` from `/etc/subuid` or `/etc/subgid`.
fn subid_range(file: &str, user: &str) -> Result<(u32, u32)> {
    let text = std::fs::read_to_string(file).with_context(|| {
        format!("reading {file} (a rootless user namespace needs a subordinate id range)")
    })?;
    parse_subid(&text, user).with_context(|| format!("in {file}"))
}

/// Parse one `name:start:count` line. Split from the IO so the parsing is testable
/// on its own: a silent misread here produces a wrong id map, and the map is
/// write-once — there is no widening it afterwards.
fn parse_subid(text: &str, user: &str) -> Result<(u32, u32)> {
    for line in text.lines() {
        let line = line.trim();
        let mut fields = line.splitn(3, ':');
        let (Some(who), Some(start), Some(count)) = (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        if who != user {
            continue;
        }
        let start: u32 = start
            .trim()
            .parse()
            .with_context(|| format!("bad start in {line:?}"))?;
        let count: u32 = count
            .trim()
            .parse()
            .with_context(|| format!("bad count in {line:?}"))?;
        if count == 0 {
            bail!("empty subordinate id range for {user:?}");
        }
        return Ok((start, count));
    }
    bail!("no entry for {user:?} — a rootless user namespace needs a subordinate id range")
}

/// Run a setuid id-mapping helper. Looked up on `PATH` rather than pinned by Nix:
/// only the setuid wrapper works, and the store copy has no setuid bit.
fn helper(bin: &str, args: &[String]) -> Result<()> {
    let status = Command::new(bin)
        .args(args)
        .status()
        .with_context(|| format!("spawning {bin} — is the setuid wrapper on PATH?"))?;
    if !status.success() {
        bail!("{bin} failed ({status}) mapping {args:?}");
    }
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
    use super::parse_subid;

    #[test]
    fn parses_the_matching_user_line() {
        let text = "alice:100000:65536\nbob:165536:65536\n";
        assert_eq!(parse_subid(text, "bob").unwrap(), (165536, 65536));
    }

    #[test]
    fn ignores_comments_and_short_lines() {
        let text = "# a comment\n\ngarbage\nalice:100000:65536\n";
        assert_eq!(parse_subid(text, "alice").unwrap(), (100000, 65536));
    }

    #[test]
    fn a_prefix_of_a_name_is_not_a_match() {
        // "al" must not match the "alice" line: that would map a range the user
        // does not own, into a namespace whose map cannot be rewritten.
        assert!(parse_subid("alice:100000:65536\n", "al").is_err());
    }

    #[test]
    fn missing_user_is_a_loud_error() {
        assert!(parse_subid("alice:100000:65536\n", "carol").is_err());
    }

    #[test]
    fn empty_range_is_rejected() {
        // A zero count maps only the invoker — nothing left to subdivide, and no
        // way to widen it later.
        assert!(parse_subid("alice:100000:0\n", "alice").is_err());
    }

    #[test]
    fn non_numeric_fields_are_rejected() {
        assert!(parse_subid("alice:lots:65536\n", "alice").is_err());
    }
}
