# Pierogi — Architecture

This document describes the component structure of the Pierogi engine, the
sandbox setup sequence, and the boundary between the Rust runtime and the Nix
configuration layer. See [vision.md](vision.md) for goals and non-goals.

## Layered overview

```
   WRAP MODE (generate at RUNTIME)        NIX MODE (built AHEAD, in store)
   pierogi run -- <bin|store-path>        pierogi run /nix/store/…-sandbox
            │                                       │
   engine generates live:                  Nix derivation pre-built:
    • spec (runtime concerns)               • ./spec    (serialized)
    • synthetic /etc                        • ./rootfs  → closure
    • rootfs view from closure                (synthetic /etc baked in)
            │                                       │
            └──────────────┬────────────────────────┘
                           ▼   (spec + rootfs, however obtained)
  ┌─────────────────────────────────────────────────────────┐
  │                       Engine (Rust)                      │
  │  obtain spec (generate │ read) → assemble OCI runtime    │
  │  spec in memory → spawn proxies → drive runtime ─────────┼──┐
  └─────────────────────────────────────────────────────────┘  │
                    supervises  │                               │ in-process
                      ┌─────────▼─────────┐         ┌───────────▼──────────┐
                      │  Proxy layer      │         │  Runtime backend     │
                      │  dbus filter, etc │         │  youki libcontainer  │
                      └───────────────────┘         │  (or libcrun FFI)    │
                                                    │  unshare/pivot_root/ │
                                                    │  cgroup/seccomp/exec │
                                                    └──────────────────────┘
```

**Key invariant:** the Engine drives the runtime from a **spec + a rootfs**,
regardless of where they came from. The two front-ends differ only in *when*
those are produced:

- **Wrap mode** generates the spec, synthesizes `/etc`, and assembles the rootfs
  view **at runtime** from the target's closure — nothing is pre-built.
- **Nix mode** points at a **store path built ahead of time** containing a
  serialized `spec` file and a `rootfs` symlink (with synthetic `/etc` already
  baked into the closure). The Engine just reads and runs.

Same engine, same security-critical core; only the provenance of its inputs
changes. The exact serialized **spec format** is still open in detail (see
[Spec & rootfs](#spec--rootfs-build-time-vs-runtime) below), but its *role* is
now fixed: it is what Nix materializes into the store and what wrap mode
generates on the fly.

**Second invariant: we don't reimplement the low-level mechanics.** The actual
`unshare`/`clone`/`pivot_root`/cgroup/seccomp-load/`exec` dance is delegated to
an embedded OCI runtime — **youki**'s `libcontainer` crate (in-process Rust) as
the primary backend, with **libcrun** (C, via FFI) as an alternative. Pierogi
drives the runtime **as a library, building its config in memory** — it never
writes an OCI bundle / `config.json` to disk and never shells out to a runtime
CLI. We reuse the runtime's battle-tested process model; Pierogi's value-add is
everything *around* it: the strict-by-default front-ends, synthetic system
state, the proxy layers, and the Nix integration.

## Language & crates

Rust. Core dependencies (candidates):

- **Runtime backend — `youki` `libcontainer`** (primary). The in-process Rust
  OCI runtime library that performs the namespace/cgroup/seccomp/`exec`
  mechanics. We construct its config in memory and drive container
  create/start directly. **`libcrun`** (C, via a thin FFI binding) is the
  fallback backend behind the same internal trait.
- **`oci-spec`** — the `oci_spec::runtime::Spec` types youki consumes. We build
  these in memory from the pierogi spec; they are an *internal* representation,
  never serialized to a `config.json` on disk.
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
and seccomp notify are a deep, security-critical, well-trodden problem. youki
and crun have solved it and are continuously hardened. Reimplementing it would
be the *least* differentiated and *most* dangerous code in the project.
Bypassing the OCI `config.json` file (building the spec in memory and calling
the library) keeps the speed and ergonomics we want without re-solving the
mechanics.

**Threading note:** namespace and `pivot_root` operations are per-thread and
interact badly with multi-threaded runtimes — which is exactly why the backend
(youki) owns the delicate fork/clone process model. Linking it **in-process as
a Rust library** (no FFI, no subprocess) is clean precisely because Rust doesn't
multiplex our logic across threads behind our back. This is a core reason we
chose Rust over Go: Go would need the runc-style cgo `nsenter` constructor hack,
and embedding a Rust runtime library would mean FFI anyway.

## Spec & rootfs (build-time vs runtime)

The Engine drives the runtime from two things: a **spec** (runtime concerns) and
a **rootfs** (the filesystem). Both can be produced two ways — generated live by
wrap mode, or pre-built into the store for nix mode — but the spec's *role* is
fixed either way.

**What the spec carries** (mostly runtime behavior, because the rootfs is a
separate, already-materialized thing — *not* a filesystem image manifest):

- namespaces to create
- uid/gid mapping (subuid/subgid ranges)
- dbus (and later, socket) proxy rules
- exec args / cwd / env
- identity for synthetic `/etc` (hostname, user)
- a pointer to the rootfs (a store path / closure)

**Provenance:**

- **Wrap mode** generates the spec in memory from the target + strict defaults,
  and assembles the rootfs view live from the target's closure.
- **Nix mode** consumes a store path built ahead of time:
  ```
  /nix/store/…-sandbox/
    ├── spec       # serialized spec (the synthetic /etc already baked into rootfs)
    └── rootfs ->  # symlink to the built closure
  ```
  The Engine reads `spec`, points the runtime at `rootfs`, and does almost no
  preparation work.

**Format: still open in detail, with one piece now pinned.** The full serialized
spec schema is deferred until M1 tells us what the engine actually needs. But
the **synthetic-identity sub-schema is fixed early** because it's the JSON the
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

1. **Obtain the spec & rootfs.** Wrap mode generates the spec from target +
   defaults; nix mode reads `spec` and resolves the `rootfs` link from the store
   path. Validate; reject anything ambiguous (defaults live in the front-ends).
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
   This struct lives only in memory; it is the runtime's input, not a file.

**Runtime-owned step (the handoff):**

5. **Drive the backend** (`libcontainer::Container::create` → `start`, or the
   libcrun equivalent). The runtime sets up namespaces in the correct order,
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
  image. Wrap mode assembles that view at launch; nix mode resolves a closure
  that was assembled at build time. Either way it's a closure, not an image.
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

### Wrap mode (CLI) — the primary surface

`pierogi run [flags] -- <binary|store-path> [args...]`

- Generates the spec in memory from strict defaults + a small set of flags;
  synthesizes `/etc` and assembles the rootfs view live.
- Defaults: read-only root, all namespaces, id mapping on, deny-all D-Bus,
  synthetic `/etc`, no network unless `--net`.
- Flags tighten or make narrow, explicit exposures — there is intentionally no
  `--bind-host-etc`-style blanket flag.
- This is Milestone 1's surface (run a shell), and the main UX of the project.

### Nix mode (CLI) — run a prebuilt store path

`pierogi run /nix/store/…-sandbox`

- Points the engine at a store path holding a `spec` file + a `rootfs` symlink
  (see [Spec & rootfs](#spec--rootfs-build-time-vs-runtime)). The engine reads
  and drives; almost no preparation work.
- How those store paths get *built* is, for now, just "some Nix expression that
  writes a spec (via `pierogi synth-etc` for the baked `/etc`) and links a
  closure." A minimal builder helper is enough.

### Config layer (Nix modules) — DEFERRED / OPEN

> **Status: last milestone, intentionally left open.** Not part of the core
> deliverable.

The ergonomic declarative layer — an options schema
(`pierogi.sandboxes.<name> = { ... }`), composable profiles (browser/gui/
network), NixOS/home-manager integration, and `systemd`-unit emission — is the
*final* step, not an early one. It's sugar that evaluates down to the same
`{spec, rootfs}` store path nix mode already consumes; lifecycle stays with
`systemd`. We design it only after the binary and both run-modes are solid, so
it is shaped by real usage rather than guessed up front.

## Runtime backend abstraction

The backend sits behind a narrow internal trait — roughly `create(oci_spec) ->
Handle`, `start(Handle)`, `wait/kill/teardown` — so youki and libcrun are
swappable and we're not married to either:

- **youki `libcontainer`** (primary): pure Rust, in-process, no FFI, builds
  from `oci-spec` types we already hold in memory. Best ergonomic + safety fit.
- **libcrun** (fallback): mature C implementation, broadest feature coverage;
  reached via a thin FFI shim if we hit a youki gap. Still driven in-memory —
  we populate crun's config struct, not a `config.json`.

The spec → OCI-spec translation is the only backend-aware code; keeping it
isolated is what makes the swap cheap and the core auditable.

## What the Engine deliberately does NOT do

- **No reimplementing low-level container mechanics.** namespaces, cgroups,
  `pivot_root`, seccomp load, and `exec` are the embedded runtime's job, not
  ours.
- **No OCI bundle on disk, no runtime CLI.** We build the OCI spec in memory and
  drive the runtime as a library — no `config.json`, no `crun run`/`youki run`
  subprocess.
- No image building, no layer cache, no registry.
- No long-lived daemon or supervisor-of-supervisors; one Engine process per
  launched sandbox, which hands off to the runtime and supervises only proxies.
- No Nix evaluation (that's the front-end's job; the Engine never shells to
  `nix`).
- No host-state borrowing as a convenience.

## Open questions

**The M0 shell-run spike exists to answer these first** — they're all about
whether the in-process runtime embedding actually works the way we hope:

- **Backend choice:** start on youki `libcontainer` and confirm its in-process
  API covers our path (in-memory spec, no bundle dir, rootless by default), or
  do we need the libcrun FFI sooner than planned?
- **How much does the runtime's process model cost us?** youki/crun use an
  intermediate + init process; measure whether that fits the single-digit-ms
  cold-start budget or whether we need a leaner create path.
- **PID 1 / reaper:** does the runtime give us an adequate init/reaper, or do we
  pair it with a tiny init for zombie reaping?

Deferred to the milestone that forces them:

- Where the proxy supervisor lives once the Engine hands off to the runtime —
  re-parent to a small supervised helper, or to `systemd`? *(M4)*
- Anything the OCI seccomp schema *can't* express that we'd need a
  `seccompiler`-style custom filter for. *(M2)*
- Cgroup v2 delegation + resource limits: let the runtime manage the cgroup, or
  delegate entirely to `systemd` slices and tell the runtime to stay out? *(M2)*

See [roadmap.md](roadmap.md) for milestone detail.
