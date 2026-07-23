//! `resolve` — attach (or assert) a **built** closure on a spec.
//!
//! There is no live `nix-store` query: pierogi leans fully into pre-baking. A
//! spec's `closure` is *supplied* at build time — from `closureInfo`, via
//! `pierogi resolve --closure` — validated, and stamped in. At run time a spec
//! must already carry its `closure`; [`resolve`] only asserts that, it never
//! computes one.
//!
//! `resolve` does not touch the `exec` target — that is resolved by the runtime
//! against the container's `PATH` at launch, in-sandbox, so only in-closure
//! binaries are found (no host-side lookup over the full store).

use std::path::PathBuf;

use anyhow::{bail, Context, Result};

use crate::spec::{Closure, Spec, StorePath};

/// The declared seeds of a spec: `root` plus every overlay-lower source. Used
/// only to stamp `closure.seeds` (provenance) — not to drive a query.
fn seeds(spec: &Spec) -> Vec<StorePath> {
    let mut seeds = vec![spec.root.clone()];
    seeds.extend(spec.mounts.iter().filter_map(|m| m.store_refs().cloned()));
    seeds
}

/// Assert `spec` carries a built `closure`. With the live query removed, an
/// unbuilt spec cannot be resolved at run time — pre-bake it first.
pub fn resolve(spec: &Spec) -> Result<()> {
    if spec.closure.is_none() {
        bail!(
            "spec has no built closure; pre-bake it with \
             `pierogi resolve <spec> --closure <store-paths>` before running"
        );
    }
    Ok(())
}

/// Build a spec's `closure` from a **supplied** closure list (the build-time /
/// `resolve --closure` entry). `seeds` is the declared seeds (provenance);
/// `resolved` is the validated, canonically-sorted bind set.
pub fn resolve_supplied(spec: &mut Spec, supplied: Vec<PathBuf>) -> Result<()> {
    if spec.closure.is_some() {
        bail!("spec already carries a closure; refusing to overwrite with --closure");
    }
    let mut resolved = validate_closure(supplied)?
        .into_iter()
        .map(StorePath::new)
        .collect::<Result<Vec<_>>>()
        .context("supplied closure contained a non-top-level store path")?;
    resolved.sort();
    spec.closure = Some(Closure {
        seeds: seeds(spec),
        resolved,
    });
    Ok(())
}

/// Validate a supplied closure list: non-empty and every entry under
/// `/nix/store`. Fails loud; the sole gate for every supplied source.
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
                terminal: false,
            },
            root: StorePath::new(root).unwrap(),
            mounts,
            env: BTreeMap::new(),
            hostname: "pierogi".into(),
            closure: None,
        }
    }

    #[test]
    fn seeds_are_root_plus_overlay_sources() {
        let spec = spec_with(
            "/nix/store/aaaa-root",
            vec![
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
        // root + overlay source; host/tmpfs contribute nothing.
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].to_string(), "/nix/store/aaaa-root");
        assert_eq!(s[1].to_string(), "/nix/store/cccc-proj");
    }

    #[test]
    fn resolve_supplied_stamps_seeds_and_resolved_sorted() {
        let mut spec = spec_with("/nix/store/aaaa-root", vec![]);
        // Supplied out of order — `resolved` must come back canonically sorted
        // so any two builds of the same inputs are byte-identical.
        resolve_supplied(
            &mut spec,
            vec![
                "/nix/store/dddd-dep".into(),
                "/nix/store/aaaa-root".into(),
                "/nix/store/bbbb-tool".into(),
            ],
        )
        .unwrap();
        let c = spec.closure.as_ref().unwrap();
        assert_eq!(c.seeds.len(), 1); // just root
        let as_str: Vec<String> = c.resolved.iter().map(|p| p.to_string()).collect();
        assert_eq!(
            as_str,
            [
                "/nix/store/aaaa-root",
                "/nix/store/bbbb-tool",
                "/nix/store/dddd-dep"
            ]
        );
    }

    #[test]
    fn resolve_supplied_rejects_non_store_path() {
        let mut spec = spec_with("/nix/store/aaaa-root", vec![]);
        assert!(resolve_supplied(&mut spec, vec!["/etc/passwd".into()]).is_err());
    }

    #[test]
    fn resolve_supplied_refuses_to_overwrite_a_present_closure() {
        let mut spec = spec_with("/nix/store/aaaa-root", vec![]);
        spec.closure = Some(Closure {
            seeds: vec![StorePath::new("/nix/store/aaaa-root").unwrap()],
            resolved: vec![StorePath::new("/nix/store/aaaa-root").unwrap()],
        });
        assert!(resolve_supplied(&mut spec, vec!["/nix/store/aaaa-root".into()]).is_err());
    }

    #[test]
    fn resolve_ok_when_present_bails_when_absent() {
        let mut spec = spec_with("/nix/store/aaaa-root", vec![]);
        // Unbuilt: no runtime query to fall back on, so it must fail loud.
        assert!(resolve(&spec).is_err());
        spec.closure = Some(Closure {
            seeds: vec![],
            resolved: vec![StorePath::new("/nix/store/aaaa-root").unwrap()],
        });
        assert!(resolve(&spec).is_ok());
    }
}
