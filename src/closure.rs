//! Resolve a target binary into the Nix store and compute its runtime closure.
//!
//! This is the "closure calculation over bash" half of the spike: the set of
//! store paths returned here becomes the read-only rootfs (one bind mount per
//! path), so the sandbox sees *exactly* the target's closure and nothing else
//! of the host store.

use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

/// Bench/test escape hatch: a `:`-separated list of store paths that *replaces*
/// the `nix-store --query --requisites` call. The query costs tens of ms, which
/// swamps the runtime-backend startup we want to measure — precompute the
/// closure once and feed it in here to isolate that. Not for production use.
const CLOSURE_OVERRIDE_ENV: &str = "PIEROGI_CLOSURE";

/// Resolve a command (bare name or path) to a canonical executable that lives
/// in `/nix/store`.
pub fn resolve_store_exe(cmd: &str) -> Result<PathBuf> {
    let candidate = if cmd.contains('/') {
        PathBuf::from(cmd)
    } else {
        which(cmd).with_context(|| format!("`{cmd}` not found in PATH"))?
    };

    // Follow symlinks (e.g. a profile's bin/bash) down to the real store file.
    let exe = std::fs::canonicalize(&candidate)
        .with_context(|| format!("canonicalizing {}", candidate.display()))?;

    if !exe.starts_with("/nix/store/") {
        bail!(
            "{cmd} resolves to {} which is not in /nix/store; the spike requires \
             a store-backed binary so it can compute a closure",
            exe.display()
        );
    }
    Ok(exe)
}

fn which(cmd: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(cmd))
        .find(|full| full.is_file())
}

/// Reduce any path inside the store to its top-level store path
/// (`/nix/store/<hash-name>`), which is what `nix-store` queries operate on.
pub fn store_root(path: &Path) -> Result<PathBuf> {
    let mut normal = path.components().filter_map(|c| match c {
        Component::Normal(p) => Some(p.to_owned()),
        _ => None,
    });

    let (nix, store, name) = (normal.next(), normal.next(), normal.next());
    match (nix.as_deref(), store.as_deref(), name) {
        (Some(n), Some(s), Some(name)) if n == "nix" && s == "store" => {
            Ok(Path::new("/nix/store").join(name))
        }
        _ => bail!("not a store path: {}", path.display()),
    }
}

/// Compute the transitive runtime closure (requisites) of a store path.
///
/// Honors the [`CLOSURE_OVERRIDE_ENV`] stub: if set, its paths are used verbatim
/// and `nix-store` is not run.
pub fn closure_of(store_path: &Path) -> Result<Vec<PathBuf>> {
    if let Some(val) = std::env::var_os(CLOSURE_OVERRIDE_ENV) {
        return parse_closure_override(&val);
    }

    let out = Command::new("nix-store")
        .arg("--query")
        .arg("--requisites")
        .arg(store_path)
        .output()
        .context("running `nix-store --query --requisites` (is nix on PATH?)")?;

    if !out.status.success() {
        bail!(
            "nix-store failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }

    let paths: Vec<PathBuf> = String::from_utf8(out.stdout)
        .context("nix-store output was not utf-8")?
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(PathBuf::from)
        .collect();

    if paths.is_empty() {
        bail!("empty closure for {}", store_path.display());
    }
    Ok(paths)
}

/// Parse the `PIEROGI_CLOSURE` stub: `:`-separated store paths. Fails loud on an
/// empty list or any entry outside `/nix/store` — a bad stub should abort, not
/// silently fall back to a real query.
fn parse_closure_override(val: &OsStr) -> Result<Vec<PathBuf>> {
    let paths: Vec<PathBuf> = std::env::split_paths(val)
        .filter(|p| !p.as_os_str().is_empty())
        .collect();

    if paths.is_empty() {
        bail!("{CLOSURE_OVERRIDE_ENV} is set but lists no paths");
    }
    for p in &paths {
        if !p.starts_with("/nix/store/") {
            bail!(
                "{CLOSURE_OVERRIDE_ENV} entry {} is not under /nix/store",
                p.display()
            );
        }
    }
    Ok(paths)
}
