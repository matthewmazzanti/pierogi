# pierogi

A fast, Nix-native sandbox runtime — between `bubblewrap` and `docker`. See
[`docs/`](docs/) for the [vision](docs/vision.md), [architecture](docs/architecture.md),
[execution modes](docs/execution-modes.md), and [roadmap](docs/roadmap.md).

## Status: M0 — shell-run spike ✅ working

The **M0 spike** is proven: we embed the youki `libcontainer` OCI runtime
*in-process* and drive it from Rust to namespace + `exec` a real shell, over a
**read-only rootfs assembled from the target's Nix closure**. Verified:
`uid=0 pid=1` (userns + pid ns), read-only root, writable `/tmp`,
`/nix/store` containing exactly the 5-path bash closure, and no host
`/etc`/`/home` visible. Deliberately crude otherwise — synthetic `/etc` and
strict defaults are M1.

### Build & run

```sh
direnv allow        # or: nix develop
cargo run -- run -- bash
```

That:

1. resolves `bash` to its `/nix/store` path and computes its closure,
2. builds a read-only rootfs from that closure (one bind mount per store path),
3. drives youki in-process to launch `bash` in user/mount/pid/uts/ipc
   namespaces.

Inside, you get a shell whose entire filesystem is bash's closure under
`/nix/store` plus tmpfs `/tmp`, `/dev`, and `proc`. Only bash builtins and the
bash binary are available (the closure has no coreutils) — that's expected; the
point is *a namespaced shell launched from Rust*.

### Findings surfaced by the spike

- **config.json on disk.** youki's public API reads a bundle's `config.json`, so
  we serialize the in-memory spec for now. The end goal is no on-disk spec;
  confirming the in-memory path is the central open M0 question.
- **read-only root needs the bundle off a `nosuid,nodev` mount.** youki remounts
  `/` RO without re-supplying a userns's locked mount flags, so a bundle on
  `XDG_RUNTIME_DIR` (a `nosuid,nodev` tmpfs) fails with EPERM. We lay the bundle
  under `$TMPDIR` instead (`run::runtime_dir`).
- **devices.** Let youki's `default_devices()` populate `/dev`; binding device
  nodes via `mounts[]` yields EINVAL.
- **cgroups.** Worked here via systemd user-session delegation; hosts without
  delegation may need `systemd-run --user --scope`.
- **libcontainer API churn.** If it stops compiling against the pinned crate,
  check the executor/builder names in `crates/pierogi-engine/src/run.rs` (the
  *shape* — `new → root path → as_init(bundle) → build → start → waitpid` — is
  stable; names drift).

## Layout

```
crates/pierogi-engine/   # closure calc, spec build, runtime driving
crates/pierogi-cli/      # `pierogi` binary (clap)
docs/                    # vision / architecture / execution-modes / roadmap
flake.nix .envrc         # dev shell: rust toolchain + libseccomp + nix
```
