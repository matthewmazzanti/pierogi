//! `resolve` — attach (or assert) a **built** closure on a spec.
//!
//! There is no live `nix-store` query: pierogi leans fully into pre-baking. At
//! build time [`resolve_from_graph`] takes Nix's `exportReferencesGraph` (the
//! reference graph of the seeds, as JSON), **traverses it from the spec's seeds
//! ourselves**, and stamps the resulting closure in. At run time a spec must
//! already carry its `closure`; [`resolve`] only asserts that, it never computes
//! one.
//!
//! The graph is delivered as the `closure` array of a `__structuredAttrs` JSON
//! file (`$NIX_ATTRS_JSON_FILE`) — the same shape `closureInfo` consumes: one
//! `{ path, references }` object per closure member. `--graph` can point straight
//! at `$NIX_ATTRS_JSON_FILE`; the rest of the attrs are ignored.
//!
//! Traversing the graph in-process (rather than trusting a flat `store-paths`
//! list) means `resolved` is provably exactly the closure of the declared seeds
//! — a tampered or mismatched graph is caught, and it's the graph we'll need for
//! the relocated-seed drop.
//!
//! `resolve` does not touch the `exec` target — that is resolved by the runtime
//! against the container's `PATH` at launch, in-sandbox, so only in-closure
//! binaries are found (no host-side lookup over the full store).

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{bail, Context, Result};
use serde::Deserialize;

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
             `pierogi resolve <spec> --graph <reference-graph>` before running"
        );
    }
    Ok(())
}

/// Build a spec's `closure` by traversing Nix's `exportReferencesGraph` JSON
/// from the spec's seeds (the build-time / `resolve --graph` entry). `seeds` is
/// the declared seeds (root + overlay lowers); `resolved` is the closure of
/// those seeds over the graph, canonically sorted.
pub fn resolve_from_graph(spec: &mut Spec, graph_json: &str) -> Result<()> {
    if spec.closure.is_some() {
        bail!("spec already carries a closure; refusing to overwrite");
    }
    let graph = parse_reference_graph(graph_json)?;
    let seeds = seeds(spec);
    let resolved = closure_of(&graph, &seeds)?;
    spec.closure = Some(Closure { seeds, resolved });
    Ok(())
}

/// Nix's `exportReferencesGraph` under `__structuredAttrs`: the `closure` array
/// of the structured-attrs JSON (`$NIX_ATTRS_JSON_FILE`), the same shape
/// `closureInfo` reads. `deny_unknown_fields` is *not* set, so `--graph` can be
/// the whole attrs file — the other attrs, and each node's `narSize`/`narHash`/
/// `valid`, are ignored.
#[derive(Deserialize)]
struct ReferenceGraph {
    closure: Vec<GraphNode>,
}

#[derive(Deserialize)]
struct GraphNode {
    path: String,
    references: Vec<String>,
}

/// Parse the `exportReferencesGraph` JSON into `path -> direct references`.
/// Covers the full closure of whatever roots Nix was given. Self-references are
/// normal (e.g. `glibc` lists itself) and harmless to the traversal.
fn parse_reference_graph(json: &str) -> Result<BTreeMap<String, Vec<String>>> {
    let graph: ReferenceGraph =
        serde_json::from_str(json).context("parsing the exportReferencesGraph JSON")?;
    if graph.closure.is_empty() {
        bail!("empty reference graph");
    }
    Ok(graph
        .closure
        .into_iter()
        .map(|n| (n.path, n.references))
        .collect())
}

/// Walk requisites from `seeds` over `graph`, returning the sorted closure as
/// validated `StorePath`s. Fails loud if a seed or reference is absent from the
/// graph (an incomplete `exportReferencesGraph`) or isn't a top-level store path.
fn closure_of(
    graph: &BTreeMap<String, Vec<String>>,
    seeds: &[StorePath],
) -> Result<Vec<StorePath>> {
    let mut visited: BTreeSet<String> = BTreeSet::new();
    let mut stack: Vec<String> = seeds.iter().map(StorePath::to_string).collect();
    while let Some(p) = stack.pop() {
        if !visited.insert(p.clone()) {
            continue;
        }
        let refs = graph.get(&p).ok_or_else(|| {
            anyhow::anyhow!("{p} is not in the reference graph (incomplete exportReferencesGraph?)")
        })?;
        for r in refs {
            if !visited.contains(r) {
                stack.push(r.clone());
            }
        }
    }
    // BTreeSet iterates sorted → canonical order.
    visited
        .into_iter()
        .map(StorePath::new)
        .collect::<Result<Vec<_>>>()
        .context("reference graph contained a non-top-level store path")
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

    // A three-node reference graph (exportReferencesGraph JSON shape): root ->
    // {dep, self}, dep -> {leaf, self}, leaf -> {}. Self-refs are normal. Extra
    // node keys (narSize, etc.) and extra attrs would be ignored.
    const GRAPH: &str = r#"{ "closure": [
        { "path": "/nix/store/aaaa-root", "references": ["/nix/store/bbbb-dep", "/nix/store/aaaa-root"] },
        { "path": "/nix/store/bbbb-dep",  "references": ["/nix/store/cccc-leaf", "/nix/store/bbbb-dep"] },
        { "path": "/nix/store/cccc-leaf", "references": [] }
    ] }"#;

    #[test]
    fn resolve_from_graph_traverses_closure_from_seeds_sorted() {
        let mut spec = spec_with("/nix/store/aaaa-root", vec![]);
        resolve_from_graph(&mut spec, GRAPH).unwrap();
        let c = spec.closure.as_ref().unwrap();
        assert_eq!(c.seeds.len(), 1); // just root
        let as_str: Vec<String> = c.resolved.iter().map(|p| p.to_string()).collect();
        // Full closure of root, canonically sorted.
        assert_eq!(
            as_str,
            [
                "/nix/store/aaaa-root",
                "/nix/store/bbbb-dep",
                "/nix/store/cccc-leaf"
            ]
        );
    }

    #[test]
    fn resolve_from_graph_excludes_paths_unreachable_from_seeds() {
        // A graph with an orphan nothing references; traversal from a seed that
        // only reaches `leaf` must omit both the orphan and the higher nodes.
        let graph = r#"{ "closure": [
            { "path": "/nix/store/aaaa-root", "references": ["/nix/store/bbbb-dep"] },
            { "path": "/nix/store/bbbb-dep",  "references": [] },
            { "path": "/nix/store/cccc-leaf", "references": [] },
            { "path": "/nix/store/zzzz-orphan", "references": [] }
        ] }"#;
        let mut spec = spec_with("/nix/store/cccc-leaf", vec![]);
        resolve_from_graph(&mut spec, graph).unwrap();
        let as_str: Vec<String> = spec
            .closure
            .as_ref()
            .unwrap()
            .resolved
            .iter()
            .map(|p| p.to_string())
            .collect();
        assert_eq!(as_str, ["/nix/store/cccc-leaf"]); // no orphan, no root/dep
    }

    #[test]
    fn resolve_from_graph_bails_when_a_seed_is_absent_from_the_graph() {
        let mut spec = spec_with("/nix/store/zzzz-absent", vec![]);
        assert!(resolve_from_graph(&mut spec, GRAPH).is_err());
    }

    #[test]
    fn resolve_from_graph_refuses_to_overwrite_a_present_closure() {
        let mut spec = spec_with("/nix/store/aaaa-root", vec![]);
        spec.closure = Some(Closure {
            seeds: vec![StorePath::new("/nix/store/aaaa-root").unwrap()],
            resolved: vec![StorePath::new("/nix/store/aaaa-root").unwrap()],
        });
        assert!(resolve_from_graph(&mut spec, GRAPH).is_err());
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
