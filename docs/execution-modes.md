# Pierogi — Execution modes

How an invocation becomes a running sandbox: the layers between a spec and an
`exec`, the verbs that tap them, and the invariant that a runnable spec already
carries its `closure` — there is no launch-time Nix query. See
[architecture.md](architecture.md) for the engine internals and
[startup-performance.md](startup-performance.md) for why closure resolution is
pushed to build time and never paid at launch.

## The pipeline

Every invocation flows through the same four layers. Only the *first* differs
by mode; everything from resolution onward is mode-agnostic. Resolution runs
**ahead of the engine** — at build time in nix mode, or in `wrap`'s runtime bake
— but never *inside* the `run` core, which only ever consumes an already-sealed
spec.

```
INPUT              BRIDGE (front-end)         RESOLVE (build time)      SPEC          ENGINE
─────              ──────────────────         ────────────────────     ────          ──────
bare argv    ─┐                            traverse the reference
config file  ─┼─►  normalize → Config  ──► graph from the seeds  ──► build_spec ──► crun (libcrun)
store path   ─┘    (+ strict defaults,     → stamp closure           (spec.rs)      load_from_memory
                    identity, scan scope)   (src/resolve.rs)
                    the "just works" layer  pre-baked, never at launch  mode-agnostic tail
```

- **Bridge (front-end):** *input form → canonical `Config` + defaults.* Where the
  "just works" ergonomics live — strict defaults (read-only root, all namespaces,
  no network, deny-all D-Bus), synthetic identity, scan-scope choice, and merging
  of layered input (a config file with CLI overrides on top). This is the layer
  the architecture doc gestures at as "the front-ends generate the spec."
- **Resolve:** *`Config` → closure.* Runs **ahead of the engine**, not inside the
  `run` core. In nix mode it's a build step: it takes Nix's `exportReferencesGraph`
  (the reference graph of the seeds, as JSON) and **traverses it from the spec's
  seeds in-process** (`resolve_from_graph`, `src/resolve.rs`), stamping the
  resulting `closure` in — no live `nix-store` query, the graph is supplied. In
  `wrap` mode the equivalent bake happens at launch, in a scratch dir (see below).
  Either way, a spec that reaches the engine without a built `closure` is a hard
  error (fail loud), never a launch-time resolve *by `run`*.
- **Spec / Engine:** `build_spec()` builds the in-memory OCI spec (`src/spec.rs`)
  and **crun** drives it — libcrun via the FFI shim, loading the spec from an
  in-memory JSON string (`libcrun_container_load_from_memory`, no `config.json`),
  `src/runtime/{mod,crun}.rs`. crun only — no runtime abstraction. Identical for
  all modes.

**Key invariant:** the security-critical core (resolve onward) never learns which
mode it is in. Modes live entirely in the bridge; the core only ever consumes a
fully built spec — which is what keeps it auditable.

## The verbs

Each verb is a **tap at a pipeline layer** — the same pipeline, stopped at a
different point. Input type splits cleanly and is what makes one surface a flag
and another a verb: **ad-hoc input (an exe/argv, or a store path to fold) →
`wrap`; a spec → `resolve` / `run`.** The wrap family is the **bridge**: it
*synthesizes* a spec from whatever ad-hoc input it's handed, then feeds the same
`resolve → spec → engine` tail. `run` and `resolve` only ever consume a spec.
**The engine runs a spec and nothing else** — the ad-hoc modes are conveniences
that build one first.

| Verb | Input | Stops after | Emits / does |
|---|---|---|---|
| `wrap -- cmd` | exe / argv | engine | **bake a scratch rootfs** at runtime over the target's closure, resolve exe within it → run (spec **synthesized**; `wrap` unimplemented) |
| `wrap <store-path> -- cmd` | store path | engine | fold the store path as `root`, `PATH` from its `bin/`, run cmd (spec **synthesized**) |
| `wrap --dry-run …` | ad-hoc | bridge | prints the **unbuilt** spec (sources, no `closure`) |
| `resolve <spec> --graph <ref-graph>` | spec + graph | resolve | prints the **built** spec (**+ `closure`**) |
| `run [overrides] <spec>` | spec | engine | runs a **pre-baked** spec (fails loud if unbuilt) |

**`pierogi wrap [flags] <input> -- <cmd>`** — the ad-hoc, zero-config "just
works" surface, and the *only* place a store path or a bare exe is turned into a
sandbox. Two ad-hoc inputs, one behavior: **an exe/argv** (resolved *within* a
baked base `root`) or **a store path to fold** (an arbitrary package / buildEnv /
profile → `root`, `PATH` from its `bin/`). Either way it's not a separate engine
path — the bridge **synthesizes a spec** from the input + strict defaults + the
default base rootfs, then runs the normal tail. `wrap --dry-run` stops after the
bridge and prints that synthesized spec **unresolved** — the editable *starting
point* for hand-authoring. (The whole `wrap` family lands at M4. The temporary
`run-argv` exe bridge that stood in for it was **rolled back** once nix-mode
build-time resolution became the way in — today the engine is spec-only: `run`
and `resolve` consume a spec, and nothing synthesizes one yet.)

> **"Run a store path" is not a `run` mode.** Folding an *arbitrary* store path
> (a package/env) to run a command in it is ad-hoc → it's a **`wrap`** input, per
> the row above. The nix-mode "store path" is a different thing: a resolved
> `spec.json` that merely *lives* in the store — running it is plain
> **`run <spec>`**, the store-path-ness incidental. So a store path reaches the
> engine two unrelated ways — folded by `wrap` (ad-hoc), or as a spec file for
> `run` (pre-baked) — and only the latter is a `run` mode.

**`pierogi resolve <spec> --graph <ref-graph>`** — takes an existing pierogi
spec plus a supplied `exportReferencesGraph`, traverses it from the spec's seeds,
and emits the spec **built: spec + `closure`**. This is the **pre-bake / freeze**
operation that produces the sealed nix-mode artifact — and the *only* place a
closure is ever computed. `run` never does this; a spec that isn't built can't
run. The Nix builder shells it (sibling to `pierogi synth-etc`), handing it the
reference graph it already has for free. Input is a spec, not argv, so it is a
verb, not a flag.

**`pierogi run [overrides] <spec>`** — runs a **pre-baked** spec. The spec must
already carry its `closure`; if it doesn't, `run` **fails loud** (pre-bake it
with `resolve --graph` first). There is no resolve-at-launch branch — `run`
consumes the built closure verbatim.

Because each verb is one layer, they **compose**:

```
pierogi wrap --dry-run -- bash            # argv → unbuilt spec       (bridge)
  │  > sandbox.json ; edit
  ▼
pierogi resolve sandbox.json --graph refs.json   # spec + graph → + closure  (build)
  │  > sandbox.frozen.json
  ▼
pierogi run sandbox.frozen.json           # spec → running process    (engine)
```

`pierogi wrap -- bash` fuses the bridge and engine, **baking a sealed rootfs at
launch** (in a scratch dir, outside the store) and running it through the same
machinery; `run` only ever consumes an already-built spec, and `resolve --graph`
is the only verb that builds one.

## Config vs Spec: one schema, three provenances

There is a **`Config`** (declarative sources) distinct from the **`Spec`**
(built — closure stamped in). The same schema appears at three points — these
three **provenances** are one `Config` at different stages, not three schemas
(and the store-path column is a pre-baked *spec* file that merely *lives* in the
store, i.e. `run`, not the ad-hoc store-path fold that belongs to `wrap`):

| | `pierogi wrap` | `pierogi run <config>` | `pierogi run <store-path spec>` |
|---|---|---|---|
| Provenance | **synthesized** from argv+defaults | **authored** by hand | **build-baked** (`resolve --graph` output) |
| `root` | injected base | declared | declared (identity-bound; kept for audit) |
| `closure` key | inherited from baked base | **absent** | **present** |
| Built when | ahead of time (baked base) | must be built before it runs | at Nix build time |
| Reproducible? | tracks the baked base | not until built | **fully sealed** |
| Runnable as-is? | yes (base pre-resolved) | **no — `run` fails loud** | **yes — closure verbatim** |
| Position | ◄ just works | ── middle ── | works consistently ► |

Provenance is the only axis that varies — **resolution is always build-time**.
There is no launch-time-resolve mode to trade against. An authored `Config`
becomes runnable exactly when it has been through `resolve --graph`; a
hand-authored spec carrying a stamped `closure` is indistinguishable from a
Nix-baked one at the engine.

Every runnable spec is built the same way: `resolve` traverses a **supplied**
`exportReferencesGraph` from the seeds and stamps the closure in. This works in a
build sandbox precisely because it needs no daemon — there is no `nix-store`
query, live or otherwise. Nix hands the builder the reference graph for free (via
`exportReferencesGraph` under `__structuredAttrs`, in `$NIX_ATTRS_JSON_FILE`), so
`resolve` there is pure graph traversal + serialization. That single graph-in /
closure-out path is the *only* way a closure is produced.

## Resolution rules

The core of this design **splits "what to exec" from "what the rootfs is"**: the
closure comes from **declared sources only**, and the exec target is *resolved
against* that closure, never a source for it. There is no exe-derived closure —
`argv[0]` is looked up in the container's `PATH`, not scanned.

The closure's seeds are a **build-time input** (`closure.seeds`): the producer
that emits the spec declares them, and `resolve` walks the graph from them
(rather than deriving them). Two kinds:

1. **`root`** (required) → the rootfs *and* base closure seed, with `PATH` from
   its `bin` dirs. `argv[0]` is resolved **within** the closure and is an **error
   if absent** — exec never expands the closure (Docker semantics; maximally
   auditable). This holds in *every* mode; there is no exe-derived fallback and no
   `root: absent` case. `resolve` fails loud if the seeds' closure doesn't cover
   `root`.
2. **overlay lowers** → each overlay mount's store-path source is an additive
   seed, dragging its own closure onto root's. The full mount taxonomy by closure
   contribution:

| Mount kind | Contributes a seed? |
|---|---|
| `Host` bind | no — not a store path (ro-default, narrow) |
| `Tmpfs` | no — writable scratch |
| `Overlay` (store-path lower) | yes — its source is a seed |

The producer follows `seeds = root ∪ ⋃ overlay-lower sources`; `resolved`
(stamped by `resolve`) is the full closure of those seeds over the supplied graph,
**including `root`**, canonically sorted. Nothing is inferred from the command
line (see [Declared-only closure](#declared-only-closure-no-discovery)).

## `closure` — the built store view

`closure` is not a new source; it is the **stamped output** of resolving the
declared seeds — `{ seeds, resolved }`. `seeds` is the provenance (`root` +
overlay lowers); `resolved` is the full closure of those seeds over the supplied
reference graph, **including `root`**, canonically sorted. Its presence means the
spec is **built and runnable**; its absence means `run` **fails loud** (there is
no launch-time query to fall back on). It is produced exactly once, by `resolve
--graph` traversing `exportReferencesGraph` in-process (`resolve_from_graph`,
`src/resolve.rs`); the engine consumes `resolved` verbatim.

`root` appears in both places: as a member of `closure.resolved` (identity-bound
so absolute references into it resolve) and as the OCI `root.path` mounted at
`/`. Both name the same sealed store path — `root` is verifiable provenance
against `resolved`, and the two are stamped together at build, never re-derived.

**Representation is swappable.** The engine realizes `resolved` as **per-path
binds** — one read-only identity bind per store path, onto mountpoint dirs baked
into `root` (`src/oci.rs`). A `rootfs -> <merged tree>` symlink is an alternative
serialization of the same "resolution done statically" idea; the choice is the
mount-count lever (see startup-performance.md, lever 6) and lives entirely behind
the spec's rootfs-representation seam. **Per-path binds for now** — so the
nix-mode artifact is just the built spec (no separate `rootfs` entry); a
merged-tree `rootfs` only appears if/when lever 6 lands.

## Declared-only closure (no discovery)

The closure's seeds are **exactly what's declared** — `root` + overlay lowers.
Nothing is inferred from `argv`, program arguments, or mount
*values*. This is the whole "everything the sandbox sees is written down" property:
the visible store is a function of the spec, not of what a command happened to
mention.

A **discovered** mode (`--auto`: scan cli args / mount sources that resolve into
`/nix/store` and fold each in as a store mount) was considered as a "just works"
convenience and **dropped for this layer for now.** With exe-derivation already
gone and the declared model covering the real cases, it earns nothing: it couples
argument *values* to the visible store (a surface a hardened sandbox shouldn't
have), it can't be complete anyway (a `dlopen`ed store path never on the command
line is missed), and it reintroduces the command-line inference the argv[0]-drop
removed. Parked — revisit only if declared-only proves too rigid in practice.

## `wrap`'s rootfs — baked live, outside the store

`wrap` bakes its sealed rootfs **at runtime**, in a scratch dir outside the store,
then runs it through the same machinery as a Nix-built root. The difference from
nix mode is only *where and when* the rootfs is assembled — the scaffolding a Nix
build does at build time (`/etc`, `/bin`, the mountpoints), `wrap` does at launch
in `$TMPDIR` — not how it's run. This is the one place a closure is assembled at
launch; the `run` core stays query-free.

- **Default base:** `wrap` ships a small base env (coreutils, etc.) pinned as a
  **runtime dependency of the pierogi binary**, so for the common `wrap -- bash`
  its closure is already present on the host store (offline, reproducible) — no
  closure computation, just the runtime scaffolding of the scratch tree.
- **`--base <profile|pkgs>` opt-in:** point `wrap` at a different, pre-built base.

Caveat: the pinned default assumes pierogi was Nix-installed (its runtime closure
carries the base); `--base` needs the alternate base built before launch. Whether
an arbitrary target's closure is computed live (a runtime query) or must be
pre-built is the open design point — `wrap` is unimplemented (M4).

## `run` overrides — rewrites only

Overrides on `run` are the bridge overlaying CLI input onto the loaded spec. The
closure is **sealed at build time**, so every override is **runtime-only — a
rewrite, never a recompute.**

| Override | Effect |
|---|---|
| `--env`, `--hostname` | rewrite spec field |
| argv (trailing `-- cmd args`) | rewrite exec — resolved **within** the closure (Docker-within-root), else **error** |
| `--mount host:SRC:DEST`, `--mount tmpfs:DEST` | layer on — dest must be a baked mountpoint in the sealed `root` |

There is **no `--mount store:`** and no closure recompute: a different closure
means a different image, built with `resolve --graph`. Relocating a store path is
a plain `Host` bind whose dependencies are already covered by the built
`closure`, so the seal never breaks at launch.

## Roadmap fit

- **The typed spec schema + `run` + `resolve`** land in **M1 (nix mode)** — the
  reproducible core. Both the authored-config (dynamic) and pre-baked forms fall
  out of the same schema.
- **pre-baked / nix-mode is M1**: `resolve --graph`-then-freeze over a *supplied*
  `exportReferencesGraph`, no query.
- **`wrap`** exists today (M0) in an exe-derived form; the redesign relocates it
  into the bridge and swaps exe-derivation for a **baked base `root`** (exe
  resolved within it) — dropped outright, no command-line closure discovery. The
  `wrap` argv front-end lands with hardening (**M4**).
- The **Nix module layer (M7)** authors one of these (almost certainly emits
  pre-baked).

## Open / deferred

- **Discovered closure (`--auto`)** — parked, not part of v0 (see [Declared-only
  closure](#declared-only-closure-no-discovery)); revisit only if declared-only
  proves too rigid.
- **Rootfs representation** — per-path binds now (closure carried in the spec, so
  the nix-mode artifact is a bare resolved spec); a merged-tree `rootfs` is the
  lever-6 perf question, behind the spec's rootfs-representation seam.
