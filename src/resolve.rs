//! `resolve()` — turn a spec with `closure: None` into one with `closure:
//! Some`. This computes *the filesystem* (the store paths to bind); it does not
//! touch the `exec` target — that is resolved by the runtime against the
//! container's `PATH` at launch, in-sandbox, so only in-closure binaries are
//! found (no host-side lookup, which would search the host's full store).
//!
//! The closure has exactly **two declared seed kinds** (`docs/spec.md`):
//!
//! ```text
//! closure = closure_of(root) ∪ ⋃ closure_of(source)   for each store/overlay mount
//! ```
//!
//! Nothing is inferred from the command line: `exec` never *expands* the closure
//! (there is no exe-derived closure) — a target outside the closure simply isn't
//! present in the sandbox and fails at exec. A present `closure` short-circuits
//! the whole query (the sealed fast-path), no Nix call.

use std::collections::BTreeSet;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};

use crate::closure::{closure_of, validate_closure};
use crate::spec::{Resolved, Spec, StorePath};

/// The declared seeds of a spec: `root` plus every store/overlay mount source.
fn seeds(spec: &Spec) -> Vec<StorePath> {
    let mut seeds = vec![spec.root.clone()];
    seeds.extend(spec.mounts.iter().filter_map(|m| m.store_refs().cloned()));
    seeds
}

/// Fill `spec.closure` if absent. A present closure short-circuits the full
/// query (see [`resolve_sealed`] for the sealed-spec cases).
pub fn resolve(spec: &mut Spec) -> Result<()> {
    let seeds = seeds(spec);

    if spec.closure.is_some() {
        return resolve_sealed(spec, seeds);
    }

    // v0: identity-bind the whole closure — no relocated-seed drop yet (that
    // needs the reference graph; the extra binds are always safe). So
    // `paths` = the full closure, `from` = the seeds.
    let mut paths = closure_of(&seeds)?
        .into_iter()
        .map(StorePath::new)
        .collect::<Result<Vec<_>>>()
        .context("closure contained a non-top-level store path")?;
    paths.sort();

    spec.closure = Some(Resolved { from: seeds, paths });
    Ok(())
}

/// Handle a spec that already carries a `closure`, comparing its stamp
/// (`closure.from`) against the currently declared `seeds`:
///
/// * **Seeds unchanged** → sealed fast-path: no query at all.
/// * **New seeds beyond the stamp** → **fail loud.** A sealed spec's closure is
///   frozen; a seed it never resolved would need `/nix/store/<hash>` mountpoints
///   the sealed `root` never baked. There is no seal-break / delta-recompute —
///   dropping runtime store additions was the whole point (`--mount store:` is
///   gone; only a hand-edited sealed spec can reach here). Re-resolve from the
///   unsealed spec to change the closure.
/// * **Seeds only removed/changed, none added** → the extra sealed paths are
///   harmless; warn about drift and proceed with the sealed set as-is.
fn resolve_sealed(spec: &mut Spec, seeds: Vec<StorePath>) -> Result<()> {
    let closure = spec
        .closure
        .as_ref()
        .expect("caller checked closure is present");
    let stamped: BTreeSet<StorePath> = closure.from.iter().cloned().collect();

    let declared: BTreeSet<StorePath> = seeds.iter().cloned().collect();
    let added: Vec<StorePath> = declared.difference(&stamped).cloned().collect();

    if !added.is_empty() {
        bail!(
            "sealed spec gained {} seed(s) its closure never resolved \
             (e.g. {}); the closure is frozen at seal time. Re-resolve from the \
             unsealed spec to change it.",
            added.len(),
            added[0]
        );
    }

    if declared != stamped {
        eprintln!(
            "pierogi: warning: closure.from does not match declared seeds \
             (spec may have drifted since it was resolved)"
        );
    }
    Ok(())
}

/// Build a `Resolved` from a **supplied** closure list (no query): the build-
/// time / `resolve --closure` source. `from` is the declared seeds; `paths` is
/// the validated supplied list.
pub fn resolve_supplied(spec: &mut Spec, supplied: Vec<PathBuf>) -> Result<()> {
    if spec.closure.is_some() {
        bail!("spec already carries a closure; refusing to overwrite with --closure");
    }
    let mut paths = validate_closure(supplied)?
        .into_iter()
        .map(StorePath::new)
        .collect::<Result<Vec<_>>>()
        .context("supplied closure contained a non-top-level store path")?;
    paths.sort();
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
        // Supplied out of order — the stamp must come back canonically sorted
        // so build-time and live resolves are byte-identical.
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
        assert_eq!(c.from.len(), 2); // the two declared seeds
        assert_eq!(c.paths.len(), 3); // the supplied closure
        let as_str: Vec<String> = c.paths.iter().map(|p| p.to_string()).collect();
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

    #[test]
    fn resolve_bails_when_store_seed_added_to_sealed() {
        // A spec sealed to just `root`, now carrying an extra store mount seed
        // (only reachable by hand-editing — `--mount store:` is gone). The
        // closure is frozen at seal time, so this must fail loud, not recompute.
        let mut spec = spec_with(
            "/nix/store/aaaa-root",
            vec![Mount::Store {
                source: StorePath::new("/nix/store/bbbb-tool").unwrap(),
                dest: None,
            }],
        );
        spec.closure = Some(Resolved {
            from: vec![StorePath::new("/nix/store/aaaa-root").unwrap()],
            paths: vec![StorePath::new("/nix/store/aaaa-root").unwrap()],
        });

        // No Nix daemon is touched: the added seed is rejected before any query.
        let err = resolve(&mut spec).unwrap_err();
        assert!(err.to_string().contains("frozen"), "got: {err}");
    }
}
