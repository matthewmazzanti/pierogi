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
    root:    StorePath,                 // the rootfs (mounted at /); PATH base; closure seed
    mounts:  Vec<Mount>,                // additive placements onto root
    env:     BTreeMap<String, String>,  // producer-filled; PATH lives here
    closure: Option<Closure>,           // built store view; None => unbuilt (run requires it)
}

struct Closure {
    seeds:    Vec<StorePath>,           // provenance stamp: root + overlay-lower sources
    resolved: Vec<StorePath>,           // the identity-bind set, sorted; excludes root (the / mount)
}

struct Exec {
    executable: String,                 // name (looked up via env.PATH) or a store path
    args:       Vec<String>,
}

#[serde(tag = "type", rename_all = "lowercase")]
enum Mount {
    // bind opaquely; source may be anything, incl. /nix/store (a relocated store
    // path — its deps are covered by `closure`, not this mount). ro unless `rw`.
    Host  { source: PathBuf, dest: PathBuf, #[serde(default)] rw: bool },

    // writable scratch; no source.
    Tmpfs { dest: PathBuf },

    // writable overlay over a store lower already in `closure`; RW merge at
    // `dest`. upper: None -> ephemeral tmpfs; Some(dir) -> persistent host dir.
    Overlay { source: StorePath, dest: PathBuf, upper: Option<PathBuf> },
}
```

`#[serde(tag = "type", rename_all = "lowercase")]` gives an internally-tagged
JSON form: a `"type"` discriminator plus that variant's fields, flat. **Mounts
are placements, not closure contributions** — nothing folds. The store view is
the `closure` object alone; a mount only puts bytes at a `dest` the sealed `root`
already baked. (This replaces the earlier fold-vs-no-fold `Store` model; see
[Sealed rootfs](#sealed-rootfs-implemented-on-the-sealed-rootfs-branch).) See
[JSON serialization](#json-serialization) for the concrete shapes.

### Fields

- **`exec`** — the process. `executable` is either a bare name (resolved via
  `env.PATH`) or an absolute store path. It is **validated against the closure,
  never a source for it** — found within the resolved closure or a hard error.
  There is no exe-derived closure.
- **`root`** — a single, **required** store path. It is the base closure seed
  (`closure_of(root)`) and where `PATH` conventionally points. Injected by
  `wrap` (its baked base), declared by an authored config or a Nix build.
  > **Superseded on the sealed-rootfs branch:** `root` is no longer "bound under
  > `/nix/store` like any closure path." It *is* the rootfs — the FHS-root
  > mounted at `/`, carrying `/etc`, `/bin`, and every baked mountpoint, and
  > excluded from the identity binds. See [Sealed rootfs](#sealed-rootfs-implemented-on-the-sealed-rootfs-branch).
- **`mounts`** — additive binds layered onto the root. See the `Mount` model
  below.
- **`env`** — producer-filled key/values. `PATH` lives here and is the single
  source of truth for `exec` lookup. The engine merges nothing; an absent `PATH`
  is just absent (the producer's responsibility). **`PATH` is an upper-layer
  (bridge / producer) concern only** — mounts never auto-extend it. A mount
  places bytes but contributes *nothing* to `PATH`; auto-pulling mount `bin/`
  dirs onto `PATH` was considered and rejected as unhelpful (it couples
  filesystem exposure to executable resolution, which the producer should decide
  deliberately). If you mount a tool, you put its `bin/` on `PATH` yourself.
- **`closure`** — the built store view, **`{ seeds, resolved }`**.
  **Present** ⇒ engine-ready, pre-baked. **Absent** ⇒ unbuilt — and since there
  is **no runtime query**, `run` fails loud; build it first with `resolve
  --graph`, which traverses Nix's `exportReferencesGraph` JSON from `seeds`
  (`resolve::resolve_from_graph`) — every node validated as a top-level store
  path, fail loud.
  - **`resolved`** — the flat identity-bind set (what actually gets bound),
    **canonically sorted** (lexicographic by store path). Order is cosmetic to
    the engine (it binds a set), but sorting makes any two builds of the same
    inputs *byte-identical*, so the artifact is reproducible regardless of closure
    source. The full closure of `seeds`, **including `root`** (identity-bound at
    `/nix/store/<self>` as well as being the `/` mount).
  - **`seeds`** — the provenance stamp: the seed store paths this closure was
    built from (`root` + overlay-lower sources). Since store paths are content
    hashes it's a complete, query-free fingerprint, but nothing reads it at
    runtime (the query is gone) — it's there to parity-check against a fresh build
    out-of-band, not to drive a re-resolve.

## The `Mount` model

The variants are **not** distinguished by where the source lives — they're
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
| `Overlay { source, dest, upper }` | store path (required) | **yes** | RW merge at `dest` (writable via `upper`) |

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
- **`Overlay` = "a `Store` mount, but writable."** Folds the lower's closure like
  `Store` (its store symlinks resolve for free), then presents a **read-write
  merge** at `dest` via an overlay upper. `upper` omitted → ephemeral tmpfs
  (writes discarded on teardown); `upper: <host dir>` → persistent. The store
  `source` stays immutable — writes land in the upper, never the lower. Unlike a
  relocated `Store`, the lower is read from its *identity* `/nix/store/…` location
  (no self-ref subtlety); only the merged view moves to `dest`. This is the answer
  to "edit code at a path" (the writable-mounts TODO). Realizes as one OCI
  `type: overlay` mount; needs kernel ≥ 5.11 for unprivileged overlayfs-in-userns
  (else a `fuse-overlayfs` fallback).
- **Why not infer fold-from-source.** Auto-folding on any `/nix/store` source
  would make closure-dragging *implicit* (bind one dir, silently gain 200
  transitive paths) — against the "everything is written down" ethos — and would
  erase the legitimate "bind a store path without its deps" capability. The fold
  is the most security-relevant thing a mount does; it stays explicit.

The enum (vs. a flat `Mount { source, dest, fold }`) makes illegal states
unrepresentable: no "fold a host path," no "fold a tmpfs," no "store bind with a
custom `rw`." The one asymmetry — `Store`'s `dest` is optional, `Host`'s is
required — is correct: a host path has no identity in the sandbox.

**Fold is a property of a store-path *reference*, not of the mount *type*.** A
single-source bind hides this (its one `source` carries the whole decision), but
`Overlay` — and any future type that names paths beyond one source — makes it
explicit. So closure seeds are collected uniformly, per variant: `Store` and
`Overlay` each contribute their `source`; `Host`/`Tmpfs` contribute none. If
`Overlay` ever generalizes to **multiple, independently-tagged lowers**
(`lowers: Vec<Source>` where `Source = Store | Host`), only the seed-collection
walk widens — the resolver, query, and dedup are unchanged. Deferred; v0 is the
single store `source`.

## Resolution invariant

`resolve()` turns a spec with `closure: None` into one with `closure: Some`. The
closure has exactly **two declared seeds** — nothing is inferred from the command
line:

```
closure = closure_of(root) ∪ ⋃ closure_of(seed)   for seed in mount.store_refs()
```

where `store_refs()` is the store-path references a mount contributes: `Store`
and `Overlay` each give their `source`; `Host` and `Tmpfs` give nothing.

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

`closure_of()` is a **pluggable source**, not always a `nix-store` call:

- **Live (wrap, on a real host):** `nix-store --query --requisites <seeds>` — one
  deduped call. Needs the daemon / Nix DB.
- **Nix build (pre-bake):** the build sandbox has **no daemon socket and no Nix
  DB**, so that query is impossible. Nix already knows the closure and supplies it
  as data — `resolve` there just stamps it. See below.

## Nix-mode closure source (build time)

In a Nix build the closure is **supplied**, via `closureInfo { rootPaths = seeds
}` where `seeds` = the spec's declared store paths (`root` + store-mount / overlay
`source`s). The same Nix expression that emits the spec drives `closureInfo`, so
they can't disagree. `closureInfo` outputs three files (verified against a real
closure):

| File | Shape | Maps to |
|---|---|---|
| `store-paths` | flat closure, one path/line, deduped, leaves-first | **`closure.paths`** — read line-by-line, re-sorted canonically |
| `registration` | per path: `path / narHash / narSize / deriver / N / N×refs` | the **reference graph** — for self-ref / relocated-seed (`from ⊋ paths`) decisions |
| `total-nar-size` | single integer (closure bytes) | tmpfs-`upper` sizing / telemetry |

`from` is just the `rootPaths` you passed — not parsed out of anything. A store
path lists **itself** in its own `registration` refs iff it self-references (a real
example: `glibc`), which is exactly the self-ref signal.

So the build-time entry is `pierogi resolve --closure <store-paths>` (plus
`--registration <file>` once the self-ref optimization is implemented): supplied
closure, no query — the same stamping codepath as a live resolve, different
source. (`exportReferencesGraph` is the lower-level primitive — graph only, `path
/ deriver / N / refs`, no hash/size; `closureInfo` is the wrapper. `__structured
Attrs`'s `.attrs.json` only echoes the requested roots, not an expanded JSON
graph, so it's no shortcut.)

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
- `Overlay` → the lower's deps stay identity-bound under `/nix/store` (in the
  closure); one OCI `type: overlay` mount at `dest` with `lowerdir` = the store
  `source`, `upperdir`/`workdir` = the provisioned writable backing (`upper`, or a
  tmpfs when omitted). Emitted *after* the closure binds so `lowerdir` exists.
- `exec` → `process.args` (with the resolved absolute path as `argv[0]`).
- `env` → `process.env`, verbatim.
- OCI `root.path` **is `root`** — the sealed rootfs store path, mounted at `/`
  and remounted read-only. `root` carries the whole skeleton (`/etc`, `/bin`,
  and the `/proc`, `/dev`, `/tmp`, `/nix/store/<hash>` mountpoint dirs —
  *including its own* `/nix/store/<self>`), baked at build time, so there is **no
  empty scaffold to assemble** and no launch-time `mkdir`. `root` is **also** in
  `closure.resolved`: identity-bound at `/nix/store/<self>` so absolute
  references into it resolve, on top of being the `/` mount — see the
  sealed-rootfs note below.

### Sealed rootfs (implemented on the `sealed-rootfs` branch)

The rootfs is a single store path with the mount topology **frozen at build
time**. Consequences that supersede parts of this doc:

- **`root` is the rootfs *and* a closure bind.** It's mounted at `/` and
  identity-bound at `/nix/store/<self>`, so absolute references into it resolve
  either way. The circularity of "root's mountpoint tree depends on root's
  closure" is broken in two places: dep mountpoints are baked from `root`'s
  *dependencies* (buildable before `root`), and root's *own* `/nix/store/<self>`
  mountpoint is baked from `$out` — which nix fixes before the builder runs
  (input-addressed), so root's hash is known during its own build.
- **No runtime query — pre-bake only.** The live `nix-store --query` path is
  gone entirely (`closure_of` and the `PIEROGI_CLOSURE` stub deleted). A spec's
  `closure` is built at *build time* by traversing a Nix `exportReferencesGraph`
  dump from the seeds (`resolve --graph`, `resolve::resolve_from_graph`) — pierogi
  walks the reference graph itself, so `resolved` is provably the closure of the
  declared seeds rather than a flat list taken on faith. `run` on an unbuilt spec
  **fails loud**. There is no `Mount::Store` type — a bare store mount was a
  closure seed in disguise (folds are gone), and a relocation is now just a
  `Host` bind whose deps live in `closure`.
- **No runtime store additions.** A store path added at launch would need a
  `/nix/store/<hash>` mountpoint the image never baked, so `--mount store:` is
  gone and a different closure means a different image. `--mount host:` /
  `tmpfs:` still work, but only onto dests the image already baked.
- **A present `closure` is authoritative.** `resolve()` only asserts it's there —
  no query, no seed-drift check. With no runtime path that can change a sealed
  spec's seeds, comparing them every launch would only catch a
  hand-edit-without-re-resolve, so it's dropped. `closure.seeds` stays as
  provenance to parity-check against a fresh build out-of-band.
- **File-vs-dir mountpoints move to build time.** A file store path (e.g. a
  `writeText` `/etc` file) needs a file mountpoint, a dir needs a dir — the root
  derivation stats each closure member and bakes the matching kind (what
  `scaffold_mountpoints` used to do at launch).

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
{ "type": "overlay", "source": "/nix/store/cccc-myproject", "dest": "/home/dev/project" }
{ "type": "overlay", "source": "/nix/store/cccc-myproject", "dest": "/home/dev/project", "upper": "/var/lib/pierogi/project" }
```

The two `overlay` forms are ephemeral (writes discarded on teardown) and
persistent (writes land in the host `upper` dir). The multi-lower generalization
(`"lowers": [{ "type": "store", … }, { "type": "host", … }]`) is deferred — v0 is
the single store `source` above.

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

- **DECIDED — closure carries an input stamp** (`{ from, paths }`), **placement is
  `Option<dest>`** (not a `StoreAt` variant), and **writable mounts are an
  `Overlay` variant** (`{ source, dest, upper }`, `upper` omitted = ephemeral
  tmpfs). All folded into the schema above.
- **TODO — overlay's operational path & generalization.** The v0 shape (single
  store `source`) is specified; still open: (a) the userns/kernel path —
  unprivileged overlayfs-in-userns needs kernel ≥ 5.11, else a `fuse-overlayfs`
  fallback; (b) tmpfs-upper sizing/limits; (c) the multi-lower generalization
  (`lowers: Vec<Source>`, `Source = Store | Host`) — additive when it lands, only
  the seed-collection walk widens.
- **TODO — `/etc` and synthetic identity.** `/etc` lives in the **root build**,
  not the spec (not a spec field per-se). Materialization is staged:
  **hand-rolled in the Nix builder first** (fixed `passwd`/`group`/`hostname` as
  Nix strings, baked into root — M1), then **generated by `pierogi-synth`** from
  an identity attrset (M4) — the same builders serve build *and* runtime. Still
  open: the static-vs-dynamic split (passwd/group/nsswitch baked vs
  resolv.conf/machine-id per-invocation), and whether an FHS-shaped root needs a
  different `/etc`. Reconcile with the identity sub-schema pinned in
  [architecture.md](architecture.md#spec--rootfs-build-time-vs-runtime).
- **DECIDED (sealed-rootfs branch) — `root` is the rootfs.** Not "a store path
  bound under `/nix/store` like any other," but the FHS-root bound at `/`: it
  carries `/etc`, `/bin`, and every mountpoint, baked at build time, and is
  excluded from the identity binds. This resolves the old "root-as-mount"
  question in the opposite direction from the earlier draft — `root` is now
  *structurally special* (the `/` mount), and its filesystem contribution is no
  longer identical to a store mount. See [Realization](#realization-what-the-engine-does).
- **TODO — the rest of the runtime surface.** Namespaces, uid/gid maps,
  hostname/cwd, dbus/seccomp rules — present in the engine today (`src/spec.rs`)
  but not yet folded into this schema's field list. Add per feature/milestone.
- **TODO — wire format.** JSON vs other, and public-contract status — deferred to
  M1 per [architecture.md](architecture.md#spec--rootfs-build-time-vs-runtime).
