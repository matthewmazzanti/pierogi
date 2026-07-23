//! The typed pierogi **Spec** — the v0 schema (see `docs/spec.md`).
//!
//! One descriptive, complete account of a single sandbox: what to run, what
//! filesystem it sees, its environment, and — once resolved — the exact set of
//! store paths to bind. It carries **no policy**: the engine is a pure realizer
//! that binds what the spec says and `exec`s.
//!
//! This replaces M0's ad-hoc `SpecInputs`. It is the *pierogi* spec, distinct
//! from the OCI runtime spec we translate it into (`crate::oci`).

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

/// A top-level Nix store path (`/nix/store/<hash-name>`).
///
/// A newtype so the schema can make "this must be a store path" a property of
/// the *type* rather than a runtime check scattered across the resolver —
/// `root` and the `Store`/`Overlay` mount sources are `StorePath`, while a
/// `Host` source is an opaque `PathBuf`. Validated on construction and on
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

/// One sandbox, described completely.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Spec {
    /// What to run.
    pub exec: Exec,
    /// The **rootfs**: a sealed store path mounted at `/` (remounted read-only)
    /// carrying the whole skeleton — `/etc`, `/bin`, and every baked mountpoint.
    /// Where `PATH` conventionally points, and the closure's provenance seed.
    /// Excluded from the `/nix/store` binds (it's the `/` mount).
    pub root: StorePath,
    /// Additive placements layered onto `root` (host / tmpfs / overlay). Each
    /// binds onto a mountpoint `root` already baked; none folds a closure.
    #[serde(default)]
    pub mounts: Vec<Mount>,
    /// Producer-filled key/values; `PATH` lives here and is the single source
    /// of truth for `exec` lookup. The engine merges nothing.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// The sandbox's UTS hostname (OCI `hostname`). Defaults to `pierogi`.
    #[serde(default = "default_hostname")]
    pub hostname: String,
    /// The **built** store view — `{ seeds, resolved }`. `Some` ⇒ pre-baked and
    /// engine-ready; `None` ⇒ unbuilt (there is no runtime query, so `run`
    /// requires it — build with `resolve --graph`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closure: Option<Closure>,
}

/// The default UTS hostname when a spec omits one.
fn default_hostname() -> String {
    "pierogi".to_string()
}

/// The process to run.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Exec {
    /// A bare name (resolved via `env.PATH`) or an absolute store path.
    /// Validated *against* the closure, never a source for it.
    pub executable: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// Allocate a controlling terminal (OCI `process.terminal`). When `true`
    /// the runtime sets up a pty and makes it the process's controlling tty,
    /// enabling job control. Default `false`: inherit the host stdio fds
    /// directly — a working shell, but no job control.
    #[serde(default)]
    pub terminal: bool,
}

/// The built store view: where it was resolved *from* and what was locked *in*.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Closure {
    /// Provenance stamp: the seed store paths this closure was built from
    /// (`root` plus any overlay-lower sources). Not an input to anything at
    /// runtime — the query is gone — but a fingerprint you can parity-check
    /// against a fresh build out-of-band.
    pub seeds: Vec<StorePath>,
    /// The flat identity-bind set bound under `/nix/store`, **canonically
    /// sorted** so any two builds of the same inputs are byte-identical. The
    /// full closure of `seeds`, **including `root`** — root is identity-bound at
    /// `/nix/store/<self>` *and* mounted at `/`. Filled at build by `resolve
    /// --graph`; `run` requires it.
    pub resolved: Vec<StorePath>,
}

/// An additive **placement** layered onto `root`. Every variant just puts
/// something at a `dest` the sealed `root` already baked — nothing folds a
/// closure (the `closure` is the authoritative, build-locked store view; a
/// runtime store addition would need an un-baked mountpoint). See `docs/spec.md`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Mount {
    /// Bind opaquely; source may be anything, including `/nix/store` (e.g. a
    /// relocated store path — its deps are covered by `closure`, not by this
    /// mount). Read-only unless `rw`.
    Host {
        source: PathBuf,
        dest: PathBuf,
        #[serde(default)]
        rw: bool,
    },
    /// Writable scratch; no source.
    Tmpfs { dest: PathBuf },
    /// A writable overlay over a store lower (already in `closure`), presenting
    /// a read-write merge at `dest`. `upper` omitted → ephemeral tmpfs;
    /// `Some(dir)` → persistent host dir. (Realization deferred within M1.)
    Overlay {
        source: StorePath,
        dest: PathBuf,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        upper: Option<PathBuf>,
    },
}

impl Mount {
    /// The store-path source this mount references as a closure seed: only
    /// `Overlay` (its lower); `Host`/`Tmpfs` contribute none. Used at build time
    /// to stamp `closure.seeds`, not to drive any runtime query.
    pub fn store_refs(&self) -> Option<&StorePath> {
        match self {
            Mount::Overlay { source, .. } => Some(source),
            Mount::Host { .. } | Mount::Tmpfs { .. } => None,
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
    fn unresolved_spec_round_trips() {
        let json = r#"{
            "exec": { "executable": "bash", "args": ["-l"] },
            "root": "/nix/store/aaaa-base-env",
            "mounts": [
                { "type": "host",  "source": "/etc/resolv.conf", "dest": "/etc/resolv.conf" },
                { "type": "tmpfs", "dest": "/tmp" }
            ],
            "env": { "PATH": "/bin" }
        }"#;
        let spec: Spec = serde_json::from_str(json).unwrap();
        assert!(spec.closure.is_none());
        assert_eq!(spec.mounts.len(), 2);
        // Defaults when omitted: no terminal, hostname `pierogi`.
        assert!(!spec.exec.terminal);
        assert_eq!(spec.hostname, "pierogi");
        // host/tmpfs contribute no seeds.
        let seeds: Vec<_> = spec.mounts.iter().filter_map(Mount::store_refs).collect();
        assert!(seeds.is_empty());

        // absent closure stays absent through a serialize round-trip
        let back = serde_json::to_string(&spec).unwrap();
        assert!(!back.contains("closure"));
        let again: Spec = serde_json::from_str(&back).unwrap();
        assert_eq!(again.mounts.len(), 2);
    }

    #[test]
    fn closure_round_trips_as_seeds_and_resolved() {
        let json = r#"{
            "exec": { "executable": "bash" },
            "root": "/nix/store/aaaa-base-env",
            "closure": {
                "seeds": [ "/nix/store/aaaa-base-env" ],
                "resolved": [ "/nix/store/0000-bash", "/nix/store/1111-glibc" ]
            }
        }"#;
        let spec: Spec = serde_json::from_str(json).unwrap();
        let c = spec.closure.as_ref().unwrap();
        assert_eq!(c.seeds.len(), 1);
        assert_eq!(c.resolved.len(), 2);
    }
}
