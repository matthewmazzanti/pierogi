//! Translate a **resolved** pierogi [`Spec`] into the in-memory OCI runtime spec
//! the backend drives — a **pure projection**, no policy of its own
//! (`docs/architecture.md`'s third principle).
//!
//! Every OCI knob is a faithful pass-through of a spec field, `absent = absent`:
//! if the spec doesn't carry `capabilities` / `seccomp` / `namespaces` / …, the
//! engine emits nothing for them (the OCI runtime's own default applies). The
//! floor is *not* here — it's `profiles.base` in the Nix layer, composed into the
//! spec before we see it. The one subtlety is that `oci-spec`'s `Process`/`Root`/
//! `Spec` builders inject their *own* opinionated defaults (nnp, 3 caps, a nofile
//! rlimit, `readonly: true`, `hostname: "youki"`) for fields we don't set — so we
//! **explicitly set every pass-through** (to `None` when the spec is silent) via
//! the setters, neutralizing those vendor defaults.
//!
//! Field by field:
//! * `closure.resolved` → one `ro,nosuid,nodev` identity bind per path under
//!   `/nix/store` (bind options are a mechanism constant — the store is immutable
//!   — not a producer knob).
//! * `mounts` → **verbatim OCI mounts**, straight through (binds, tmpfs,
//!   overlays, idmapped mounts — no special mount kind).
//! * `executable`+`args` → `process.args`; `tty` → `process.terminal` (+ devpts);
//!   `working_dir` → `process.cwd`; `environment` → `process.env`.
//! * `read_only`/`no_new_privileges`/`capabilities`/`rlimits`/`masked_paths`/
//!   `readonly_paths`/`rootfs_propagation`/`namespaces`/`{uid,gid}_mappings`/
//!   `seccomp`/`devices` → the matching OCI fields, pass-through.
//! * OCI `root.path` **is `root`** — the sealed rootfs mounted at `/`.

use std::path::PathBuf;
use std::str::FromStr;

use anyhow::{Context, Result};
use oci_spec::runtime::{
    Capabilities, Capability, LinuxBuilder, LinuxCapabilitiesBuilder, LinuxIdMapping,
    LinuxIdMappingBuilder, Mount as OciMount, MountBuilder, ProcessBuilder, RootBuilder,
    Spec as OciSpec, SpecBuilder, UserBuilder,
};

use crate::spec::{HostId, IdMapping, Spec};

/// Everything the OCI translation needs beyond the resolved spec itself.
pub struct RealizeInputs<'a> {
    /// The resolved spec (`closure.resolved` must be `Some`).
    pub spec: &'a Spec,
    /// The invoking user's uid/gid — used to resolve `HostId::Invoker` (`"self"`)
    /// in the id mappings, the one host value only known at launch.
    pub uid: u32,
    pub gid: u32,
}

pub fn build_spec(input: &RealizeInputs) -> Result<OciSpec> {
    let spec = input.spec;
    let resolved = spec
        .closure
        .resolved
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("build_spec called on an unresolved spec"))?;

    // ── linux: namespaces (verbatim) + id mappings + the pass-through rest ──
    let uid_mappings = build_id_mappings(&spec.uid_mappings, input.uid)?;
    let gid_mappings = build_id_mappings(&spec.gid_mappings, input.gid)?;

    let mut linux = LinuxBuilder::default().build()?;
    linux.set_namespaces(non_empty(spec.namespaces.clone()));
    linux.set_uid_mappings(non_empty(uid_mappings));
    linux.set_gid_mappings(non_empty(gid_mappings));
    linux.set_masked_paths(non_empty(spec.masked_paths.clone()));
    linux.set_readonly_paths(non_empty(spec.readonly_paths.clone()));
    linux.set_rootfs_propagation(spec.rootfs_propagation.clone());
    linux.set_seccomp(spec.seccomp.clone());
    linux.set_devices(non_empty(spec.devices.clone()));

    // ── mounts: engine-essential base, then closure binds, then placements ──
    let mut mounts = base_mounts()?;
    if spec.tty {
        mounts.push(devpts_mount()?);
    }
    // Closure identity binds: ro,nosuid,nodev — a mechanism constant (the store is
    // immutable and holds no device nodes), never a producer knob.
    let closure_opts: Vec<String> = ["bind", "ro", "nosuid", "nodev"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    for path in resolved {
        let p = path.to_string();
        mounts.push(bind_mount(&p, &p, &closure_opts)?);
    }
    // Placements: verbatim OCI mounts pass straight through (the producer owns
    // every field — binds, tmpfs, overlays, idmapped mounts alike). If crun can't
    // realize one (e.g. rootless overlayfs on an old kernel), it fails loud at
    // launch — the runtime's concern, not an engine opinion.
    mounts.extend(spec.mounts.iter().cloned());

    // ── process: always-set fields, then the pass-throughs via setters ──
    let user = UserBuilder::default().uid(0u32).gid(0u32).build()?;
    let env: Vec<String> = spec
        .environment
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect();
    let mut argv = vec![spec.executable.clone()];
    argv.extend(spec.args.iter().cloned());
    let cwd = spec
        .working_dir
        .clone()
        .unwrap_or_else(|| PathBuf::from("/"));

    let mut process = ProcessBuilder::default()
        .terminal(spec.tty)
        .user(user)
        .args(argv)
        .env(env)
        .cwd(cwd)
        .build()?;
    // Neutralize oci-spec's Process::default() (nnp=true, 3 caps, a nofile
    // rlimit): emit exactly what the spec carries, or nothing.
    process.set_no_new_privileges(spec.no_new_privileges);
    process.set_capabilities(build_capabilities(spec.capabilities.as_deref())?);
    process.set_rlimits(non_empty(spec.rlimits.clone()));

    // ── root: the sealed rootfs; readonly is a pass-through (default `true` in
    // oci-spec would otherwise leak) ──
    let mut root = RootBuilder::default()
        .path(spec.root.as_path().to_path_buf())
        .build()?;
    root.set_readonly(spec.read_only);

    let mut oci = SpecBuilder::default()
        .version("1.1.0")
        .root(root)
        .process(process)
        .mounts(mounts)
        .linux(linux)
        .build()?;
    // hostname is a pass-through too (oci-spec defaults it to "youki").
    oci.set_hostname(spec.hostname.clone());
    Ok(oci)
}

/// `Some(vec)` when non-empty, else `None` — so an absent list becomes an absent
/// OCI field rather than an empty one (keeps `absent = absent` honest).
fn non_empty<T>(v: Vec<T>) -> Option<Vec<T>> {
    if v.is_empty() {
        None
    } else {
        Some(v)
    }
}

/// `spec.{uid,gid}_mappings` → OCI id mappings, resolving `HostId::Invoker`
/// (`"self"`) to the invoking user's id.
fn build_id_mappings(specs: &[IdMapping], invoker: u32) -> Result<Vec<LinuxIdMapping>> {
    specs
        .iter()
        .map(|m| {
            let host = match m.host {
                HostId::Numeric(n) => n,
                HostId::Invoker => invoker,
            };
            LinuxIdMappingBuilder::default()
                .container_id(m.container)
                .host_id(host)
                .size(m.size)
                .build()
                .context("building an id mapping")
        })
        .collect()
}

/// `spec.capabilities` → `process.capabilities`. `None` → `None` (emit nothing);
/// `Some(names)` → the same set in every capability list (`Some([])` drops all).
fn build_capabilities(
    names: Option<&[String]>,
) -> Result<Option<oci_spec::runtime::LinuxCapabilities>> {
    let Some(names) = names else {
        return Ok(None);
    };
    let caps: Capabilities = names
        .iter()
        .map(|n| parse_cap(n))
        .collect::<Result<_>>()?;
    Ok(Some(
        LinuxCapabilitiesBuilder::default()
            .bounding(caps.clone())
            .effective(caps.clone())
            .permitted(caps.clone())
            .inheritable(caps.clone())
            .ambient(caps)
            .build()?,
    ))
}

/// Parse a capability name — accepts `NET_BIND_SERVICE`, `CAP_NET_BIND_SERVICE`,
/// or lowercase, normalizing to the OCI enum. Fail loud on unknown.
fn parse_cap(name: &str) -> Result<Capability> {
    let upper = name.trim().to_uppercase();
    let bare = upper.strip_prefix("CAP_").unwrap_or(&upper);
    Capability::from_str(bare).map_err(|_| anyhow::anyhow!("unknown capability {name:?}"))
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

fn bind_mount(dest: &str, source: &str, options: &[String]) -> Result<OciMount> {
    Ok(MountBuilder::default()
        .destination(PathBuf::from(dest))
        .source(PathBuf::from(source))
        .typ("bind".to_string())
        .options(options.to_vec())
        .build()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::{Closure, StorePath};
    use oci_spec::runtime::{LinuxNamespaceBuilder, LinuxNamespaceType};
    use std::collections::BTreeMap;

    /// A minimal resolved spec with no pass-throughs set (a raw, profile-less
    /// spec) — used to prove the engine adds no floor of its own.
    fn bare_resolved_spec() -> Spec {
        Spec {
            root: StorePath::new("/nix/store/aaaa-root").unwrap(),
            closure: Closure {
                seeds: vec![StorePath::new("/nix/store/aaaa-root").unwrap()],
                resolved: Some(vec![StorePath::new("/nix/store/aaaa-root").unwrap()]),
            },
            executable: "bash".into(),
            args: vec![],
            tty: false,
            working_dir: None,
            hostname: None,
            environment: BTreeMap::new(),
            read_only: None,
            no_new_privileges: None,
            capabilities: None,
            rlimits: vec![],
            masked_paths: vec![],
            readonly_paths: vec![],
            rootfs_propagation: None,
            namespaces: vec![],
            uid_mappings: vec![],
            gid_mappings: vec![],
            seccomp: None,
            devices: vec![],
            mounts: vec![],
        }
    }

    #[test]
    fn bare_spec_inherits_no_oci_defaults() {
        // The regression guard: with nothing set, the engine must NOT leak
        // oci-spec's Process::default() (3 caps, nnp, nofile) or Root/Spec
        // defaults (readonly=true, hostname="youki").
        let spec = bare_resolved_spec();
        let oci = build_spec(&RealizeInputs {
            spec: &spec,
            uid: 1000,
            gid: 1000,
        })
        .unwrap();
        let process = oci.process().as_ref().unwrap();
        assert!(process.capabilities().is_none(), "must not inherit 3-cap default");
        assert!(process.no_new_privileges().is_none());
        assert!(process.rlimits().is_none());
        assert!(oci.root().as_ref().unwrap().readonly().is_none());
        assert!(oci.hostname().is_none());
        let linux = oci.linux().as_ref().unwrap();
        assert!(linux.namespaces().is_none());
        assert!(linux.masked_paths().is_none());
    }

    #[test]
    fn passthroughs_project_faithfully() {
        let mut spec = bare_resolved_spec();
        spec.read_only = Some(true);
        spec.no_new_privileges = Some(true);
        spec.hostname = Some("box".into());
        spec.capabilities = Some(vec!["NET_BIND_SERVICE".into()]);
        spec.masked_paths = vec!["/proc/kcore".into()];
        spec.namespaces = vec![LinuxNamespaceBuilder::default()
            .typ(LinuxNamespaceType::User)
            .build()
            .unwrap()];
        spec.uid_mappings = vec![IdMapping {
            container: 0,
            host: HostId::Invoker,
            size: 1,
        }];

        let oci = build_spec(&RealizeInputs {
            spec: &spec,
            uid: 4242,
            gid: 99,
        })
        .unwrap();
        let process = oci.process().as_ref().unwrap();
        assert!(process.no_new_privileges().unwrap());
        // capabilities present, exactly the one requested, in the bounding set.
        let caps = process.capabilities().as_ref().unwrap();
        let bounding = caps.bounding().as_ref().unwrap();
        assert!(bounding.contains(&Capability::NetBindService));
        assert_eq!(bounding.len(), 1);
        assert_eq!(oci.root().as_ref().unwrap().readonly(), Some(true));
        assert_eq!(oci.hostname().as_deref(), Some("box"));
        let linux = oci.linux().as_ref().unwrap();
        assert_eq!(linux.masked_paths().as_ref().unwrap(), &["/proc/kcore"]);
        // "self" resolved to the invoking uid.
        let maps = linux.uid_mappings().as_ref().unwrap();
        assert_eq!(maps[0].host_id(), 4242);
    }

    #[test]
    fn drop_all_caps_is_distinct_from_absent() {
        let mut spec = bare_resolved_spec();
        spec.capabilities = Some(vec![]); // drop-all
        let oci = build_spec(&RealizeInputs {
            spec: &spec,
            uid: 1,
            gid: 1,
        })
        .unwrap();
        let caps = oci
            .process()
            .as_ref()
            .unwrap()
            .capabilities()
            .as_ref()
            .expect("drop-all still emits an (empty) capabilities block");
        assert!(caps.bounding().as_ref().unwrap().is_empty());
    }

    #[test]
    fn unknown_capability_fails_loud() {
        let mut spec = bare_resolved_spec();
        spec.capabilities = Some(vec!["NOT_A_CAP".into()]);
        assert!(build_spec(&RealizeInputs {
            spec: &spec,
            uid: 1,
            gid: 1
        })
        .is_err());
    }

    #[test]
    fn mounts_pass_through_verbatim() {
        let mut spec = bare_resolved_spec();
        spec.mounts = vec![MountBuilder::default()
            .destination(PathBuf::from("/data"))
            .source(PathBuf::from("/data"))
            .typ("bind".to_string())
            .options(vec![
                "bind".into(),
                "rw".into(),
                "nosuid".into(),
                "nodev".into(),
            ])
            .build()
            .unwrap()];
        let oci = build_spec(&RealizeInputs {
            spec: &spec,
            uid: 1,
            gid: 1,
        })
        .unwrap();
        let m = oci
            .mounts()
            .as_ref()
            .unwrap()
            .iter()
            .find(|m| m.destination() == &PathBuf::from("/data"))
            .unwrap();
        assert_eq!(
            m.options().as_ref().unwrap(),
            &["bind", "rw", "nosuid", "nodev"]
        );
    }
}
