//! Orchestrate a run: obtain a spec → assert built → realize → drive a backend.
//!
//! Two entry points, both spec-in (no ad-hoc `wrap`/exe bridge — that path was
//! rolled back once nix-mode build-time resolution became the way in):
//! * [`run_spec`] — the `run` verb: load a `spec.json`, apply CLI overrides,
//!   launch. The spec must already carry its `closure:` (there is no runtime
//!   query — pierogi is pre-bake only).
//! * [`resolve_cmd`] — the `resolve` verb: load, traverse a supplied `--graph`
//!   (build-time; Nix's `exportReferencesGraph` JSON) from the spec's seeds,
//!   print the built spec. This is the sole path that fills a closure.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::oci::{build_spec, RealizeInputs};
use crate::overrides::Overrides;
use crate::resolve::{resolve, resolve_from_graph};
use crate::spec::Spec;
use crate::tty::ConsoleRelay;

/// `pierogi run [overrides] <spec.json>` — load a spec, overlay any CLI
/// overrides, and launch.
pub fn run_spec(spec_path: PathBuf, overrides: Overrides) -> Result<()> {
    let mut spec = load_spec(&spec_path)?;
    // Overlay CLI overrides (env / hostname / argv / host-tmpfs mounts). None of
    // these touch the closure — it's sealed at build time — so they layer on
    // without a query or a re-resolve.
    if !overrides.is_empty() {
        eprintln!("pierogi: applying CLI overrides to {}", spec_path.display());
        overrides.apply(&mut spec);
    }
    launch(spec)
}

/// `pierogi resolve <spec> --graph <reference-graph>` — build a spec: traverse
/// the `exportReferencesGraph` dump from the spec's seeds, stamp the resulting
/// closure, and print it. The build-time freeze entry (no query).
pub fn resolve_cmd(spec_path: PathBuf, graph_file: PathBuf) -> Result<()> {
    let mut spec = load_spec(&spec_path)?;
    let graph = std::fs::read_to_string(&graph_file)
        .with_context(|| format!("reading {}", graph_file.display()))?;
    resolve_from_graph(&mut spec, &graph)?;
    // Serialize the built spec to stdout (the frozen artifact).
    let json = serde_json::to_string_pretty(&spec).context("serializing built spec")?;
    println!("{json}");
    Ok(())
}

/// The mode-agnostic tail: assert built → realize → drive crun → clean up.
fn launch(spec: Spec) -> Result<()> {
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

    // Nothing but the container state dir touches disk. crun takes the spec as
    // an in-memory JSON string (no config.json), and `root` *is* the rootfs (an
    // absolute store path mounted at `/`, everything baked in — see
    // `oci::build_spec`), so there is no bundle to assemble. crun still wants a
    // bundle *context* path, but with an absolute `root.path` its content is
    // irrelevant — we point it at `work`.
    let id = format!("pierogi-{}", std::process::id());
    let work = runtime_dir().join(&id);
    let state = work.join("state");
    std::fs::create_dir_all(&state).with_context(|| format!("mkdir {}", state.display()))?;

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

    // Drive crun: it loads the spec from memory (no config.json) and blocks
    // until the container exits.
    let result = crate::runtime::run(&oci, &id, &work, &state, console);

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

/// Where to lay out crun's state dir. Just needs to exist and be writable — the
/// rootfs is a `/nix/store` path (not under here), and crun re-supplies the
/// locked mount flags on the RO-root remount, so unlike the youki-era bundle
/// this has no `nosuid`/`nodev` constraint. `$TMPDIR` is a fine default.
fn runtime_dir() -> PathBuf {
    std::env::temp_dir()
}
