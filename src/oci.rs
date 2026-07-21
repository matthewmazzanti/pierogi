//! Translate a **resolved** pierogi [`Spec`] into the in-memory OCI runtime
//! spec the backend drives (`docs/spec.md#realization`).
//!
//! Realization, field by field:
//! * `closure.paths` → one read-only identity bind per path under `/nix/store`.
//! * `mounts` → OCI mounts: `Store` (identity = already covered by the closure;
//!   relocate = one bind at `dest`), `Host` (ro/rw bind), `Tmpfs`. `Overlay` is
//!   deferred within M1.
//! * `env` → `process.env`, verbatim. `exec` → `process.args` (resolved
//!   absolute path as `argv[0]`).
//! * OCI `root.path` is the empty scaffold, not `root` — `root` lives in the
//!   closure like any store path.

use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use oci_spec::runtime::{
    LinuxBuilder, LinuxIdMappingBuilder, LinuxNamespaceBuilder, LinuxNamespaceType,
    Mount as OciMount, MountBuilder, ProcessBuilder, RootBuilder, Spec as OciSpec, SpecBuilder,
    UserBuilder,
};

use crate::spec::{Mount, Spec};

/// Everything the OCI translation needs beyond the resolved spec itself.
pub struct RealizeInputs<'a> {
    /// The resolved spec (`closure` must be `Some`).
    pub spec: &'a Spec,
    /// Absolute in-store executable — becomes `argv[0]` (from `resolve_exec`).
    pub exe: &'a Path,
    /// Rootfs path, relative to the bundle dir (e.g. `"rootfs"`).
    pub rootfs_rel: &'a str,
    /// Host uid/gid that container-root (0) maps to (1:1 as M0).
    pub uid: u32,
    pub gid: u32,
}

pub fn build_spec(input: &RealizeInputs) -> Result<OciSpec> {
    let spec = input.spec;
    let closure = spec
        .closure
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("build_spec called on an unresolved spec"))?;

    let namespaces = [
        LinuxNamespaceType::User,
        LinuxNamespaceType::Mount,
        LinuxNamespaceType::Pid,
        LinuxNamespaceType::Uts,
        LinuxNamespaceType::Ipc,
    ]
    .into_iter()
    .map(|typ| LinuxNamespaceBuilder::default().typ(typ).build())
    .collect::<Result<Vec<_>, _>>()?;

    // Single 1:1 id mapping (container root -> the invoking user). subuid/subgid
    // ranges and a fuller mapping come with hardening (M4).
    let uid_map = LinuxIdMappingBuilder::default()
        .host_id(input.uid)
        .container_id(0u32)
        .size(1u32)
        .build()?;
    let gid_map = LinuxIdMappingBuilder::default()
        .host_id(input.gid)
        .container_id(0u32)
        .size(1u32)
        .build()?;

    let linux = LinuxBuilder::default()
        .namespaces(namespaces)
        .uid_mappings(vec![uid_map])
        .gid_mappings(vec![gid_map])
        .build()?;

    // Engine-essential mounts (runtime surface, not yet spec fields): youki
    // populates /dev's device nodes itself (default_devices + bind in rootless
    // mode), so we only mount the /dev tmpfs and let it.
    let mut mounts = base_mounts()?;

    // A controlling terminal needs a devpts instance in the container's /dev so
    // the runtime can allocate the pty (open /dev/ptmx → grantpt). No `gid=` —
    // only container-gid 0 is mapped in our 1:1 userns, so the usual `gid=5`
    // (tty) would be unmapped and EINVAL the mount.
    if spec.exec.terminal {
        mounts.push(devpts_mount()?);
    }

    // The closure: one ro identity bind per path under /nix/store. Emitted
    // first so any relocated/overlay lower exists before it's referenced.
    for path in &closure.paths {
        let p = path.to_string();
        mounts.push(bind_mount(&p, &p, &["bind", "ro"])?);
    }

    // Additive spec mounts.
    for m in &spec.mounts {
        match m {
            // Identity store mount: its closure (incl. itself) is already bound
            // above — nothing to add.
            Mount::Store { dest: None, .. } => {}
            // Relocated store mount: deps stay identity-bound in the closure;
            // bind the path itself at `dest`.
            Mount::Store {
                source,
                dest: Some(dest),
            } => {
                mounts.push(bind_mount(
                    &dest.to_string_lossy(),
                    &source.to_string(),
                    &["bind", "ro"],
                )?);
            }
            Mount::Host { source, dest, rw } => {
                let opts: &[&str] = if *rw {
                    &["bind", "rw"]
                } else {
                    &["bind", "ro"]
                };
                mounts.push(bind_mount(
                    &dest.to_string_lossy(),
                    &source.to_string_lossy(),
                    opts,
                )?);
            }
            Mount::Tmpfs { dest } => {
                mounts.push(tmpfs_mount(dest)?);
            }
            Mount::Overlay { .. } => {
                bail!("overlay mount realization is deferred within M1 (Store/Host/Tmpfs only)");
            }
        }
    }

    let user = UserBuilder::default().uid(0u32).gid(0u32).build()?;

    // env, verbatim. The engine merges nothing; an absent PATH is just absent.
    let env: Vec<String> = spec.env.iter().map(|(k, v)| format!("{k}={v}")).collect();

    // argv: resolved absolute exe as argv[0], then the declared args.
    let mut argv = vec![input.exe.to_string_lossy().into_owned()];
    argv.extend(spec.exec.args.iter().cloned());

    let process = ProcessBuilder::default()
        .terminal(spec.exec.terminal) // spec-driven; false = inherit host stdio
        .user(user)
        .args(argv)
        .env(env)
        .cwd(PathBuf::from("/"))
        .build()?;

    let root = RootBuilder::default()
        .path(PathBuf::from(input.rootfs_rel))
        .readonly(true)
        .build()?;

    Ok(SpecBuilder::default()
        .version("1.1.0")
        .hostname(spec.hostname.clone())
        .root(root)
        .process(process)
        .mounts(mounts)
        .linux(linux)
        .build()?)
}

fn base_mounts() -> Result<Vec<OciMount>> {
    Ok(vec![
        MountBuilder::default()
            .destination(PathBuf::from("/proc"))
            .source(PathBuf::from("proc"))
            .typ("proc".to_string())
            .build()?,
        MountBuilder::default()
            .destination(PathBuf::from("/dev"))
            .source(PathBuf::from("tmpfs"))
            .typ("tmpfs".to_string())
            .options(vec![
                "nosuid".into(),
                "strictatime".into(),
                "mode=755".into(),
                "size=65536k".into(),
            ])
            .build()?,
    ])
}

fn devpts_mount() -> Result<OciMount> {
    Ok(MountBuilder::default()
        .destination(PathBuf::from("/dev/pts"))
        .source(PathBuf::from("devpts"))
        .typ("devpts".to_string())
        .options(vec![
            "nosuid".into(),
            "noexec".into(),
            "newinstance".into(),
            "ptmxmode=0666".into(),
            "mode=0620".into(),
        ])
        .build()?)
}

fn tmpfs_mount(dest: &Path) -> Result<OciMount> {
    Ok(MountBuilder::default()
        .destination(dest.to_path_buf())
        .source(PathBuf::from("tmpfs"))
        .typ("tmpfs".to_string())
        .options(vec!["nosuid".into(), "nodev".into(), "mode=1777".into()])
        .build()?)
}

fn bind_mount(dest: &str, source: &str, options: &[&str]) -> Result<OciMount> {
    Ok(MountBuilder::default()
        .destination(PathBuf::from(dest))
        .source(PathBuf::from(source))
        .typ("bind".to_string())
        .options(options.iter().map(|s| s.to_string()).collect::<Vec<_>>())
        .build()?)
}
