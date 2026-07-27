//! exec backend: shell out to the `crun` *binary* instead of linking libcrun.
//!
//! This exists as the other half of a head-to-head against [`super::crun`]: same
//! runtime, same version (`$CRUN_DEV/bin/crun` is the very store path whose
//! `libcrun.so` the shim links — crun 1.27.1, same build), so the only thing
//! that differs between the two paths is the process boundary. Selected at
//! runtime by `PIEROGI_BACKEND=exec` (see [`super`]); the delta measures the
//! cost of fork+exec+dynamic-link+config-parse versus the in-process call.
//!
//! Unlike the libcrun path, the crun binary has no `load_from_memory` — it reads
//! the spec off disk. So this writes the OCI JSON to `<bundle>/config.json` (the
//! one materialization dropping libcrun costs) and points crun at it with an
//! **absolute** `-f`: crun resolves a relative `-f` against the invocation cwd
//! *before* it chdirs into the bundle, so only an absolute path is safe. `-b` is
//! the already-created `bundle` dir (its contents are irrelevant — `root.path`
//! is absolute — but crun still requires a bundle to chdir into).
//!
//! stdio is inherited so the container's output and crun's own logging land on
//! the caller's terminal, matching the shim's `libcrun_init_logging` → stderr.

use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context, Result};
use oci_spec::runtime::Spec;

/// The crun dev tree we link against; `$CRUN_DEV/bin/crun` is the same-build
/// binary. Pinned at compile time so the exec path can never drift onto a
/// different crun than the one the libcrun shim is measured against.
const CRUN_DEV: &str = env!("CRUN_DEV");

pub fn run(
    spec: &Spec,
    id: &str,
    bundle: &Path,
    state: &Path,
    console_socket: Option<&Path>,
) -> Result<i32> {
    // No `load_from_memory` for the binary: materialize the spec as the bundle's
    // config.json (the same bytes the shim hands libcrun in memory).
    let json = serde_json::to_string(spec).context("serializing spec to JSON")?;
    let config = bundle.join("config.json");
    std::fs::write(&config, &json).with_context(|| format!("writing {}", config.display()))?;

    let crun = format!("{CRUN_DEV}/bin/crun");
    let mut cmd = Command::new(&crun);
    // Mirror the shim's context: per-run state root, this bundle, this id, and an
    // absolute `-f` (relative would resolve pre-chdir, not against the bundle).
    cmd.arg("--root")
        .arg(state)
        .arg("run")
        .arg("-b")
        .arg(bundle)
        .arg("-f")
        .arg(&config);
    // A terminal spec: crun connects to this socket and sends the pty master,
    // exactly as in the libcrun path (our `tty` relay is on the other end).
    if let Some(sock) = console_socket {
        cmd.arg("--console-socket").arg(sock);
    }
    cmd.arg(id);

    // `crun run` (detach=false) blocks until the container exits and propagates
    // its exit status as crun's own — so the child's code *is* the exit code.
    let status = cmd
        .status()
        .with_context(|| format!("spawning {crun}"))?;

    // Return the code as-is to match the libcrun path, which surfaces a non-zero
    // *container* exit as `Ok(code)` too (unlike the shim we can't tell a crun
    // failure from the container's own code — its inherited stderr says which).
    match status.code() {
        Some(code) => Ok(code),
        None => bail!("crun terminated by signal: {status}"),
    }
}
