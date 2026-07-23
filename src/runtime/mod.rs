//! Runtime: drive a resolved OCI spec to a running, namespaced process.
//!
//! One backend, **crun** (libcrun via the FFI shim in `csrc/shim.c`). It loads
//! the spec from an in-memory JSON string — no `config.json` on disk. youki was
//! a second backend but couldn't RO-remount a `/nix/store` rootfs (its bare
//! `MS_REMOUNT|MS_RDONLY` hits the store's kernel-locked `nosuid`/`nodev`
//! flags); crun re-supplies the flags and handles it, so youki was dropped and
//! the backend indirection with it.
//!
//! [`run`] returns the container's exit code. When the spec asks for a terminal,
//! `console_socket` is the path crun connects to and sends the pty master over
//! (see [`crate::tty`]).

mod crun;

pub use crun::run;
