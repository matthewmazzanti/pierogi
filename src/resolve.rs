//! `resolve` — stamp (or assert) the **built** closure on a spec.
//!
//! There is no live `nix-store` query: pierogi leans fully into pre-baking. The
//! spec's `closure.seeds` is a **build-time input** — the store paths to
//! traverse *from*. At build time [`resolve_from_graph`] takes Nix's
//! `exportReferencesGraph` (the reference graph of those seeds, as JSON),
//! **traverses it from `closure.seeds` ourselves**, and stamps the resulting
//! `closure.resolved` in. At run time a spec must already carry its
//! `closure.resolved`; [`resolve`] only asserts that, it never computes one.
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

use crate::spec::{Spec, StorePath};

/// Assert `spec` carries a built closure (`closure.resolved`). With the live
/// query removed, an unbuilt spec cannot be resolved at run time — pre-bake it.
pub fn resolve(spec: &Spec) -> Result<()> {
    if spec.closure.resolved.is_none() {
        bail!(
            "spec has no built closure; pre-bake it with \
             `pierogi resolve <spec> --graph <reference-graph>` before running"
        );
    }
    Ok(())
}

/// Stamp a spec's `closure.resolved` by traversing Nix's `exportReferencesGraph`
/// JSON from the spec's **build-time** `closure.seeds` (the `resolve --graph`
/// entry). `resolved` is the closure of those seeds over the graph, canonically
/// sorted. Fails loud if the seeds are empty or their closure doesn't cover
/// `root` (which is both the `/` mount and an identity bind, so it must be in).
pub fn resolve_from_graph(spec: &mut Spec, graph_json: &str) -> Result<()> {
    if spec.closure.resolved.is_some() {
        bail!("spec already carries a built closure (`closure.resolved`); refusing to overwrite");
    }
    if spec.closure.seeds.is_empty() {
        bail!(
            "spec declares no `closure.seeds`; the build must set them \
             (conventionally root plus any overlay lowers) before resolve"
        );
    }
    let graph = parse_reference_graph(graph_json)?;
    let resolved = closure_of(&graph, &spec.closure.seeds)?;
    if !resolved.contains(&spec.root) {
        bail!(
            "the seeds' closure does not cover root {}; add it to `closure.seeds`",
            spec.root
        );
    }
    spec.closure.resolved = Some(resolved);
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
    use crate::spec::Closure;
    use std::collections::BTreeMap;

    /// Build an unbuilt spec seeded from `root` (the common case). Tests that need
    /// other seeds set `spec.closure.seeds` directly — it's a producer-declared
    /// input, not something the engine derives.
    fn spec_with(root: &str) -> Spec {
        let root = StorePath::new(root).unwrap();
        Spec {
            executable: "bash".into(),
            args: vec![],
            tty: false,
            working_dir: None,
            hostname: None,
            closure: Closure {
                seeds: vec![root.clone()],
                resolved: None,
            },
            root,
            mounts: vec![],
            environment: BTreeMap::new(),
            read_only: None,
            no_new_privileges: None,
            capabilities: None,
            rlimits: vec![],
            masked_paths: vec![],
            readonly_paths: vec![],
            rootfs_propagation: None,
            namespaces: vec![],
            uid_mappings: vec![],
            gid_mappings: vec![],
            seccomp: None,
            devices: vec![],
        }
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
        let mut spec = spec_with("/nix/store/aaaa-root");
        resolve_from_graph(&mut spec, GRAPH).unwrap();
        assert_eq!(spec.closure.seeds.len(), 1); // just root
        let resolved = spec.closure.resolved.as_ref().unwrap();
        let as_str: Vec<String> = resolved.iter().map(|p| p.to_string()).collect();
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
        let mut spec = spec_with("/nix/store/cccc-leaf");
        resolve_from_graph(&mut spec, graph).unwrap();
        let as_str: Vec<String> = spec
            .closure
            .resolved
            .as_ref()
            .unwrap()
            .iter()
            .map(|p| p.to_string())
            .collect();
        assert_eq!(as_str, ["/nix/store/cccc-leaf"]); // no orphan, no root/dep
    }

    #[test]
    fn resolve_from_graph_bails_when_a_seed_is_absent_from_the_graph() {
        let mut spec = spec_with("/nix/store/zzzz-absent");
        assert!(resolve_from_graph(&mut spec, GRAPH).is_err());
    }

    #[test]
    fn resolve_from_graph_refuses_to_overwrite_a_built_closure() {
        let mut spec = spec_with("/nix/store/aaaa-root");
        spec.closure.resolved = Some(vec![StorePath::new("/nix/store/aaaa-root").unwrap()]);
        assert!(resolve_from_graph(&mut spec, GRAPH).is_err());
    }

    #[test]
    fn resolve_from_graph_bails_when_seeds_dont_cover_root() {
        // Seeds reach only `cccc-leaf`, but root is `aaaa-root` — not covered.
        let mut spec = spec_with("/nix/store/aaaa-root");
        spec.closure.seeds = vec![StorePath::new("/nix/store/cccc-leaf").unwrap()];
        assert!(resolve_from_graph(&mut spec, GRAPH).is_err());
    }

    #[test]
    fn resolve_from_graph_bails_on_empty_seeds() {
        let mut spec = spec_with("/nix/store/aaaa-root");
        spec.closure.seeds.clear();
        assert!(resolve_from_graph(&mut spec, GRAPH).is_err());
    }

    #[test]
    fn resolve_ok_when_built_bails_when_unbuilt() {
        let mut spec = spec_with("/nix/store/aaaa-root");
        // Unbuilt (no `resolved`): no runtime query to fall back on — fail loud.
        assert!(resolve(&spec).is_err());
        spec.closure.resolved = Some(vec![StorePath::new("/nix/store/aaaa-root").unwrap()]);
        assert!(resolve(&spec).is_ok());
    }
}
