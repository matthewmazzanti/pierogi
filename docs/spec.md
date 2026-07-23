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
- **`root`** — a single, **required** store path: the **rootfs**. The FHS-root
  mounted at `/` (read-only), carrying `/etc`, `/bin`, and every baked mountpoint.
  It's the first closure seed (`seeds` starts with it) and where `PATH`
  conventionally points, and it's in `closure.resolved` too — identity-bound at
  `/nix/store/<self>` so absolute references into it resolve — on top of being the
  `/` mount. Produced by a Nix build (or an authored config). See
  [Sealed rootfs](#sealed-rootfs-implemented-on-the-sealed-rootfs-branch).
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

Mounts are **placements**, not closure contributions. The `closure` object is
the authoritative store view — the full, build-locked set of paths bound under
`/nix/store`. A mount only *puts something at a `dest`* the sealed `root` already
baked; **nothing folds a closure**, because a runtime store addition would need
an un-baked mountpoint (the topology is frozen at build time).

| Variant | Source | Placement |
|---|---|---|
| `Host { source, dest, rw }` | anything, incl. `/nix/store` | bind at `dest` (ro unless `rw`) |
| `Tmpfs { dest }` | none | writable scratch at `dest` |
| `Overlay { source, dest, upper }` | store path (already in `closure`) | RW merge at `dest` (writable via `upper`) |

- **`Host` = "bind these bytes at `dest`."** Source may be anything, including a
  `/nix/store` path. A **relocated store path** is just this — a host bind of
  `/nix/store/<x>` at some `dest` — and its dependencies resolve because they're
  already in `closure`, not because the mount folds anything. Read-only by default
  (strict-by-default); `rw: true` is the explicit opt-in to writability.
- **`Tmpfs` = "a writable empty dir at `dest`."** No source.
- **`Overlay` = "a writable view over a store lower."** A read-write merge at
  `dest` whose lower is a store path already in `closure`; writes land in an
  `upper` (omitted → ephemeral tmpfs, discarded on teardown; `<host dir>` →
  persistent), the store lower immutable. This is the answer to "edit code at a
  path." It's the one mount whose `source` is a store path, so the only mount that
  contributes a closure **seed**. Realization is deferred (`oci` bails); see
  [Open / deferred](#open--deferred).

There is **no `Store` variant**. A bare store mount used to mean "fold this
path's closure," but with the closure authoritative and build-locked that isn't a
mount at all — it's a *seed of the closure*, declared where the closure is built.
What's left for `mounts` is genuinely placing things.

**Seeds** are collected per variant by `store_refs()`: only `Overlay` contributes
its `source`; `Host`/`Tmpfs` contribute none. So `seeds = root + overlay lowers`
— the roots the closure is traversed from (see [Resolution invariant](#resolution-invariant)).

## Resolution invariant

`resolve --graph` turns a spec with `closure: None` into one with `closure: Some`
— at **build time**, from a supplied reference graph, never a live query. The
closure is the transitive closure of the spec's **seeds** over that graph:

```
resolved = closure(seeds)      seeds = { root } ∪ { overlay lowers }
```

- `root` is always a seed and is in `resolved` — identity-bound at
  `/nix/store/<self>` *and* the `/` mount.
- `Host` and `Tmpfs` mounts contribute **nothing** to the closure — they're
  realized as direct engine mounts onto baked mountpoints.
- `exec` contributes **nothing** — it's resolved *within* the closure by the
  runtime against the container's `PATH` at launch (in-sandbox), and errors if
  absent. No host-side exec lookup.
- At run time there is no resolution at all: `run` requires a spec that already
  carries its `closure` and **fails loud** otherwise. There is no live
  `nix-store` query anywhere — pierogi is pre-bake only.

The traversal runs **in-process** over the graph Nix supplies (below), so
`resolved` is provably exactly the closure of the declared seeds — a tampered or
incomplete graph is caught. See
[execution-modes.md](execution-modes.md#resolution-rules) for the fuller picture.

## Nix-mode closure source (build time)

The reference graph is **supplied** by Nix, via the `exportReferencesGraph`
derivation parameter — the build sandbox has no daemon or Nix DB to query. Under
`__structuredAttrs`, setting `exportReferencesGraph.closure = [ root ]` makes Nix
expand the full graph as the top-level `closure` array of `$NIX_ATTRS_JSON_FILE`:
one `{ path, references, narSize, narHash, … }` object per closure member — the
same shape `closureInfo` consumes internally.

`pierogi resolve <spec> --graph $NIX_ATTRS_JSON_FILE` reads that JSON (ignoring
the rest of the attrs), builds a `path → references` map, and walks requisites
from the spec's seeds (`resolve::resolve_from_graph`). No `store-paths` /
`registration` files, no `nix-store` call, no flat list taken on faith.

> Two gotchas the code encodes: it's the **top-level** `.closure` key that holds
> the expanded graph — `.exportReferencesGraph.closure` in the same file only
> echoes the requested roots, so it's no shortcut. And `closureInfo` still appears
> in the sandbox build, but only as a convenience for the flat mkdir list of
> mountpoints to *bake* — not as pierogi's input.

## Realization (what the engine does)

- `closure.resolved` → one ro identity bind per path under `/nix/store`, onto the
  mountpoint dirs baked into `root`. Includes `root` itself (bound at
  `/nix/store/<self>`). Per-path binds today; the merged-tree representation is
  the lever-6 perf question — see [startup-performance.md](startup-performance.md).
- `Host` → an ro/rw bind at `dest` (a relocated store path is just this; its deps
  are already bound via the closure). `Tmpfs` → a tmpfs at `dest`. Both bind onto
  a mountpoint `root` already baked.
- `Overlay` → **deferred** (`oci` bails). When realized: one OCI `type: overlay`
  at `dest`, `lowerdir` = the store `source` (already in the closure),
  `upperdir`/`workdir` = provisioned writable backing (`upper`, or a tmpfs when
  omitted), emitted after the closure binds so `lowerdir` exists.
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

**Mounts** — one object per variant (`Host` / `Tmpfs` / `Overlay`):

```json
{ "type": "host",  "source": "/etc/resolv.conf", "dest": "/etc/resolv.conf" }
{ "type": "host",  "source": "/data", "dest": "/data", "rw": true }
{ "type": "host",  "source": "/nix/store/cccc-myproject", "dest": "/home/dev/project" }
{ "type": "tmpfs", "dest": "/tmp" }
{ "type": "overlay", "source": "/nix/store/cccc-myproject", "dest": "/home/dev/project" }
{ "type": "overlay", "source": "/nix/store/cccc-myproject", "dest": "/home/dev/project", "upper": "/var/lib/pierogi/project" }
```

The third `host` line is a **relocated store path** — a plain bind of a store path
at a non-store `dest`; its deps resolve because they're in `closure`, not because
the mount folds anything. The two `overlay` forms are ephemeral (writes discarded
on teardown) and persistent (writes land in the host `upper`); overlay realization
is deferred (`oci` bails). The multi-lower generalization (`"lowers": […]`) is
deferred too.

**Unbuilt spec** (hand-authored, or the sources a Nix builder emits before
`resolve`) — no `closure` key. It is **not runnable**: `run` fails loud on it.
Build it with `resolve --graph` first.

```json
{
  "exec": { "executable": "bash", "args": ["-l"] },
  "root": "/nix/store/aaaa-base-env",
  "mounts": [
    { "type": "host",  "source": "/etc/resolv.conf", "dest": "/etc/resolv.conf" },
    { "type": "tmpfs", "dest": "/tmp" }
  ],
  "env": {
    "PATH": "/bin",
    "HOME": "/home/dev",
    "TERM": "xterm-256color"
  }
}
```

**Built spec** (`resolve --graph` output / a pre-baked artifact) — the same
document plus `closure`, in the `{ seeds, resolved }` form:

```json
  "closure": {
    "seeds": [ "/nix/store/aaaa-base-env" ],
    "resolved": [
      "/nix/store/aaaa-base-env",
      "/nix/store/0000-bash-5.2",
      "/nix/store/1111-glibc-2.40",
      "/nix/store/2222-ncurses-6.5",
      "/nix/store/3333-coreutils-9.5"
    ]
  }
```

`seeds` is just `root` here (no overlay mounts); `resolved` is the closure of the
seeds over the reference graph, **including `root`**, canonically sorted.

## Worked examples: closure resolution

Each walks `resolve --graph` over the spec above's building blocks. There is no
`nix-store` query — the closure is the traversal of Nix's `exportReferencesGraph`
from the seeds (see [Nix-mode closure source](#nix-mode-closure-source-build-time)).

**1 — bare rootfs.** `root = base-env`, `exec = bash`, no mounts, `PATH = /bin`.
- seeds `= [base-env]` → traverse the graph from `base-env` → `resolved =
  {base-env, bash, glibc, ncurses, coreutils, …}` (root included).
- realize: mount `base-env` at `/`, identity-bind every `resolved` path under
  `/nix/store` (onto baked mountpoints), run. `exec bash` is resolved by the
  runtime against `PATH` in-sandbox → `/bin/bash` → the store bash; errors if it's
  absent from the closure.

**2 — in the closure, but not on `PATH` (the gotcha).** The image's closure
includes `ripgrep` (a build-time decision — the rootfs derivation references it),
but `exec = rg` with `PATH = /bin` and no `rg` in `/bin`.
- `rg` is bound under `/nix/store`, but exec resolves via `PATH` only. `which rg`
  over `PATH` → **not found → hard error.** Fix is the producer's: put
  `ripgrep/bin` on `env.PATH`, or symlink `rg` into the rootfs's `/bin`. Being in
  the closure never extends `PATH`.

**3 — relocated project.** `{ type: host, source: /nix/store/…-myproject, dest:
"/home/dev/project" }`, `exec = bash`. `myproject` is a source tree whose
`node_modules/*` symlink into `/nix/store` (dep: `nodejs`).
- A `Host` mount contributes **no** seed, so `myproject`'s closure is *not* pulled
  in automatically. For the inner symlinks to resolve, `nodejs` must be in
  `closure.resolved` — the producer seeds it from the rootfs derivation at build
  time (and `myproject` too, if anything references it by store path).
- realize: `myproject` bound only at `/home/dev/project` (a plain host bind);
  `nodejs` identity-bound because it's in `resolved`. This is the sealed-world
  replacement for the old "relocated store mount": *placement* (the bind) and
  *closure coverage* (the deps) are decided separately, both at build time.

**4 — sealed launch (`closure` present).** A pre-baked spec — the only runnable
form.
- `resolve` sees `closure` → returns it **verbatim**: no query, no seed
  comparison, no re-derivation.
- realize from `closure.resolved` (root at `/`, every member identity-bound),
  `exec` resolved against `PATH` in-sandbox. There is no live fallback — an
  unbuilt spec would have failed loud at `run`.

## Open / deferred

- **DECIDED — closure is `{ seeds, resolved }`** (provenance seeds + the built
  bind set), and **writable mounts are an `Overlay` variant** (`{ source, dest,
  upper }`, `upper` omitted = ephemeral tmpfs). Relocation is a plain `Host` bind
  of a store path (there is no `Store` variant / `Option<dest>`). All folded into
  the schema above.
- **MOOT — relocated-seed drop.** `resolved` keeps every node reachable from the
  seeds, and that's fine: the old "`from ⊋ paths`" optimization (drop a seed
  nothing references by store path) was a `Store`-mount concept the sealed model
  dissolved. Relocation is now a plain `Host` bind that contributes no seed (never
  over-binds); overlay lowers are needed at their identity path; and `root` is the
  only possibly-unreferenced seed, but we identity-bind it deliberately (see
  [Sealed rootfs](#sealed-rootfs-implemented-on-the-sealed-rootfs-branch)). Nothing
  droppable remains.
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
- **DECIDED (sealed-rootfs branch) — `root` is the rootfs.** The FHS-root bound at
  `/`: it carries `/etc`, `/bin`, and every mountpoint (including its own
  `/nix/store/<self>`), baked at build time. It's also in `closure.resolved` —
  identity-bound at `/nix/store/<self>` so absolute references into it resolve —
  on top of being the `/` mount. This resolves the old "root-as-mount" question:
  `root` is now *structurally special* (the `/` mount), not just another store
  bind. See [Realization](#realization-what-the-engine-does).
- **TODO — the rest of the runtime surface.** Namespaces, uid/gid maps,
  hostname/cwd, dbus/seccomp rules — present in the engine today (`src/spec.rs`)
  but not yet folded into this schema's field list. Add per feature/milestone.
- **TODO — wire format.** JSON vs other, and public-contract status — deferred to
  M1 per [architecture.md](architecture.md#spec--rootfs-build-time-vs-runtime).
