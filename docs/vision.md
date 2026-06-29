# Pierogi — Vision & Scope

## One-line

A fast, Nix-native sandbox runtime that sits between `bubblewrap` and
`docker`: it launches processes into strictly-namespaced environments in
milliseconds, with synthetic system state and no escape hatches.

## The gap it fills

| Tool | What it is | Why it's not enough |
|------|-----------|---------------------|
| **bubblewrap** | Low-level unprivileged namespace setup tool | No declarative config story, no Nix integration, every sandbox is a bespoke pile of `--ro-bind` flags. Easy to accidentally punch holes. |
| **docker / podman** | Full container platform | Image build system, daemon, registries, management layer, networking stack — none of which we want. Heavyweight cold start. Roots in mutable images, not the Nix store. |
| **systemd-nspawn** | Machine/container manager | Geared toward full OS containers and long-lived machines, not tiny fast processes. |
| **firejail** | SUID sandbox | SUID binary attack surface; profile model leans heavily on host `/etc` and escape hatches. |

**Pierogi** takes bubblewrap's "just set up namespaces and exec" philosophy,
adds a declarative Nix configuration layer for repeatable sandboxes, and bakes
in strict, secure-by-default behavior so the easy path is the safe path.

## Goals

1. **Fast cold start.** Launching a sandboxed process should be cheap enough
   that you reach for it reflexively for one-off commands. Target: low
   single-digit milliseconds of runtime overhead before `exec`.
2. **Strict by default.** Read-only root, user-namespace + subuid/subgid
   mapping always on, minimal mounts, synthetic system files. You opt *into*
   exposure, never out of confinement.
3. **Two run-modes.**
   - *Wrap mode* — point it at a binary or store path, get a sane sandbox with
     zero config.
   - *Config mode* — a Nix module describes precisely what the namespace sees.
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
- **No reinventing the runtime.** The low-level namespace/cgroup/seccomp/`exec`
  mechanics are delegated to an embedded OCI runtime (youki, or libcrun) driven
  as a library — Pierogi builds its config in memory and skips the OCI
  `config.json` intermediate entirely. See [architecture.md](architecture.md).
- **No "just bind-mount the host" escape hatches.** Exposing host `/etc`, host
  `/home`, or the raw D-Bus socket defeats the purpose. Where exposure is
  unavoidable it must be explicit, narrow, and visible in config — never a
  blanket convenience flag.

## Design philosophy

- **The point is the boundary.** Every feature is judged by whether it
  *tightens* or *dissolves* the sandbox boundary. Convenience flags that
  dissolve it don't ship.
- **Generate, don't borrow.** When a sandbox needs `/etc/passwd` or a hostname,
  synthesize a minimal correct version from declared identity. Borrowing host
  files leaks host state and creates surprising coupling.
- **Unprivileged first.** Everything that can work via user namespaces without
  root or SUID, does. No SUID binary; no persistent privileged daemon.
- **Declarative confinement.** The config layer describes a *whole* sandbox
  that can be diffed, reviewed, and reproduced — not a sequence of imperative
  mount commands.
- **Composition over configuration.** Profiles (browser, gui-app, network-app)
  are composable Nix modules, not a monolithic option set.

## Success looks like

- `pierogi run -- bash` drops you into a confined shell with a synthetic
  environment, read-only root, and your UID mapped — in a blink.
- A `pierogi.sandboxes.firefox` Nix module produces a hardened browser sandbox
  whose entire view of the system is auditable in one file.
- Reaching for Pierogi for a sketchy one-off binary feels as natural as `sudo`
  feels today — but safer.

## Naming the layers (shared vocabulary)

- **Engine** — the Rust binary that prepares a sandbox and drives an embedded
  OCI runtime to `exec` the target. The shipped deliverable. Knows nothing about
  Nix evaluation.
- **Spec** — the resolved description of one sandbox's *runtime concerns*
  (namespaces, ids, env, dbus rules, seccomp, exec args, a pointer to the
  rootfs). Generated in memory by wrap mode; serialized into the store by nix
  mode. Exact format mostly open; the synthetic-identity slice is pinned early.
- **`pierogi-synth`** — the single Rust library of synthetic-file builders
  (`/etc/passwd`, etc.), consumed both at runtime (wrap) and at build time
  (nix), so the logic is never reimplemented in the Nix language.
- **Run-modes** — *wrap mode* (generate everything live from a binary/store
  path) and *nix mode* (point at a prebuilt store path holding `spec` + a
  `rootfs` link).
- **Config layer** — the *deferred, last-step* Nix module/options sugar that
  evaluates down to a nix-mode `{spec, rootfs}` store path. Not part of the core
  deliverable.
- **Proxy layer** — the D-Bus (and later, other socket) filtering processes the
  Engine supervises alongside the target.

See [architecture.md](architecture.md) for how these fit together and
[roadmap.md](roadmap.md) for the milestone path.
