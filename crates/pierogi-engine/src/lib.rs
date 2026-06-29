//! Pierogi engine — **M0 shell-run spike**.
//!
//! Goal of this milestone: prove we can embed an OCI runtime (youki
//! `libcontainer`) *in-process* and drive it from Rust to namespace + `exec` a
//! real shell, over a **read-only rootfs assembled from the Nix closure** of the
//! target binary. Everything here is deliberately crude — no synthetic `/etc`,
//! no strict defaults, no nice spec format. See `docs/roadmap.md` (M0).
//!
//! Status: **working** — launches a namespaced (user/mount/pid/uts/ipc) shell
//! over a read-only, closure-backed rootfs. Findings from getting here:
//!
//! * **config.json on disk.** The eventual design builds the OCI spec in memory
//!   and never serializes a bundle. youki's *public* API still reads
//!   `config.json` from a bundle dir, so for now we write the in-memory spec
//!   out. Whether we can inject it in-memory (lower-level API / upstream patch)
//!   is the central embedding question — still open.
//! * **devices.** Don't put device nodes in `mounts[]` (bind of a char device
//!   via the generic mount path → EINVAL). youki's `default_devices()` +
//!   `create_devices(.., bind=true)` populates `/dev` itself, bind-mounting host
//!   nodes in rootless mode. We just mount the `/dev` tmpfs and let it.
//! * **read-only root needs the bundle off a `nosuid,nodev` mount.** youki
//!   remounts `/` RO as a bare `MS_RDONLY|MS_REMOUNT|MS_BIND` without
//!   re-supplying the flags the kernel locks on a userns's mounts. So a bundle
//!   on `XDG_RUNTIME_DIR` (a `nosuid,nodev` tmpfs) fails the RO remount with
//!   EPERM; we lay the bundle under `$TMPDIR` instead. See `run::runtime_dir`.
//! * **cgroups.** Worked here via systemd user-session delegation (rootless,
//!   `user.slice`). On hosts without delegation, expect cgroup setup to fail.

mod closure;
mod run;
mod spec;

pub use run::run;
