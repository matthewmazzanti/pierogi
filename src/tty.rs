//! Host-side terminal relay for `exec.terminal = true` sandboxes.
//!
//! We drive the OCI **console-socket** protocol ourselves so one implementation
//! serves both backends — crun and youki each take a console-socket *path*,
//! nothing more. The flow:
//!
//!   1. bind an `AF_UNIX` socket and hand its path to the runtime;
//!   2. the runtime, *inside* the container (where it opens the pty and makes
//!      the slave the process's controlling tty), connects back and sends us the
//!      pty **master** fd over `SCM_RIGHTS`;
//!   3. we put our own stdin in raw mode and pump bytes between our stdio and
//!      the master until the container's pty closes, propagating `SIGWINCH`;
//!   4. on exit we restore the terminal.
//!
//! Only the master-side UX (raw mode, window size, the copy loop) lives here —
//! the controlling-tty setup is the runtime's job, delegated by `terminal:true`.
//!
//! The syscall surface goes through `nix`'s checked wrappers (sockets, the
//! `SCM_RIGHTS` recvmsg, poll, termios, signal masks). Two things stay raw
//! `libc`: the `TIOCGWINSZ`/`TIOCSWINSZ` ioctls, and the one-byte `write` in the
//! `SIGWINCH` handler (which must be async-signal-safe).

use std::io::IoSliceMut;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI32, Ordering};
use std::thread::JoinHandle;

use anyhow::{bail, Context, Result};
use nix::errno::Errno;
use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
use nix::sys::signal::{sigaction, SaFlags, SigAction, SigHandler, SigSet, Signal};
use nix::sys::socket::{
    accept, bind, listen, recvmsg, socket, AddressFamily, Backlog, ControlMessageOwned, MsgFlags,
    SockFlag, SockType, UnixAddr,
};
use nix::sys::termios::{self, SetArg};
use nix::unistd::{isatty, pipe, read, write};

const STDIN: RawFd = 0;

/// Write-end of the control pipe, published for the async-signal `SIGWINCH`
/// handler. `-1` when no relay is active. The handler only ever `write()`s a
/// single byte to it — async-signal-safe.
static CTL_WRITE: AtomicI32 = AtomicI32::new(-1);

/// A bound console socket plus the thread servicing it. Backend-agnostic: hand
/// [`socket_path`](Self::socket_path) to whichever runtime, run the backend
/// (which blocks until the container exits), then [`finish`](Self::finish) to
/// restore the terminal and join.
pub struct ConsoleRelay {
    socket_path: PathBuf,
    ctl_write: OwnedFd,
    handle: Option<JoinHandle<()>>,
}

impl ConsoleRelay {
    /// Bind the console socket at `socket_path` and spawn the relay thread. The
    /// thread blocks accepting the runtime's connection; call this *before*
    /// driving the backend so the socket exists when the runtime connects.
    pub fn bind(socket_path: PathBuf) -> Result<Self> {
        let listener = bind_listen(&socket_path)?;

        // Control pipe: the relay thread's wakeup channel. The SIGWINCH handler
        // writes `w` (window changed); `finish` writes `q` (quit) so the thread
        // unblocks even if it's still waiting for a connection that never comes.
        let (ctl_read, ctl_write) = pipe().context("console control pipe")?;
        CTL_WRITE.store(ctl_write.as_raw_fd(), Ordering::SeqCst);
        install_sigwinch()?;

        let handle = std::thread::spawn(move || {
            // Block SIGCHLD in this thread. It's process-directed, so the kernel
            // would otherwise be free to deliver the container's exit signal
            // *here* — where its default disposition discards it — and the
            // runtime's signalfd-based reaper (on the main thread) would wait
            // forever on an already-dead child. Safe to do at thread entry: the
            // container doesn't exist yet, so no SIGCHLD can be pending.
            block_sigchld();
            if let Err(e) = serve(listener, ctl_read) {
                eprintln!("pierogi: console relay: {e:#}");
            }
        });

        Ok(ConsoleRelay {
            socket_path,
            ctl_write,
            handle: Some(handle),
        })
    }

    /// The path to hand the runtime as its console socket.
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// Stop the relay, restore the terminal, and clean up. Consumes `self`.
    pub fn finish(mut self) {
        // Nudge the relay thread to quit, then wait for it (it restores termios
        // in its own guard as it unwinds).
        let _ = write(&self.ctl_write, b"q");
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        CTL_WRITE.store(-1, Ordering::SeqCst);
        let _ = std::fs::remove_file(&self.socket_path);
        // `ctl_write` (OwnedFd) closes on drop.
    }
}

/// Accept the runtime's connection, receive the pty master, and relay until the
/// pty closes (or `finish` asks us to quit). Owns `listener`/`ctl_read`, which
/// close as it returns.
fn serve(listener: OwnedFd, ctl_read: OwnedFd) -> Result<()> {
    let master = match accept_master(&listener, &ctl_read)? {
        Some(fd) => fd,
        None => return Ok(()), // asked to quit before the runtime ever connected
    };

    // Raw mode + initial window size only once we actually own the master.
    let _raw = RawModeGuard::enable();
    copy_winsize(master.as_raw_fd());
    relay(&master, &ctl_read)
}

/// Wait for either the runtime to connect (→ receive & return the master fd) or
/// a `q` on the control pipe (→ `None`, quit).
fn accept_master(listener: &OwnedFd, ctl_read: &OwnedFd) -> Result<Option<OwnedFd>> {
    loop {
        let mut pfds = [
            PollFd::new(listener.as_fd(), PollFlags::POLLIN),
            PollFd::new(ctl_read.as_fd(), PollFlags::POLLIN),
        ];
        if !wait(&mut pfds)? {
            continue; // EINTR
        }
        if has(&pfds[1], PollFlags::POLLIN) && drain_ctl_is_quit(ctl_read) {
            return Ok(None);
        }
        if has(&pfds[0], PollFlags::POLLIN) {
            let conn = retry(|| accept(listener.as_raw_fd())).context("accept console socket")?;
            // Own the accepted fd so it closes when we're done receiving.
            let conn = unsafe { OwnedFd::from_raw_fd(conn) };
            return Ok(Some(recv_fd(&conn)?));
        }
    }
}

/// The bidirectional copy loop: stdin → master, master → stdout, until the
/// master (pty) closes or we're told to quit.
fn relay(master: &OwnedFd, ctl_read: &OwnedFd) -> Result<()> {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let stdin_fd = stdin.as_fd();
    let mut buf = [0u8; 4096];
    let mut stdin_open = true;
    loop {
        let mut pfds = vec![
            PollFd::new(master.as_fd(), PollFlags::POLLIN),
            PollFd::new(ctl_read.as_fd(), PollFlags::POLLIN),
        ];
        if stdin_open {
            pfds.push(PollFd::new(stdin_fd, PollFlags::POLLIN));
        }
        if !wait(&mut pfds)? {
            continue; // EINTR
        }

        // Control: `q` → quit, `w` → re-propagate window size.
        if has(&pfds[1], PollFlags::POLLIN) {
            let mut c = [0u8; 64];
            if let Ok(n) = retry(|| read(ctl_read, &mut c)) {
                if c[..n].contains(&b'q') {
                    break;
                }
                if c[..n].contains(&b'w') {
                    copy_winsize(master.as_raw_fd());
                }
            }
        }

        // Master → stdout. A closed slave surfaces as EIO / POLLHUP; either ends
        // the session.
        if has(&pfds[0], PollFlags::POLLIN) {
            match retry(|| read(master, &mut buf)) {
                Ok(n) if n > 0 => write_all(&stdout, &buf[..n]),
                _ => break,
            }
        }
        if has(&pfds[0], PollFlags::POLLHUP | PollFlags::POLLERR) {
            break;
        }

        // Stdin → master. Our own EOF just stops that direction; the container
        // keeps running until its pty closes.
        if stdin_open {
            if has(&pfds[2], PollFlags::POLLIN) {
                match retry(|| read(stdin_fd, &mut buf)) {
                    Ok(n) if n > 0 => write_all(master, &buf[..n]),
                    _ => stdin_open = false,
                }
            } else if has(&pfds[2], PollFlags::POLLHUP | PollFlags::POLLERR) {
                stdin_open = false;
            }
        }
    }
    Ok(())
}

/// `poll` with an infinite timeout; `Ok(false)` on `EINTR` (caller loops),
/// `Ok(true)` when fds are ready.
fn wait(pfds: &mut [PollFd]) -> Result<bool> {
    match poll(pfds, PollTimeout::NONE) {
        Ok(_) => Ok(true),
        Err(Errno::EINTR) => Ok(false),
        Err(e) => Err(e).context("poll"),
    }
}

/// Whether any of `flags` is set in a pollfd's returned events.
fn has(pfd: &PollFd, flags: PollFlags) -> bool {
    pfd.revents()
        .unwrap_or(PollFlags::empty())
        .intersects(flags)
}

/// Read and discard the control pipe; `true` if a `q` byte was present.
fn drain_ctl_is_quit(ctl_read: &OwnedFd) -> bool {
    let mut c = [0u8; 64];
    matches!(retry(|| read(ctl_read, &mut c)), Ok(n) if c[..n].contains(&b'q'))
}

/// Receive a single fd sent over `conn` via `SCM_RIGHTS`. `nix` parses the
/// control message; we take ownership of the first fd.
fn recv_fd(conn: &OwnedFd) -> Result<OwnedFd> {
    // The runtime sends a short data payload (the container id) alongside the
    // fd; read it into a throwaway buffer, care only about the control message.
    let mut data = [0u8; 64];
    let mut iov = [IoSliceMut::new(&mut data)];
    let mut cmsgspace = nix::cmsg_space!(RawFd);
    let msg = recvmsg::<()>(
        conn.as_raw_fd(),
        &mut iov,
        Some(&mut cmsgspace),
        MsgFlags::empty(),
    )
    .context("recvmsg console fd")?;

    for cmsg in msg.cmsgs().context("parsing console control message")? {
        if let ControlMessageOwned::ScmRights(fds) = cmsg {
            if let Some(&fd) = fds.first() {
                return Ok(unsafe { OwnedFd::from_raw_fd(fd) });
            }
        }
    }
    bail!("console socket message carried no fd (expected SCM_RIGHTS)")
}

/// Bind an `AF_UNIX` stream socket at `path` and listen. Removes a stale socket
/// file first.
fn bind_listen(path: &Path) -> Result<OwnedFd> {
    let _ = std::fs::remove_file(path);
    let sock = socket(
        AddressFamily::Unix,
        SockType::Stream,
        SockFlag::empty(),
        None,
    )
    .context("socket(AF_UNIX)")?;
    let addr = UnixAddr::new(path).with_context(|| format!("console addr {}", path.display()))?;
    bind(sock.as_raw_fd(), &addr)
        .with_context(|| format!("bind console socket {}", path.display()))?;
    listen(&sock, Backlog::new(1).unwrap_or(Backlog::MAXCONN)).context("listen console socket")?;
    Ok(sock)
}

/// Write the whole buffer, retrying short writes and `EINTR`. Best-effort: a
/// hard error just ends the write (the session is tearing down anyway).
fn write_all<Fd: AsFd>(fd: Fd, mut buf: &[u8]) {
    while !buf.is_empty() {
        match retry(|| write(fd.as_fd(), buf)) {
            Ok(n) if n > 0 => buf = &buf[n..],
            _ => break,
        }
    }
}

/// Retry a `nix` syscall thunk while it fails with `EINTR`.
fn retry<T>(mut f: impl FnMut() -> nix::Result<T>) -> nix::Result<T> {
    loop {
        match f() {
            Err(Errno::EINTR) => continue,
            other => return other,
        }
    }
}

/// Copy our terminal's window size onto the pty master. Best-effort — skipped
/// silently when our stdio isn't a tty. Raw `libc`: `nix` has no safe wrapper
/// for these ioctls.
fn copy_winsize(master: RawFd) {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    if unsafe { libc::ioctl(STDIN, libc::TIOCGWINSZ, &mut ws) } == 0 {
        unsafe { libc::ioctl(master, libc::TIOCSWINSZ, &ws) };
    }
}

fn install_sigwinch() -> Result<()> {
    let action = SigAction::new(
        SigHandler::Handler(on_sigwinch),
        SaFlags::SA_RESTART,
        SigSet::empty(),
    );
    // SAFETY: the handler is async-signal-safe (a lone one-byte write).
    unsafe { sigaction(Signal::SIGWINCH, &action) }.context("installing SIGWINCH handler")?;
    Ok(())
}

extern "C" fn on_sigwinch(_sig: libc::c_int) {
    let fd = CTL_WRITE.load(Ordering::SeqCst);
    if fd >= 0 {
        // async-signal-safe: a lone raw write of one byte.
        unsafe { libc::write(fd, b"w".as_ptr().cast(), 1) };
    }
}

/// Block `SIGCHLD` on the calling thread — see the call site: keeps the relay
/// thread from stealing the container's exit signal from the runtime's reaper.
fn block_sigchld() {
    let mut set = SigSet::empty();
    set.add(Signal::SIGCHLD);
    let _ = set.thread_block();
}

/// Puts `STDIN` in raw mode for the relay's lifetime, restoring it on drop. A
/// no-op when stdin isn't a tty (piped input still relays, just unmodified).
struct RawModeGuard {
    saved: Option<termios::Termios>,
}

impl RawModeGuard {
    fn enable() -> Self {
        let stdin = std::io::stdin();
        if !isatty(&stdin).unwrap_or(false) {
            return RawModeGuard { saved: None };
        }
        let saved = match termios::tcgetattr(&stdin) {
            Ok(t) => t,
            Err(_) => return RawModeGuard { saved: None },
        };
        let mut raw = saved.clone();
        termios::cfmakeraw(&mut raw);
        let _ = termios::tcsetattr(&stdin, SetArg::TCSANOW, &raw);
        RawModeGuard { saved: Some(saved) }
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        if let Some(saved) = &self.saved {
            let _ = termios::tcsetattr(std::io::stdin(), SetArg::TCSANOW, saved);
        }
    }
}
