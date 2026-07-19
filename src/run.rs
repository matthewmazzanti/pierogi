//! Orchestrate a run: obtain a spec → resolve → realize → drive a backend.
//!
//! Three entry points:
//! * [`run_spec`] — the M1 verb: load a `spec.json`, resolve (no query if it
//!   already carries a `closure:`), launch.
//! * [`resolve_cmd`] — the M1 `resolve` verb: load, resolve (live *or* a
//!   supplied `--closure`), print the resolved spec.
//! * [`run_argv`] — a **temporary** exe-derived path preserving M0's `run --
//!   bash` behavior for `bench.sh` and quick smoke tests. Slated for re-eval
//!   when `wrap` lands (M4); it synthesizes a spec and feeds the same tail.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{ensure, Context, Result};

use crate::closure::{resolve_store_exe, store_root};
use crate::oci::{build_spec, RealizeInputs};
use crate::resolve::{resolve, resolve_exec, resolve_supplied};
use crate::runtime::Backend;
use crate::spec::{Exec, Mount, Spec, StorePath};

/// `pierogi run <spec.json>` — load a spec, resolve it, and launch.
pub fn run_spec(spec_path: PathBuf, backend: Backend) -> Result<()> {
    let spec = load_spec(&spec_path)?;
    launch(spec, backend)
}

/// `pierogi resolve <spec> [--closure <store-paths>]` — freeze a spec: fill
/// `closure:` (live query, or the supplied build-time list) and print it.
pub fn resolve_cmd(spec_path: PathBuf, closure_file: Option<PathBuf>) -> Result<()> {
    let mut spec = load_spec(&spec_path)?;
    match closure_file {
        Some(file) => {
            let supplied = read_store_paths(&file)?;
            resolve_supplied(&mut spec, supplied)?;
        }
        None => resolve(&mut spec)?,
    }
    // Serialize the resolved spec to stdout (the frozen artifact).
    let json = serde_json::to_string_pretty(&spec).context("serializing resolved spec")?;
    println!("{json}");
    Ok(())
}

/// **Temporary** M0-compatible path: derive the closure from `argv[0]`.
pub fn run_argv(argv: Vec<String>, backend: Backend) -> Result<()> {
    ensure!(!argv.is_empty(), "no command given");

    let exe = resolve_store_exe(&argv[0])?;
    let root = StorePath::new(store_root(&exe)?)?;
    let bin_dir = exe
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| "/".to_string());

    let env = BTreeMap::from([
        ("PATH".to_string(), bin_dir),
        ("HOME".to_string(), "/".to_string()),
        ("TERM".to_string(), "xterm-256color".to_string()),
        ("PS1".to_string(), "pierogi:\\w# ".to_string()),
    ]);

    let spec = Spec {
        exec: Exec {
            executable: argv[0].clone(),
            args: argv[1..].to_vec(),
        },
        root,
        mounts: vec![Mount::Tmpfs {
            dest: PathBuf::from("/tmp"),
        }],
        env,
        closure: None,
    };
    launch(spec, backend)
}

/// The mode-agnostic tail: resolve → realize → drive a backend → clean up.
fn launch(mut spec: Spec, backend: Backend) -> Result<()> {
    // Resolve: fill `closure:` (live query) or short-circuit if already sealed.
    resolve(&mut spec)?;
    let exe = resolve_exec(&spec)?;

    let closure = spec.closure.as_ref().expect("resolved above");
    eprintln!(
        "pierogi: root {} -> closure of {} paths; assembling read-only rootfs",
        spec.root,
        closure.paths.len()
    );

    let uid = unsafe { libc::getuid() };
    let gid = unsafe { libc::getgid() };

    // Lay out a bundle (config.json + empty rootfs with mountpoints).
    let id = format!("pierogi-{}", std::process::id());
    let work = runtime_dir().join(&id);
    let bundle = work.join("bundle");
    let rootfs = bundle.join("rootfs");
    let state = work.join("state");
    std::fs::create_dir_all(&state).with_context(|| format!("mkdir {}", state.display()))?;
    scaffold_mountpoints(&rootfs, &spec)?;

    let oci = build_spec(&RealizeInputs {
        spec: &spec,
        exe: &exe,
        rootfs_rel: "rootfs",
        uid,
        gid,
    })?;

    // Drive the backend. youki serializes config.json to the bundle; crun loads
    // the spec from memory. Both block until exit.
    let result = backend.run(&oci, &id, &bundle, &state);

    // Best-effort cleanup.
    let _ = std::fs::remove_dir_all(&work);

    let code = result?;
    eprintln!("pierogi: process exited ({code})");
    Ok(())
}

/// Pre-create mountpoints in the empty (read-only) rootfs. The runtime creates
/// the many per-closure-path targets under `/nix/store` itself (as in M0); we
/// create the skeleton and each additive spec-mount dest.
fn scaffold_mountpoints(rootfs: &Path, spec: &Spec) -> Result<()> {
    for sub in ["proc", "dev", "nix/store"] {
        std::fs::create_dir_all(rootfs.join(sub))?;
    }
    for m in &spec.mounts {
        match m {
            Mount::Store { dest: None, .. } => {} // covered by the closure
            Mount::Store {
                dest: Some(dest), ..
            }
            | Mount::Tmpfs { dest }
            | Mount::Overlay { dest, .. } => {
                mkdir_under(rootfs, dest)?;
            }
            Mount::Host { source, dest, .. } => {
                // A file source needs a file mountpoint; a dir needs a dir.
                if source.is_file() {
                    if let Some(parent) = rooted(rootfs, dest).parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    let _ = std::fs::File::create(rooted(rootfs, dest));
                } else {
                    mkdir_under(rootfs, dest)?;
                }
            }
        }
    }
    Ok(())
}

fn rooted(rootfs: &Path, dest: &Path) -> PathBuf {
    // dest is absolute (`/tmp`); join it under rootfs by stripping the leading `/`.
    rootfs.join(dest.strip_prefix("/").unwrap_or(dest))
}

fn mkdir_under(rootfs: &Path, dest: &Path) -> Result<()> {
    let p = rooted(rootfs, dest);
    std::fs::create_dir_all(&p).with_context(|| format!("mkdir {}", p.display()))
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
