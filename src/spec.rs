//! The typed pierogi **Spec** — the v0 schema (see `docs/spec.md`).
//!
//! The spec **mirrors the OCI runtime spec's structure** — `root`, `process`,
//! `linux`, `mounts`, `hostname`, … — plus two non-OCI carve-outs: `closure`
//! (the Nix store view) and `dbus` (the filter proxies). The engine is a **pure
//! realizer** (`docs/architecture.md`'s third principle): it holds **no policy**
//! and projects whatever the spec carries into the OCI spec, `absent = absent`.
//! The default-closed floor is not here and not in the engine — it's
//! `profiles.base` in the Nix producer layer, composed into a spec before the
//! engine sees it. See `docs/control-surface.md`.
//!
//! Verbatim-OCI leaves reuse `oci_spec` types directly (`User`,
//! `LinuxCapabilities`, `LinuxSeccomp`, `LinuxDevice`, `LinuxResources`,
//! `LinuxNamespace`, `PosixRlimit`, `LinuxNetDevice`, `LinuxPersonality`,
//! `Mount`). Deliberate divergences from a literal OCI mirror: `root.path` is a
//! `StorePath` (the sealed rootfs); `process.executable` + `process.args` keep
//! the **execve** split; `process.env` is a map (ergonomics); id-mapping `host`
//! admits `"self"`. `deny_unknown_fields` at every level turns an unmodeled OCI
//! field into a hard error rather than a silent drop.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{bail, Result};
use oci_spec::runtime::{
    LinuxCapabilities, LinuxDevice, LinuxNamespace, LinuxNetDevice, LinuxPersonality,
    LinuxResources, LinuxSeccomp, Mount as OciMount, PosixRlimit, User,
};
use serde::{Deserialize, Serialize};

/// A top-level Nix store path (`/nix/store/<hash-name>`).
///
/// A newtype so the schema can make "this must be a store path" a property of
/// the *type* rather than a runtime check. Validated on construction and on
/// deserialize: any non-store path is rejected at parse time, fail-loud.
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

/// One sandbox, described completely — mirroring the OCI runtime `Spec`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Spec {
    /// The rootfs (OCI `root`), whose `path` is a sealed store path.
    pub root: Root,
    /// The process to run (OCI `process`).
    pub process: Process,
    /// Placements (OCI `mounts`) — **verbatim OCI mounts** (bind / tmpfs /
    /// overlay / idmapped). Pass-through; a mount seeds the closure only if the
    /// producer adds its source to `closure.seeds`.
    #[serde(default)]
    pub mounts: Vec<OciMount>,
    /// UTS hostname (OCI `hostname`). Absent → not emitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
    /// NIS domain name (OCI `domainname`). Absent → not emitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domainname: Option<String>,
    /// Arbitrary metadata (OCI `annotations`) — not confinement, just a record
    /// for tooling/provenance. `BTreeMap` for deterministic artifacts.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub annotations: BTreeMap<String, String>,
    /// Linux-specific knobs (OCI `linux`).
    #[serde(default)]
    pub linux: Linux,
    /// The Nix store view — a **carve-out** (not OCI). `seeds` is a build-time
    /// input; `resolved` is stamped by `resolve --graph`.
    #[serde(default)]
    pub closure: Closure,
    /// D-Bus filter proxies — a **carve-out** (not OCI; D-Bus proxying has no
    /// OCI representation). A list of supervised `xdg-dbus-proxy` bus/socket
    /// pairs, each an independently-filtered bus routed into the sandbox. Absent
    /// or empty ⇒ no bus in the sandbox — deny-all is the *absence* of a proxy,
    /// not an engine policy. See [`DbusProxy`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dbus: Vec<DbusProxy>,
}

/// The rootfs (OCI `root`). `path` is a sealed store path mounted at `/` and
/// identity-bound at `/nix/store/<self>`; not `image` — a store-path rootfs.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Root {
    pub path: StorePath,
    /// `root.readonly`. Absent → not emitted (a producer/`profiles.base` sets it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub readonly: Option<bool>,
}

/// The process (OCI `process`). Argv keeps the **execve** split (`executable` +
/// `args`); `env` is a map and `capabilities` an add-list for ergonomics.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Process {
    /// The program: a bare name (resolved via `env.PATH` in-sandbox) or an
    /// absolute store path — `execve`'s `argv[0]`, validated against the closure.
    pub executable: String,
    /// The rest of argv.
    #[serde(default)]
    pub args: Vec<String>,
    /// Allocate a controlling terminal (OCI `process.terminal`); also drives the
    /// host-side console relay.
    #[serde(default)]
    pub terminal: bool,
    /// Run-as identity (verbatim OCI `process.user`). Absent → `uid 0/gid 0`.
    /// `username` is inert on Linux; `umask`/`additionalGids` do act.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<User>,
    /// Working directory (OCI `process.cwd`). Absent → `/`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    /// Environment (OCI `process.env`, but a **map** here). `PATH` lives here and
    /// is the single source of truth for `executable`.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// `process.capabilities`, verbatim OCI (`LinuxCapabilities` — the five sets
    /// `bounding`/`effective`/`inheritable`/`permitted`/`ambient`, each an
    /// optional set of `CAP_*` names). `None` → not emitted (the OCI runtime's
    /// default applies); `Some` → exactly those sets, an absent set left absent
    /// and an empty set dropping all in it. The engine passes it straight
    /// through; the curated posture is a producer concern (`profiles.base`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<LinuxCapabilities>,
    /// `process.rlimits`, verbatim OCI.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rlimits: Vec<PosixRlimit>,
    /// `process.noNewPrivileges`. Absent → not emitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub no_new_privileges: Option<bool>,
    /// `process.oomScoreAdj` — OOM-killer bias. Absent → not emitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oom_score_adj: Option<i32>,
}

/// Linux-specific knobs (OCI `linux`). Every field is a faithful pass-through;
/// `absent = absent`. The floor these carry comes from a producer, not the engine.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Linux {
    /// `linux.namespaces`, verbatim OCI (`{ type, path? }`). A `path` **joins** an
    /// existing namespace (host's, a pod peer's, a `pasta`/`slirp` netns); omit it
    /// for a fresh private one. Absent → shares the host's.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub namespaces: Vec<LinuxNamespace>,
    /// `linux.uidMappings`. `host` may be `"self"` (the invoking user's id,
    /// resolved at launch) or a concrete number.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub uid_mappings: Vec<IdMapping>,
    /// `linux.gidMappings`. Same `"self"`/numeric grammar.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gid_mappings: Vec<IdMapping>,
    /// `linux.maskedPaths` — paths hidden inside the sandbox.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub masked_paths: Vec<String>,
    /// `linux.readonlyPaths` — paths remounted read-only inside the sandbox.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub readonly_paths: Vec<String>,
    /// `linux.rootfsPropagation` (e.g. `rprivate`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rootfs_propagation: Option<String>,
    /// `linux.seccomp`, verbatim OCI. The engine loads it as-is; the curated
    /// profile is a producer concern.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seccomp: Option<LinuxSeccomp>,
    /// `linux.devices`, verbatim OCI (rootless binds the node; no cgroup allowlist).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub devices: Vec<LinuxDevice>,
    /// `linux.resources`, verbatim OCI. Needs cgroup-v2 delegation to enforce
    /// rootless — an unenforceable limit is crun's to reject.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<LinuxResources>,
    /// `linux.cgroupsPath`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cgroups_path: Option<PathBuf>,
    /// `linux.sysctl` — namespaced sysctls only, rootless. `BTreeMap` for
    /// deterministic artifacts; the producer only sets in-namespace-settable keys.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub sysctl: BTreeMap<String, String>,
    /// `linux.netDevices` — host netdevs to move into the sandbox netns.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub net_devices: BTreeMap<String, LinuxNetDevice>,
    /// `linux.personality` — execution domain (e.g. `LINUX32`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub personality: Option<LinuxPersonality>,
}

/// The Nix store view: what the closure was traversed *from* (`seeds`, a
/// build-time input) and — once built — what was locked *in* (`resolved`).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Closure {
    /// The seed store paths the closure is traversed *from* — a build-time input
    /// (conventionally `root.path` plus any overlay-lower sources).
    #[serde(default)]
    pub seeds: Vec<StorePath>,
    /// The flat identity-bind set, canonically sorted, the full closure of
    /// `seeds` including `root.path`. `Some` ⇒ built; `None` ⇒ unbuilt (`run`
    /// fails loud — build with `resolve --graph`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved: Option<Vec<StorePath>>,
}

/// One supervised `xdg-dbus-proxy` bus/socket pair.
///
/// **The engine mirrors the proxy, holds no policy.** Just as the rest of the
/// spec mirrors the OCI runtime (the realizer for everything else), the `dbus`
/// section mirrors `xdg-dbus-proxy`'s own model — its realizer. So the shape is
/// a *list* of bus/socket pairs (the proxy takes N `BUSADDR SOCKETPATH` groups),
/// and the policy is *rule-kind-major* (one bucket per proxy flag), a literal
/// transcription of the CLI rather than any grouping pierogi invents. The Nix
/// producer layer (`profiles.*`) is the policy engine — it fills the buckets; the
/// engine only translates them to argv and supervises the process.
///
/// **Why a list.** Session + system are two distinct buses at two distinct
/// in-sandbox paths, each with an independent allowlist (`docs/architecture.md`,
/// `docs/roadmap.md` M2); the a11y bus or a custom address are occasional further
/// entries. A fixed `{ session, system }` struct would bake "there are exactly
/// two well-known buses" into the engine — a policy-shaped assumption that belongs
/// in Nix. The list mirrors the proxy and lets the producer decide how many
/// entries exist and what each is. (How the engine runs the list — one process
/// with N pairs vs. N processes — is a supervision detail, invisible here.)
///
/// **`--filter` is always engaged** — mechanism, not policy. An entry with an
/// empty [`DbusPolicy`] still filters: the sandbox can connect and complete the
/// SASL handshake but talk to nothing. A non-filtering proxy would be a raw
/// socket with a uid fix — the exact escape hatch the project refuses — so
/// "the filter is on" is the engine's mechanism; the *rules* are the policy, and
/// they live here in the spec.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DbusProxy {
    /// The host bus the proxy connects to (its upstream). A [`BusAddress`].
    pub bus: BusAddress,
    /// Where the proxy's filtered socket lands **inside** the sandbox — a baked
    /// mountpoint the engine binds the socket onto (e.g. `/run/user/1000/bus`
    /// for the session bus, `/run/dbus/system_bus_socket` for the system bus).
    /// The client reaches the bus here; the host bus address never leaks in.
    pub socket: PathBuf,
    /// The allowlist. Absent ⇒ deny-all (connect + authenticate, talk to
    /// nothing) — the filter is still engaged (see the type note).
    #[serde(default)]
    pub policy: DbusPolicy,
}

/// The upstream host bus of a [`DbusProxy`] — where the proxy connects.
///
/// `Session`/`System` are launch-time sentinels, the [`HostId::Invoker`]
/// precedent applied to the bus: the spec records the auditable word
/// (`"session"`) while the engine resolves the concrete host path at launch
/// (`$DBUS_SESSION_BUS_ADDRESS`, or `/run/user/<uid>/bus`; the well-known system
/// socket) — so the host address is never baked into the artifact. `Address`
/// carries an explicit `unix:path=…`/`unix:abstract=…` bus address verbatim (the
/// a11y bus, a nested bus, a test daemon).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BusAddress {
    /// `"session"` — the user's session bus, resolved at launch.
    Session,
    /// `"system"` — the machine's system bus, resolved at launch.
    System,
    /// An explicit D-Bus address (`unix:path=…`), verbatim.
    Address(String),
}

impl Serialize for BusAddress {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            BusAddress::Session => s.serialize_str("session"),
            BusAddress::System => s.serialize_str("system"),
            BusAddress::Address(addr) => s.serialize_str(addr),
        }
    }
}

impl<'de> Deserialize<'de> for BusAddress {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        // The sentinels are bare words; a real bus address always contains a
        // transport prefix (`unix:`, `tcp:`, …), so there is no collision.
        let s = String::deserialize(de)?;
        Ok(match s.as_str() {
            "session" => BusAddress::Session,
            "system" => BusAddress::System,
            _ => BusAddress::Address(s),
        })
    }
}

/// A D-Bus allowlist — **rule-kind-major**, one bucket per `xdg-dbus-proxy`
/// policy flag, a literal transcription of the CLI (`docs/architecture.md`'s
/// pure-realizer principle: the engine mirrors the tool, the Nix layer curates).
///
/// Access is hierarchical `see < talk < own`; `call`/`broadcast` add
/// method/signal-level rules on top of a name the app can already talk to. Names
/// accept `xdg-dbus-proxy` wildcards (`org.foo.*`). Every bucket is additive and
/// defaults empty — nothing here is a floor the engine supplies.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DbusPolicy {
    /// `--see=NAME` — the app may know the name exists (enumerate / `GetNameOwner`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub see: Vec<String>,
    /// `--talk=NAME` — the app may send messages to the name.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub talk: Vec<String>,
    /// `--own=NAME` — the app may own the name (and receive messages to it).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub own: Vec<String>,
    /// `--call=NAME=RULE` — method-call rules per name. Each rule in the list
    /// becomes one `--call=NAME=RULE` flag (e.g. `Notify`, `Interface.*`, `*`).
    /// `BTreeMap` for deterministic artifacts.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub call: BTreeMap<String, Vec<String>>,
    /// `--broadcast=NAME=RULE` — signal rules per name, same per-name shape as
    /// `call`. Governs which broadcast signals the app may receive.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub broadcast: BTreeMap<String, Vec<String>>,
}

/// One `linux.{uid,gid}Mappings` entry: `container` id → `host` id, `size` wide.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IdMapping {
    pub container: u32,
    pub host: HostId,
    pub size: u32,
}

/// The host side of an [`IdMapping`]: a concrete id, or `"self"` — the invoking
/// user's id, resolved by the engine at launch. A build can't name the invoker's
/// uid numerically, so `"self"` lets `profiles.base` declare the map at build
/// time and keep it in the artifact.
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
    fn unbuilt_spec_mirrors_oci_structure() {
        // Nested like the OCI spec; only the fields present, pass-throughs absent.
        let json = r#"{
            "root": { "path": "/nix/store/aaaa-base-env" },
            "process": { "executable": "bash", "args": ["-l"], "terminal": true,
                         "env": { "PATH": "/bin" } },
            "mounts": [ { "destination": "/tmp", "type": "tmpfs", "options": ["nosuid","nodev"] } ],
            "closure": { "seeds": [ "/nix/store/aaaa-base-env" ] }
        }"#;
        let spec: Spec = serde_json::from_str(json).unwrap();
        assert_eq!(spec.process.executable, "bash");
        assert!(spec.process.terminal);
        assert_eq!(spec.process.env.get("PATH").unwrap(), "/bin");
        assert_eq!(spec.closure.seeds.len(), 1);
        assert!(spec.closure.resolved.is_none());
        // absent = absent, at every level.
        assert!(spec.root.readonly.is_none());
        assert!(spec.process.no_new_privileges.is_none());
        assert!(spec.process.capabilities.is_none());
        assert!(spec.linux.namespaces.is_empty());
        let back = serde_json::to_string(&spec).unwrap();
        assert!(!back.contains("readonly"));
        assert!(!back.contains("namespaces"));
    }

    #[test]
    fn passthrough_floor_and_full_process_linux() {
        let json = r#"{
            "root": { "path": "/nix/store/aaaa-r", "readonly": true },
            "process": {
                "executable": "bash", "no_new_privileges": true,
                "capabilities": {
                    "bounding": ["CAP_NET_BIND_SERVICE"], "effective": ["CAP_NET_BIND_SERVICE"],
                    "permitted": ["CAP_NET_BIND_SERVICE"], "inheritable": [], "ambient": []
                },
                "oom_score_adj": -500,
                "user": { "uid": 1000, "gid": 100, "umask": 18, "additionalGids": [5], "username": "dev" },
                "rlimits": [ { "type": "RLIMIT_CORE", "soft": 0, "hard": 0 } ]
            },
            "linux": {
                "rootfs_propagation": "rprivate",
                "masked_paths": [ "/proc/kcore" ],
                "namespaces": [ { "type": "user" }, { "type": "network", "path": "/proc/9/ns/net" } ],
                "uid_mappings": [ { "container": 0, "host": "self", "size": 1 } ],
                "gid_mappings": [ { "container": 0, "host": 100, "size": 1 } ],
                "resources": { "memory": { "limit": 536870912 }, "pids": { "limit": 128 } },
                "sysctl": { "net.ipv4.ip_forward": "0" }
            },
            "domainname": "box.local",
            "annotations": { "org.pierogi/built": "test" }
        }"#;
        let spec: Spec = serde_json::from_str(json).unwrap();
        assert_eq!(spec.root.readonly, Some(true));
        // The full five-set structure: only NET_BIND_SERVICE kept, others empty.
        let caps = spec.process.capabilities.as_ref().unwrap();
        assert!(caps
            .bounding()
            .as_ref()
            .unwrap()
            .contains(&oci_spec::runtime::Capability::NetBindService));
        assert!(caps.ambient().as_ref().unwrap().is_empty()); // dropped
        assert_eq!(spec.process.oom_score_adj, Some(-500));
        assert_eq!(spec.process.user.as_ref().unwrap().username().as_deref(), Some("dev"));
        assert_eq!(spec.linux.masked_paths, ["/proc/kcore"]);
        assert_eq!(spec.linux.namespaces.len(), 2);
        assert!(spec.linux.namespaces[1].path().is_some());
        assert_eq!(spec.linux.uid_mappings[0].host, HostId::Invoker);
        assert_eq!(spec.linux.gid_mappings[0].host, HostId::Numeric(100));
        assert!(spec.linux.resources.is_some());
        assert_eq!(spec.linux.sysctl.get("net.ipv4.ip_forward").unwrap(), "0");
        assert_eq!(spec.domainname.as_deref(), Some("box.local"));
        // round-trips (self stays "self", the capability sets survive).
        let back = serde_json::to_string(&spec).unwrap();
        assert!(back.contains(r#""host":"self""#));
        let _again: Spec = serde_json::from_str(&back).unwrap();
    }

    #[test]
    fn unknown_field_is_rejected_at_every_level() {
        // deny_unknown_fields: an unmodeled OCI field fails loud, not silently.
        let top = r#"{ "root": {"path":"/nix/store/a"}, "process": {"executable":"b"}, "resourcez": {} }"#;
        assert!(serde_json::from_str::<Spec>(top).is_err());
        let proc = r#"{ "root": {"path":"/nix/store/a"}, "process": {"executable":"b","scheduler":{}} }"#;
        assert!(serde_json::from_str::<Spec>(proc).is_err());
        let lin = r#"{ "root": {"path":"/nix/store/a"}, "process": {"executable":"b"}, "linux": {"intelRdt":{}} }"#;
        assert!(serde_json::from_str::<Spec>(lin).is_err());
    }

    #[test]
    fn id_mapping_host_rejects_bogus_string() {
        let json = r#"{ "container": 0, "host": "root", "size": 1 }"#;
        assert!(serde_json::from_str::<IdMapping>(json).is_err());
    }

    #[test]
    fn dbus_absent_is_absent() {
        // No `dbus` ⇒ empty list, and it doesn't serialize back (deny-all is the
        // absence of a proxy, not a value the engine emits).
        let json = r#"{ "root": {"path":"/nix/store/a"}, "process": {"executable":"b"} }"#;
        let spec: Spec = serde_json::from_str(json).unwrap();
        assert!(spec.dbus.is_empty());
        assert!(!serde_json::to_string(&spec).unwrap().contains("dbus"));
    }

    #[test]
    fn dbus_session_and_system_with_independent_policy() {
        // The canonical two-bus case: session + system, each independently
        // filtered, at distinct in-sandbox sockets.
        let json = r#"{
            "root": { "path": "/nix/store/aaaa-r" },
            "process": { "executable": "app" },
            "dbus": [
                {
                    "bus": "session",
                    "socket": "/run/user/1000/bus",
                    "policy": {
                        "talk": ["org.freedesktop.Notifications"],
                        "own": ["org.mpris.MediaPlayer2.app"],
                        "call": { "org.freedesktop.Notifications": ["Notify"] },
                        "broadcast": { "org.freedesktop.Notifications": ["*"] }
                    }
                },
                {
                    "bus": "system",
                    "socket": "/run/dbus/system_bus_socket",
                    "policy": { "see": ["org.freedesktop.login1"] }
                },
                { "bus": "unix:path=/tmp/a11y", "socket": "/run/user/1000/at-spi/bus" }
            ]
        }"#;
        let spec: Spec = serde_json::from_str(json).unwrap();
        assert_eq!(spec.dbus.len(), 3);

        let session = &spec.dbus[0];
        assert_eq!(session.bus, BusAddress::Session);
        assert_eq!(session.socket, PathBuf::from("/run/user/1000/bus"));
        assert_eq!(session.policy.talk, ["org.freedesktop.Notifications"]);
        assert_eq!(
            session.policy.call.get("org.freedesktop.Notifications").unwrap(),
            &["Notify"]
        );

        assert_eq!(spec.dbus[1].bus, BusAddress::System);
        // A third bus with no `policy` ⇒ deny-all (filter engaged, empty buckets).
        assert_eq!(
            spec.dbus[2].bus,
            BusAddress::Address("unix:path=/tmp/a11y".into())
        );
        assert!(spec.dbus[2].policy.talk.is_empty());

        // Sentinels round-trip as bare words; an explicit address stays verbatim.
        let back = serde_json::to_string(&spec).unwrap();
        assert!(back.contains(r#""bus":"session""#));
        assert!(back.contains(r#""bus":"system""#));
        assert!(back.contains(r#""bus":"unix:path=/tmp/a11y""#));
        let _again: Spec = serde_json::from_str(&back).unwrap();
    }

    #[test]
    fn dbus_rejects_unknown_policy_field() {
        // deny_unknown_fields: a mistyped rule kind fails loud, not silently.
        let json = r#"{
            "root": { "path": "/nix/store/a" },
            "process": { "executable": "b" },
            "dbus": [ { "bus": "session", "socket": "/run/user/1000/bus",
                        "policy": { "tolk": ["org.foo"] } } ]
        }"#;
        assert!(serde_json::from_str::<Spec>(json).is_err());
    }
}
