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
//! `SCM_RIGHTS` recvmsg, poll, termios, eventfds, signal masks). Two things stay
//! raw `libc`: the `TIOCGWINSZ`/`TIOCSWINSZ` ioctls, and the eventfd counter
//! bump in the `SIGWINCH` handler (which must be async-signal-safe).

use std::io::IoSliceMut;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

use anyhow::{bail, Context, Result};
use nix::errno::Errno;
use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
use nix::sys::eventfd::{EfdFlags, EventFd};
use nix::sys::signal::{sigaction, SaFlags, SigAction, SigHandler, SigSet, Signal};
use nix::sys::socket::{
    accept, bind, listen, recvmsg, socket, AddressFamily, Backlog, ControlMessageOwned, MsgFlags,
    SockFlag, SockType, UnixAddr,
};
use nix::sys::termios::{self, SetArg};
use nix::unistd::{isatty, read, write};

const STDIN: RawFd = 0;

/// Raw fd of the resize `eventfd`, published for the async-signal `SIGWINCH`
/// handler. `-1` when no relay is active. The handler only ever bumps the
/// counter with a lone `write()` — async-signal-safe. The eventfd it points at
/// is kept alive by [`ConsoleRelay`] (an `Arc`), so this fd stays valid until
/// `finish` resets it to `-1`.
static WAKE_FD: AtomicI32 = AtomicI32::new(-1);

/// A bound console socket plus the thread servicing it. Backend-agnostic: hand
/// [`socket_path`](Self::socket_path) to whichever runtime, run the backend
/// (which blocks until the container exits), then [`finish`](Self::finish) to
/// restore the terminal and join.
pub struct ConsoleRelay {
    socket_path: PathBuf,
    /// Poked (from normal code) to make the relay thread stop.
    quit: Arc<EventFd>,
    /// The resize eventfd — held only to keep it alive for the SIGWINCH handler
    /// (which reaches it via [`WAKE_FD`]); the relay thread holds the other
    /// `Arc` and does the reading.
    _wake: Arc<EventFd>,
    /// The SIGWINCH disposition from before we installed ours, restored on
    /// teardown so the relay leaves the process's signal state as it found it.
    prev_sigwinch: SigAction,
    handle: Option<JoinHandle<()>>,
}

impl ConsoleRelay {
    /// Bind the console socket at `socket_path` and spawn the relay thread. The
    /// thread blocks accepting the runtime's connection; call this *before*
    /// driving the backend so the socket exists when the runtime connects.
    pub fn bind(socket_path: PathBuf) -> Result<Self> {
        let listener = bind_listen(&socket_path)?;

        // Two eventfds are the relay thread's wakeup channels: `wake`, bumped by
        // the SIGWINCH handler (self-pipe trick, but an eventfd — one fd, an
        // 8-byte counter); and `quit`, poked by `finish` so the thread unblocks
        // even while still waiting for a connection that never comes. Both are
        // `Arc`-shared so the handler's `wake` fd outlives the relay thread.
        let flags = EfdFlags::EFD_CLOEXEC | EfdFlags::EFD_NONBLOCK;
        let wake = Arc::new(EventFd::from_flags(flags).context("resize eventfd")?);
        let quit = Arc::new(EventFd::from_flags(flags).context("quit eventfd")?);
        let prev_sigwinch = install_sigwinch(&wake)?;

        let (wake_r, quit_r) = (Arc::clone(&wake), Arc::clone(&quit));
        let handle = std::thread::spawn(move || {
            // Block SIGCHLD in this thread. It's process-directed, so the kernel
            // would otherwise be free to deliver the container's exit signal
            // *here* — where its default disposition discards it — and the
            // runtime's signalfd-based reaper (on the main thread) would wait
            // forever on an already-dead child. Safe to do at thread entry: the
            // container doesn't exist yet, so no SIGCHLD can be pending.
            block_sigchld();
            if let Err(e) = serve(listener, wake_r, quit_r) {
                eprintln!("pierogi: console relay: {e:#}");
            }
        });

        Ok(ConsoleRelay {
            socket_path,
            quit,
            _wake: wake,
            prev_sigwinch,
            handle: Some(handle),
        })
    }

    /// The path to hand the runtime as its console socket.
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// Stop the relay, restore the terminal, and clean up. Consumes `self`.
    pub fn finish(mut self) {
        // Tear down the SIGWINCH wiring first (its target fd is about to go
        // away), then nudge the relay thread to quit and wait for it (it
        // restores termios in its own guard as it unwinds).
        uninstall_sigwinch(&self.prev_sigwinch);
        let _ = self.quit.write(1);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        let _ = std::fs::remove_file(&self.socket_path);
        // The eventfds (Arc<EventFd>) close once both this and the relay's clones
        // drop — the relay's are already gone after the join above.
    }
}

/// Accept the runtime's connection, receive the pty master, and relay until the
/// pty closes (or `finish` asks us to quit). Owns `listener`, which closes as it
/// returns; `wake`/`quit` are the shared eventfds.
fn serve(listener: OwnedFd, wake: Arc<EventFd>, quit: Arc<EventFd>) -> Result<()> {
    let master = match accept_master(&listener, &quit)? {
        Some(fd) => fd,
        None => return Ok(()), // asked to quit before the runtime ever connected
    };

    // Raw mode + initial window size only once we actually own the master.
    let _raw = RawModeGuard::enable();
    copy_winsize(master.as_raw_fd());
    relay(&master, &wake, &quit)
}

/// Wait for either the runtime to connect (→ receive & return the master fd) or
/// a `quit` poke (→ `None`, quit).
fn accept_master(listener: &OwnedFd, quit: &EventFd) -> Result<Option<OwnedFd>> {
    loop {
        let mut pfds = [
            PollFd::new(listener.as_fd(), PollFlags::POLLIN),
            PollFd::new(quit.as_fd(), PollFlags::POLLIN),
        ];
        if !wait(&mut pfds)? {
            continue; // EINTR
        }
        let [listener_pf, quit_pf] = &pfds;
        if has(quit_pf, PollFlags::POLLIN) {
            let _ = quit.read();
            return Ok(None);
        }
        if has(listener_pf, PollFlags::POLLIN) {
            let conn = retry(|| accept(listener.as_raw_fd())).context("accept console socket")?;
            // Own the accepted fd so it closes when we're done receiving.
            let conn = unsafe { OwnedFd::from_raw_fd(conn) };
            return Ok(Some(recv_fd(&conn)?));
        }
    }
}

/// The bidirectional copy loop: stdin → master, master → stdout, until the
/// master (pty) closes or we're told to quit. `wake` fires on `SIGWINCH`
/// (re-propagate window size); `quit` ends the loop.
fn relay(master: &OwnedFd, wake: &EventFd, quit: &EventFd) -> Result<()> {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let stdin_fd = stdin.as_fd();
    let mut buf = [0u8; 4096];
    let mut stdin_open = true;
    loop {
        let mut pfds = vec![
            PollFd::new(master.as_fd(), PollFlags::POLLIN),
            PollFd::new(wake.as_fd(), PollFlags::POLLIN),
            PollFd::new(quit.as_fd(), PollFlags::POLLIN),
        ];
        if stdin_open {
            pfds.push(PollFd::new(stdin_fd, PollFlags::POLLIN));
        }
        if !wait(&mut pfds)? {
            continue; // EINTR
        }

        // Name the poll slots so readiness reads by role, not index. `stdin_pf`
        // is a 0-or-1 slice — empty once our stdin has hit EOF and we stopped
        // polling it.
        let [master_pf, wake_pf, quit_pf, stdin_pf @ ..] = pfds.as_slice() else {
            unreachable!("relay always polls master, wake, and quit");
        };

        // Quit → stop; drain the counter first (tidy, though we're leaving).
        if has(quit_pf, PollFlags::POLLIN) {
            let _ = quit.read();
            break;
        }
        // Resize → drain the counter and re-propagate the window size.
        if has(wake_pf, PollFlags::POLLIN) {
            let _ = wake.read();
            copy_winsize(master.as_raw_fd());
        }

        // Master → stdout. A closed slave surfaces as EIO / POLLHUP; either ends
        // the session.
        if has(master_pf, PollFlags::POLLIN) {
            match retry(|| read(master, &mut buf)) {
                Ok(n) if n > 0 => write_all(&stdout, &buf[..n]),
                _ => break,
            }
        }
        if has(master_pf, PollFlags::POLLHUP | PollFlags::POLLERR) {
            break;
        }

        // Stdin → master. Our own EOF just stops that direction (drops stdin from
        // the poll set next iteration); the container keeps running.
        if let Some(stdin_pf) = stdin_pf.first() {
            if has(stdin_pf, PollFlags::POLLIN) {
                match retry(|| read(stdin_fd, &mut buf)) {
                    Ok(n) if n > 0 => write_all(master, &buf[..n]),
                    _ => stdin_open = false,
                }
            } else if has(stdin_pf, PollFlags::POLLHUP | PollFlags::POLLERR) {
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

/// Publish the eventfd the handler bumps, then install the handler — the two
/// halves of the global SIGWINCH wiring, kept together.
fn install_sigwinch(wake: &EventFd) -> Result<SigAction> {
    WAKE_FD.store(wake.as_raw_fd(), Ordering::SeqCst);
    let action = SigAction::new(
        SigHandler::Handler(on_sigwinch),
        SaFlags::SA_RESTART,
        SigSet::empty(),
    );
    // SAFETY: the handler is async-signal-safe (a lone eventfd counter bump).
    let prev =
        unsafe { sigaction(Signal::SIGWINCH, &action) }.context("installing SIGWINCH handler")?;
    Ok(prev)
}

/// Mirror of [`install_sigwinch`], in reverse order: restore the *saved*
/// disposition first — not a blind `SIG_DFL`, so we don't clobber a handler
/// another part of the process may have installed — then retire `WAKE_FD`.
/// Removing our handler before the fd is unpublished keeps the invariant "our
/// handler installed ⇒ `WAKE_FD` valid" holding through teardown, rather than
/// leaning on the sentinel guard during a transient window.
fn uninstall_sigwinch(prev: &SigAction) {
    // SAFETY: `prev` is the disposition `sigaction` handed back at install time.
    let _ = unsafe { sigaction(Signal::SIGWINCH, prev) };
    WAKE_FD.store(-1, Ordering::SeqCst);
}

extern "C" fn on_sigwinch(_sig: libc::c_int) {
    let fd = WAKE_FD.load(Ordering::SeqCst);
    if fd >= 0 {
        // async-signal-safe: a lone eventfd counter bump (write of a u64).
        let one: u64 = 1;
        unsafe { libc::write(fd, (&one as *const u64).cast(), std::mem::size_of::<u64>()) };
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
