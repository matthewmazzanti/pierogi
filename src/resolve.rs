//! `resolve()` — turn a spec with `closure: None` into one with `closure:
//! Some`, and resolve `exec` *within* that closure.
//!
//! The closure has exactly **two declared seed kinds** (`docs/spec.md`):
//!
//! ```text
//! closure = closure_of(root) ∪ ⋃ closure_of(source)   for each store/overlay mount
//! ```
//!
//! Nothing is inferred from the command line: `exec` is *resolved against* the
//! closure (found via `env.PATH`, asserted under a closure path) and errors if
//! absent — it never expands the closure. A present `closure` short-circuits
//! the whole query (the sealed fast-path); `exec` is still resolved against it,
//! host-side, no Nix call.

use std::collections::BTreeSet;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};

use crate::closure::{closure_of, store_root, validate_closure, which};
use crate::spec::{Resolved, Spec, StorePath};

/// The declared seeds of a spec: `root` plus every store/overlay mount source.
fn seeds(spec: &Spec) -> Vec<StorePath> {
    let mut seeds = vec![spec.root.clone()];
    seeds.extend(spec.mounts.iter().filter_map(|m| m.store_refs().cloned()));
    seeds
}

/// Fill `spec.closure` if absent. A present closure short-circuits the query;
/// we only run a cheap, query-free drift check on its stamp.
pub fn resolve(spec: &mut Spec) -> Result<()> {
    let seeds = seeds(spec);

    if let Some(closure) = &spec.closure {
        // Sealed fast-path: no query. Query-free drift check — `from` is the
        // seed set this closure was stamped from; if it no longer matches the
        // declared seeds, the spec drifted. Policy (error/warn/allow) is
        // deferred (see docs); v0 warns and proceeds.
        let stamped: BTreeSet<_> = closure.from.iter().collect();
        let declared: BTreeSet<_> = seeds.iter().collect();
        if stamped != declared {
            eprintln!(
                "pierogi: warning: closure.from does not match declared seeds \
                 (spec may have drifted since it was resolved)"
            );
        }
        return Ok(());
    }

    // v0: identity-bind the whole closure — no relocated-seed drop yet (that
    // needs the reference graph; the extra binds are always safe). So
    // `paths` = the full closure, `from` = the seeds.
    let paths = closure_of(&seeds)?
        .into_iter()
        .map(StorePath::new)
        .collect::<Result<Vec<_>>>()
        .context("closure contained a non-top-level store path")?;

    spec.closure = Some(Resolved { from: seeds, paths });
    Ok(())
}

/// Resolve `exec` to the absolute in-store executable that becomes `argv[0]`.
///
/// Requires a resolved spec. `executable` is a bare name (looked up via
/// `env.PATH`) or an absolute store path; either way it is canonicalized and
/// asserted to live under one of the closure's paths. Never a source for the
/// closure — absent ⇒ hard error.
pub fn resolve_exec(spec: &Spec) -> Result<PathBuf> {
    let closure = spec
        .closure
        .as_ref()
        .context("resolve_exec called on an unresolved spec")?;

    let exe = &spec.exec.executable;
    let candidate = if exe.contains('/') {
        PathBuf::from(exe)
    } else {
        let path = spec.env.get("PATH").map(String::as_str).unwrap_or("");
        which(path, exe).with_context(|| {
            format!("`{exe}` not found on env.PATH (a store mount folds the closure but does not extend PATH)")
        })?
    };

    // Canonicalize to *validate* — follow symlinks to the real store file and
    // assert it lives in the closure. This must NOT become argv[0]: for a
    // multi-call binary (coreutils, busybox) `bin/ls` canonicalizes to
    // `bin/coreutils`, and argv[0]'s basename is how the dispatcher picks the
    // applet. So we keep the *found* path as argv[0].
    let canon = std::fs::canonicalize(&candidate)
        .with_context(|| format!("canonicalizing exec {}", candidate.display()))?;
    let in_closure = |p: &std::path::Path| {
        store_root(p)
            .map(|root| closure.paths.iter().any(|c| c.as_path() == root))
            .unwrap_or(false)
    };
    if !in_closure(&canon) {
        bail!(
            "exec {} resolves to {} which is not in the resolved closure",
            exe,
            canon.display()
        );
    }

    // argv[0]: prefer the found path (preserves the applet name for multi-call
    // binaries) when it too lives in-closure and thus exists in the sandbox;
    // otherwise fall back to the canonical target (validated in-closure above).
    if in_closure(&candidate) {
        Ok(candidate)
    } else {
        Ok(canon)
    }
}

/// Build a `Resolved` from a **supplied** closure list (no query): the build-
/// time / `resolve --closure` source. `from` is the declared seeds; `paths` is
/// the validated supplied list.
pub fn resolve_supplied(spec: &mut Spec, supplied: Vec<PathBuf>) -> Result<()> {
    if spec.closure.is_some() {
        bail!("spec already carries a closure; refusing to overwrite with --closure");
    }
    let paths = validate_closure(supplied)?
        .into_iter()
        .map(StorePath::new)
        .collect::<Result<Vec<_>>>()
        .context("supplied closure contained a non-top-level store path")?;
    spec.closure = Some(Resolved {
        from: seeds(spec),
        paths,
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::{Exec, Mount};
    use std::collections::BTreeMap;

    fn spec_with(root: &str, mounts: Vec<Mount>) -> Spec {
        Spec {
            exec: Exec {
                executable: "bash".into(),
                args: vec![],
            },
            root: StorePath::new(root).unwrap(),
            mounts,
            env: BTreeMap::new(),
            closure: None,
        }
    }

    #[test]
    fn seeds_are_root_plus_store_and_overlay_sources() {
        let spec = spec_with(
            "/nix/store/aaaa-root",
            vec![
                Mount::Store {
                    source: StorePath::new("/nix/store/bbbb-tool").unwrap(),
                    dest: None,
                },
                Mount::Host {
                    source: "/etc/resolv.conf".into(),
                    dest: "/etc/resolv.conf".into(),
                    rw: false,
                },
                Mount::Tmpfs {
                    dest: "/tmp".into(),
                },
                Mount::Overlay {
                    source: StorePath::new("/nix/store/cccc-proj").unwrap(),
                    dest: "/work".into(),
                    upper: None,
                },
            ],
        );
        let s = seeds(&spec);
        // root + store source + overlay source; host/tmpfs contribute nothing.
        assert_eq!(s.len(), 3);
        assert_eq!(s[0].to_string(), "/nix/store/aaaa-root");
    }

    #[test]
    fn resolve_supplied_stamps_from_and_paths_without_query() {
        let mut spec = spec_with(
            "/nix/store/aaaa-root",
            vec![Mount::Store {
                source: StorePath::new("/nix/store/bbbb-tool").unwrap(),
                dest: None,
            }],
        );
        resolve_supplied(
            &mut spec,
            vec![
                "/nix/store/aaaa-root".into(),
                "/nix/store/bbbb-tool".into(),
                "/nix/store/dddd-dep".into(),
            ],
        )
        .unwrap();
        let c = spec.closure.as_ref().unwrap();
        assert_eq!(c.from.len(), 2); // the two declared seeds
        assert_eq!(c.paths.len(), 3); // the supplied closure
    }

    #[test]
    fn resolve_supplied_rejects_non_store_path() {
        let mut spec = spec_with("/nix/store/aaaa-root", vec![]);
        let err = resolve_supplied(&mut spec, vec!["/etc/passwd".into()]);
        assert!(err.is_err());
    }

    #[test]
    fn resolve_short_circuits_when_closure_present() {
        let mut spec = spec_with("/nix/store/aaaa-root", vec![]);
        spec.closure = Some(Resolved {
            from: vec![StorePath::new("/nix/store/aaaa-root").unwrap()],
            paths: vec![StorePath::new("/nix/store/aaaa-root").unwrap()],
        });
        // No Nix daemon is touched: a present closure returns Ok unchanged.
        resolve(&mut spec).unwrap();
        assert_eq!(spec.closure.as_ref().unwrap().paths.len(), 1);
    }
}
