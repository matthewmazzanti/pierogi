//! Orchestrate one spike run: closure -> bundle -> drive youki -> wait.

use std::path::{Path, PathBuf};

use anyhow::{ensure, Context, Result};

use crate::closure::{closure_of, resolve_store_exe, store_root};
use crate::spec::{build_spec, SpecInputs};

/// Run `argv` in a namespaced, read-only sandbox built from its Nix closure.
pub fn run(argv: Vec<String>) -> Result<()> {
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

    // NOTE(spike): youki's public builder reads config.json from the bundle, so
    // we serialize the in-memory spec here. Removing this round-trip (in-memory
    // injection) is a core M0 finding — see lib.rs / docs/roadmap.md.
    spec.save(bundle.join("config.json"))
        .context("writing config.json")?;

    // 3. Drive youki in-process: create -> start -> wait.
    let result = drive(&id, &bundle, &state);

    // 4. Best-effort cleanup.
    let _ = std::fs::remove_dir_all(&work);
    result
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

/// Build (create) + start the container, then block until it exits.
///
/// NOTE(spike): the youki `libcontainer` surface has churned across releases.
/// If this doesn't compile against the pinned version, the likely culprits are
/// the executor (`libcontainer::workload::default`) and the builder method
/// names — check the crate docs and adjust. The *shape* (new -> root path ->
/// as_init(bundle) -> build -> start -> wait on pid) is stable.
fn drive(id: &str, bundle: &Path, state: &Path) -> Result<()> {
    use libcontainer::container::builder::ContainerBuilder;
    use libcontainer::syscall::syscall::SyscallType;
    use libcontainer::workload::default::DefaultExecutor;

    let mut container = ContainerBuilder::new(id.to_string(), SyscallType::default())
        .with_executor(DefaultExecutor {})
        .with_root_path(state.to_path_buf())
        .context("setting container state path")?
        .as_init(bundle.to_path_buf())
        .with_systemd(false)
        .with_detach(false)
        .build()
        .context("creating container (namespaces, rootfs, mounts)")?;

    container.start().context("starting container")?;

    let pid = container
        .pid()
        .context("container reported no pid after start")?
        .as_raw();

    let mut status: libc::c_int = 0;
    let rc = unsafe { libc::waitpid(pid, &mut status, 0) };
    if rc < 0 {
        return Err(std::io::Error::last_os_error()).context("waitpid on container init");
    }

    // Clean up container state (ignore errors — process already exited).
    let _ = container.delete(true);

    let code = if libc::WIFEXITED(status) {
        libc::WEXITSTATUS(status)
    } else {
        0
    };
    eprintln!("pierogi: shell exited ({code})");
    Ok(())
}
