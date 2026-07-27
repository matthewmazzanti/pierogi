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
  `libcontainer`/`tracing-subscriber`/`libseccomp` deps. The spec is built in
  memory, then serialized to a `config.json` in the per-run bundle dir for the
  `crun` binary (crun reads its config off disk; only that config and crun's
  state dir touch disk). `nix build .#pierogi` + `.#bash-sandbox` verified.
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
  bump in the handler (async-signal-safe, must stay raw). The old crun FFI shim
  (`csrc/shim.c`) was the largest unsafe surface; it's gone now that crun is
  driven as a subprocess (`crun.rs` shells out — no C boundary). Goal:
  shrink the remaining count where it's a real reduction, not just relocation.

- [ ] **Find a better pattern for `retry()`.** The manual EINTR-retry loop wraps
  the blocking syscalls. Note: with `SA_RESTART` on SIGWINCH and SIGCHLD blocked
  on the relay thread, the *reads* barely EINTR at all — it's `poll` that needs
  it (handled in `wait()`). Considered + rejected: wrapping fds in
  `std::io::Read` (buffering breaks poll semantics; fds are borrowed
  process-globals). Look for a cleaner combinator, or decide `retry()` is fine
  and just document why.

## dbus filter proxy + runtime identity (M2 / M4)

Context, so it isn't lost: M2 landed the `xdg-dbus-proxy` supervisor
(`src/dbus.rs`) and a working `nix/probe/dbus.nix`. Making the probe pass
surfaced that **D-Bus SASL `EXTERNAL` couples the container app-uid to the host
uid**: the client sends its own `getuid()` in-band (`AUTH EXTERNAL <hex-uid>`,
confirmed on the wire) *and* the server independently reads `SO_PEERCRED`
(kernel-translated into the server's namespace), then requires the two to match.
So uid **normalization** (a goal: the container always sees `1000` regardless of
the host uid) fights it — the client claims `1000` while the host-side proxy
reads the real host uid, and the handshake is rejected. The proxy dissolves the
*upstream* hop (it re-originates the bus connection as the real host uid) but
re-verifies the *client* against peercred, so routing alone doesn't clear it. The
probe sidesteps this with an identity map — which passes **only because the test
host happens to be uid 1000** (`container 1000 → host 1000` is an identity, not a
real normalization); on a host where the user is e.g. 1001 the probe's approach
would fail the client→proxy auth. See the `dbus.nix` header for the full writeup.

- [ ] **Fork `xdg-dbus-proxy` to launder the client uid.** Keep the client-side
  uid **validation** (the sandbox must present the *expected container* uid — no
  spoofing an arbitrary id), but then **rewrite** the claimed uid to the matching
  *host* uid before the upstream auth. Concretely it's one SASL line: the client
  sends `AUTH EXTERNAL <hex container-uid>`; the fork validates it (against the
  expected container id and peercred), then rewrites to `<hex host-uid>` (or an
  empty `AUTH EXTERNAL`, which makes the bus fall back to peercred) on its
  upstream connection. This **preserves normalization** (container keeps seeing
  1000) while satisfying the bus's `EXTERNAL` check — the proxy is already the
  identity seam (it re-auths upstream as the host user), so laundering the
  client's claimed id belongs there. Do it in a fork rather than a front-shim to
  keep it one component. The rewrite needs a **container↔host uid/gid map**. Like
  the OCI `{uid,gid}_mappings`, but — following the theme (engine holds no policy,
  producer curates) — **independent of the engine's mappings** (not auto-derived
  from them) and **pinned via the Nix layer** (producer data: a field on the
  `dbus` section, or a sibling, authored in `profiles.*`). That keeps the D-Bus
  uid translation an explicit, auditable producer choice, not an implicit
  coupling to the sandbox's id map. (Alternatives considered and deferred: run the
  proxy inside the sandbox's userns — only works if its upstream auth is empty-
  `EXTERNAL`, and re-plumbs supervision; or drop normalization for D-Bus sandboxes
  via an identity map — cheapest but concedes the normalization goal for exactly
  the sandboxes that want the bus.)

- [ ] **Reconsider `self` and how runtime-only values enter — argument
  overrides.** `self` (`HostId::Invoker`, resolved to `getuid()`/`getgid()` by the
  engine at launch — `src/spec.rs`, `src/oci.rs`) is a bit of a wart: an in-band
  sentinel the engine resolves by **querying the host**, a small crack in the
  "engine holds no policy, runs no host query" line. The same shape recurs in
  `BusAddress::Session`/`System` (the engine reads `$DBUS_SESSION_BUS_ADDRESS` /
  the well-known path at launch — `src/dbus.rs`). Intuition to develop: **runtime
  values are supplied as args** — genuinely launch-time-only values (the invoker
  uid/gid, the host session/system bus address, the host Wayland socket) should
  enter through the explicit CLI-override / "runtime producer" channel
  (like `--env`/`--mount`/`--hostname`), keeping the spec static/pinned and the
  engine free of implicit host resolution. Then `self` dissolves: the caller/
  wrapper passes the concrete uid as an override. Needs a **principled** split,
  not a point fix — the clean cut looks like: *build-time/producer* values live in
  Nix (static, auditable — e.g. the normalized container uid `1000`), *genuinely-
  runtime* values (unknowable until launch — the invoker's real uid, host socket
  addrs) come in as launch args. `self` today conflates the two by having the
  engine auto-resolve the runtime half in-band. This interacts with the proxy fork
  above: that uid map has a build-time half (the normalized container id → Nix) and
  a runtime half (the host invoker id → arg), same seam. Work out the model before
  touching the schema.
