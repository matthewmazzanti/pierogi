//! Runtime backends: drive an OCI spec to a running, namespaced process.
//!
//! Two implementations sit behind one [`Backend`]:
//!   - [`youki`] — the `libcontainer` crate, in-process. Reads `config.json`
//!     from the bundle, so it serializes the spec to disk first.
//!   - [`crun`] — libcrun via an FFI shim (behind the `crun` feature). Takes the
//!     spec as an in-memory JSON string — no `config.json` round-trip.
//!
//! Both get the same laid-out rootfs (`bundle/rootfs`) and state dir; each
//! serializes the spec however it needs. `run` returns the container's exit
//! code.

use std::path::Path;

use anyhow::Result;
use oci_spec::runtime::Spec;

pub mod youki;

#[cfg(feature = "crun")]
pub mod crun;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Backend {
    #[default]
    Youki,
    #[cfg(feature = "crun")]
    Crun,
}

impl Backend {
    /// Parse a `--runtime` value, failing loudly if `crun` is requested but the
    /// feature wasn't compiled in.
    pub fn parse(name: &str) -> Result<Self> {
        match name {
            "youki" => Ok(Backend::Youki),
            #[cfg(feature = "crun")]
            "crun" => Ok(Backend::Crun),
            #[cfg(not(feature = "crun"))]
            "crun" => anyhow::bail!(
                "runtime `crun` not available: rebuild with `--features crun` inside `nix develop`"
            ),
            other => anyhow::bail!("unknown runtime `{other}` (expected `youki` or `crun`)"),
        }
    }

    /// Run `spec` to completion, returning the container's exit code.
    pub fn run(self, spec: &Spec, id: &str, bundle: &Path, state: &Path) -> Result<i32> {
        match self {
            Backend::Youki => youki::run(spec, id, bundle, state),
            #[cfg(feature = "crun")]
            Backend::Crun => crun::run(spec, id, bundle, state),
        }
    }
}
