//! Orchestrate a run: obtain a spec → assert built → realize → drive a backend.
//!
//! Two entry points, both spec-in (no ad-hoc `wrap`/exe bridge — that path was
//! rolled back once nix-mode build-time resolution became the way in):
//! * [`run_spec`] — the `run` verb: load a `spec.json`, apply CLI overrides,
//!   launch. The spec must already carry its `closure:` (there is no runtime
//!   query — pierogi is pre-bake only).
//! * [`resolve_cmd`] — the `resolve` verb: load, stamp a **supplied**
//!   `--closure` (build-time; `closureInfo`'s `store-paths`), print the built
//!   spec. This is the sole path that fills a closure.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::oci::{build_spec, RealizeInputs};
use crate::overrides::Overrides;
use crate::resolve::{resolve, resolve_supplied};
use crate::runtime::Backend;
use crate::spec::Spec;
use crate::tty::ConsoleRelay;

/// `pierogi run [overrides] <spec.json>` — load a spec, overlay any CLI
/// overrides, resolve, and launch.
pub fn run_spec(spec_path: PathBuf, backend: Backend, overrides: Overrides) -> Result<()> {
    let mut spec = load_spec(&spec_path)?;
    // Overlay CLI overrides (env / hostname / argv / host-tmpfs mounts). None of
    // these touch the closure — it's sealed at build time — so they layer on
    // without a query or a re-resolve.
    if !overrides.is_empty() {
        eprintln!("pierogi: applying CLI overrides to {}", spec_path.display());
        overrides.apply(&mut spec);
    }
    launch(spec, backend)
}

/// `pierogi resolve <spec> --closure <store-paths>` — build a spec: stamp the
/// supplied closure (no query) and print it. The build-time freeze entry.
pub fn resolve_cmd(spec_path: PathBuf, closure_file: PathBuf) -> Result<()> {
    let mut spec = load_spec(&spec_path)?;
    let supplied = read_store_paths(&closure_file)?;
    resolve_supplied(&mut spec, supplied)?;
    // Serialize the built spec to stdout (the frozen artifact).
    let json = serde_json::to_string_pretty(&spec).context("serializing built spec")?;
    println!("{json}");
    Ok(())
}

/// The mode-agnostic tail: assert built → realize → drive a backend → clean up.
fn launch(spec: Spec, backend: Backend) -> Result<()> {
    // Assert the spec is pre-baked (carries `closure:`). There is no runtime
    // query. The exec target is resolved by the runtime against the container's
    // PATH at launch (in-sandbox), not here.
    resolve(&spec)?;

    let closure = spec.closure.as_ref().expect("asserted above");
    eprintln!(
        "pierogi: sealed rootfs {} + {} closure binds",
        spec.root,
        closure.resolved.len()
    );

    let uid = unsafe { libc::getuid() };
    let gid = unsafe { libc::getgid() };

    // Lay out a bundle for config.json + runtime state. There is no rootfs to
    // scaffold: `root` *is* the rootfs (mounted at `/`), and every mountpoint
    // the closure and spec mounts need is baked into it. See `oci::build_spec`.
    let id = format!("pierogi-{}", std::process::id());
    let work = runtime_dir().join(&id);
    let bundle = work.join("bundle");
    let state = work.join("state");
    std::fs::create_dir_all(&state).with_context(|| format!("mkdir {}", state.display()))?;
    std::fs::create_dir_all(&bundle).with_context(|| format!("mkdir {}", bundle.display()))?;

    let oci = build_spec(&RealizeInputs {
        spec: &spec,
        uid,
        gid,
    })?;

    // A `terminal:true` spec needs a controlling tty: bind a console socket and
    // start the relay *before* the backend runs, so the runtime can connect and
    // hand us the pty master as it starts the container.
    let relay = if spec.exec.terminal {
        Some(ConsoleRelay::bind(work.join("console.sock"))?)
    } else {
        None
    };
    let console = relay.as_ref().map(ConsoleRelay::socket_path);

    // Drive the backend. youki serializes config.json to the bundle; crun loads
    // the spec from memory. Both block until exit.
    let result = backend.run(&oci, &id, &bundle, &state, console);

    // Restore the terminal (join the relay) before anything else prints.
    if let Some(relay) = relay {
        relay.finish();
    }

    // Best-effort cleanup.
    let _ = std::fs::remove_dir_all(&work);

    let code = result?;
    eprintln!("pierogi: process exited ({code})");
    Ok(())
}

fn load_spec(path: &Path) -> Result<Spec> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))
}

/// Read a closure list from a file (one store path per line — the shape of
/// `closureInfo`'s `store-paths`). Validation happens in `resolve_supplied`.
fn read_store_paths(path: &Path) -> Result<Vec<PathBuf>> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    Ok(text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(PathBuf::from)
        .collect())
}

/// Where to lay out the bundle/state.
///
/// NOTE(spike finding): *not* `XDG_RUNTIME_DIR`. youki remounts the read-only
/// root as a plain `MS_RDONLY|MS_REMOUNT|MS_BIND` without re-supplying the
/// flags the kernel locks on a userns's mounts. `XDG_RUNTIME_DIR` is typically
/// a `nosuid,nodev` tmpfs, so those locked flags make the RO-root remount fail
/// with EPERM. A bundle on a filesystem without locked `nosuid`/`nodev` (e.g.
/// `/tmp` here) lets the remount succeed.
fn runtime_dir() -> PathBuf {
    std::env::temp_dir()
}
