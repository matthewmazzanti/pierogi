//! Orchestrate a run: obtain a spec → assert built → realize → drive a backend.
//!
//! Two entry points, both spec-in (no ad-hoc `wrap`/exe bridge — that path was
//! rolled back once nix-mode build-time resolution became the way in):
//! * [`run_spec`] — the `run` verb: load a `spec.json`, apply CLI overrides,
//!   launch. The spec must already carry its built `closure.resolved` (there is
//!   no runtime query — pierogi is pre-bake only).
//! * [`resolve_cmd`] — the `resolve` verb: load, traverse a supplied `--graph`
//!   (build-time; Nix's `exportReferencesGraph` JSON) from the spec's
//!   `closure.seeds`, print the built spec. This is the sole path that stamps
//!   `closure.resolved`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::oci::{build_spec, RealizeInputs};
use crate::overrides::Overrides;
use crate::resolve::{resolve, resolve_from_graph};
use crate::spec::{Spec, StorePath};
use crate::tty::ConsoleRelay;

/// `pierogi run [overrides] <spec.json>` — load a spec, overlay any CLI
/// overrides, and launch.
pub fn run_spec(spec_path: PathBuf, overrides: Overrides) -> Result<()> {
    let mut spec = load_spec(&spec_path)?;
    // Overlay CLI overrides (env / hostname / args / host-tmpfs mounts). None of
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
fn launch(mut spec: Spec) -> Result<()> {
    // Assert the spec is pre-baked (carries `closure.resolved`). There is no
    // runtime query. The exec target is resolved by the runtime against the
    // container's PATH at launch (in-sandbox), not here.
    resolve(&spec)?;

    // Own the resolved set so `spec.mounts` stays mutable below (the D-Bus proxy
    // binds are appended before realization).
    let resolved = spec.closure.resolved.clone().expect("asserted above");
    eprintln!(
        "pierogi: sealed rootfs {} + {} closure binds",
        spec.root.path,
        resolved.len()
    );

    let uid = unsafe { libc::getuid() };
    let gid = unsafe { libc::getgid() };

    // `root` *is* the rootfs (an absolute store path mounted at `/` — see
    // `oci::build_spec`), so there is no bundle to assemble. What touches disk is
    // a per-run `work` dir: crun's own `state/`, the `/nix/store` mountpoint stubs
    // the engine binds read-only (`scaffold_store`), and the `config.json` the
    // crun backend writes into it. crun still wants a bundle *context* path, but
    // with an absolute `root.path` its content is irrelevant — we point it at
    // `work`.
    let id = format!("pierogi-{}", std::process::id());
    let work = runtime_dir().join(&id);
    let state = work.join("state");
    std::fs::create_dir_all(&state).with_context(|| format!("mkdir {}", state.display()))?;

    // The `/nix/store` mountpoint tree: one stub per resolved path, bound RO at
    // `/nix/store` so the store topology is provided here rather than baked into
    // the sealed root (see `oci::build_spec`).
    let store = work.join("store");
    scaffold_store(&store, &resolved)?;

    // Spawn the D-Bus filter proxies (if any) and route their sockets in. Started
    // *before* realization so their binds join `spec.mounts`, and before crun so
    // each socket exists when the container's bind resolves. A spec with no `dbus`
    // makes this a no-op; the proxies are supervised for the run and torn down
    // after crun returns (or on any early `?`-return, via `DbusProxies`' Drop).
    let proxies = crate::dbus::DbusProxies::start(&spec.dbus, uid, &work)?;
    spec.mounts.extend(proxies.mounts());

    let oci = build_spec(&RealizeInputs {
        spec: &spec,
        uid,
        gid,
        store_root: &store,
    })?;

    // A `terminal:true` spec needs a controlling tty: bind a console socket and
    // start the relay *before* the backend runs, so the runtime can connect and
    // hand us the pty master as it starts the container.
    let relay = if spec.process.terminal {
        Some(ConsoleRelay::bind(work.join("console.sock"))?)
    } else {
        None
    };
    let console = relay.as_ref().map(ConsoleRelay::socket_path);

    // Drive crun: the backend writes the spec to `work/config.json`, execs the
    // crun binary, and blocks until the container exits.
    let result = crate::crun::run(&oci, &id, &work, &state, console);

    // Restore the terminal (join the relay) before anything else prints.
    if let Some(relay) = relay {
        relay.finish();
    }

    // Tear the proxies down (kill + reap) before removing the work dir their
    // sockets live in.
    proxies.finish();

    // Best-effort cleanup.
    let _ = std::fs::remove_dir_all(&work);

    let code = result?;
    eprintln!("pierogi: process exited ({code})");
    Ok(())
}

/// Pre-create the `/nix/store` mountpoint tree the engine binds read-only: one
/// stub per resolved path under `store_root`, shaped to match its source — a
/// file store path (e.g. a `writeText` `/etc` file) needs a file mountpoint, a
/// dir needs a dir, because a bind's target kind must match its source. The
/// closure identity-binds onto these stubs, so `/nix/store` is provided at
/// runtime yet stays read-only: nothing is created at mount time because the
/// stubs already exist here. (This is the file-vs-dir logic the pre-sealing
/// `scaffold_mountpoints` ran, back to the engine but for the store tree only.)
fn scaffold_store(store_root: &Path, resolved: &[StorePath]) -> Result<()> {
    std::fs::create_dir_all(store_root)
        .with_context(|| format!("mkdir {}", store_root.display()))?;
    for path in resolved {
        let p = path.to_string();
        let src = Path::new(&p);
        let name = src
            .file_name()
            .ok_or_else(|| anyhow::anyhow!("store path {p:?} has no basename"))?;
        let stub = store_root.join(name);
        if src.is_dir() {
            std::fs::create_dir(&stub).with_context(|| format!("mkdir {}", stub.display()))?;
        } else {
            std::fs::File::create(&stub).with_context(|| format!("touch {}", stub.display()))?;
        }
    }
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
