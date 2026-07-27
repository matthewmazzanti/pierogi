//! crun backend: run a resolved OCI spec by exec'ing the `crun` binary.
//!
//! pierogi links nothing from crun — it materializes the spec as a `config.json`
//! and drives the `crun` CLI as a subprocess. (An earlier design linked
//! `libcrun` in-process via an FFI shim to keep the spec in memory; a head-to-head
//! showed that bought only a few milliseconds — below the noise of the ~15-20ms
//! namespace/mount setup — so the shim, the harvested libcrun dev tree, and the
//! `--enable-shared` build override were dropped for the plain binary. See
//! `docs/startup-performance.md`.)
//!
//! The crun binary reads its config off disk, so we write the OCI JSON to
//! `<bundle>/config.json` and point crun at it with an **absolute** `-f`: crun
//! resolves a relative `-f` against the invocation cwd *before* it chdirs into
//! the bundle, so only an absolute path is safe. `-b` is the already-created
//! `bundle` dir (its contents are irrelevant — `root.path` is absolute — but crun
//! still requires a bundle to chdir into). `run` (detach=false) blocks until the
//! container exits and propagates its exit status as crun's own.
//!
//! stdio is inherited so the container's output and crun's own logging land on
//! the caller's terminal.

use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context, Result};
use oci_spec::runtime::Spec;

/// Absolute path to the `crun` binary, pinned at build time by Nix
/// (`CRUN_BIN=${pkgs.crun}/bin/crun`). Baking the store path also makes crun a
/// runtime dependency via Nix's reference scan — no PATH lookup, no drift.
const CRUN: &str = env!("CRUN_BIN");

pub fn run(
    spec: &Spec,
    id: &str,
    bundle: &Path,
    state: &Path,
    console_socket: Option<&Path>,
) -> Result<i32> {
    // crun reads config off disk: materialize the spec as the bundle's
    // config.json.
    let json = serde_json::to_string(spec).context("serializing spec to JSON")?;
    let config = bundle.join("config.json");
    std::fs::write(&config, &json).with_context(|| format!("writing {}", config.display()))?;

    let mut cmd = Command::new(CRUN);
    // Per-run state root, this bundle, this id, and an absolute `-f` (a relative
    // one would resolve pre-chdir, not against the bundle).
    cmd.arg("--root")
        .arg(state)
        .arg("run")
        .arg("-b")
        .arg(bundle)
        .arg("-f")
        .arg(&config);
    // A terminal spec: crun connects to this socket and sends the pty master,
    // which our `tty` relay (the listener at the other end) pumps against the
    // caller's stdio.
    if let Some(sock) = console_socket {
        cmd.arg("--console-socket").arg(sock);
    }
    cmd.arg(id);

    let status = cmd.status().with_context(|| format!("spawning {CRUN}"))?;

    // crun's exit code is the container's (detach=false propagates it); we can't
    // distinguish a crun failure from the container's own non-zero exit, but its
    // inherited stderr says which. Signal death has no code.
    match status.code() {
        Some(code) => Ok(code),
        None => bail!("crun terminated by signal: {status}"),
    }
}
