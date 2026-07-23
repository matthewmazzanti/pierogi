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
- [x] **Removed `Mount::Store` + the live query.** Dropped the `Store` variant
  (a bare store mount was a closure seed in disguise; relocation → a `Host`
  bind), and the whole `nix-store --query` path (`closure_of`, `PIEROGI_CLOSURE`,
  `closure.rs`). `closure` split into `{ seeds, resolved }`; `run` requires a
  pre-baked spec (fails loud otherwise); `resolve --closure` is the sole
  closure-filling entry.
- [ ] **`Overlay` under the frozen-topology model.** Still deferred (`oci` bails).
  Its lower must be in `closure` and its `dest` baked; it's the last `Mount`
  variant that names a store `source` (so the only `store_refs` contributor).
  Revisit realization when it lands.
- [ ] **Derive/validate `resolved` by traversing the reference graph from
  `seeds`.** `resolve --closure` today trusts `closureInfo`'s flat `store-paths`
  list wholesale. Instead, take the reference graph (`closureInfo`'s
  `registration`, or `exportReferencesGraph`) and walk requisites from `seeds`
  ourselves: `resolved = closure(seeds)` (root included — it's in `seeds`). Buys:
  (a) a real check that the supplied set *is* exactly the closure of the declared
  seeds — catches a tampered/mismatched `--closure`; (b) the graph needed for the
  relocated-seed drop (a seed nothing references need not be identity-bound); (c)
  makes `seeds` a load-bearing input, not just a stamp. Wants `seeds` to be the
  **complete** traversal-root set, i.e. to include `root` (see the root-in-`seeds`
  note below) — which it does.
- [ ] **Reconcile the rest of `docs/spec.md` + `execution-modes.md`.** The schema
  block, `closure` field docs, realization, and the Sealed-rootfs section are
  updated. **Still stale (describe the old dynamic-closure/fold world):** the
  "Mount model" section (fold-vs-no-fold table, `Store` prose), "Resolution
  invariant", the `closureInfo` table's `closure.paths` references, and all five
  "Worked examples" (relocation/`from ⊋ paths`, seal-break). Needs a full sweep.
- [x] **`/tmp` stays spec-declared.** DECIDED: writable `/tmp` is a spec `tmpfs`
  mount onto a baked `/tmp` dir, not an engine-essential base mount — the engine
  carries no policy. (`/proc`, `/dev` remain base mounts as runtime surface.)
- [x] **`root` repeated in `seeds` — kept (framing 1).** `seeds` = the complete
  graph-traversal root set (`root` + overlay lowers), so `root` also lives in
  `spec.root` — an intentional denormalization that makes `closure` self-
  describing (`resolved = closure(seeds)`, root included, nothing excluded). The
  alternative (seeds = extras only) would be empty for every spec today and split
  provenance across two fields. The `resolved`-excludes-root wrinkle is gone now
  that root is identity-bound too.
- [x] **Dropped the sealed-spec seed-drift check.** With `--mount store:` gone,
  nothing at runtime can change a sealed spec's seeds, so `resolve()` now uses a
  present `closure` verbatim — no `seeds()` recompute, no drift bail/warn. The
  only divergence path left is a hand-edit-without-re-resolve ("don't hold it
  wrong"). `closure.from` kept as out-of-band provenance.

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
