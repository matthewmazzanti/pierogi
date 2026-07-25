# TODO

Loose ends and cleanups, not blocking. Context captured so the reasoning isn't lost.

## sealed-rootfs branch (`root` is the rootfs)

Direction explored and working on **crun** (the default). `root` is now the FHS
rootfs mounted at `/` (baked `/etc`, `/bin`, and all mountpoints); no launch-time
scaffold; `--mount store:` dropped; closure frozen at build time. See
`docs/spec.md#sealed-rootfs`.

- [x] **Dropped youki; crun is the only backend.** youki couldn't RO-remount a
  `/nix/store` rootfs (bare `MS_REMOUNT|MS_RDONLY` vs the store's locked
  `nosuid`/`nodev`); crun re-supplies the flags. Removed `youki.rs`, the `Backend`
  enum + `--runtime` flag, the `crun` feature gate (crun always compiled), and the
  `libcontainer`/`tracing-subscriber`/`libseccomp` deps. The spec now reaches crun
  purely as an in-memory JSON string — no `config.json`, and the on-disk *bundle*
  is gone too (crun's bundle context points at the work dir; only its state dir
  touches disk). `nix build .#pierogi` + `.#bash-sandbox` verified.
- [ ] **README + `docs/*.md` still describe youki as *the* runtime.** README's
  "M0 spike" front-matter ("embed the youki `libcontainer` runtime", "drives youki
  in-process", the youki-specific findings) and mentions in `docs/architecture.md`
  / `roadmap.md` / `startup-performance.md` / `vision.md` predate the crun-only,
  sealed-rootfs world. Fold into the doc sweep below.
- [x] **Removed `Mount::Store` + the live query.** Dropped the `Store` variant
  (a bare store mount was a closure seed in disguise; relocation → a `Host`
  bind), and the whole `nix-store --query` path (`closure_of`, `PIEROGI_CLOSURE`,
  `closure.rs`). `closure` split into `{ seeds, resolved }`; `run` requires a
  pre-baked spec (fails loud otherwise); `resolve --closure` is the sole
  closure-filling entry.
- [ ] **`--executable` override flag on `run`.** The trailing `-- args` override
  now rewrites `process.args` only — the executable is the spec's, sealed by the
  image (a fixed entrypoint; `bash-sandbox`'s wrapper leans on this to pass argv
  straight to bash). That deliberately removed the escape hatch for a one-off
  different entrypoint. Add an explicit `--executable NAME` (opt-in, orthogonal to
  the args override) for the ad-hoc case — e.g. run `env`/`coreutils` directly
  instead of via `bash -c`. Still resolved *within* the sealed closure at launch
  (in-sandbox `PATH` lookup), so it changes what runs, never the store view. See
  `src/overrides.rs` (`apply`) and `docs/execution-modes.md#run-overrides`.
- [ ] **`Overlay` under the frozen-topology model.** Still deferred (`oci` bails).
  Its lower must be in `closure` and its `dest` baked; it's the last `Mount`
  variant that names a store `source` (so the only kind a producer adds to
  `closure.seeds` beyond `root`). Revisit realization when it lands.
- [x] **Traverse the reference graph from `seeds` in-process.** `resolve --graph`
  takes Nix's `exportReferencesGraph` as JSON and walks requisites from `seeds`
  itself (`resolve::resolve_from_graph`): `resolved = closure(seeds)`, root
  included. Catches a tampered/mismatched graph, and makes `seeds` a load-bearing
  input. The graph is the top-level `closure` array of the `__structuredAttrs`
  JSON (`$NIX_ATTRS_JSON_FILE`) — same shape `closureInfo` consumes, one
  `{ path, references }` per node; pierogi serde-parses it and ignores the rest
  of the attrs. (Note: `.exportReferencesGraph.closure` in that file only echoes
  the roots; the *expanded* graph is the sibling top-level `.closure` key.)
  - [x] **Follow-on: relocated-seed drop — CLOSED (moot).** A `Store`-era idea
    (`from ⊋ paths`): drop a seed nothing references by store path from the bind
    set. The sealed model dissolved it — `Store` is gone, so relocation is a
    plain `Host` bind that contributes no seed and isn't in the closure at all
    (never over-binds); overlay lowers are needed at their identity path; and
    `root` is the only seed that *could* be unreferenced, but we deliberately
    identity-bind it (`/nix/store/<self>`) for robustness. No current mount
    produces a droppable seed — at most one bind (root's self-bind) could go, and
    only by undoing that decision. Not worth doing.
  - [ ] **Follow-on: bake mountpoints from the same graph.** `bash-sandbox.nix`
    still uses `closureInfo { rootPaths = deps }` for the flat mkdir list (a
    separate build, pre-`root`). Could derive it from the deps' `references` JSON
    instead, dropping `closureInfo` — kept for now as it's a clean multi-root
    flat-list helper.
- [ ] **Reconcile the rest of `docs/spec.md` + `execution-modes.md`.** The schema
  block, `closure` field docs, realization, and the Sealed-rootfs section are
  updated. **Still stale (describe the old dynamic-closure/fold world):** the
  "Mount model" section (fold-vs-no-fold table, `Store` prose), "Resolution
  invariant", the "Nix-mode closure source" section (the `closureInfo`
  three-file table + `resolve --closure`/`--registration` — now
  `exportReferencesGraph` JSON traversed by `resolve --graph`), and all five
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

## examples / device passthrough

- [ ] **GPU (`/dev/dri`) passthrough for graphical Wayland clients.** The
  `wayland-sandbox` example (`nix/wayland-sandbox.nix`) routes the compositor
  socket in and `wayland-info` enumerates globals fine, but logs
  `drmGetDeviceFromDevId failed` because no render node is bound — `wl_shm`
  (software) works, `linux-dmabuf`/EGL won't. A graphical follow-on (foot,
  weston-flower, or anything EGL) needs `/dev/dri/renderD128` (+ maybe `card0`)
  bound in. Device nodes can't go through `mounts[]` (bind of a char device →
  EINVAL, see `main.rs`); the runtime populates `/dev` from a tmpfs, so this
  wants either an OCI `linux.devices` entry (allow + mknod the render node) or a
  `/dev/dri` bind handled the way `/dev` is. Design when the graphical example
  lands. Also revisit whether SHM needs `/dev/shm` (libwayland prefers
  `memfd_create`, so it didn't here — note it if a client falls back).

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
