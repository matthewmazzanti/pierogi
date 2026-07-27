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
//! * `closure.resolved` → a **read-only** `/nix/store` bind of a runtime state
//!   dir holding one pre-created mountpoint stub per path (file- or dir-shaped
//!   to match its source), then one `ro,nosuid,nodev` identity bind per path
//!   onto its stub. The store tree is provided at runtime (not baked into the
//!   sealed root) yet stays read-only — the stubs pre-exist, so nothing is
//!   created at mount time. All mechanism constants (the store is immutable).
//! * `mounts` → **verbatim OCI mounts**, straight through (binds, tmpfs,
//!   overlays, idmapped mounts — no special mount kind).
//! * `process.executable`+`args` → `process.args`; `process.terminal` (+ devpts);
//!   `process.cwd`; `process.env`; `process.{user,capabilities,rlimits,
//!   no_new_privileges,oom_score_adj}` → the matching OCI process fields.
//! * `linux.*` (namespaces, `{uid,gid}_mappings`, masks, seccomp, devices,
//!   resources, sysctl, …) and `root.readonly` → the matching OCI fields.
//! * The spec **mirrors the OCI structure** (`root`/`process`/`linux`), so most
//!   of this is a `spec.<obj>.<field>` → OCI `<obj>.<field>` copy.
//! * OCI `root.path` is `spec.root.path` — the sealed rootfs mounted at `/`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use oci_spec::runtime::{
    LinuxBuilder, LinuxIdMapping, LinuxIdMappingBuilder, Mount as OciMount, MountBuilder,
    ProcessBuilder, RootBuilder, Spec as OciSpec, SpecBuilder, UserBuilder,
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
    /// The runtime state dir holding the `/nix/store` mountpoint stubs (one per
    /// resolved path, file- or dir-shaped). Bound read-only at `/nix/store`;
    /// assembled by `run::scaffold_store`.
    pub store_root: &'a Path,
}

pub fn build_spec(input: &RealizeInputs) -> Result<OciSpec> {
    let spec = input.spec;
    let resolved = spec
        .closure
        .resolved
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("build_spec called on an unresolved spec"))?;

    let (proc, lin) = (&spec.process, &spec.linux);

    // ── linux: namespaces (verbatim) + id mappings + the pass-through rest ──
    let uid_mappings = build_id_mappings(&lin.uid_mappings, input.uid)?;
    let gid_mappings = build_id_mappings(&lin.gid_mappings, input.gid)?;

    let mut linux = LinuxBuilder::default().build()?;
    linux.set_namespaces(non_empty(lin.namespaces.clone()));
    linux.set_uid_mappings(non_empty(uid_mappings));
    linux.set_gid_mappings(non_empty(gid_mappings));
    linux.set_masked_paths(non_empty(lin.masked_paths.clone()));
    linux.set_readonly_paths(non_empty(lin.readonly_paths.clone()));
    linux.set_rootfs_propagation(lin.rootfs_propagation.clone());
    linux.set_seccomp(lin.seccomp.clone());
    linux.set_devices(non_empty(lin.devices.clone()));
    linux.set_resources(lin.resources.clone());
    linux.set_cgroups_path(lin.cgroups_path.clone());
    linux.set_sysctl(non_empty_map(lin.sysctl.clone()));
    linux.set_net_devices(non_empty_map(lin.net_devices.clone()));
    linux.set_personality(lin.personality.clone());

    // ── mounts: the store tree, then closure binds, then placements, then the
    //    terminal devpts ──
    // The engine injects no *base* mounts (`/proc`, `/dev`, `/dev/pts` are all
    // producer placements from the Nix profile layer). Its only mounts are the
    // store-topology mechanism (below) and, for a `terminal`, the console devpts.
    let mut mounts: Vec<OciMount> = Vec::new();
    // ro,nosuid,nodev — a mechanism constant (the store is immutable and holds no
    // device nodes), never a producer knob. Shared by the `/nix/store` parent
    // bind and every identity bind.
    let closure_opts: Vec<String> = ["bind", "ro", "nosuid", "nodev"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    // The `/nix/store` mountpoint tree: a read-only bind of the runtime state dir
    // (`store_root`), which holds one pre-created stub per resolved path. The
    // sealed root bakes only an empty `/nix/store` dir; the stubs live in the
    // state dir, so `/nix/store` is provided at runtime yet stays RO — the closure
    // binds land on stubs that already exist (nothing created at mount time). This
    // keeps the store topology OUT of the sealed root, so root's Nix closure is
    // exactly what it references — no baked `/nix/store/<hash>` mountpoints.
    // Mounted first, so the identity binds find their stubs in place.
    mounts.push(bind_mount(
        "/nix/store",
        &input.store_root.to_string_lossy(),
        &closure_opts,
    )?);
    // Closure identity binds: each resolved path onto its stub.
    for path in resolved {
        let p = path.to_string();
        mounts.push(bind_mount(&p, &p, &closure_opts)?);
    }
    // Placements: verbatim OCI mounts pass straight through (the producer owns
    // every field — binds, tmpfs, overlays, idmapped mounts alike). /proc and the
    // /dev tmpfs are among these now — lifted out of the engine's base mounts into
    // the producer layer. If crun can't realize one (e.g. rootless overlayfs on an
    // old kernel), it fails loud at launch — the runtime's concern, not an engine opinion.
    mounts.extend(spec.mounts.iter().cloned());
    // A `terminal` console allocates its pty from a devpts, which mounts onto the
    // /dev tmpfs — now a producer placement (above). So the devpts is appended
    // *after* the placements, guaranteeing the producer's /dev is mounted first.
    // (foot runs the pump-less path with `terminal = false` and declares its own
    // devpts as a placement.)
    if proc.terminal {
        mounts.push(devpts_mount()?);
    }

    // ── process: always-set fields, then the pass-throughs via setters ──
    // Run-as identity: the producer's `user` verbatim, or uid 0/gid 0 by default.
    let user = match &proc.user {
        Some(u) => u.clone(),
        None => UserBuilder::default().uid(0u32).gid(0u32).build()?,
    };
    let env: Vec<String> = proc.env.iter().map(|(k, v)| format!("{k}={v}")).collect();
    let mut argv = vec![proc.executable.clone()];
    argv.extend(proc.args.iter().cloned());
    let cwd = proc.cwd.clone().unwrap_or_else(|| PathBuf::from("/"));

    let mut process = ProcessBuilder::default()
        .terminal(proc.terminal)
        .user(user)
        .args(argv)
        .env(env)
        .cwd(cwd)
        .build()?;
    // Neutralize oci-spec's Process::default() (nnp=true, 3 caps, a nofile
    // rlimit): emit exactly what the spec carries, or nothing.
    process.set_no_new_privileges(proc.no_new_privileges);
    // Verbatim OCI: the five capability sets pass straight through (None emits
    // nothing, neutralizing oci-spec's 3-cap Process::default()).
    process.set_capabilities(proc.capabilities.clone());
    process.set_rlimits(non_empty(proc.rlimits.clone()));
    process.set_oom_score_adj(proc.oom_score_adj);

    // ── root: the sealed rootfs; readonly is a pass-through (default `true` in
    // oci-spec would otherwise leak) ──
    let mut root = RootBuilder::default()
        .path(spec.root.path.as_path().to_path_buf())
        .build()?;
    root.set_readonly(spec.root.readonly);

    let mut oci = SpecBuilder::default()
        .version("1.1.0")
        .root(root)
        .process(process)
        .mounts(mounts)
        .linux(linux)
        .build()?;
    // hostname is a pass-through too (oci-spec defaults it to "youki").
    oci.set_hostname(spec.hostname.clone());
    oci.set_domainname(spec.domainname.clone());
    oci.set_annotations(non_empty_map(spec.annotations.clone()));
    Ok(oci)
}

/// Like [`non_empty`] for maps: `Some(HashMap)` when non-empty, else `None`.
/// Also converts our deterministic `BTreeMap` to the OCI `HashMap`.
fn non_empty_map<V>(m: std::collections::BTreeMap<String, V>) -> Option<HashMap<String, V>> {
    if m.is_empty() {
        None
    } else {
        Some(m.into_iter().collect())
    }
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
    use crate::spec::{Closure, Linux, Process, Root, StorePath};
    use oci_spec::runtime::{
        Capabilities, Capability, LinuxCapabilitiesBuilder, LinuxNamespaceBuilder,
        LinuxNamespaceType,
    };
    use std::collections::BTreeMap;

    /// A minimal resolved spec with no pass-throughs set (a raw, profile-less
    /// spec) — used to prove the engine adds no floor of its own.
    fn bare_resolved_spec() -> Spec {
        Spec {
            root: Root {
                path: StorePath::new("/nix/store/aaaa-root").unwrap(),
                readonly: None,
            },
            closure: Closure {
                seeds: vec![StorePath::new("/nix/store/aaaa-root").unwrap()],
                resolved: Some(vec![StorePath::new("/nix/store/aaaa-root").unwrap()]),
            },
            process: Process {
                executable: "bash".into(),
                args: vec![],
                terminal: false,
                user: None,
                cwd: None,
                env: BTreeMap::new(),
                capabilities: None,
                rlimits: vec![],
                no_new_privileges: None,
                oom_score_adj: None,
            },
            hostname: None,
            domainname: None,
            annotations: BTreeMap::new(),
            linux: Linux::default(),
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
            store_root: Path::new("/run/pierogi-test/store"),
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
        spec.root.readonly = Some(true);
        spec.process.no_new_privileges = Some(true);
        spec.hostname = Some("box".into());
        spec.process.capabilities = Some(
            LinuxCapabilitiesBuilder::default()
                .bounding([Capability::NetBindService])
                .build()
                .unwrap(),
        );
        spec.linux.masked_paths = vec!["/proc/kcore".into()];
        spec.linux.namespaces = vec![LinuxNamespaceBuilder::default()
            .typ(LinuxNamespaceType::User)
            .build()
            .unwrap()];
        spec.linux.uid_mappings = vec![IdMapping {
            container: 0,
            host: HostId::Invoker,
            size: 1,
        }];

        let oci = build_spec(&RealizeInputs {
            spec: &spec,
            uid: 4242,
            gid: 99,
            store_root: Path::new("/run/pierogi-test/store"),
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
    fn new_passthroughs_project_and_default_user_is_zero() {
        let mut spec = bare_resolved_spec();
        spec.process.oom_score_adj = Some(-800);
        spec.linux.cgroups_path = Some("/pierogi/box".into());
        spec.linux.sysctl.insert("net.ipv4.ip_forward".into(), "0".into());
        spec.annotations.insert("k".into(), "v".into());
        let oci = build_spec(&RealizeInputs {
            spec: &spec,
            uid: 7,
            gid: 7,
            store_root: Path::new("/run/pierogi-test/store"),
        })
        .unwrap();
        let process = oci.process().as_ref().unwrap();
        assert_eq!(process.oom_score_adj(), Some(-800));
        // absent `user` → uid 0/gid 0.
        assert_eq!(process.user().uid(), 0);
        let linux = oci.linux().as_ref().unwrap();
        assert_eq!(linux.cgroups_path().as_ref().unwrap(), &PathBuf::from("/pierogi/box"));
        assert!(linux.sysctl().is_some());
        assert!(oci.annotations().is_some());
    }

    #[test]
    fn drop_all_caps_is_distinct_from_absent() {
        let mut spec = bare_resolved_spec();
        // drop-all: an empty bounding set is Some (a block), distinct from absent.
        spec.process.capabilities = Some(
            LinuxCapabilitiesBuilder::default()
                .bounding(Capabilities::default())
                .build()
                .unwrap(),
        );
        let oci = build_spec(&RealizeInputs {
            spec: &spec,
            uid: 1,
            gid: 1,
            store_root: Path::new("/run/pierogi-test/store"),
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
            store_root: Path::new("/run/pierogi-test/store"),
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

    #[test]
    fn engine_injects_no_base_mounts() {
        // /proc and /dev are both producer placements now. A bare spec (no
        // placements, no terminal) yields only the store-topology mounts (the
        // /nix/store tmpfs + the closure identity binds) — the engine mounts no
        // base of its own.
        let spec = bare_resolved_spec();
        let oci = build_spec(&RealizeInputs {
            spec: &spec,
            uid: 1,
            gid: 1,
            store_root: Path::new("/run/pierogi-test/store"),
        })
        .unwrap();
        let mounts = oci.mounts().as_ref().unwrap();
        assert!(
            mounts
                .iter()
                .all(|m| m.destination().starts_with("/nix/store")),
            "only the /nix/store bind + closure binds; the engine adds no /proc or /dev"
        );
    }

    #[test]
    fn store_bind_precedes_closure_binds() {
        // /nix/store is a read-only bind of the runtime state dir (the mountpoint
        // stubs), mounted first — the closure identity binds land on those stubs,
        // so the parent bind must come before any bind whose dest is under it.
        let spec = bare_resolved_spec(); // one resolved path: /nix/store/aaaa-root
        let oci = build_spec(&RealizeInputs {
            spec: &spec,
            uid: 1,
            gid: 1,
            store_root: Path::new("/run/pierogi-test/store"),
        })
        .unwrap();
        let mounts = oci.mounts().as_ref().unwrap();
        // First mount is the read-only /nix/store bind of the state dir.
        assert_eq!(mounts[0].destination(), &PathBuf::from("/nix/store"));
        assert_eq!(mounts[0].typ().as_deref(), Some("bind"));
        assert_eq!(
            mounts[0].source().as_deref(),
            Some(Path::new("/run/pierogi-test/store"))
        );
        assert!(mounts[0]
            .options()
            .as_ref()
            .unwrap()
            .contains(&"ro".to_string()));
        // The identity bind for the resolved path follows it (lands on a stub).
        let bind_i = mounts
            .iter()
            .position(|m| m.destination() == &PathBuf::from("/nix/store/aaaa-root"))
            .expect("the resolved path is identity-bound");
        assert!(bind_i > 0, "the closure bind must follow the /nix/store bind");
    }

    #[test]
    fn dev_is_a_placement_and_terminal_devpts_follows_it() {
        // /dev is no longer engine-injected. A terminal spec's devpts is appended
        // AFTER the placements, so a producer /dev tmpfs (a placement) is mounted
        // before the devpts that sits on it.
        let mut spec = bare_resolved_spec();
        spec.process.terminal = true;
        spec.mounts = vec![MountBuilder::default()
            .destination(PathBuf::from("/dev"))
            .source(PathBuf::from("tmpfs"))
            .typ("tmpfs".to_string())
            .options(vec!["nosuid".into(), "mode=755".into()])
            .build()
            .unwrap()];
        let oci = build_spec(&RealizeInputs {
            spec: &spec,
            uid: 1,
            gid: 1,
            store_root: Path::new("/run/pierogi-test/store"),
        })
        .unwrap();
        let mounts = oci.mounts().as_ref().unwrap();
        // Exactly one /dev — the producer's; the engine adds none.
        let dev_count = mounts
            .iter()
            .filter(|m| m.destination() == &PathBuf::from("/dev"))
            .count();
        assert_eq!(dev_count, 1, "only the producer's /dev, none from the engine");
        let dev_i = mounts
            .iter()
            .position(|m| m.destination() == &PathBuf::from("/dev"))
            .unwrap();
        let pts_i = mounts
            .iter()
            .position(|m| m.destination() == &PathBuf::from("/dev/pts"))
            .expect("terminal spec emits a devpts");
        assert!(dev_i < pts_i, "the /dev tmpfs must precede the terminal devpts");
    }
}
