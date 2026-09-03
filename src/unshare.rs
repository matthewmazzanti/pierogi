//! `pierogi unshare` — hold a **user + mount namespace** open in a pause process,
//! and run commands inside it.
//!
//! **The map is `0 → <uid>` then the `/etc/subuid` range**, in that order. The
//! first entry is not cosmetic: it gives anything that later joins this namespace
//! a *mapped* identity, and therefore capabilities that survive `execve`. With
//! nothing mapped, a joiner lands as the kernel's overflow uid — `setns` into a
//! user namespace grants a full capability set, and `execve` takes it straight
//! back unless euid is 0, so entering works and doing anything does not. podman's
//! rootless map has the same two-entry shape, for the same reason.
//!
//! The map is written by the *parent*, because the child cannot map itself — a
//! multi-range map needs `newuidmap`, which is setuid-root and must be invoked
//! from outside against the child's pid. And the parent cannot write it until the
//! child has unshared. Neither side can go first, so the two rendezvous over a
//! pipe pair; see [`start_pause`].
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

use std::ffi::CString;
use std::os::fd::OwnedFd;
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;

use anyhow::{bail, Context, Result};
use nix::errno::Errno;
use nix::fcntl::{open, OFlag};
use nix::mount::{mount, MsFlags};
use nix::sched::{setns, unshare, CloneFlags};
use nix::sys::stat::Mode;
use nix::unistd::mkdir;
use nix::unistd::{
    dup2_stderr, dup2_stdin, dup2_stdout, fork, getgid, getuid, pipe, read, setsid, write,
    ForkResult, Pid, User,
};
use serde::{Deserialize, Serialize};

/// What the pause process holds. `CLONE_NEWUSER` is the authority everything else
/// will rest on; `CLONE_NEWNS` is what will let namespaces be named.
/// A registry tmpfs holds empty mount targets and nothing else.
const MS_NODEV_NOEXEC: MsFlags = MsFlags::MS_NODEV.union(MsFlags::MS_NOEXEC);

const PIN_NAMESPACES: CloneFlags = CloneFlags::CLONE_NEWUSER.union(CloneFlags::CLONE_NEWNS);

/// A live pause process. Both namespace inodes are recorded because a pid on its
/// own is not a stable handle: pids are recycled, and a recycled one would
/// otherwise resolve to an unrelated process's namespaces — a failure that
/// *succeeds*, which is the dangerous kind.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pin {
    pub pid: i32,
    pub user_ns: u64,
    pub mnt_ns: u64,
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

/// Join a pause process's user + mount namespace **in this process**, permanently.
///
/// For a helper that does nothing else this is simpler than a `pre_exec`: call it
/// first thing, and everything after runs inside the pin as a normal program, with
/// allocation and error handling available. Two requirements:
///
/// * **Single-threaded.** `setns(CLONE_NEWUSER)` demands it, so this must precede
///   any thread spawn. Rust spawns none on its own — unlike Go, which needs a C
///   constructor running before its runtime to get the same window.
/// * **One-way.** Once inside U there is no route back: authority over anything
///   owned by `init_user_ns` is gone, permanently, even holding an fd for it. A
///   process that needs the host network namespace must never call this.
///
/// Order matters: entering the user namespace is what grants the `CAP_SYS_ADMIN`
/// that makes the mount-namespace join legal.
pub fn enter_pin(pid: i32) -> Result<()> {
    // Open both before joining anything: once inside U, `/proc/<pin>/...` is no
    // longer openable from a sibling user namespace.
    let user_ns = std::fs::File::open(format!("/proc/{pid}/ns/user"))
        .with_context(|| format!("opening the pause process's user namespace (pid {pid})"))?;
    let mnt_ns = std::fs::File::open(format!("/proc/{pid}/ns/mnt"))
        .with_context(|| format!("opening the pause process's mount namespace (pid {pid})"))?;
    setns(&user_ns, CloneFlags::CLONE_NEWUSER).context("joining the pause user namespace")?;
    setns(&mnt_ns, CloneFlags::CLONE_NEWNS).context("joining the pause mount namespace")?;
    Ok(())
}

/// Return a live pause process, starting one if the record is missing, dead, or
/// has been recycled onto a different process.
pub fn ensure_pin() -> Result<Pin> {
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
    // Create the mountpoint before forking: the child does no allocation, and the
    // directory has to exist on the underlying filesystem either way.
    let registry = registry_path();
    std::fs::create_dir_all(&registry).with_context(|| format!("mkdir {}", registry.display()))?;
    let reg_c = CString::new(registry.as_os_str().as_encoded_bytes())
        .context("registry path contains a NUL")?;
    let net_c = CString::new(registry.join("net").as_os_str().as_encoded_bytes())
        .context("registry path contains a NUL")?;

    let (ready_r, ready_w) = pipe().context("creating pause ready pipe")?;
    let (go_r, go_w) = pipe().context("creating pause release pipe")?;

    // SAFETY: called before any thread is spawned, so the child inherits a
    // single-threaded process — required by unshare(CLONE_NEWUSER). The child
    // touches only syscalls before it blocks.
    match unsafe { fork() }.context("forking the pause process")? {
        ForkResult::Child => {
            drop(ready_r);
            drop(go_w);
            let code = i32::from(pause_child(ready_w, go_r, &reg_c, &net_c).is_err());
            unsafe { libc::_exit(code) }
        }
        ForkResult::Parent { child } => {
            drop(ready_w);
            drop(go_r);
            match finish_pause(child, &ready_r, &go_w) {
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
fn pause_child(
    ready: OwnedFd,
    go: OwnedFd,
    registry: &CString,
    net_dir: &CString,
) -> Result<(), Errno> {
    // Leave the caller's session so the pin outlives the terminal that started it:
    // a pause process killed by Ctrl-C takes everything it holds with it.
    setsid()?;
    unshare(PIN_NAMESPACES)?;
    write(&ready, &[1u8])?;
    drop(ready);

    // Wait to be released. Until the parent has written our id maps we map
    // nothing, and anything that joined would land as the overflow uid with no
    // capabilities. EOF here means the parent failed or died before mapping us —
    // in which case there is nothing worth holding open, so we exit rather than
    // linger as an unusable namespace nobody can reach.
    let mut byte = [0u8; 1];
    loop {
        match read(&go, &mut byte) {
            Ok(1) => break,
            Ok(_) => return Err(Errno::EPIPE),
            Err(Errno::EINTR) => continue,
            Err(e) => return Err(e),
        }
    }
    drop(go);

    // Now that we are mapped, mount the registry. Everything below this point is
    // invisible to the host: it can see the (empty) mountpoint and nothing else.
    mount_registry(registry, net_dir)?;

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
fn finish_pause(child: Pid, ready: &OwnedFd, go: &OwnedFd) -> Result<Pin> {
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
    write_id_maps(child)?;

    let pin = Pin {
        pid: child.as_raw(),
        user_ns: ns_ino(child.as_raw(), "user")?,
        mnt_ns: ns_ino(child.as_raw(), "mnt")?,
    };
    write(go, &[1u8]).context("releasing the pause process")?;
    Ok(pin)
}

/// Write the pause process's id maps through the setuid helpers.
///
/// Two entries, in this order: the invoking user at container id 0, then their
/// whole subordinate range. The first entry is not cosmetic — it is what gives
/// anything that later joins this namespace a *mapped* identity, and therefore
/// capabilities that survive `execve`. A pin mapping only the subordinate range
/// would strand its own callers as the overflow uid, stripped at the first exec.
/// (podman's rootless map has exactly this shape, for exactly this reason.)
///
/// The map is **write-once**: this is the only chance to size it, and it bounds
/// every user namespace later nested inside.
fn write_id_maps(child: Pid) -> Result<()> {
    let uid = getuid();
    let gid = getgid();
    let user = User::from_uid(uid)
        .context("looking up the invoking user")?
        .ok_or_else(|| anyhow::anyhow!("no passwd entry for uid {uid}"))?;

    let (sub_uid, uid_count) = subid_range("/etc/subuid", &user.name)?;
    let (sub_gid, gid_count) = subid_range("/etc/subgid", &user.name)?;

    id_helper(
        "newuidmap",
        child,
        [uid.to_string(), sub_uid.to_string(), uid_count.to_string()],
    )?;
    id_helper(
        "newgidmap",
        child,
        [gid.to_string(), sub_gid.to_string(), gid_count.to_string()],
    )
}

/// Run a setuid id-mapping helper: `newXidmap <pid> 0 <own> 1 1 <start> <count>`.
///
/// Looked up on `PATH` rather than pinned by Nix like `CRUN_BIN` — only the setuid
/// wrapper functions, and the store copy carries no setuid bit. This is the one
/// place the engine resolves a binary by name, and its absence is a loud error.
fn id_helper(bin: &str, child: Pid, [own, start, count]: [String; 3]) -> Result<()> {
    let args = [
        child.to_string(),
        "0".into(),
        own,
        "1".into(),
        "1".into(),
        start,
        count,
    ];
    let status = Command::new(bin)
        .args(&args)
        .status()
        .with_context(|| format!("spawning {bin} — is the setuid wrapper on PATH?"))?;
    if !status.success() {
        bail!("{bin} failed ({status}) mapping {args:?}");
    }
    Ok(())
}

/// Read the subordinate id range for `user` from `/etc/subuid` or `/etc/subgid`.
fn subid_range(file: &str, user: &str) -> Result<(u32, u32)> {
    let text = std::fs::read_to_string(file).with_context(|| {
        format!("reading {file} (a rootless user namespace needs a subordinate id range)")
    })?;
    parse_subid(&text, user).with_context(|| format!("in {file}"))
}

/// Parse one `name:start:count` line. Split from the IO so the parsing is testable
/// on its own: a misread here produces a wrong map, and the map is write-once.
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

/// Mount the private tmpfs the network registry lives on, and create `net/` in it.
///
/// The alternative — pinning network namespaces straight onto the host's runtime
/// directory, which is what podman does — leaves the *mount targets* visible to
/// the host as empty regular files, indistinguishable at a glance from live pins
/// and outliving the process that made them. A private filesystem gives the
/// registry exactly one lifetime: kill the pause process and every name it held
/// is gone, with no residue in a directory other tools also write to.
///
/// It does **not** remove the need to check that a name is really an `nsfs` inode
/// rather than a leftover empty file — a teardown that unmounts and then dies
/// before unlinking still leaves one, just inside the pin rather than forever.
fn mount_registry(registry: &CString, net_dir: &CString) -> Result<(), Errno> {
    mount(
        Some(c"tmpfs"),
        registry.as_c_str(),
        Some(c"tmpfs"),
        MsFlags::MS_NOSUID | MS_NODEV_NOEXEC,
        // Small on purpose: it holds nothing but empty mount targets.
        Some(c"size=1m,mode=0700"),
    )?;
    mkdir(net_dir.as_c_str(), Mode::S_IRWXU)?;
    Ok(())
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

/// Where pinned network namespaces are named, inside the registry tmpfs.
pub fn net_dir() -> PathBuf {
    registry_path().join("net")
}

/// The registry mountpoint. This directory is created on the *host* filesystem —
/// a bind mount needs a target, and the target has to exist before anything can
/// be mounted over it. The host therefore sees one empty directory, permanently,
/// and never sees a single network name: those live on the tmpfs mounted over it,
/// which exists only inside the pause process's mount namespace.
pub fn registry_path() -> PathBuf {
    state_dir().join("registry")
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

#[cfg(test)]
mod subid_tests {
    use super::parse_subid;

    #[test]
    fn parses_the_matching_user_line() {
        let text = "alice:100000:65536\nbob:165536:65536\n";
        assert_eq!(parse_subid(text, "bob").unwrap(), (165536, 65536));
    }

    #[test]
    fn ignores_comments_and_short_lines() {
        assert_eq!(
            parse_subid("# a comment\n\ngarbage\nalice:100000:65536\n", "alice").unwrap(),
            (100000, 65536)
        );
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
        // A zero count maps only the invoker — nothing to subdivide, and the map
        // is write-once, so there is no widening it later.
        assert!(parse_subid("alice:100000:0\n", "alice").is_err());
    }

    #[test]
    fn non_numeric_fields_are_rejected() {
        assert!(parse_subid("alice:lots:65536\n", "alice").is_err());
    }
}
