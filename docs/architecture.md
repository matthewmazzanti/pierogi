# Pierogi — Architecture

This document describes the component structure of the Pierogi engine, the
sandbox setup sequence, and the boundary between the Rust runtime and the Nix
configuration layer. See [vision.md](vision.md) for goals and non-goals, and
[execution-modes.md](execution-modes.md) for the verbs (`wrap`/`run`/`resolve`),
the config→spec resolution pipeline, and the `closure:` fast-path.

## Layered overview

```
   WRAP MODE (rootfs baked live)          NIX MODE (built AHEAD, in store)
   pierogi wrap -- <bin|store-path>       pierogi run /nix/store/…-sandbox.json
            │  (not yet implemented)                │
   pierogi bakes at runtime, in a          Nix derivation pre-built:
   scratch dir OUTSIDE the store:           • one self-contained resolved spec
    • a sealed rootfs — /etc, /bin,         • root IS the rootfs: /etc, /bin &
      mountpoints — over the target's         base mountpoints baked at build;
      closure                                 the /nix/store tree binds RO at
    • the matching spec                       launch; closure.resolved absolute
            │                                       │
            └──────────────┬────────────────────────┘
                    ▼   (a sealed rootfs + its resolved spec, however baked)
  ┌─────────────────────────────────────────────────────────┐
  │                       Engine (Rust)                      │
  │  obtain spec (generate │ read) → translate to OCI runtime│
  │  spec in memory → spawn proxies → drive crun ────────────┼──┐
  └─────────────────────────────────────────────────────────┘  │
                    supervises  │                     exec crun subprocess
                      ┌─────────▼─────────┐                     │
                      │  Proxy layer      │    write config.json → crun run:
                      │  dbus filter, etc │    unshare/pivot_root/cgroup/
                      └───────────────────┘    seccomp/exec
```

**Key invariant:** the Engine drives the runtime from a **spec**, regardless of
where it came from. The spec *carries its own filesystem* — `root` **is** the
rootfs (a `/nix/store` path mounted at `/`, read-only) and its `closure.resolved`
name the whole closure by absolute store path — so the spec is a self-contained
JSON file with no separate rootfs input to thread through (see
[Spec & rootfs](#spec--rootfs-build-time-vs-runtime)). The two front-ends differ
only in *when* the spec is produced:

- **Wrap mode** (*not yet implemented*) bakes the sealed rootfs **at runtime**, in
  a scratch dir *outside* the store: it computes the target's closure and
  scaffolds `/etc`, `/bin`, and the mountpoints on the fly — the same tree a Nix
  build produces, just in `$TMPDIR` instead of `/nix/store` — then hands it to the
  identical machinery (root mounted at `/`, closure bound in). The one runtime
  closure computation lives here, in the front-end; the `run` core stays
  query-free and only ever runs an already-sealed rootfs.
- **Nix mode** points at a **store path built ahead of time** that *is* the
  resolved spec (a JSON file). Its `closure.resolved` name every store path
  absolutely, so the file is self-contained — Nix pins the whole closure as the
  artifact's references automatically (it scans the output for `/nix/store/…`
  strings), and `/etc`, `/bin`, and the base mountpoint dirs are baked into `root`
  at build time. The `/nix/store` tree is a read-only launch-time bind; the Engine
  assembles it and runs.

Same engine, same security-critical core; only the provenance of its inputs
changes. The exact serialized **spec format** is still open in detail (see
[Spec & rootfs](#spec--rootfs-build-time-vs-runtime) below), but its *role* is
now fixed: it is what Nix materializes into the store and what wrap mode
generates on the fly.

**Second principle: delegate the low-level mechanics *by default* — a
security-cost decision, not dogma.** The goal is *namespaces (and the rest of the
confinement dance) by any means that clears the security bar*. The actual
`unshare`/`clone`/`pivot_root`/cgroup/seccomp-load/`exec` sequence is deep,
security-critical, and the *least* differentiated code we could write — so
we delegate it to an existing runtime rather than own it. **Hand-rolling it is a
live option, currently rejected on that cost basis, not refused on principle** —
it comes back on the table the moment it can meet the security bar. The runtime
is fixed to **crun**, driven as a **subprocess**. youki was evaluated and dropped:
its `libcontainer` could not read-only-remount a `/nix/store` rootfs, whereas crun
re-supplies the kernel-locked mount flags. Pierogi builds crun's config **in
memory**, serializes it to a `config.json` in the per-run bundle dir, and execs
`crun run` against it. (An earlier design linked `libcrun` in-process via an FFI
shim to keep the spec in memory and skip a subprocess; a head-to-head benchmark
showed that bought only ~a few milliseconds — within the noise of the namespace/
mount setup that dominates startup — so the shim was dropped for the plain
binary. See [startup-performance.md](startup-performance.md).) Pierogi's value-add
is everything *around* the mechanics: the strict-by-default front-ends, synthetic
system state, the proxy layers, and the Nix integration.

**Third principle: the engine holds no policy — it is runc, the Nix builder is
podman.** The security posture (which caps, which masks, the seccomp profile, the
id-mapping, the default-closed floor) is **not** in pierogi-the-binary. The engine
is a *pure realizer*: a lightly-extended OCI runtime (runc's role) plus nix
carve-outs (sealed rootfs, closure identity-binds, overlay/relocation). It
faithfully projects whatever the resolved spec carries into the OCI spec and
`exec`s — `absent = absent`, no default applied. All policy, defaults, and
curation live one level **up**, in the Nix producer layer (`mkSandbox` /
`profiles.base`, podman's role), which composes a fully-resolved spec. Two
payoffs: the engine has a **bounded, finishable** surface (the OCI spec is finite)
while policy churns cheaply in Nix; and the resolved spec is the **complete,
auditable record**, because every floor value was written by a producer and is
visible in the artifact rather than applied invisibly by the engine. The
consequence for the schema and `oci.rs`: OCI knobs (caps, masks, rlimits,
namespaces, mappings, `noNewPrivileges`, propagation) are **pass-through fields**,
not engine behavior. See
[control-surface.md](control-surface.md#decided--the-producer-writes-the-floor-the-engine-holds-no-policy).

## Language & crates

Rust. Core dependencies (candidates):

- **Runtime — the `crun` binary.** A mature C runtime driven as a subprocess
  (`src/crun.rs`): construct the OCI config in memory, serialize it to a
  `config.json` in the per-run bundle dir, and exec `crun run` against it. crun is
  also the fastest backend measured (see
  [startup-performance.md](startup-performance.md)). See
  [runtime backend](#runtime-backend) for why crun is fixed and youki dropped.
- **`oci-spec`** — the `oci_spec::runtime::Spec` types crun consumes. We build
  these in memory from the pierogi spec, then serialize them to the `config.json`
  the crun binary reads off disk.
- **`serde`** — for the synthetic-identity JSON the builders consume, and for
  the serialized `spec` file nix mode reads from the store. Wrap mode may keep
  the spec entirely in memory; the format beyond the identity slice is TBD (see
  [Spec & rootfs](#spec--rootfs-build-time-vs-runtime)).
- **`clap`** — CLI for wrap mode.
- **`rustix`** / **`nix`** — only for the thin bits Pierogi still does itself
  *around* the runtime (synthetic-`/etc` tmpfs writes, proxy socket setup).
- Seccomp and capability handling are configured *declaratively* in the OCI
  spec we hand the runtime (`linux.seccomp`, `process.capabilities`); we
  generally do not assemble BPF ourselves. A `seccompiler`-style escape hatch
  remains available if we need a profile the OCI seccomp schema can't express.
- Avoid pulling a large async runtime into the launch path. The proxy
  supervisor may use a minimal event loop; the runtime's setup → `exec` path is
  handled by the backend.

**Why drive a runtime instead of hand-rolling it:** the unshare/clone ordering,
the intermediate-process model, `pivot_root` edge cases, cgroup v2 delegation,
and seccomp notify are a deep, security-critical, well-trodden problem. crun has
solved it and is continuously hardened. Reimplementing it would be the *least*
differentiated and *most* dangerous code in the project. crun has solved it, so
we build the OCI spec in memory and hand it off as a `config.json` — keeping the
speed and ergonomics we want without re-solving the mechanics.

**Threading note:** namespace and `pivot_root` operations are per-thread and
interact badly with multi-threaded runtimes — which is exactly why crun owns the
delicate fork/clone process model. Driving crun as a subprocess keeps that
boundary clean: crun owns the intermediate/init process where the per-thread
syscalls happen, in its own address space.

## Spec & rootfs (build-time vs runtime)

The Engine drives the runtime from a **spec** (runtime concerns) plus the
**filesystem** it describes. These are two *roles*, not two *files*: `root` **is**
the rootfs — a `/nix/store` path mounted at `/` (read-only) that bakes `/etc`,
`/bin`, and the base mountpoint dirs at build time — and the rest of the
filesystem is the spec's closure, carried **inside** the spec as `closure.resolved`
(per-path identity binds under a read-only `/nix/store` assembled at launch). The
static topology is frozen at build, the store tree bound read-only at launch; there
is no separate rootfs input to thread through. So "spec & rootfs" below names the
boundary, not a two-entry on-disk layout.

**What the spec carries** (runtime behavior *plus* the closure that is the
filesystem — the store paths are named, not shipped as an image manifest):

- namespaces to create
- uid/gid mapping (subuid/subgid ranges)
- dbus (and later, socket) proxy rules
- exec args / cwd / env
- identity for synthetic `/etc` (hostname, user)
- `root` — the rootfs store path, mounted at `/` read-only
- `closure.resolved` — the closure as absolute store paths, each bound read-only
  onto a launch-time `/nix/store/<hash>` stub (`root` itself is included,
  identity-bound at `/nix/store/<self>` as well as being the `/` mount)

**Provenance:**

- **Wrap mode** generates the spec in memory from the target + strict defaults,
  and resolves the target's closure into `closure.resolved`.
- **Nix mode** consumes a store path built ahead of time that *is* the resolved
  spec — a single JSON file, e.g. `/nix/store/…-sandbox.json`. The file is
  self-contained: `closure.resolved` name every store path absolutely, `root`
  bakes `/etc`, `/bin`, and the base mountpoint dirs (the `/nix/store` tree is a
  read-only launch-time bind), and Nix pins the whole closure as the artifact's
  runtime references automatically by scanning the output for `/nix/store/…`
  strings. The
  Engine reads the file, and because it is already resolved (`closure:` present)
  it runs the sealed fast-path — this is exactly `run <spec.json>`, no separate
  code path.

**Format: still open in detail, with one piece now pinned.** The *serialized wire
format* (JSON vs other, public-contract status) is deferred until M1 tells us what
the engine actually needs — but the **field-level schema is now a converging
draft** in [spec.md](spec.md) (`exec` / `root` / `mounts` / `env` / `closure`;
mounts are additive *placements* onto baked mountpoints — nothing folds a closure,
the `closure` is authoritative). The **synthetic-identity sub-schema is fixed
early** because
it's the JSON the
synthetic builders consume (see next section) — hostname, user (name/uid/gid/
home), and the list of `/etc` files to emit. Pinning that slice lets both wrap
mode and the Nix build call the same generator without waiting on the rest.

> **Still undecided (resolve at M1):** the rest of the serialized format
> (JSON vs other), whether it's a stable public contract or an internal detail,
> and how much it overlaps the OCI runtime spec we build anyway (could it be a
> thin OCI-spec subset rather than a bespoke schema?).

## Synthetic file generation

Synthetic system files (`/etc/passwd`, `group`, `hostname`, `hosts`,
`resolv.conf`, `machine-id`, `nsswitch.conf`) are **generated by one
implementation, in Rust, called at two different times.** We do *not* reimplement
this logic in the Nix language.

- A `pierogi-synth` crate holds pure builders: `passwd(identity) -> String`,
  `group(...)`, `hosts(...)`, `machine_id(...)`, etc. Their input is a typed
  struct `serde`-deserialized from the spec's synthetic-identity JSON slice.
- **JSON is the seam** between untyped build-time Nix and the typed Rust
  builders. Three call sites, one code path:
  1. **Linked into the engine** — wrap mode renders `/etc` at *runtime* into the
     staging tmpfs.
  2. **A CLI subcommand** — `pierogi synth-etc <spec.json> <outdir>` on the same
     shipped binary (one artifact, not a second bin).
  3. **Nix build-time** — a derivation writes the identity JSON, runs
     `pierogi synth-etc`, and bakes the result into the store rootfs. Nix
     produces JSON and shells the binary; it never re-derives `passwd` logic.

This is why nix mode's rootfs can ship with `/etc` already baked: the build step
called the same generator the engine would have called at runtime. The only cost
is that building a nix-mode sandbox has a build-time dependency on the pierogi
binary — which is the package we ship anyway.

Synthetic files are always emitted into a Pierogi-controlled location and bound
**read-only**; they are never bind-mounted from the host.

## Engine launch sequence

The Engine's job is to *prepare everything the runtime needs*, then hand off.
The numbered low-level mechanics (namespaces, id maps, mounts, seccomp, caps,
`exec`) are performed **by the runtime backend** from the OCI spec we build —
Pierogi does not issue those syscalls itself.

**Pierogi-owned steps (around the runtime):**

1. **Obtain the spec.** Wrap mode generates the spec from target + defaults; nix
   mode reads the self-contained spec JSON from the store path — `root` is the
   rootfs, so there is no separate `rootfs` link to resolve. Validate; reject
   anything ambiguous (defaults live in the front-ends).
2. **Synthesize `/etc`** *(wrap mode only)*. Render `passwd`, `group`,
   `hostname`, `hosts`, `resolv.conf`, `machine-id`, `nsswitch.conf` via
   `pierogi-synth` into a Pierogi-managed staging dir, to be bound read-only.
   **In nix mode this is skipped** — those files are already baked into the
   prebuilt rootfs (by the same generator at build time).
3. **Spawn the proxy layer.** Start the D-Bus proxy (and future socket
   proxies); their listening sockets get added to the OCI spec as bind mounts
   at the paths the guest expects. Wait for ready before handoff.
4. **Translate the spec → in-memory OCI runtime spec.** Map the spec's
   fields onto the OCI schema:
   - read-only root → `root.readonly = true`
   - id mapping → `linux.uidMappings` / `linux.gidMappings` (from
     `subuid`/`subgid`); confines the sandbox "root" to an unprivileged host
     range
   - namespaces → `linux.namespaces`
   - store paths, synthetic `/etc`, tmpfs scratch, proxy sockets →
     `mounts[]` (read-only where possible; `/tmp` + runtime dir as tmpfs)
   - hostname → `hostname`
   - seccomp profile → `linux.seccomp`
   - capability drop → `process.capabilities` (usually empty), `noNewPrivileges`
   - argv/cwd/env → `process`
   This struct is built in memory, then serialized to a `config.json` in the
   per-run bundle dir — the crun binary reads its config off disk.

**Runtime-owned step (the handoff):**

5. **Drive crun** (write the `config.json`, then exec `crun run` against the
   bundle as a subprocess). The runtime sets up namespaces
   in the correct order,
   writes id maps with `setgroups deny`, builds and `pivot_root`s the rootfs,
   loads the seccomp filter, drops capabilities, and `exec`s the target. When
   it's done, only the target and Pierogi's supervised proxies remain.

Failure before handoff must tear down cleanly (remove staging dir, kill
proxies). The runtime owns clean teardown of its own namespaces/mounts on
failure during create/start.

## Mount & rootfs strategy

- **Read-only store, read-only root.** The base root is assembled from Nix
  store paths bound read-only. Nothing in the closure is writable.
- **Writable = explicit tmpfs.** Scratch space (`/tmp`, runtime dir, optionally
  a declared persistent state dir) is tmpfs or an explicit bind, never an
  accidental writable root.
- **Closure, not image.** A sandbox's filesystem is the transitive closure of
  the declared store paths plus synthetic files — never a layered, mutable
  image. Wrap mode resolves that closure at launch; nix mode consumes the
  build-time `closure` verbatim (topology frozen, no run-time resolve). Either
  way it's a closure, not an image.
- **No host `/etc`, `/home`, `/usr` exposure.** If a real host path must be
  exposed, it's a narrow declared bind in the spec with `ro` defaulting true.

## subuid / subgid mapping

- Default to mapping the sandbox's full id range into an unprivileged host
  subordinate range (`/etc/subuid`, `/etc/subgid`), so "root" inside the
  sandbox is a powerless high host uid.
- The invoking user's real uid may optionally map to a stable sandbox uid
  (e.g. 1000) for ergonomic home ownership, without granting host privilege.
- `setgroups` is denied before gid mapping per kernel requirement for
  unprivileged user namespaces.

## Synthetic system state

A dedicated module generates minimal-correct versions of:

- `/etc/passwd`, `/etc/group` — just the sandbox user + root + `nobody`.
- `/etc/hostname`, `/etc/hosts` — the declared hostname, loopback entries.
- `/etc/resolv.conf` — generated from the net config (or empty when net is off).
- `/etc/machine-id` — stable-per-sandbox or random, configurable; never the
  host's.
- `/etc/nsswitch.conf` — minimal, files-only unless net services are declared.

This is its own well-tested module because "what the guest believes about the
system" is a security and correctness surface in its own right.

## D-Bus proxy layer

Sandboxed apps frequently need *some* bus access (notifications, portals,
media keys). Handing over the raw socket is exactly the escape hatch we refuse.

- The Engine supervises a filtering proxy (design akin to `xdg-dbus-proxy`):
  the sandbox sees a proxy socket; the proxy holds the real connection and
  enforces an allowlist of `talk` / `own` / `call` / `broadcast` rules from the
  Spec's `dbus` section.
- Default policy is **deny-all**; rules are additive and declared per sandbox.
- Both session and system bus are supported, each with independent rules.
- Built in (or tightly supervised) rather than an optional add-on, so the
  *configurable filter* is a first-class layer, not a bolt-on.
- The proxy is the template for future socket-filtering layers (e.g. Wayland,
  PipeWire) — same supervise-and-bind-a-filtered-socket pattern.

## The two run-modes

**The deliverable is a runnable binary** — a normal program you invoke, not a
Nix module set. Both run-modes are surfaces of that one binary.

The CLI factors into three verbs — `wrap` (argv-first), `run <spec>`, and
`resolve` (freeze a spec) — each a tap at a different pipeline layer; see
[execution-modes.md](execution-modes.md) for the full model. The two run-modes
below are the endpoints those verbs serve.

### Wrap mode (CLI) — the primary surface

`pierogi wrap [flags] -- <binary|store-path> [args...]`

- Synthesizes the spec in memory from strict defaults + a small set of flags;
  synthesizes `/etc` and resolves the target's closure live. `wrap --dry-run` prints
  that spec *unresolved* instead of running — the editable starting point for a
  hand-authored config.
- Defaults: read-only root, all namespaces, id mapping on, deny-all D-Bus,
  synthetic `/etc`, no network unless `--net`.
- Flags tighten or make narrow, explicit exposures — there is intentionally no
  `--bind-host-etc`-style blanket flag.
- This is Milestone 1's surface (run a shell), and the main UX of the project.

### Nix mode (CLI) — run a prebuilt store path

`pierogi run /nix/store/…-sandbox.json`

- Points the engine at a store path that *is* the resolved spec (a self-contained
  JSON file; see [Spec & rootfs](#spec--rootfs-build-time-vs-runtime)). The engine
  reads and drives; almost no preparation work — the spec already carries its
  baked `closure:`, so no runtime query runs, `root` is the rootfs, and its
  `closure.resolved` name the filesystem absolutely.
- How those store paths get *built* is, for now, just "some Nix expression that
  writes a spec (via `pierogi synth-etc` for the baked `/etc` — M4; hand-rolled
  in Nix for M1 — and `pierogi resolve --graph` to bake the `closure:`)." The
  builder feeds pierogi the reference graph of `root` via `exportReferencesGraph`
  (using `__structuredAttrs`, expanded into `$NIX_ATTRS_JSON_FILE`), which
  pierogi traverses in-process from the spec's seeds — not `closureInfo`
  store-paths. A minimal builder helper is enough.

### Config layer (Nix modules) — DEFERRED / OPEN

> **Status: last milestone, intentionally left open.** Not part of the core
> deliverable.

The ergonomic declarative layer — an options schema
(`pierogi.sandboxes.<name> = { ... }`), composable profiles (browser/gui/
network), NixOS/home-manager integration, and `systemd`-unit emission — is the
*final* step, not an early one. It's sugar that evaluates down to the same
resolved-spec store path nix mode already consumes; lifecycle stays with
`systemd`. We design it only after the binary and both run-modes are solid, so
it is shaped by real usage rather than guessed up front.

## Runtime backend

The runtime is **crun**, and only crun. There is no backend trait or
abstraction: the OCI spec is built in memory, serialized to a `config.json` in
the per-run bundle dir, and run by exec'ing the `crun` binary as a subprocess
(`src/crun.rs`). An earlier design linked `libcrun` in-process via an FFI
shim (spec kept in memory, no `config.json`, no subprocess); a head-to-head
benchmark showed that saved only ~a few milliseconds — within the noise of the
namespace/mount setup that dominates startup — so it was dropped for the plain
binary, along with the C shim, the harvested libcrun dev tree, and the linking
(see [startup-performance.md](startup-performance.md)).

**Why crun, and why youki was dropped.** youki's `libcontainer` was the earlier
candidate (pure Rust, in-process, no FFI). It was dropped because it could not
read-only-remount a `/nix/store` rootfs — the kernel locks mount flags on a bind
of an already-read-only source, and youki did not re-supply them. crun does
re-supply the kernel-locked mount flags, which is exactly what the sealed
`/nix/store`-as-`root` topology needs. crun is also the fastest backend measured
(see [startup-performance.md](startup-performance.md)).

The spec → OCI translation (`src/oci.rs`) is the only runtime-aware code; keeping
it isolated is what keeps the core auditable.

## What the Engine deliberately does NOT do

- **No reimplementing low-level container mechanics *today*.** namespaces,
  cgroups, `pivot_root`, seccomp load, and `exec` are delegated to crun — a
  security-cost decision, not a permanent refusal. If a hand-rolled path can meet
  the security bar it's back on the table (see the runtime discussion above).
- **No bundle rootfs, minimal on-disk footprint.** We build the OCI spec in
  memory and serialize it to a `config.json` in a per-run bundle dir — nothing
  beyond that config and crun's state dir touches disk. The bundle carries no
  rootfs (`root.path` is the absolute store path); crun still needs a dir to
  chdir into, but it is otherwise empty.
- No image building, no layer cache, no registry.
- No long-lived daemon or supervisor-of-supervisors; one Engine process per
  launched sandbox, which hands off to the runtime and supervises only proxies.
- No Nix evaluation (that's the front-end's job; the Engine never shells to
  `nix`).
- **No policy or defaults.** The engine applies no floor, no default caps/masks/
  seccomp, no id-mapping of its own — it projects exactly what the resolved spec
  carries (`absent = absent`). The default-closed posture is `profiles.base` in the
  Nix layer, not engine behavior (see the third principle above).
- No host-state borrowing as a convenience.

## Open questions

**Runtime embedding — answered by the M0 spike.** The runtime backend is settled:
the crun binary driven as a subprocess (OCI spec built in memory, serialized to a
`config.json`, `crun run` exec'd against it, rootless by default), youki dropped
(see [runtime backend](#runtime-backend)). What remains open around the runtime:

- **How much does crun's process model cost us?** crun uses an intermediate +
  init process; measure whether that fits the single-digit-ms cold-start budget
  or whether we need a leaner create path.
- **PID 1 / reaper:** does crun give us an adequate init/reaper, or do we pair it
  with a tiny init for zombie reaping?

Deferred to the milestone that forces them:

- Where the proxy supervisor lives once the Engine hands off to the runtime —
  re-parent to a small supervised helper, or to `systemd`? *(M2 — D-Bus proxy)*
- Anything the OCI seccomp schema *can't* express that we'd need a
  `seccompiler`-style custom filter for. *(M4 — hardening)*
- Cgroup v2 delegation + resource limits: let the runtime manage the cgroup, or
  delegate entirely to `systemd` slices and tell the runtime to stay out? *(M4 — hardening)*

See [roadmap.md](roadmap.md) for milestone detail.
