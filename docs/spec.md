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

A consequence worth stating outright: because the engine realizes exactly the
spec and nothing more, **the resolved spec is the complete, auditable record** of
the sandbox — every loosening (a host bind, an opened namespace, a dropped
default) is visible in it or does not happen. This is what makes a default-safe
posture compatible with letting a user opt *out* of confinement: the opt-out is
representable, but never ambient — it lands in the artifact. How each control
maps to the OCI runtime spec, which direction it moves the boundary, and how the
surface is meant to grow are catalogued in
[control-surface.md](control-surface.md).

## v0 schema

The spec **mirrors the OCI runtime spec's structure** — `root`, `process`,
`linux`, `mounts`, `hostname`, … — plus one nix carve-out, `closure`. The engine
holds **no policy** and projects whatever the spec carries, `absent = absent` (see
[control-surface.md](control-surface.md), [architecture.md](architecture.md)'s
third principle). So the OCI knobs are **pass-through**, and the default-closed
floor is *not here*: it's `profiles.base` in the Nix producer layer, composed into
a spec before the engine sees it. Verbatim-OCI leaves reuse `oci_spec` types
directly. `deny_unknown_fields` at every level makes an unmodeled OCI field a hard
error, not a silent drop.

```rust
struct Spec {                              // mirrors oci_spec::Spec
    root:        Root,                     // OCI root; path is a StorePath (carve-out)
    process:     Process,                  // OCI process
    mounts:      Vec<oci_spec::Mount>,     // verbatim OCI mounts (bind/tmpfs/overlay/idmapped)
    hostname:    Option<String>,           // OCI hostname
    domainname:  Option<String>,           // OCI domainname
    annotations: BTreeMap<String, String>, // OCI annotations (metadata)
    linux:       Linux,                    // OCI linux
    closure:     Closure,                  // Nix store view { seeds, resolved } — the carve-out
}

struct Root {                              // OCI root
    path:     StorePath,                   // sealed rootfs (carve-out: a store path, not PathBuf)
    readonly: Option<bool>,                // root.readonly
}

struct Process {                           // OCI process (execve split + map/list ergonomics)
    executable:        String,             // execve argv[0] (via env.PATH or a store path)
    args:              Vec<String>,        // execve: the rest of argv
    terminal:          bool,               // process.terminal (+ console relay)
    user:              Option<User>,       // process.user            (oci_spec type)
    cwd:               Option<PathBuf>,    // process.cwd, absent => /
    env:               BTreeMap<String, String>, // process.env (a MAP here); PATH lives here
    capabilities:      Option<LinuxCapabilities>, // process.capabilities, the five OCI sets (oci_spec type)
    rlimits:           Vec<PosixRlimit>,   // process.rlimits         (oci_spec type)
    no_new_privileges: Option<bool>,       // process.noNewPrivileges
    oom_score_adj:     Option<i32>,        // process.oomScoreAdj
}

struct Linux {                             // OCI linux — all faithful pass-throughs
    namespaces:      Vec<LinuxNamespace>,  // (oci_spec type; path => join)
    uid_mappings:    Vec<IdMapping>,       // linux.uidMappings (host: N | "self")
    gid_mappings:    Vec<IdMapping>,
    masked_paths:    Vec<String>,
    readonly_paths:  Vec<String>,
    rootfs_propagation: Option<String>,
    seccomp:         Option<LinuxSeccomp>, // (oci_spec type)
    devices:         Vec<LinuxDevice>,     // (oci_spec type)
    resources:       Option<LinuxResources>, // memory/cpu/pids/…  (oci_spec type)
    cgroups_path:    Option<PathBuf>,
    sysctl:          BTreeMap<String, String>,
    net_devices:     BTreeMap<String, LinuxNetDevice>, // (oci_spec type)
    personality:     Option<LinuxPersonality>,         // (oci_spec type)
}

struct Closure {
    seeds:    Vec<StorePath>,              // build-time INPUT: paths to traverse from
    resolved: Option<Vec<StorePath>>,      // stamped at resolve: closure(seeds), sorted, incl. root; None => unbuilt
}

// The one custom mapping type: host may be a number or "self" (the invoking
// user's id, resolved by the engine at launch — a build can't name it).
struct IdMapping { container: u32, host: HostId, size: u32 }
enum   HostId    { Numeric(u32), Invoker /* "self" */ }
```

Deliberate divergences from a literal OCI mirror: `root.path` is a `StorePath`;
argv keeps the **execve** split (`process.executable` + `args`, vs OCI's single
`args`); `process.env` is a map.

**There is no custom `Mount` type.** A `Host` bind, a `Tmpfs`, an overlay
(`type: "overlay"` with `lowerdir`/`upperdir`/`workdir` in `options`), and an
idmapped mount are all just `oci_spec::Mount` (`{ destination, source, type,
options, … }`), passed straight through. Mounts contribute **nothing** to the
closure automatically — if a mount's source (a relocated store path, an overlay
lower) needs its closure bound, the producer adds it to `closure.seeds`. See
[JSON serialization](#json-serialization) for the concrete shapes.

### Fields

- **`executable` / `args`** — the process, **execve**-shaped: `executable` is
  either a bare name (resolved via `environment.PATH`) or an absolute store path,
  and `args` is the rest of argv. (Compose splits ENTRYPOINT/CMD; pierogi keeps
  the single execve pair.) `executable` is **validated against the closure, never
  a source for it** — found within the resolved closure or a hard error. There is
  no exe-derived closure.
- **`tty`** — allocate a controlling terminal (compose `tty`). `true` → the
  runtime sets up a pty and makes it the controlling tty (job control); default
  `false` → inherit the host stdio fds.
- **`working_dir`** — the process cwd (compose `working_dir`). Absent → `/`.
- **`read_only`** — whether `/` is read-only (compose `read_only`). **Pass-through,
  `absent = absent`** — the engine defaults nothing. `profiles.base` sets it
  `true` (the floor); `read_only: false` is a producer's OPEN. See
  [control-surface.md](control-surface.md).
- **`hostname`** — the UTS hostname (compose `hostname`). Pass-through; absent →
  the engine emits none (`profiles.base` supplies one).
- **OCI pass-throughs** (`no_new_privileges`, `capabilities`, `rlimits`,
  `masked_paths`, `readonly_paths`, `rootfs_propagation`, `namespaces`,
  `uid_mappings`, `gid_mappings`, `seccomp`, `devices`) — each projects verbatim
  to the matching OCI field, `absent = absent`. `capabilities` is the full OCI
  `LinuxCapabilities` (the five sets `bounding`/`effective`/`inheritable`/
  `permitted`/`ambient`); an empty set **drops all** in it (distinct from absent).
  `capabilities`/`namespaces`/`seccomp`/`devices`/`rlimits` reuse `oci_spec`
  types. An id-mapping `host` may be `"self"` — the invoking
  user's id, resolved at launch. **The floor these carry comes from a producer**
  (`profiles.base`), never the engine.
- **`root`** — a single, **required** store path: the **rootfs**. The FHS-root
  mounted at `/` (read-only), carrying `/etc`, `/bin`, and the static mountpoint
  dirs (`/proc`, `/dev`, `/tmp`, an empty `/nix/store`) — the `/nix/store/<hash>`
  tree is a read-only launch-time bind, not baked in.
  It's a closure seed (declared in `closure.seeds`) and where `PATH` conventionally
  points, and it's in `closure.resolved` too — identity-bound at
  `/nix/store/<self>` so absolute references into it resolve — on top of being the
  `/` mount. Produced by a Nix build (or an authored config). Named `root`, not
  `image` — a store-path rootfs, not a pullable registry image. See
  [Sealed rootfs](#sealed-rootfs-implemented-on-the-sealed-rootfs-branch).
- **`mounts`** — additive binds layered onto the root. Kept as a `type`-tagged
  union rather than compose's `volumes` + `tmpfs`. See the `Mount` model below.
- **`environment`** — producer-filled key/values (compose `environment`). `PATH`
  lives here and is the single source of truth for `executable` lookup. The engine
  merges nothing; an absent `PATH` is just absent (the producer's responsibility).
  **`PATH` is an upper-layer (bridge / producer) concern only** — mounts never
  auto-extend it. A mount places bytes but contributes *nothing* to `PATH`;
  auto-pulling mount `bin/` dirs onto `PATH` was considered and rejected as
  unhelpful (it couples filesystem exposure to executable resolution, which the
  producer should decide deliberately). If you mount a tool, you put its `bin/` on
  `PATH` yourself.
- **`closure`** — the Nix store view, **`{ seeds, resolved }`**, **present from
  the build**. Its `resolved` is what marks a spec built: **present** ⇒
  engine-ready, pre-baked; **absent** ⇒ unbuilt — and since there is **no runtime
  query**, `run` fails loud. Build it with `resolve --graph`, which traverses
  Nix's `exportReferencesGraph` JSON from `seeds` (`resolve::resolve_from_graph`)
  — every node validated as a top-level store path, fail loud.
  - **`seeds`** — a **build-time input**: the seed store paths the closure is
    traversed *from*, set by the producer that emits the spec (conventionally
    `root` + overlay-lower sources; `Host`/`Tmpfs` mounts contribute none). This
    is what changed: seeds is no longer *derived* at resolve — the build declares
    it, and `resolve` walks the reference graph from it. `resolve` fails loud if
    `seeds` is empty or its closure doesn't cover `root`.
  - **`resolved`** — the flat identity-bind set (what actually gets bound),
    **stamped at resolve** and **canonically sorted** (lexicographic by store
    path). Order is cosmetic to the engine (it binds a set), but sorting makes any
    two builds of the same inputs *byte-identical*, so the artifact is reproducible
    regardless of closure source. The full closure of `seeds`, **including `root`**
    (identity-bound at `/nix/store/<self>` as well as being the `/` mount).

## The mount model

`mounts` is `Vec<oci_spec::Mount>` — **verbatim OCI mounts**, no custom type. A
`Host` bind, a `Tmpfs`, an overlay, and an idmapped mount are all the same shape
(`{ destination, source, type, options, uidMappings?, … }`), passed straight
through; the producer (CLI / Nix) owns every field.

| Kind | How it's expressed as an OCI mount |
|---|---|
| host bind | `{ type: "bind", source, destination, options: ["bind","ro"/"rw","nosuid",…] }` |
| tmpfs | `{ type: "tmpfs", destination, options: ["nosuid","nodev","mode=1777"] }` |
| overlay | `{ type: "overlay", destination, options: ["lowerdir=…","upperdir=…","workdir=…"] }` |
| idmapped | any of the above plus `uidMappings`/`gidMappings` |

- **A relocated store path** is just a host bind whose `source` is `/nix/store/<x>`
  — its deps resolve because they're in `closure`, not because the mount folds
  anything.
- **An overlay** over a store lower is an OCI `type: "overlay"` whose `lowerdir` is
  a store path. Realization (rootless overlayfs needs a recent kernel, else
  `fuse-overlayfs`) is the runtime's business — the engine passes it through and
  crun fails loud if it can't.
- **Mounts never seed the closure.** If a mount's source (a relocated path, an
  overlay lower) needs its closure bound, the **producer adds it to
  `closure.seeds`**. There is no engine-side seed collection.

**Seeds** are a pure **build-time input** (`closure.seeds`), declared by the
producer — `root` plus whatever store sources it wants bound. These are the roots
the closure is traversed from (see [Resolution invariant](#resolution-invariant)).

## Resolution invariant

`resolve --graph` turns a spec whose `closure.resolved` is `None` into one where
it's `Some` — at **build time**, from a supplied reference graph, never a live
query. `resolved` is the transitive closure of the spec's declared
`closure.seeds` over that graph:

```
resolved = closure(seeds)      seeds = { root } ∪ { overlay lowers }   (declared by the build)
```

- `seeds` is **declared, not derived**: the build sets `closure.seeds`, and
  `resolve` walks the graph from it. It **fails loud** if `seeds` is empty or its
  closure doesn't cover `root`.
- `root` must be covered by the seeds' closure, and is in `resolved` —
  identity-bound at `/nix/store/<self>` *and* the `/` mount.
- `Host` and `Tmpfs` mounts contribute **nothing** to the closure — they're
  realized as direct engine mounts onto baked mountpoints.
- `executable` contributes **nothing** — it's resolved *within* the closure by
  the runtime against the container's `PATH` at launch (in-sandbox), and errors if
  absent. No host-side exec lookup.
- At run time there is no resolution at all: `run` requires a spec that already
  carries its `closure.resolved` and **fails loud** otherwise. There is no live
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

> A gotcha the code encodes: it's the **top-level** `.closure` key that holds the
> expanded graph — `.exportReferencesGraph.closure` in the same file only echoes
> the requested roots, so it's no shortcut. (`closureInfo` no longer appears in
> the sandbox build at all — the store mountpoints it used to feed are now
> assembled read-only at launch, not baked into `root`.)

## Realization (what the engine does)

- `closure.resolved` → a **read-only** `/nix/store` bind of a launch-time state
  dir (one mountpoint stub per path, file- or dir-shaped to match), then one ro
  identity bind per path onto its stub. Includes `root` itself (bound at
  `/nix/store/<self>`). The store tree is provided at runtime — not baked into
  `root` — yet stays read-only (the stubs pre-exist, nothing is created at mount
  time). Per-path binds today; the merged-tree representation is the lever-6 perf
  question — see [startup-performance.md](startup-performance.md).
- `mounts` → appended **verbatim** after the closure binds (so an overlay
  `lowerdir` or relocated source already exists). Binds, tmpfs, overlays, idmapped
  mounts alike — the engine adds nothing; if crun can't realize one (rootless
  overlayfs on an old kernel), it fails loud.
- OCI pass-throughs (`capabilities`, `seccomp`, `namespaces`, mappings, masks,
  `rlimits`, `rootfs_propagation`, `no_new_privileges`, `read_only`) → the matching
  OCI fields, projected faithfully; `id-mapping host: "self"` resolved to the
  invoking uid/gid. Absent → the engine emits nothing (it even neutralizes
  `oci-spec`'s own builder defaults).
- `executable` + `args` → `process.args` (`executable` as `argv[0]`). `tty` →
  `process.terminal` (+ a devpts mount). `working_dir` → `process.cwd` (absent →
  `/`).
- `environment` → `process.env`, verbatim.
- `read_only` → `root.readonly` (default `true`; `false` is the recorded OPEN).
- OCI `root.path` **is `root`** — the sealed rootfs store path, mounted at `/`
  and remounted read-only. `root` carries the skeleton it can bake statically:
  `/etc`, `/bin`, and the base mountpoint dirs (`/proc`, `/dev`, `/tmp`, and an
  empty `/nix/store`). It does **not** carry the `/nix/store/<hash>` tree — that
  topology depends on the closure, so it's assembled at launch (a state dir of
  stubs, bound read-only at `/nix/store`; see the `closure.resolved` bullet
  above), which keeps `root`'s own Nix references to just its real content. `root`
  is **also** in `closure.resolved`: identity-bound at `/nix/store/<self>` so
  absolute references into it resolve, on top of being the `/` mount — see the
  sealed-rootfs note below.

### Sealed rootfs (implemented on the `sealed-rootfs` branch)

The rootfs is a single store path whose *content* is frozen at build time; its
`/nix/store` mount tree is assembled read-only at launch. Consequences that
supersede parts of this doc:

- **`root` is the rootfs *and* a closure bind.** It's mounted at `/` and
  identity-bound at `/nix/store/<self>`, so absolute references into it resolve
  either way. There is no build-time circularity to break: `root` bakes no
  `/nix/store/<hash>` mountpoints (that tree is a launch-time bind), so its Nix
  closure is simply what it references, seeded from `root` itself — no need to
  seed from its dependencies or fix up a self-mountpoint from `$out`.
- **No runtime query — pre-bake only.** The live `nix-store --query` path is
  gone entirely (`closure_of` and the `PIEROGI_CLOSURE` stub deleted). A spec's
  `closure` is built at *build time* by traversing a Nix `exportReferencesGraph`
  dump from the seeds (`resolve --graph`, `resolve::resolve_from_graph`) — pierogi
  walks the reference graph itself, so `resolved` is provably the closure of the
  declared seeds rather than a flat list taken on faith. `run` on an unbuilt spec
  **fails loud**. There is no `Mount::Store` type — a bare store mount was a
  closure seed in disguise (folds are gone), and a relocation is now just a
  `Host` bind whose deps live in `closure`.
- **No runtime store additions.** A store path added at launch would change the
  closure, which is frozen at build time (its resolved set drives the launch-time
  store binds, and overrides never re-resolve), so `--mount store:` is gone and a
  different closure means a different image. `--mount host:` / `tmpfs:` still
  work, but only onto dests the image already baked.
- **A built `closure` is authoritative.** `resolve()` only asserts
  `closure.resolved` is there — no query, no seed-drift check. With no runtime
  path that can change a sealed spec's seeds, comparing them every launch would
  only catch a hand-edit-without-re-resolve, so it's dropped. `closure.seeds`
  stays as the build-time input `resolved` was traversed from — also a
  fingerprint to parity-check against a fresh build out-of-band.
- **File-vs-dir mountpoints are made at launch.** A file store path (e.g. a
  `writeText` `/etc` file) needs a file mountpoint, a dir needs a dir — the
  engine stats each resolved path and creates the matching stub kind in the state
  dir it binds read-only at `/nix/store` (`run::scaffold_store`). This covers the
  store tree only; the *static* mountpoints (`/proc`, `/dev`, `/tmp`, and base
  dirs like `/home`) are still baked into `root` at build time.

## JSON serialization

Serde-native. `mounts` are **verbatim OCI mounts** (`{ destination, source, type,
options }`); the OCI pass-throughs use their OCI shapes. The store paths below use
short fake hashes for readability.

**Mounts** — OCI mounts, one object each:

```json
{ "type": "bind",  "source": "/etc/resolv.conf", "destination": "/etc/resolv.conf", "options": ["bind","ro","nosuid","nodev"] }
{ "type": "bind",  "source": "/data", "destination": "/data", "options": ["bind","rw","nosuid","nodev"] }
{ "type": "bind",  "source": "/nix/store/cccc-myproject", "destination": "/home/dev/project", "options": ["bind","ro"] }
{ "type": "tmpfs", "destination": "/tmp", "options": ["nosuid","nodev","mode=1777"] }
{ "type": "overlay", "destination": "/home/dev/project", "options": ["lowerdir=/nix/store/cccc-myproject","upperdir=/var/lib/pierogi/up","workdir=/var/lib/pierogi/work"] }
```

The third `bind` line is a **relocated store path** — a plain bind of a store path
at a non-store `destination`; its deps resolve because they're in `closure` (the
producer seeded `cccc-myproject` or its deps), not because the mount folds
anything. The `overlay` line's `lowerdir` is a store path — again, add it to
`closure.seeds` if its closure must be bound.

**Unbuilt spec** (hand-authored, or the sources a Nix builder emits before
`resolve`). Here **profile-less** — no floor fields — so `run` would realize it
*open* (the runc contract); normally a producer composes `profiles.base` first. It
is **not runnable** either way until built: `run` fails loud without
`closure.resolved`.

```json
{
  "root": { "path": "/nix/store/aaaa-base-env" },
  "process": {
    "executable": "bash",
    "args": ["-l"],
    "terminal": true,
    "cwd": "/home/dev",
    "env": {
      "PATH": "/bin",
      "HOME": "/home/dev",
      "TERM": "xterm-256color"
    }
  },
  "mounts": [
    { "type": "bind",  "source": "/etc/resolv.conf", "destination": "/etc/resolv.conf", "options": ["bind","ro","nosuid","nodev"] },
    { "type": "tmpfs", "destination": "/tmp", "options": ["nosuid","nodev","mode=1777"] }
  ],
  "closure": { "seeds": [ "/nix/store/aaaa-base-env" ] }
}
```

Absent floor fields (`read_only`, `capabilities`, `namespaces`, …) stay absent —
the engine adds nothing. Composing `profiles.base` is what fills them, visibly, in
the resolved artifact.

**Built spec** (`resolve --graph` output / a pre-baked artifact) — the same
document with `closure.resolved` stamped in alongside the input `seeds`:

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

`seeds` is just `root` here (no overlay mounts) — the build-time input; `resolved`
is the closure of the seeds over the reference graph, **including `root`**,
canonically sorted, stamped by `resolve`.

## Worked examples: closure resolution

Each walks `resolve --graph` over the spec above's building blocks. There is no
`nix-store` query — the closure is the traversal of Nix's `exportReferencesGraph`
from the seeds (see [Nix-mode closure source](#nix-mode-closure-source-build-time)).

**1 — bare rootfs.** `root = base-env`, `executable = bash`, no mounts, `PATH =
/bin`.
- the build declares `closure.seeds = [base-env]` → `resolve` traverses the graph
  from `base-env` → `resolved = {base-env, bash, glibc, ncurses, coreutils, …}`
  (root included).
- realize: mount `base-env` at `/`, identity-bind every `resolved` path under
  `/nix/store` (onto baked mountpoints), run. `executable bash` is resolved by the
  runtime against `PATH` in-sandbox → `/bin/bash` → the store bash; errors if it's
  absent from the closure.

**2 — in the closure, but not on `PATH` (the gotcha).** The image's closure
includes `ripgrep` (a build-time decision — the rootfs derivation references it),
but `executable = rg` with `PATH = /bin` and no `rg` in `/bin`.
- `rg` is bound under `/nix/store`, but exec resolves via `PATH` only. `which rg`
  over `PATH` → **not found → hard error.** Fix is the producer's: put
  `ripgrep/bin` on `environment.PATH`, or symlink `rg` into the rootfs's `/bin`.
  Being in the closure never extends `PATH`.

**3 — relocated project.** `{ type: host, source: /nix/store/…-myproject, dest:
"/home/dev/project" }`, `executable = bash`. `myproject` is a source tree whose
`node_modules/*` symlink into `/nix/store` (dep: `nodejs`).
- A `Host` mount contributes **no** seed, so `myproject`'s closure is *not* pulled
  in automatically. For the inner symlinks to resolve, `nodejs` must be in
  `closure.resolved` — the producer seeds it from the rootfs derivation at build
  time (and `myproject` too, if anything references it by store path).
- realize: `myproject` bound only at `/home/dev/project` (a plain host bind);
  `nodejs` identity-bound because it's in `resolved`. This is the sealed-world
  replacement for the old "relocated store mount": *placement* (the bind) and
  *closure coverage* (the deps) are decided separately, both at build time.

**4 — sealed launch (`closure.resolved` present).** A pre-baked spec — the only
runnable form.
- `resolve` sees `closure.resolved` → asserts it and returns **verbatim**: no
  query, no seed comparison, no re-derivation.
- realize from `closure.resolved` (root at `/`, every member identity-bound),
  `executable` resolved against `PATH` in-sandbox. There is no live fallback — an
  unbuilt spec (no `resolved`) would have failed loud at `run`.

## Open / deferred

- **DECIDED — closure is `{ seeds, resolved }`** — **`seeds` a build-time input**
  (the paths to traverse from, declared by the producer) and **`resolved` stamped
  at resolve** (the built bind set).
- **DECIDED — no custom mount type.** `mounts` is `Vec<oci_spec::Mount>`; binds,
  tmpfs, overlays, and idmapped mounts are all verbatim OCI mounts. An overlay is
  a `type: "overlay"` mount whose `lowerdir` is a store path; a relocation is a
  bind whose source is a store path. Neither seeds the closure — the producer adds
  the source to `closure.seeds` if its closure must be bound. (This dissolves the
  old `Host`/`Tmpfs`/`Overlay` union and the `Store` variant.)
- **TODO — overlay's operational path.** Realization is crun's (the engine passes
  the mount through): unprivileged overlayfs-in-userns needs kernel ≥ 5.11, else a
  `fuse-overlayfs` fallback; tmpfs-upper sizing/limits still open.
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
  `/`: it carries `/etc`, `/bin`, and the static mountpoint dirs (`/proc`, `/dev`,
  `/tmp`, an empty `/nix/store`), baked at build time. The `/nix/store/<hash>`
  tree is not baked in — it's a read-only launch-time bind (a state dir of stubs).
  `root` is also in `closure.resolved` — identity-bound at `/nix/store/<self>` so
  absolute references into it resolve — on top of being the `/` mount. This
  resolves the old "root-as-mount" question: `root` is now *structurally special*
  (the `/` mount), not just another store bind. See
  [Realization](#realization-what-the-engine-does).
- **TODO — the rest of the runtime surface.** `hostname`, `tty`, `working_dir`,
  and `read_only` are now folded into the flat schema (compose-flavoured); still
  outstanding: namespaces, uid/gid maps (`user`), dbus/seccomp rules — present in
  the engine today (`src/spec.rs`/`src/oci.rs`) but not yet spec fields. Add per
  feature/milestone, running each through
  [control-surface.md](control-surface.md)'s litmus.
- **TODO — wire format.** JSON vs other, and public-contract status — deferred to
  M1 per [architecture.md](architecture.md#spec--rootfs-build-time-vs-runtime).
