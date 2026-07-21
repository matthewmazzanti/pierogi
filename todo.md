# TODO

Loose ends and cleanups, not blocking. Context captured so the reasoning isn't lost.

## tty relay polish (`src/tty.rs`)

- [ ] **Clean up remaining error ignores (`let _ = …`).** A few reads/writes still
  discard their result. Audit each: some are *deliberate* (idempotent /
  fire-and-forget), some could surface a real bug like the eventfd drains now do
  (`.context(…)?`). Known sites: `finish()` (`quit.write`, `h.join`,
  `remove_file`), `RawModeGuard::drop` (`tcsetattr` restore), `on_sigwinch`
  (`libc::write` — must stay ignored, async-signal-safe), `accept_master`
  (`quit.read()` drain), `run.rs` (`remove_dir_all` cleanup). Decide per-site:
  keep-with-comment vs. handle.

- [ ] **Clear further `unsafe`, if possible.** Remaining blocks after the recent
  pass: two `OwnedFd::from_raw_fd` (adopting the accepted conn + the SCM_RIGHTS
  master — likely irreducible, `nix` hands back a `RawFd`); the
  `TIOCGWINSZ`/`TIOCSWINSZ` ioctls (libc holdout — `nix` has no wrapper, could
  use its `ioctl!` macros); `sigaction` install/uninstall; the eventfd counter
  bump in the handler (async-signal-safe, must stay raw). The FFI in
  `csrc/shim.c` / `runtime/crun.rs` is inherently unsafe (crun). Goal: shrink the
  count where it's a real reduction, not just relocation.

- [ ] **Find a better pattern for `retry()`.** The manual EINTR-retry loop wraps
  the blocking syscalls. Note: with `SA_RESTART` on SIGWINCH and SIGCHLD blocked
  on the relay thread, the *reads* barely EINTR at all — it's `poll` that needs
  it (handled in `wait()`). Considered + rejected: wrapping fds in
  `std::io::Read` (buffering breaks poll semantics; fds are borrowed
  process-globals). Look for a cleaner combinator, or decide `retry()` is fine
  and just document why.
