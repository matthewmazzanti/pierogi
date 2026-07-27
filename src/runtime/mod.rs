//! Runtime: drive a resolved OCI spec to a running, namespaced process.
//!
//! One backend, **crun**, run as a subprocess (see [`crun`]): pierogi writes the
//! OCI spec to a `config.json` and execs the `crun` binary. youki was once
//! evaluated but couldn't RO-remount a `/nix/store` rootfs (its bare
//! `MS_REMOUNT|MS_RDONLY` hits the store's kernel-locked `nosuid`/`nodev` flags);
//! crun re-supplies the flags and handles it. An earlier libcrun-in-process FFI
//! path was dropped once benchmarking showed it saved only a few milliseconds
//! over the plain binary (`docs/startup-performance.md`).
//!
//! [`run`] returns the container's exit code. When the spec asks for a terminal,
//! `console_socket` is the path crun connects to and sends the pty master over
//! (see [`crate::tty`]).

mod crun;

pub use crun::run;
