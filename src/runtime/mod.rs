//! Runtime: drive a resolved OCI spec to a running, namespaced process.
//!
//! The production backend is **libcrun** (`crun` submodule, via the FFI shim in
//! `csrc/shim.c`): it loads the spec from an in-memory JSON string — no
//! `config.json` on disk. youki was once a second backend but couldn't
//! RO-remount a `/nix/store` rootfs (its bare `MS_REMOUNT|MS_RDONLY` hits the
//! store's kernel-locked `nosuid`/`nodev` flags); crun re-supplies the flags and
//! handles it, so youki was dropped.
//!
//! A second **`exec`** backend shells out to the `crun` *binary* instead of
//! linking libcrun. It exists purely to price the embedded-libcrun design: it's
//! the same crun build (`$CRUN_DEV/bin/crun`), so the wall-clock delta against
//! the shim is exactly the fork+exec+dynamic-link+config-parse cost. Select it
//! with `PIEROGI_BACKEND=exec`; the default (unset or anything else) is `lib`.
//! Both backends share [`run`]'s signature, so everything upstream (spec build,
//! store scaffold, state dir) is identical — the benchmark isolates the boundary.
//!
//! [`run`] returns the container's exit code. When the spec asks for a terminal,
//! `console_socket` is the path crun connects to and sends the pty master over
//! (see [`crate::tty`]).

use std::path::Path;

use anyhow::Result;
use oci_spec::runtime::Spec;

mod crun;
mod exec;

/// Dispatch to the selected backend. `PIEROGI_BACKEND=exec` picks the crun
/// binary; anything else (the default) picks the in-process libcrun shim.
pub fn run(
    spec: &Spec,
    id: &str,
    bundle: &Path,
    state: &Path,
    console_socket: Option<&Path>,
) -> Result<i32> {
    match std::env::var("PIEROGI_BACKEND").as_deref() {
        Ok("exec") => exec::run(spec, id, bundle, state, console_socket),
        _ => crun::run(spec, id, bundle, state, console_socket),
    }
}
