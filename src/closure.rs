//! Compute (or accept) the Nix closure that becomes the sandbox's `/nix/store`.
//!
//! `closure_of()` is a **pluggable source** (see `docs/spec.md`):
//!
//! * **live** — `nix-store --query --requisites <seeds>`, one deduped call over
//!   all seeds. Needs the Nix daemon / DB.
//! * **supplied** — a precomputed list (the `PIEROGI_CLOSURE` bench stub here,
//!   or a `closureInfo` `store-paths` file in a Nix build, or `resolve
//!   --closure`). No query — the build sandbox has no daemon to ask.
//!
//! Both funnel through [`validate_closure`]: all under `/nix/store`, non-empty,
//! fail loud. A malformed supplied list aborts rather than silently falling
//! back to a query.

use std::ffi::OsStr;
use std::path::PathBuf;
use std::process::Command;

use anyhow::{bail, Context, Result};

use crate::spec::StorePath;

/// Bench/test escape hatch: a `:`-separated list of store paths that *replaces*
/// the live `nix-store --query --requisites` call. The query costs tens of ms,
/// which swamps the runtime-backend startup we want to measure — precompute the
/// closure once and feed it in here to isolate that. Not for production use.
const CLOSURE_OVERRIDE_ENV: &str = "PIEROGI_CLOSURE";

/// Compute the transitive runtime closure (requisites) of all `seeds`, deduped.
///
/// Honors the [`CLOSURE_OVERRIDE_ENV`] stub: if set, its paths are used
/// verbatim and `nix-store` is not run — the query-free "supplied" source
/// delivered via env.
pub fn closure_of(seeds: &[StorePath]) -> Result<Vec<PathBuf>> {
    if seeds.is_empty() {
        bail!("closure has no seeds (need at least `root`)");
    }

    if let Some(val) = std::env::var_os(CLOSURE_OVERRIDE_ENV) {
        return parse_closure_override(&val);
    }

    let out = Command::new("nix-store")
        .arg("--query")
        .arg("--requisites")
        .args(seeds.iter().map(StorePath::as_path))
        .output()
        .context("running `nix-store --query --requisites` (is nix on PATH?)")?;

    if !out.status.success() {
        bail!(
            "nix-store failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }

    let paths = String::from_utf8(out.stdout)
        .context("nix-store output was not utf-8")?
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(PathBuf::from)
        .collect();

    validate_closure(paths)
}

/// Validate a closure list — the shared gate for every source (live query,
/// `PIEROGI_CLOSURE`, `resolve --closure`, `closureInfo`): non-empty and every
/// entry under `/nix/store`. Fails loud; never falls back to a query.
pub fn validate_closure(paths: Vec<PathBuf>) -> Result<Vec<PathBuf>> {
    if paths.is_empty() {
        bail!("empty closure");
    }
    for p in &paths {
        if !p.starts_with("/nix/store/") {
            bail!("closure entry {} is not under /nix/store", p.display());
        }
    }
    Ok(paths)
}

/// Parse the `PIEROGI_CLOSURE` stub: `:`-separated store paths.
fn parse_closure_override(val: &OsStr) -> Result<Vec<PathBuf>> {
    let paths = std::env::split_paths(val)
        .filter(|p| !p.as_os_str().is_empty())
        .collect();
    validate_closure(paths).context(CLOSURE_OVERRIDE_ENV)
}
