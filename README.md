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

The runtime is **crun** (libcrun via the FFI shim in `csrc/shim.c`), driven
in-process. The OCI spec is handed to it as an in-memory JSON string — no
`config.json`, no on-disk bundle.

## Build & run

```sh
nix develop            # or: direnv allow  (.envrc is `use flake`)

# Build a self-contained, pre-baked sandbox spec (a static bash + coreutils
# rootfs with the closure stamped in), then run it:
nix build .#bash-sandbox
cargo run -- run ./result
```

Inside, you get a shell whose filesystem is the sealed rootfs at `/` plus its
closure under `/nix/store` and a writable tmpfs `/tmp`, `/dev`, and `/proc`.
`PATH` is `/bin` (the rootfs's own symlink farm into the store).

### The two verbs

- **`pierogi run <spec.json>`** — run a **pre-baked** spec. The spec must already
  carry its `closure:` (there is no runtime Nix query); `run` on an unbuilt spec
  fails loud. CLI overrides (`--env`, `--hostname`, `--mount host:…|tmpfs:…`, and
  a trailing `-- cmd args`) overlay onto the loaded spec, but the closure is
  frozen — there is no `--mount store:`.
- **`pierogi resolve <spec.json> --graph <graph>`** — build a spec: traverse
  Nix's `exportReferencesGraph` (supplied as JSON) from the spec's seeds, stamp
  the resulting closure, and print the frozen spec. This is what a Nix builder
  calls; see [`nix/bash-sandbox.nix`](nix/bash-sandbox.nix).

### Findings still worth knowing

- **In-memory spec.** `libcrun_container_load_from_memory` takes the spec as a
  JSON string, so nothing touches disk but crun's own state dir. (youki, the
  original second backend, was dropped — it couldn't RO-remount a `/nix/store`
  rootfs; crun re-supplies the kernel-locked mount flags and handles it.)
- **devices.** Don't bind device nodes via `mounts[]` (EINVAL). Mount a `/dev`
  tmpfs and let crun populate its default device nodes.
- **cgroups.** Work via systemd user-session delegation (rootless); hosts
  without delegation may need `systemd-run --user --scope`.

## Layout

```
src/            # the pierogi crate: resolve, spec, oci translation, run, tty relay
src/runtime/    # crun backend (libcrun via the FFI shim)
csrc/shim.c     # Rust <-> libcrun bridge
build.rs        # compiles + links the shim
nix/            # bash-sandbox.nix — the sealed-rootfs build pipeline
flake.nix .envrc# dev shell: rust toolchain + the harvested libcrun dev tree
docs/           # vision / architecture / execution-modes / spec / control-surface / roadmap
```
