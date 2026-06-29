//! Build the in-memory OCI runtime spec for the spike.
//!
//! This is the thinnest possible `pierogi-spec -> oci-spec` translation: just
//! enough to launch a shell in user/mount/pid/uts/ipc namespaces over a
//! read-only, closure-backed rootfs. The "real" translation grows here in M1.

use std::path::PathBuf;

use anyhow::Result;
use oci_spec::runtime::{
    LinuxBuilder, LinuxIdMappingBuilder, LinuxNamespaceBuilder, LinuxNamespaceType, Mount,
    MountBuilder, ProcessBuilder, RootBuilder, Spec, SpecBuilder, UserBuilder,
};

/// Inputs the spike needs to assemble a spec.
pub struct SpecInputs<'a> {
    /// In-container argv; `argv[0]` is the absolute store path of the binary.
    pub argv: &'a [String],
    /// Store paths bound read-only under `/nix/store` (the target's closure).
    pub closure: &'a [PathBuf],
    /// Rootfs path, relative to the bundle dir (e.g. `"rootfs"`).
    pub rootfs_rel: &'a str,
    /// `PATH` entry inside the container (the target binary's directory).
    pub bin_dir: &'a str,
    /// Host uid/gid that container-root (0) maps to.
    pub uid: u32,
    pub gid: u32,
}

pub fn build_spec(input: &SpecInputs) -> Result<Spec> {
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
    // ranges and a fuller mapping come in M1.
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

    // youki populates /dev's device nodes itself (default_devices + bind in
    // rootless mode), so we only mount the /dev tmpfs and bind the closure.
    let mut mounts = base_mounts()?;
    for path in input.closure {
        let p = path.to_string_lossy();
        mounts.push(bind_mount(&p, &p, &["bind", "ro"])?);
    }

    let user = UserBuilder::default().uid(0u32).gid(0u32).build()?;
    let env = vec![
        format!("PATH={}", input.bin_dir),
        "HOME=/".to_string(),
        "TERM=xterm-256color".to_string(),
        // Visible marker that you're inside the sandbox.
        "PS1=pierogi:\\w# ".to_string(),
    ];
    let process = ProcessBuilder::default()
        .terminal(false) // inherit the host stdio (so an interactive tty works)
        .user(user)
        .args(input.argv.to_vec())
        .env(env)
        .cwd(PathBuf::from("/"))
        .build()?;

    let root = RootBuilder::default()
        .path(PathBuf::from(input.rootfs_rel))
        .readonly(true)
        .build()?;

    Ok(SpecBuilder::default()
        .version("1.1.0")
        .hostname("pierogi")
        .root(root)
        .process(process)
        .mounts(mounts)
        .linux(linux)
        .build()?)
}

fn base_mounts() -> Result<Vec<Mount>> {
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
        MountBuilder::default()
            .destination(PathBuf::from("/tmp"))
            .source(PathBuf::from("tmpfs"))
            .typ("tmpfs".to_string())
            .options(vec!["nosuid".into(), "nodev".into(), "mode=1777".into()])
            .build()?,
    ])
}

fn bind_mount(dest: &str, source: &str, options: &[&str]) -> Result<Mount> {
    Ok(MountBuilder::default()
        .destination(PathBuf::from(dest))
        .source(PathBuf::from(source))
        .typ("bind".to_string())
        .options(options.iter().map(|s| s.to_string()).collect::<Vec<_>>())
        .build()?)
}
