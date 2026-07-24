# Pierogi — Vision & Scope

## One-line

A fast, Nix-native sandbox runtime that sits between `bubblewrap` and
`docker`: it launches processes into strictly-namespaced environments in
milliseconds, with synthetic system state and no hidden escape hatches.

## The gap it fills

| Tool | What it is | Why it's not enough |
|------|-----------|---------------------|
| **bubblewrap** | Low-level unprivileged namespace setup tool | No declarative config story, no Nix integration, every sandbox is a bespoke pile of `--ro-bind` flags. Easy to accidentally punch holes. |
| **docker / podman** | Full container platform | Image build system, daemon, registries, management layer, networking stack — none of which we want. Heavyweight cold start. Roots in mutable images, not the Nix store. |
| **systemd-nspawn** | Machine/container manager | Geared toward full OS containers and long-lived machines, not tiny fast processes. |
| **firejail** | SUID sandbox | SUID binary attack surface; profile model leans heavily on host `/etc` and escape hatches. |
| **`buildFHSEnv`** (nixpkgs) | FHS-compatible chroot for running non-Nix binaries | A *compatibility* shim, not a security boundary: bind-mounts a broad slice of the host (`/etc`, `/sys`, `/dev`, host `/proc`), ships a large default base package set, and does no user-namespace confinement. The path of least resistance is a leaky sandbox. |

**Pierogi** takes bubblewrap's "just set up namespaces and exec" philosophy,
adds a declarative Nix configuration layer for repeatable sandboxes, and bakes
in strict, secure-by-default behavior so the easy path is the safe path.

### Nix-world replacement targets

A resolved sandbox is a self-contained spec — its closure (the filesystem) plus
a declarative description of what the namespace sees. That single source of truth
is what existing Nix packaging functions approximate, less safely — so each is a
target to subsume:

| Replaces | Used today for | What Pierogi changes |
|----------|----------------|----------------------|
| `buildFHSEnv` | Running non-Nix binaries that expect a normal Linux layout | A deliberately minimal base set in place of the broad FHS default, and real host isolation (user namespaces, synthetic `/etc`, no host bind-mounts) in place of the compatibility chroot. |
| `dockerTools.buildImage` / `streamLayeredImage` | Shipping a closure to docker/podman/k8s consumers | Emit an OCI image *from the same resolved spec* — no daemon, no registry, no mutable-layer build; the image is just one serialization of the sandbox we already describe. |
| *(thinking big)* a portable, archival bundle | Handing someone a single artifact that runs on a non-Nix host (the niche flatpak/AppImage fill) | A self-contained bundle carrying its closure + spec, runnable on hosts without Nix, with the same confinement applied — `nix bundle` for sandboxes. |

These are **emission targets for an already-declared sandbox**, not an internal
image-build or registry system — that distinction is what keeps them consistent
with the non-goals below. The first is near-term; the latter two are
deliberately aspirational and only make sense once the spec/closure model
(M1+, nix-mode) is solid.

## Goals

1. **Fast cold start.** Launching a sandboxed process should be cheap enough
   that you reach for it reflexively for one-off commands. Target: low
   single-digit milliseconds of runtime overhead before `exec`.
2. **Default-safe, not safe-only.** Read-only root, user-namespace +
   subuid/subgid mapping, minimal mounts, and synthetic system files are the
   *default* posture — the easy path is the safe path. You opt *into* exposure;
   opting *out* of confinement is possible but never the default, never ambient,
   and always recorded in the resolved spec. The boundary is a default, not a
   cage — see [Design philosophy](#design-philosophy) and
   [control-surface.md](control-surface.md).
3. **Two run-modes** (`pierogi wrap` argv-first, `pierogi run <spec>` for a
   declared/prebuilt spec; `pierogi resolve` freezes a spec ahead of time — see
   [execution-modes.md](execution-modes.md)).
   - *Wrap mode* — point it at a binary or store path, get a sane sandbox with
     zero config.
   - *Config mode* — a spec (a hand-authored config or a Nix module) describes
     precisely what the namespace sees.
4. **Nix-native.** The host store is the source of truth. Closures, not images.
   No layer cache, no build system — `nix` already does that.
5. **Synthetic system state.** `/etc/hostname`, `/etc/passwd`, `/etc/group`,
   `/etc/resolv.conf`, machine-id, etc. are *generated* to match the sandbox's
   declared identity, never bind-mounted from the host.
6. **A real D-Bus story.** A built-in, configurable D-Bus proxy/filter layer so
   sandboxed apps can talk to the session/system bus through an allowlist
   instead of getting the raw socket.

## Non-goals (delegated or refused)

- **No image build system.** `nix build` produces closures; we consume them.
- **No daemon / management layer.** Lifecycle, restart, logging, scheduling are
  `systemd`'s job. Pierogi is `exec`-and-get-out-of-the-way.
- **No registry / distribution.** The Nix store and substituters are the
  distribution mechanism.
- **No orchestration / networking fabric.** No overlay networks, no compose.
- **Reinventing the runtime is a *cost*, not a blanket refusal.** The goal is
  namespaces by any means that clears the security bar; owning the low-level
  namespace/cgroup/seccomp/`exec` mechanics is a large security-critical surface,
  so *by default* we delegate to an embedded runtime driven as a library (config
  built in memory, no `config.json`). Hand-rolling stays a live option if it can
  meet that bar. The runtime is crun (libcrun), driven as an embedded library
  via FFI with its config passed in memory as a JSON string — no `config.json`
  — see [architecture.md](architecture.md).
- **No *ambient* or *blanket* escape hatches.** Exposing host `/etc`, host
  `/home`, or the raw D-Bus socket is *possible* — the engine is a pure realizer
  and binds what a spec declares — but never as a default, never as a one-word
  omnibus (`--privileged`-style), and never invisibly. Where exposure is wanted
  it must be explicit, granular, and visible in the resolved spec. What we refuse
  is not the *capability* to loosen but *unauditable* loosening — anything that
  widens the boundary without appearing in the artifact (a `hooks.d`-style
  ambient injection is the archetype). See [Design philosophy](#design-philosophy)
  and [control-surface.md](control-surface.md).

## Design philosophy

- **The point is the boundary.** Every feature is judged by whether it
  *tightens* or *dissolves* the sandbox boundary. A one-word convenience that
  dissolves it invisibly doesn't ship; a granular, explicit loosening that lands
  in the resolved spec is fine — the user may shoot their own foot, but only
  deliberately and on the record.
- **Auditability is the load-bearing invariant.** The resolved spec is the
  complete, honest record of what the sandbox sees and does — every loosening
  included. This is what makes "you may opt out of confinement" acceptable: you
  can always see exactly what was opened, because nothing widens the boundary
  without being written down. Safe *defaults* are the bridge/producer's job (it
  materializes them into the spec before the engine sees it); the engine stays a
  pure realizer that neither invents nor conceals.
- **Generate, don't borrow.** When a sandbox needs `/etc/passwd` or a hostname,
  synthesize a minimal correct version from declared identity. Borrowing host
  files leaks host state and creates surprising coupling.
- **Unprivileged first.** Everything that can work via user namespaces without
  root or SUID, does. No SUID binary; no persistent privileged daemon.
- **Declarative confinement.** The config layer describes a *whole* sandbox
  that can be diffed, reviewed, and reproduced — not a sequence of imperative
  mount commands.
- **Composition over configuration.** Profiles (browser, gui-app, network-app)
  are composable Nix modules, not a monolithic option set. The Nix layer lets
  *default policy itself be a library* — a profile encodes the minimum,
  vetted exposure an app class needs, authored once and shared through the store,
  rather than each user hand-rolling (and mis-rolling) it. Because a profile
  evaluates to a resolved spec, the shared policy stays fully auditable — see
  [control-surface.md](control-surface.md#the-producer-layer--where-defaults-and-shared-policy-live).

## Success looks like

- `pierogi wrap -- bash` drops you into a confined shell with a synthetic
  environment, read-only root, and your UID mapped — in a blink.
- A `pierogi.sandboxes.firefox` Nix module produces a hardened browser sandbox
  whose entire view of the system is auditable in one file.
- Reaching for Pierogi for a sketchy one-off binary feels as natural as `sudo`
  feels today — but safer.
- A workload reached for today via `buildFHSEnv` runs under Pierogi instead —
  see [Nix-world replacement targets](#nix-world-replacement-targets).

## Naming the layers (shared vocabulary)

- **Engine** — the Rust binary that prepares a sandbox and drives an embedded
  OCI runtime to `exec` the target. The shipped deliverable. Knows nothing about
  Nix evaluation.
- **Spec** — the description of one sandbox's *runtime concerns* (namespaces,
  ids, env, dbus rules, seccomp, exec args, and its `root` rootfs), carrying its
  resolved `closure:` — required for `run`, where an unbuilt spec fails loud.
  Synthesized in memory by `wrap`,
  hand-authored, or frozen by `pierogi resolve`; serialized into the store by nix
  mode. Exact format mostly open; the synthetic-identity slice is pinned early.
- **`pierogi-synth`** — the single Rust library of synthetic-file builders
  (`/etc/passwd`, etc.), consumed both at runtime (wrap) and at build time
  (nix), so the logic is never reimplemented in the Nix language.
- **Run-modes / verbs** — `pierogi wrap` (synthesize a spec from a binary/store
  path and run it live), `pierogi run <spec>` (run a spec — a hand-authored config
  or a prebuilt, self-contained spec store path), and `pierogi resolve` (freeze a
  spec by baking its `closure:`). Full model:
  [execution-modes.md](execution-modes.md).
- **Config layer** — the *deferred, last-step* Nix module/options sugar that
  evaluates down to a nix-mode resolved-spec store path. Not part of the core
  deliverable.
- **Proxy layer** — the D-Bus (and later, other socket) filtering processes the
  Engine supervises alongside the target.

See [architecture.md](architecture.md) for how these fit together and
[roadmap.md](roadmap.md) for the milestone path.
