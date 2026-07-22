# TODO

Loose ends and cleanups, not blocking. Context captured so the reasoning isn't lost.

## sealed-rootfs branch (`root` is the rootfs)

Direction explored and working on **crun** (the default). `root` is now the FHS
rootfs mounted at `/` (baked `/etc`, `/bin`, and all mountpoints); no launch-time
scaffold; `--mount store:` dropped; closure frozen at build time. See
`docs/spec.md#sealed-rootfs`.

- [ ] **youki: RO-root remount EPERMs on the store-path rootfs.** With the rootfs
  now a `/nix/store` path, youki's bare `MS_REMOUNT|MS_RDONLY|MS_BIND` hits the
  store's locked `nosuid`/`nodev` flags (the exact M0 finding, previously dodged
  by putting the bundle on `/tmp`). crun handles it (re-supplies flags). Options:
  re-supply the locked flags on youki's remount (upstream-shaped), bind the store
  path into a `/tmp` bundle first (re-adds one bind), or accept crun-only for
  sealed rootfs. **crun is the default, so not blocking.**
- [ ] **Decide `Mount::Store`'s fate.** The type + its `resolve`/`oci` handling
  still exist (spec-level relocations), but it's no longer CLI-constructable and
  the bash-sandbox doesn't use it. Under a sealed rootfs a relocated store mount
  still needs its `dest` baked. Keep as a build-time-only spec construct, or
  remove? Also revisit `Overlay` (still deferred) under the frozen-topology model.
- [ ] **Reconcile the rest of `docs/spec.md`.** The realization + root-as-mount
  sections and the field note are updated; the `Mount` model, closure-fold, and
  seal-break narrative (`execution-modes.md` too) still describe the old
  dynamic-closure world. Sweep once the direction is committed.
- [ ] **`/tmp` as policy?** Currently a spec `tmpfs` mount onto a baked `/tmp`
  dir. Consider whether writable `/tmp` (and `/dev`, `/proc`) should be
  engine-essential base mounts vs. spec-declared. Leaning spec-declared (no
  policy), but most sealed images want `/tmp`.

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
