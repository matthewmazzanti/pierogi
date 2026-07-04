# Pierogi — The spec

> **Status: converging draft.** This captures the v0 field-level schema as it's
> being designed. Open forks are marked **TODO** rather than resolved. The
> serialized wire format (JSON vs other) and its public-contract status are still
> deferred — see [architecture.md](architecture.md#spec--rootfs-build-time-vs-runtime).
> For how a spec is *produced and consumed* (the pipeline, the verbs, the modes),
> see [execution-modes.md](execution-modes.md); this doc is only about the *shape*.

## What the spec is

One **descriptive, complete** account of a single sandbox: what to run, what
filesystem it sees, what environment it has, and — once resolved — the exact set
of store paths to bind. It carries **no policy**: the engine is a pure realizer
that binds what the spec says and `exec`s. Every "just works" default is
materialized *into* the spec by the bridge before the engine sees it (see
[execution-modes.md](execution-modes.md) — the four-layer pipeline). If a field
is absent, it is absent — the engine adds nothing.

## v0 schema

Start on the core (`exec`, `root`, `mounts`, `env`) and expand per feature.

```rust
struct Spec {
    exec:    Exec,                      // what to run
    root:    StorePath,                 // required base: closure seed + PATH base
    mounts:  Vec<Mount>,                // additive binds
    env:     BTreeMap<String, String>,  // producer-filled; PATH lives here
    closure: Option<Vec<StorePath>>,    // resolved bind set; None => resolve() fills it
}

struct Exec {
    executable: String,                 // name (looked up via env.PATH) or a store path
    args:       Vec<String>,
}

enum Mount {
    // bind + FOLD closure; source must be a store path.
    // `at` = None -> identity bind at /nix/store/<path>; Some(dest) -> + extra bind at dest.
    Store { path: StorePath, at: Option<PathBuf> },

    // bind opaquely, NO fold; source may be anything, including /nix/store.
    Host  { source: PathBuf, dest: PathBuf, ro: bool },

    // writable scratch; no source.
    Tmpfs { dest: PathBuf },
}
```

### Fields

- **`exec`** — the process. `executable` is either a bare name (resolved via
  `env.PATH`) or an absolute store path. It is **validated against the closure,
  never a source for it** — found within the resolved closure or a hard error.
  There is no exe-derived closure.
- **`root`** — a single, **required** store path. It is the base closure seed
  (`closure_of(root)`) and where `PATH` conventionally points. **Distinct from
  mounts** by role, not by mechanism: in v0 its filesystem contribution is
  identical to a store mount (bound under `/nix/store` like any closure path);
  its distinction is *seed + PATH base*. Injected by `wrap` (its baked base),
  declared by an authored config or a Nix build.
- **`mounts`** — additive binds layered onto the root. See the `Mount` model
  below.
- **`env`** — producer-filled key/values. `PATH` lives here and is the single
  source of truth for `exec` lookup. The engine merges nothing; an absent `PATH`
  is just absent (the producer's responsibility).
- **`closure`** — the memoized resolution output: the exact flat list of store
  paths to bind. **Present** ⇒ engine-ready, resolution already happened, no Nix
  query. **Absent** ⇒ `resolve()` computes it. This is the production form of the
  `PIEROGI_CLOSURE` bench stub in `src/closure.rs`; reuse its validation (all
  under `/nix/store`, non-empty, fail loud).

## The `Mount` model

The three variants are **not** distinguished by where the source lives — they're
distinguished by **whether the closure folds**. Source location is orthogonal.
Pull a mount apart into its real properties:

1. **source** — a path to bind. A bind is a bind; host vs store is identical to
   `mount(2)`.
2. **dest** — where it appears.
3. **fold the closure?** — union `closure_of(source)` into `/nix/store` so the
   source's store symlinks resolve.

Only #3 is load-bearing, and it's a **choice, not an inference** from the source:

| Variant | Source | Folds closure? | Placement |
|---|---|---|---|
| `Store { path, at }` | store path (required) | **yes** | identity (`at: None`) or `at` dest |
| `Host { source, dest, ro }` | anything, incl. `/nix/store` | **no** | explicit dest |
| `Tmpfs { dest }` | none | n/a | explicit dest |

- **`Store` = "place the path *and* fold its dependencies."** What makes the
  store symlinks *inside* the path resolve is its **dependencies**
  (`closure_of(path) \ {path}`) being identity-bound under `/nix/store` — those
  refs point at the path's children, not at the path itself. So:
  - `at: None` → place the path at its own identity location (`/nix/store/<path>`).
    Indistinguishable from "folding the path" — which is why a bare store mount
    looks like a pure closure contribution.
  - `at: Some(dest)` → **relocate**: deps under `/nix/store`, the path at `dest`.
    The path's *own* identity bind is **not** generally needed — it's required
    only when the path is **referenced by store path**: it self-references
    (`path ∈ nix-store -q --references path` — self-referential RPATH, wrapper,
    baked `$out`) or another retained seed depends on it (then it's identity-bound
    anyway, the "already exposed" case). v0 may either do the self-ref check or
    conservatively identity-bind it too (one negligible extra bind, always safe).

  Always read-only (the store is immutable), so no `ro` field.
- **`Host` = "bind these bytes, *no* fold."** A `/nix/store` source under `Host`
  is legal and meaningful: bind the path opaquely and deliberately *not* widen
  `/nix/store` with its closure. Inner store symlinks dangle, on purpose — a
  narrowing posture (expose these bytes, keep `/nix/store` minimal).
- **Why not infer fold-from-source.** Auto-folding on any `/nix/store` source
  would make closure-dragging *implicit* (bind one dir, silently gain 200
  transitive paths) — against the "everything is written down" ethos — and would
  erase the legitimate "bind a store path without its deps" capability. The fold
  is the most security-relevant thing a mount does; it stays explicit.

The enum (vs. a flat `Mount { source, dest, fold }`) makes illegal states
unrepresentable: no "fold a host path," no "fold a tmpfs," no "store bind with a
custom `ro`." The one asymmetry — `Store`'s placement is optional, `Host`'s dest
is required — is correct: a host path has no identity in the sandbox.

## Resolution invariant

`resolve()` turns a spec with `closure: None` into one with `closure: Some`. The
closure has exactly **two declared seeds** — nothing is inferred from the command
line:

```
closure = closure_of(root) ∪ ⋃ closure_of(store mount .path)
```

- `Host` and `Tmpfs` mounts contribute **nothing** to the closure — they're
  realized as direct engine mounts.
- `exec` contributes **nothing** — it's resolved *within* the closure (found via
  `env.PATH`, asserted under a closure path) and errors if absent.
- A present `closure` short-circuits the whole query (the sealed fast-path);
  `exec` is still resolved against it, host-side, no Nix call.

Because store paths bind at their own paths (identity binds), any path that
resolves on the host store is valid in-sandbox — so all of resolution runs
host-side, before the engine touches a namespace. See
[execution-modes.md](execution-modes.md#resolution-rules) for the full algorithm
and the mount-cost taxonomy.

## Realization (what the engine does)

- `closure` → one ro bind per path under `/nix/store` (per-path binds today; the
  merged-tree representation is the lever-6 perf question — see
  [startup-performance.md](startup-performance.md)).
- `Store { at: Some(dest) }` → **relocation**: the path's deps stay identity-bound
  under `/nix/store` (they're in the closure); the path itself is bound at `dest`,
  and identity-bound *only* if referenced by store path (self-ref or another
  seed). The scaffold must pre-create `dest`'s mountpoint dir (generalizes the
  existing "make `/nix/store`, `/tmp`" step).
- `Host` / `Tmpfs` → direct engine mounts.
- `exec` → `process.args` (with the resolved absolute path as `argv[0]`).
- `env` → `process.env`, verbatim.
- OCI `root.path` is the **empty scaffold**, not `root` — `root` lives in the
  closure like any store path.

## Open / deferred

- **TODO — stamp the closure with its input seeds (leaning yes).** Make `closure`
  carry both the requisites and the seeds it was resolved *from* (`{ from: [root ∪
  store-mount paths ∪ --auto discovered], paths: [...] }`). Because store paths are
  content hashes and a store closure is immutable, the seed list is a complete,
  query-free stamp: `from == current seeds` is a set comparison, not a `nix-store`
  query — cheap drift detection, and the query-free half of the `closure ==
  resolve(root)` provenance assertion in
  [execution-modes.md](execution-modes.md#closure--memoized-resolution). Marginal
  value over the `run`-overrides recompute rule is catching *hand-edited* drift
  (edit `root`, forget to re-resolve). It is a **mechanism only** — what a
  `from` ≠ declared-seeds divergence *means* (error / warn / allowed
  seal-override) is the deferred sealed-vs-delta policy, not settled by adding the
  stamp.
- **TODO — `Option<at>` vs a `StoreAt { path, dest }` variant.** Leaning `Option`
  (the only difference is placement); a separate variant duplicates the
  closure-folding semantics for no gain.
- **TODO — writable / overlay mounts.** All store binds are read-only (immutable
  store). "Edit code at `/home/whatever/code`" needs overlay/copy semantics — a
  separate feature, deferred.
- **TODO — `/etc` and synthetic identity.** Moved out of the spec into the
  **root build** (not a spec field per-se). Still open: the static-vs-dynamic
  split (passwd/group/nsswitch baked into root vs resolv.conf/machine-id
  per-invocation), and whether an FHS-shaped root needs a different `/etc` setup.
  Reconcile with the identity sub-schema pinned in
  [architecture.md](architecture.md#spec--rootfs-build-time-vs-runtime).
- **TODO — `root`-as-mount question.** Whether `root` stays a distinct field or
  collapses into a first mount. Currently **distinct** (locked for v0); it only
  becomes structurally special if we add an FHS-root-bound-at-`/` variant.
- **TODO — the rest of the runtime surface.** Namespaces, uid/gid maps,
  hostname/cwd, dbus/seccomp rules — present in the engine today (`src/spec.rs`)
  but not yet folded into this schema's field list. Add per feature/milestone.
- **TODO — wire format.** JSON vs other, and public-contract status — deferred to
  M1 per [architecture.md](architecture.md#spec--rootfs-build-time-vs-runtime).
