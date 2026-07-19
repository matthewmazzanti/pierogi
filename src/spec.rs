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
    /// Required base: the closure seed and where `PATH` conventionally points.
    pub root: StorePath,
    /// Additive binds layered onto the root.
    #[serde(default)]
    pub mounts: Vec<Mount>,
    /// Producer-filled key/values; `PATH` lives here and is the single source
    /// of truth for `exec` lookup. The engine merges nothing.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// The memoized resolution output. `Some` ⇒ engine-ready, no Nix query;
    /// `None` ⇒ `resolve()` computes it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closure: Option<Resolved>,
}

/// The process to run.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Exec {
    /// A bare name (resolved via `env.PATH`) or an absolute store path.
    /// Validated *against* the closure, never a source for it.
    pub executable: String,
    #[serde(default)]
    pub args: Vec<String>,
}

/// The resolved bind set plus its input stamp — the production form of the
/// `PIEROGI_CLOSURE` bench stub.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Resolved {
    /// Input stamp: the seeds this closure was resolved from (`root` +
    /// store-mount / overlay sources). A query-free drift fingerprint.
    pub from: Vec<StorePath>,
    /// The flat identity-bind set (what actually gets bound). May omit a
    /// relocated store-mount source that nothing references (`from ⊋ paths`).
    pub paths: Vec<StorePath>,
}

/// An additive mount. The variants are distinguished by **whether the closure
/// folds**, not by where the source lives (see `docs/spec.md`).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Mount {
    /// Bind + **fold** the source's closure; source must be a store path.
    /// `dest` omitted → identity bind at `/nix/store/<source>`; present →
    /// relocate to `dest`. Always read-only (the store is immutable).
    Store {
        source: StorePath,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        dest: Option<PathBuf>,
    },
    /// Bind opaquely, **no** fold; source may be anything, including
    /// `/nix/store`. Read-only unless `rw`.
    Host {
        source: PathBuf,
        dest: PathBuf,
        #[serde(default)]
        rw: bool,
    },
    /// Writable scratch; no source.
    Tmpfs { dest: PathBuf },
    /// A `Store` mount, but writable: fold the lower's closure, present a
    /// read-write merge at `dest`. `upper` omitted → ephemeral tmpfs;
    /// `Some(dir)` → persistent host dir. (Realization deferred within M1.)
    Overlay {
        source: StorePath,
        dest: PathBuf,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        upper: Option<PathBuf>,
    },
}

impl Mount {
    /// The store-path references this mount contributes as closure seeds:
    /// `Store` and `Overlay` give their `source`; `Host`/`Tmpfs` give none.
    pub fn store_refs(&self) -> Option<&StorePath> {
        match self {
            Mount::Store { source, .. } | Mount::Overlay { source, .. } => Some(source),
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
                { "type": "store", "source": "/nix/store/bbbb-ripgrep-14.1.0" },
                { "type": "store", "source": "/nix/store/cccc-myproject", "dest": "/home/dev/project" },
                { "type": "host",  "source": "/etc/resolv.conf", "dest": "/etc/resolv.conf" },
                { "type": "tmpfs", "dest": "/tmp" }
            ],
            "env": { "PATH": "/nix/store/aaaa-base-env/bin" }
        }"#;
        let spec: Spec = serde_json::from_str(json).unwrap();
        assert!(spec.closure.is_none());
        assert_eq!(spec.mounts.len(), 4);
        // seeds = root + the two store sources (host/tmpfs contribute none)
        let seeds: Vec<_> = spec.mounts.iter().filter_map(Mount::store_refs).collect();
        assert_eq!(seeds.len(), 2);

        // absent closure stays absent through a serialize round-trip
        let back = serde_json::to_string(&spec).unwrap();
        assert!(!back.contains("closure"));
        let again: Spec = serde_json::from_str(&back).unwrap();
        assert_eq!(again.mounts.len(), 4);
    }

    #[test]
    fn store_source_must_be_a_store_path() {
        let json = r#"{
            "exec": { "executable": "bash" },
            "root": "/nix/store/aaaa-base-env",
            "mounts": [ { "type": "store", "source": "/etc/passwd" } ]
        }"#;
        assert!(serde_json::from_str::<Spec>(json).is_err());
    }
}
