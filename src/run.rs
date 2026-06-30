//! Orchestrate one spike run: closure -> bundle -> drive a backend -> exit.

use std::path::PathBuf;

use anyhow::{ensure, Context, Result};

use crate::closure::{closure_of, resolve_store_exe, store_root};
use crate::runtime::Backend;
use crate::spec::{build_spec, SpecInputs};

/// Run `argv` in a namespaced, read-only sandbox built from its Nix closure,
/// driven by the chosen runtime backend.
pub fn run(argv: Vec<String>, backend: Backend) -> Result<()> {
    ensure!(!argv.is_empty(), "no command given");

    // 1. Resolve the target into the store and compute its closure.
    let exe = resolve_store_exe(&argv[0])?;
    let root = store_root(&exe)?;
    let closure = closure_of(&root)?;
    let bin_dir = exe
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| "/".to_string());

    // argv[0] becomes the absolute in-store path so the runtime can exec it.
    let mut in_argv = argv;
    in_argv[0] = exe.to_string_lossy().into_owned();

    eprintln!(
        "pierogi: {} -> closure of {} paths; assembling read-only rootfs",
        root.display(),
        closure.len()
    );

    let uid = unsafe { libc::getuid() };
    let gid = unsafe { libc::getgid() };

    // 2. Lay out a bundle (config.json + empty rootfs with mountpoints).
    let id = format!("pierogi-{}", std::process::id());
    let work = runtime_dir().join(&id);
    let bundle = work.join("bundle");
    let rootfs = bundle.join("rootfs");
    let state = work.join("state");
    std::fs::create_dir_all(&state).with_context(|| format!("mkdir {}", state.display()))?;
    for sub in ["proc", "dev", "tmp", "nix/store"] {
        std::fs::create_dir_all(rootfs.join(sub))?;
    }

    let spec = build_spec(&SpecInputs {
        argv: &in_argv,
        closure: &closure,
        rootfs_rel: "rootfs",
        bin_dir: &bin_dir,
        uid,
        gid,
    })?;

    // 3. Drive the chosen backend in-process. youki serializes config.json to
    // the bundle; crun loads the spec from memory. Both block until exit.
    let result = backend.run(&spec, &id, &bundle, &state);

    // 4. Best-effort cleanup.
    let _ = std::fs::remove_dir_all(&work);

    let code = result?;
    eprintln!("pierogi: shell exited ({code})");
    Ok(())
}

/// Where to lay out the bundle/state.
///
/// NOTE(spike finding): *not* `XDG_RUNTIME_DIR`. youki remounts the read-only
/// root as a plain `MS_RDONLY|MS_REMOUNT|MS_BIND` without re-supplying the
/// flags the kernel locks on a userns's mounts. `XDG_RUNTIME_DIR` is typically
/// a `nosuid,nodev` tmpfs, so those locked flags make the RO-root remount fail
/// with EPERM. A bundle on a filesystem without locked `nosuid`/`nodev` (e.g.
/// `/tmp` here) lets the remount succeed. Proper fix: carry locked flags
/// (upstream youki) or build the rootfs mount ourselves.
fn runtime_dir() -> PathBuf {
    std::env::temp_dir()
}
