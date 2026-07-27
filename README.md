# pierogi

A fast, Nix-native sandbox runtime — between `bubblewrap` and `docker`. See
[`docs/`](docs/) for the [vision](docs/vision.md), [architecture](docs/architecture.md),
[execution modes](docs/execution-modes.md), the [spec](docs/spec.md), the
[control surface](docs/control-surface.md), and the [roadmap](docs/roadmap.md).

## What it does

pierogi runs a process in a rootless namespace sandbox whose root filesystem
**is a sealed Nix store path**. A single `/nix/store/<hash>` is mounted at `/`
(read-only) carrying the whole skeleton — `/etc`, `/bin`, and every mountpoint
the sandbox needs — and the store paths it depends on are bind-mounted in under
`/nix/store`. Nothing is assembled at launch: the rootfs and its exact closure
are baked at **build time** by a Nix expression, so a launch is just namespaces
+ binds + `exec`, no Nix query (see [startup performance](docs/startup-performance.md)).

The runtime is the **`crun` binary**, driven as a subprocess. The OCI spec is
built in memory, serialized to a `config.json` in a per-run bundle dir, and
`crun run` is exec'd against it — no FFI, no C shim, no libcrun linking.

## Build & run

```sh
nix develop            # or: direnv allow  (.envrc is `use flake`)

# Build & run a self-contained, pre-baked sandbox (a static bash + coreutils
# rootfs with the closure stamped in). `.#bash-sandbox` is a wrapper that execs
# `pierogi run` against the built spec and hands its argv to bash in the sandbox:
nix run .#bash-sandbox                       # interactive bash (sources /etc/bashrc)
nix run .#bash-sandbox -- -c 'echo hello'    # args go straight to bash

# A second example: route the host Wayland socket into the sandbox and run
# `wayland-info` against your live compositor (needs a Wayland session). The
# wrapper binds $XDG_RUNTIME_DIR/$WAYLAND_DISPLAY onto a baked socket mountpoint.
nix run .#wayland-sandbox
```

Inside, you get a shell whose filesystem is the sealed rootfs at `/` plus its
closure under `/nix/store` and a writable tmpfs `/tmp`, `/dev`, and `/proc`.
`PATH` is `/bin` (the rootfs's own symlink farm into the store).

### The two verbs

- **`pierogi run <spec.json>`** — run a **pre-baked** spec. The spec must already
  carry its `closure:` (there is no runtime Nix query); `run` on an unbuilt spec
  fails loud. CLI overrides (`--env`, `--hostname`, `--mount host:…|tmpfs:…`, and
  a trailing `-- args` that replaces the process args) overlay onto the loaded
  spec, but the executable and closure are frozen — no `--mount store:`, and the
  image's entrypoint executable is sealed (only its args vary).
- **`pierogi resolve <spec.json> --graph <graph>`** — build a spec: traverse
  Nix's `exportReferencesGraph` (supplied as JSON) from the spec's seeds, stamp
  the resulting closure, and print the frozen spec. This is what a Nix builder
  calls; see [`nix/sandbox/bash.nix`](nix/sandbox/bash.nix).

### Findings still worth knowing

- **In-memory spec, serialized per run.** The OCI spec is built in memory, then
  written to a `config.json` in the per-run bundle dir for the crun binary — the
  only things that touch disk are that config and crun's own state dir. (youki,
  the original second backend, was dropped — it couldn't RO-remount a `/nix/store`
  rootfs; crun re-supplies the kernel-locked mount flags and handles it.)
- **devices.** Don't bind device nodes via `mounts[]` (EINVAL). Mount a `/dev`
  tmpfs and let crun populate its default device nodes.
- **cgroups.** Work via systemd user-session delegation (rootless); hosts
  without delegation may need `systemd-run --user --scope`.

## Layout

```
src/            # the pierogi crate: resolve, spec, oci translation, run, tty relay
src/crun.rs     # crun backend (execs the crun binary as a subprocess)
nix/            # lib/profiles/terminal-tools + sandbox/ — the sealed-rootfs build pipeline
flake.nix .envrc# dev shell: rust toolchain (crun binary pinned via CRUN_BIN)
docs/           # vision / architecture / execution-modes / spec / control-surface / roadmap
```
