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

#[serde(tag = "type", rename_all = "lowercase")]
enum Mount {
    // bind + FOLD closure; source must be a store path (enforced by the type).
    // `dest` omitted -> identity bind at /nix/store/<source>; present -> relocate to `dest`.
    Store { source: StorePath, dest: Option<PathBuf> },

    // bind opaquely, NO fold; source may be anything, including /nix/store.
    // ro by default; `rw` is the explicit opt-in to writability.
    Host  { source: PathBuf, dest: PathBuf, #[serde(default)] rw: bool },

    // writable scratch; no source.
    Tmpfs { dest: PathBuf },
}
```

`#[serde(tag = "type", rename_all = "lowercase")]` gives an internally-tagged
JSON form: a `"type"` discriminator plus that variant's fields, flat. Field names
are uniform — `source → dest` everywhere (`dest` optional only for `Store`,
absent for `Tmpfs`). The fold-vs-no-fold decision is **not** echoed in the field
names (that would be redundant); it lives in the `type` tag and in the Rust type
of `source` (`StorePath` folds, `PathBuf` is opaque). See
[JSON serialization](#json-serialization) for the concrete shapes.

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
  is just absent (the producer's responsibility). **`PATH` is an upper-layer
  (bridge / producer) concern only** — mounts never auto-extend it. A store mount
  folds the closure but contributes *nothing* to `PATH`; auto-pulling mount
  `bin/` dirs onto `PATH` was considered and rejected as unhelpful (it couples
  filesystem exposure to executable resolution, which the producer should decide
  deliberately). If you mount a tool, you put its `bin/` on `PATH` yourself.
- **`closure`** — the memoized resolution output: the store paths to bind.
  **Present** ⇒ engine-ready, resolution already happened, no Nix query.
  **Absent** ⇒ `resolve()` computes it. This is the production form of the
  `PIEROGI_CLOSURE` bench stub in `src/closure.rs`; reuse its validation (all
  under `/nix/store`, non-empty, fail loud). Shown below as the input-stamped
  `{ from, paths }` (the leaning-yes TODO); pre-stamp it's a flat `Vec<StorePath>`
  of just the `paths`.

## The `Mount` model

The three variants are **not** distinguished by where the source lives — they're
distinguished by **whether the closure folds**. Source location is orthogonal.
Pull a mount apart into its real properties:

1. **source** — a path to bind. A bind is a bind; host vs store is identical to
   `mount(2)`.
2. **dest** — where it appears (`dest`, optional for `Store` = identity).
3. **fold the closure?** — union `closure_of(source)` into `/nix/store` so the
   source's store symlinks resolve.

Only #3 is load-bearing, and it's a **choice, not an inference** from the source:

| Variant | Source | Folds closure? | Placement |
|---|---|---|---|
| `Store { source, dest }` | store path (required) | **yes** | identity (`dest` omitted) or relocate to `dest` |
| `Host { source, dest, rw }` | anything, incl. `/nix/store` | **no** | required `dest` (ro unless `rw`) |
| `Tmpfs { dest }` | none | n/a | required `dest` |

- **`Store` = "place the path *and* fold its dependencies."** What makes the
  store symlinks *inside* the path resolve is its **dependencies**
  (`closure_of(path) \ {path}`) being identity-bound under `/nix/store` — those
  refs point at the path's children, not at the path itself. So:
  - `dest` omitted → place the path at its own identity location
    (`/nix/store/<source>`). Indistinguishable from "folding the path" — which is
    why a bare store mount looks like a pure closure contribution.
  - `dest` set → **relocate**: deps under `/nix/store`, the path at `dest`.
    The path's *own* identity bind is **not** generally needed — it's required
    only when the path is **referenced by store path**: it self-references
    (`source ∈ nix-store -q --references source` — self-referential RPATH,
    wrapper, baked `$out`) or another retained seed depends on it (then it's
    identity-bound anyway, the "already exposed" case). v0 may either do the
    self-ref check or conservatively identity-bind it too (one negligible extra
    bind, always safe).

  Always read-only (the store is immutable), so no `ro` field.
- **`Host` = "bind these bytes, *no* fold."** A `/nix/store` source under `Host`
  is legal and meaningful: bind the path opaquely and deliberately *not* widen
  `/nix/store` with its closure. Inner store symlinks dangle, on purpose — a
  narrowing posture (expose these bytes, keep `/nix/store` minimal). Read-only by
  default (strict-by-default); `rw: true` is the explicit opt-in to writability.
- **Why not infer fold-from-source.** Auto-folding on any `/nix/store` source
  would make closure-dragging *implicit* (bind one dir, silently gain 200
  transitive paths) — against the "everything is written down" ethos — and would
  erase the legitimate "bind a store path without its deps" capability. The fold
  is the most security-relevant thing a mount does; it stays explicit.

The enum (vs. a flat `Mount { source, dest, fold }`) makes illegal states
unrepresentable: no "fold a host path," no "fold a tmpfs," no "store bind with a
custom `rw`." The one asymmetry — `Store`'s `dest` is optional, `Host`'s is
required — is correct: a host path has no identity in the sandbox.

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
- `Store` with `dest` set → **relocation**: the path's deps stay identity-bound
  under `/nix/store` (they're in the closure); the path itself is bound at `dest`,
  and identity-bound *only* if referenced by store path (self-ref or another
  seed). The scaffold must pre-create `dest`'s mountpoint dir (generalizes the
  existing "make `/nix/store`, `/tmp`" step).
- `Host` / `Tmpfs` → direct engine mounts.
- `exec` → `process.args` (with the resolved absolute path as `argv[0]`).
- `env` → `process.env`, verbatim.
- OCI `root.path` is the **empty scaffold**, not `root` — `root` lives in the
  closure like any store path.

## JSON serialization

Serde-native, internally tagged (`#[serde(tag = "type")]`): a `"type"`
discriminator plus that variant's fields, flat. The store paths below use short
fake hashes for readability.

**Mounts** — one object per variant:

```json
{ "type": "store", "source": "/nix/store/bbbb-ripgrep-14.1.0" }
{ "type": "store", "source": "/nix/store/cccc-myproject", "dest": "/home/dev/project" }
{ "type": "host",  "source": "/etc/resolv.conf", "dest": "/etc/resolv.conf" }
{ "type": "host",  "source": "/data", "dest": "/data", "rw": true }
{ "type": "tmpfs", "dest": "/tmp" }
```

**Unresolved spec** (a hand-authored config, or `wrap --dry-run` output) — no
`closure` key, so `run`/`resolve` will compute it live:

```json
{
  "exec": { "executable": "bash", "args": ["-l"] },
  "root": "/nix/store/aaaa-base-env",
  "mounts": [
    { "type": "store", "source": "/nix/store/bbbb-ripgrep-14.1.0" },
    { "type": "store", "source": "/nix/store/cccc-myproject", "dest": "/home/dev/project" },
    { "type": "host",  "source": "/etc/resolv.conf", "dest": "/etc/resolv.conf" },
    { "type": "tmpfs", "dest": "/tmp" }
  ],
  "env": {
    "PATH": "/nix/store/aaaa-base-env/bin:/nix/store/bbbb-ripgrep-14.1.0/bin",
    "HOME": "/home/dev",
    "TERM": "xterm-256color"
  }
}
```

**Resolved spec** (`resolve` output / a pre-baked artifact) — same document plus
`closure`, shown in the input-stamped `{ from, paths }` form:

```json
  "closure": {
    "from": [
      "/nix/store/aaaa-base-env",
      "/nix/store/bbbb-ripgrep-14.1.0",
      "/nix/store/cccc-myproject"
    ],
    "paths": [
      "/nix/store/aaaa-base-env",
      "/nix/store/0000-bash-5.2",
      "/nix/store/1111-glibc-2.40",
      "/nix/store/2222-ncurses-6.5",
      "/nix/store/3333-coreutils-9.5",
      "/nix/store/bbbb-ripgrep-14.1.0",
      "/nix/store/4444-pcre2-10.44",
      "/nix/store/5555-libgcc",
      "/nix/store/6666-nodejs-22"
    ]
  }
```

Note **`cccc-myproject` is in `from` but not in `paths`** — it's a relocated,
self-contained source tree (bound at `/home/dev/project`, nothing references it by
store path), so it drops out of the identity-bind set while its dep `nodejs`
stays. That `from ⊋ paths` gap is exactly the relocated-seed case; see example 3.

## Worked examples: closure resolution

Each walks `resolve()` on the spec above's building blocks. `closure_of` is one
`nix-store --query --requisites` call over *all* seeds (it dedups internally).

**1 — bare wrap (root only).** `root = base-env`, `exec = bash`, no mounts,
`PATH = base-env/bin`.
- seeds `= [base-env]` → `closure_of` = `{base-env, bash, glibc, ncurses,
  coreutils, …}` (one query).
- exec: `which bash` over `PATH` → `base-env/bin/bash` → canonicalize →
  `/nix/store/0000-bash-5.2/bin/bash`; assert under a closure path ✓ → that's
  `argv[0]`.
- realize: identity-bind every closure path, scaffold `/`, run. The common
  `wrap -- bash` shape.

**2 — tool present, but not on `PATH` (the gotcha).** Add
`{ type: store, source: ripgrep }`, `exec = rg`, but leave `PATH = base-env/bin`.
- seeds `= [base-env, ripgrep]` → closure gains `ripgrep, pcre2, libgcc`. ripgrep
  *is* bound.
- exec: `which rg` over `PATH` → **not found → hard error.** A store mount folds
  the closure; it does **not** extend `PATH`. Fix is the producer's: add
  `/nix/store/bbbb-ripgrep-14.1.0/bin` to `env.PATH`. Shows exec resolves via
  `PATH` only, never by scanning the closure.

**3 — relocated project (relocation + the `from ⊋ paths` gap).**
`{ type: store, source: myproject, dest: "/home/dev/project" }`, `exec = bash`.
`myproject` is a source tree whose `node_modules/*` symlink into `/nix/store`
(dep: `nodejs`).
- seeds `= [base-env, myproject]` → closure = base-env's + `{myproject, nodejs}`.
- identity-bind decision: `myproject` is referenced by store path? Its inner
  symlinks point at `nodejs`, *not* at itself; base-env doesn't depend on it; not
  self-referential → **drop its identity bind.**
- realize: base-env closure + `nodejs` at identity; `myproject` bound *only* at
  `/home/dev/project`. Inside, `node_modules/x → /nix/store/6666-nodejs-22`
  resolves (nodejs is identity-bound). So `closure.from` has `myproject`,
  `closure.paths` doesn't.
- *variant:* if `myproject` were a package output with a self-referential wrapper
  (`/nix/store/cccc-myproject/bin/foo` hardcoded inside), the self-ref check keeps
  it in `paths` too — identity-bound *and* at the dest.

**4 — already-exposed store mount (dedup).** `root = base-env` which *depends on*
`openssl`; add `{ type: store, source: openssl }` (bare).
- seeds `= [base-env, openssl]` → the single `nix-store` call returns `openssl`
  once; it was already in `closure_of(base-env)`. Closure is unchanged → the
  mount is a **no-op** (candidate for a "redundant mount" warning).
- *variant:* `{ …, dest: "/opt/ssl" }` → deps already present and `openssl` already
  identity-bound (base-env references it), **plus** one extra bind at `/opt/ssl`.
  Placement is independent of the closure already being exposed.

**5 — sealed fast-path (`closure` present).** A pre-baked spec.
- `resolve()` sees `closure` → **short-circuits: no `nix-store` query.** Optional
  query-free drift check: `closure.from == {root} ∪ {store-mount paths}` ✓.
- exec: `which bash` over `PATH`, checked against `closure.paths`, entirely
  host-side — still no query.
- realize from `closure.paths`, run. This is the launch that dodges the ~35 ms
  cost (see [startup-performance.md](startup-performance.md)).

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
- **TODO — `Option<dest>` vs a `StoreAt { source, dest }` variant.** Leaning `Option`
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
