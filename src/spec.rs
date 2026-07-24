//! The typed pierogi **Spec** — the v0 schema (see `docs/spec.md`).
//!
//! One descriptive, complete account of a single sandbox. The engine is a **pure
//! realizer** (`docs/architecture.md`'s third principle): it holds **no policy**
//! and projects whatever this spec carries into the OCI runtime spec,
//! `absent = absent`. So the schema is a **faithful projection of the OCI spec**
//! (`capabilities`, `seccomp`, `namespaces`, mappings, masks, rlimits, …, all
//! pass-through) plus two nix carve-outs (`root` as a sealed rootfs, `closure` as
//! the identity-bind set). The default-closed *floor* is not here and not in the
//! engine — it is `profiles.base` in the Nix producer layer, composed into a spec
//! before the engine ever sees it. See `docs/control-surface.md`.
//!
//! Ergonomics ride on top: compose-flavoured names (`environment`, `working_dir`,
//! `tty`, `hostname`, `read_only`) and an execve argv split (`executable` +
//! `args`). The big verbatim-OCI fields (`seccomp`, `devices`, `rlimits`) reuse
//! `oci_spec` types directly rather than mirror them.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{bail, Result};
use oci_spec::runtime::{
    LinuxDevice, LinuxNamespace, LinuxSeccomp, Mount as OciMount, PosixRlimit,
};
use serde::{Deserialize, Serialize};

/// A top-level Nix store path (`/nix/store/<hash-name>`).
///
/// A newtype so the schema can make "this must be a store path" a property of
/// the *type* rather than a runtime check scattered across the resolver —
/// `root` and the `Overlay` mount source are `StorePath`, while a `Host` source
/// is an opaque `PathBuf`. Validated on construction and on deserialize: any
/// non-store path is rejected at parse time, fail-loud.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct StorePath(PathBuf);

impl StorePath {
    /// Construct from a path, asserting it is a well-formed top-level store
    /// path (`/nix/store/<name>`, exactly one component past the store dir).
    pub fn new(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        let mut comps = path.components().filter_map(|c| match c {
            std::path::Component::Normal(p) => Some(p),
            _ => None,
        });
        match (comps.next(), comps.next(), comps.next(), comps.next()) {
            (Some(nix), Some(store), Some(_name), None) if nix == "nix" && store == "store" => {
                Ok(StorePath(path))
            }
            _ => bail!("{} is not a top-level /nix/store path", path.display()),
        }
    }

    pub fn as_path(&self) -> &std::path::Path {
        &self.0
    }
}

impl std::fmt::Display for StorePath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.display().fmt(f)
    }
}

impl<'de> Deserialize<'de> for StorePath {
    fn deserialize<D>(de: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let path = PathBuf::deserialize(de)?;
        StorePath::new(path).map_err(serde::de::Error::custom)
    }
}

/// One sandbox, described completely. Flat process/behavior fields plus the OCI
/// pass-throughs and the nix carve-outs. Every pass-through is `absent = absent`:
/// omit it and the engine emits nothing (the OCI runtime's own default applies).
/// A hardened sandbox is one whose producer composed `profiles.base`, so these
/// carry the floor values — visible in the artifact, not applied by the engine.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Spec {
    // ── nix carve-outs (not OCI) ──
    /// The **rootfs**: a sealed store path mounted at `/`, carrying the whole
    /// skeleton (`/etc`, `/bin`, every baked mountpoint) and identity-bound at
    /// `/nix/store/<self>` (it appears in `closure.resolved`). Named `root`, not
    /// `image`: a store-path rootfs, not a pullable registry image.
    pub root: StorePath,
    /// The Nix store view — `{ seeds, resolved }`. `seeds` is a build input;
    /// `resolved` is stamped by `resolve --graph`. `run` requires `resolved`.
    #[serde(default)]
    pub closure: Closure,

    // ── process: execve argv + compose-flavoured descriptive fields ──
    /// The program: a bare name (resolved via `environment.PATH`) or an absolute
    /// store path. `execve`'s first argument, validated *against* the closure.
    pub executable: String,
    /// The rest of argv (`execve`'s argv tail).
    #[serde(default)]
    pub args: Vec<String>,
    /// Allocate a controlling terminal (compose `tty`; OCI `process.terminal`),
    /// which also drives the host-side console relay.
    #[serde(default)]
    pub tty: bool,
    /// Working directory (compose `working_dir`; OCI `process.cwd`). Absent → `/`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_dir: Option<PathBuf>,
    /// UTS hostname (compose `hostname`; OCI `hostname`). Absent → engine emits
    /// none (a producer/`profiles.base` supplies it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
    /// Producer-filled key/values (compose `environment`; OCI `process.env`).
    /// `PATH` lives here and is the single source of truth for `executable`.
    #[serde(default)]
    pub environment: BTreeMap<String, String>,

    // ── OCI pass-throughs (the floor lives in a producer, not here) ──
    /// `root.readonly` (compose `read_only`). Absent → not emitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_only: Option<bool>,
    /// `process.noNewPrivileges`. Absent → not emitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub no_new_privileges: Option<bool>,
    /// `process.capabilities`. `None` → not emitted (runtime default);
    /// `Some([])` → **drop all**; `Some([names])` → exactly those (names without
    /// the `CAP_` prefix, e.g. `NET_BIND_SERVICE`; the engine puts each in every
    /// capability set).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<Vec<String>>,
    /// `process.rlimits`, verbatim OCI (`{ type, soft, hard }`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rlimits: Vec<PosixRlimit>,
    /// `linux.maskedPaths` — paths hidden inside the sandbox (file → `/dev/null`,
    /// dir → empty ro tmpfs). The curated list is a `profiles.base` concern.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub masked_paths: Vec<String>,
    /// `linux.readonlyPaths` — paths remounted read-only inside the sandbox.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub readonly_paths: Vec<String>,
    /// `linux.rootfsPropagation` (e.g. `rprivate`). Absent → not emitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rootfs_propagation: Option<String>,
    /// `linux.namespaces`, verbatim OCI (`{ type, path? }`). A `path` **joins** an
    /// existing namespace (host's, a pod peer's, or a `pasta`/`slirp4netns` netns
    /// for rootless networking); omit it for a fresh private one. Absent → no
    /// namespaces (the sandbox shares the host's — a producer supplies the set).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub namespaces: Vec<LinuxNamespace>,
    /// `linux.uidMappings`. `host` may be `"self"` (the invoking user's uid,
    /// resolved at launch — the host side of a rootless map isn't known until run
    /// time) or a concrete number.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub uid_mappings: Vec<IdMapping>,
    /// `linux.gidMappings`. Same `"self"`/numeric grammar as `uid_mappings`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gid_mappings: Vec<IdMapping>,
    /// `linux.seccomp`, verbatim OCI seccomp profile. The engine loads it as-is;
    /// authoring/curating the profile is a producer (`profiles.base`) concern.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seccomp: Option<LinuxSeccomp>,
    /// `linux.devices`, verbatim OCI. Realized via `linux.devices` (rootless binds
    /// the host node — never a `mounts[]` char-device bind, which EINVALs; see
    /// `main.rs`). No cgroup device allowlist (moot rootless).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub devices: Vec<LinuxDevice>,

    // ── placements layered onto `root` ──
    /// **Verbatim OCI mounts** (`{ destination, source, type, options, … }`) —
    /// binds, tmpfs, idmapped mounts, *and overlays* (`type: "overlay"` with
    /// `lowerdir`/`upperdir`/`workdir` in `options`). There is **no special mount
    /// kind**: pass-through, the producer (CLI / Nix) owns every field. Each binds
    /// onto a mountpoint `root` already baked. A relocated store path is a bind
    /// whose source is `/nix/store/<x>`; an overlay over a store lower is an OCI
    /// overlay whose `lowerdir` is a store path — in both cases, if the source's
    /// closure must be bound, the producer adds it to `closure.seeds` (mounts
    /// contribute nothing to the closure automatically).
    #[serde(default)]
    pub mounts: Vec<OciMount>,
}

/// The Nix store view: what the closure was traversed *from* (`seeds`, a
/// build-time input) and — once built — what was locked *in* (`resolved`, stamped
/// by `resolve --graph`). See `crate::resolve`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Closure {
    /// The seed store paths the closure is traversed *from* — a build-time input
    /// (conventionally `root` plus any overlay-lower sources).
    #[serde(default)]
    pub seeds: Vec<StorePath>,
    /// The flat identity-bind set bound under `/nix/store`, canonically sorted,
    /// the full closure of `seeds` including `root`. `Some` ⇒ built; `None` ⇒
    /// unbuilt (`run` fails loud — build with `resolve --graph`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved: Option<Vec<StorePath>>,
}

/// One `linux.{uid,gid}Mappings` entry: `container` id → `host` id, `size` wide.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IdMapping {
    pub container: u32,
    pub host: HostId,
    pub size: u32,
}

/// The host side of an [`IdMapping`]: a concrete id, or `"self"` — the invoking
/// user's id, resolved by the engine at launch. A build-time producer can't name
/// the invoker's uid numerically, so `"self"` lets `profiles.base` express
/// "map container-0 to whoever runs it" declaratively.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostId {
    /// A concrete host uid/gid.
    Numeric(u32),
    /// The invoking user's uid/gid, resolved at launch (`"self"`).
    Invoker,
}

impl Serialize for HostId {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            HostId::Numeric(n) => s.serialize_u32(*n),
            HostId::Invoker => s.serialize_str("self"),
        }
    }
}

impl<'de> Deserialize<'de> for HostId {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Repr {
            Num(u32),
            Str(String),
        }
        match Repr::deserialize(de)? {
            Repr::Num(n) => Ok(HostId::Numeric(n)),
            Repr::Str(s) if s == "self" => Ok(HostId::Invoker),
            Repr::Str(s) => Err(serde::de::Error::custom(format!(
                "id-mapping host must be a number or \"self\", got {s:?}"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_path_accepts_top_level_and_rejects_others() {
        assert!(StorePath::new("/nix/store/aaaa-bash-5.2").is_ok());
        assert!(StorePath::new("/nix/store/aaaa-bash-5.2/bin/bash").is_err());
        assert!(StorePath::new("/nix/store").is_err());
        assert!(StorePath::new("/etc/passwd").is_err());
    }

    #[test]
    fn unbuilt_spec_round_trips_with_pure_passthrough() {
        // A hand-authored, profile-less spec: only the fields present, and the
        // OCI pass-throughs absent (the engine will emit nothing for them).
        let json = r#"{
            "root": "/nix/store/aaaa-base-env",
            "executable": "bash",
            "args": ["-l"],
            "tty": true,
            "environment": { "PATH": "/bin" },
            "mounts": [ { "destination": "/tmp", "type": "tmpfs", "options": ["nosuid","nodev"] } ],
            "closure": { "seeds": [ "/nix/store/aaaa-base-env" ] }
        }"#;
        let spec: Spec = serde_json::from_str(json).unwrap();
        assert_eq!(spec.closure.seeds.len(), 1);
        assert!(spec.closure.resolved.is_none());
        // Absent = absent: no floor applied by the schema.
        assert!(spec.read_only.is_none());
        assert!(spec.no_new_privileges.is_none());
        assert!(spec.capabilities.is_none());
        assert!(spec.hostname.is_none());
        assert!(spec.namespaces.is_empty());
        assert!(spec.masked_paths.is_empty());
        // Absent pass-throughs stay absent through a serialize round-trip.
        let back = serde_json::to_string(&spec).unwrap();
        assert!(!back.contains("read_only"));
        assert!(!back.contains("capabilities"));
        assert!(!back.contains("namespaces"));
        let again: Spec = serde_json::from_str(&back).unwrap();
        assert!(again.read_only.is_none());
    }

    #[test]
    fn passthrough_fields_carry_a_composed_floor() {
        // What `profiles.base` composes into a spec.
        let json = r#"{
            "root": "/nix/store/aaaa-r",
            "executable": "bash",
            "read_only": true,
            "no_new_privileges": true,
            "capabilities": [],
            "rootfs_propagation": "rprivate",
            "rlimits": [ { "type": "RLIMIT_CORE", "soft": 0, "hard": 0 } ],
            "masked_paths": [ "/proc/kcore" ],
            "readonly_paths": [ "/proc/sys" ],
            "namespaces": [ { "type": "user" }, { "type": "mount" }, { "type": "network", "path": "/proc/9/ns/net" } ],
            "uid_mappings": [ { "container": 0, "host": "self", "size": 1 } ],
            "gid_mappings": [ { "container": 0, "host": 100, "size": 1 } ]
        }"#;
        let spec: Spec = serde_json::from_str(json).unwrap();
        assert_eq!(spec.read_only, Some(true));
        assert_eq!(spec.capabilities.as_deref(), Some(&[][..])); // drop-all, not None
        assert_eq!(spec.rlimits.len(), 1);
        assert_eq!(spec.masked_paths, ["/proc/kcore"]);
        // namespaces: two private + one join (path set).
        assert_eq!(spec.namespaces.len(), 3);
        assert!(spec.namespaces[0].path().is_none());
        assert!(spec.namespaces[2].path().is_some());
        // id-mapping host: "self" sentinel and a numeric.
        assert_eq!(spec.uid_mappings[0].host, HostId::Invoker);
        assert_eq!(spec.gid_mappings[0].host, HostId::Numeric(100));
        // round-trips (self stays "self", drop-all stays []).
        let back = serde_json::to_string(&spec).unwrap();
        assert!(back.contains(r#""host":"self""#));
        assert!(back.contains(r#""capabilities":[]"#));
        let _again: Spec = serde_json::from_str(&back).unwrap();
    }

    #[test]
    fn id_mapping_host_rejects_bogus_string() {
        let json = r#"{ "container": 0, "host": "root", "size": 1 }"#;
        assert!(serde_json::from_str::<IdMapping>(json).is_err());
    }

    #[test]
    fn closure_round_trips_as_seeds_and_resolved() {
        let json = r#"{
            "root": "/nix/store/aaaa-base-env",
            "executable": "bash",
            "closure": {
                "seeds": [ "/nix/store/aaaa-base-env" ],
                "resolved": [ "/nix/store/0000-bash", "/nix/store/1111-glibc" ]
            }
        }"#;
        let spec: Spec = serde_json::from_str(json).unwrap();
        assert_eq!(spec.closure.seeds.len(), 1);
        assert_eq!(spec.closure.resolved.as_ref().unwrap().len(), 2);
    }
}
