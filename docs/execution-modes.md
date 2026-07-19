# Pierogi — Execution modes

How an invocation becomes a running sandbox: the layers between `pierogi wrap
-- bash` and an `exec`, the verbs that tap them, and the one knob (`closure:`)
that decides whether the expensive step runs at all. See [architecture.md](architecture.md)
for the engine internals and [startup-performance.md](startup-performance.md)
for why the closure query is the cost this design is built to dodge.

## The pipeline

Every invocation flows through the same four layers. Only the *first* differs
by mode; everything from `resolve()` onward is mode-agnostic.

```
INPUT              BRIDGE (front-end)            RESOLVE                 SPEC          ENGINE
─────              ──────────────────            ───────                 ────          ──────
bare argv    ─┐                              ┌ closure_of() ─┐
config file  ─┼─►  normalize → Config    ──► │  OR           │ ──► build_spec ──► backend.run
store path   ─┘    (+ strict defaults,       └ short-circuit ┘     (spec.rs)
                    identity, scan scope)        if closure:
                    the "just works" layer      the "expensive" layer   mode-agnostic tail
```

- **Bridge (front-end):** *input form → canonical `Config` + defaults.* Where the
  "just works" ergonomics live — strict defaults (read-only root, all namespaces,
  no network, deny-all D-Bus), synthetic identity, scan-scope choice, and merging
  of layered input (a config file with CLI overrides on top). This is the layer
  the architecture doc gestures at as "the front-ends generate the spec."
- **Resolve:** *`Config` → closure / rootfs.* The only layer that ever touches
  Nix — and only when the `Config` carries no precomputed `closure:` to
  short-circuit it. Today this is `closure_of()` → `nix-store --query
  --requisites` (see `src/closure.rs`).
- **Spec / Engine:** `build_spec()` builds the in-memory OCI spec (`src/spec.rs`)
  and the backend drives it (`src/runtime`). Identical for all modes.

**Key invariant:** the security-critical core (resolve onward) never learns which
mode it is in. Modes live entirely in the bridge plus the one resolve
short-circuit — which is what keeps the core auditable.

## The verbs

Each verb is a **tap at a pipeline layer** — the same pipeline, stopped at a
different point. Input type splits cleanly and is what makes one surface a flag
and another a verb: **argv → `wrap`; a spec → `resolve` / `run`.**

| Verb | Input | Stops after | Emits / does |
|---|---|---|---|
| `wrap -- cmd` | argv | engine | runs the process (all layers fused) |
| `wrap --dry-run -- cmd` | argv | bridge | prints the **unresolved** spec (sources, no `closure:`) |
| `resolve <spec>` | spec | resolve() | prints the **resolved** spec (**+ `closure:`**) |
| `run [overrides] <spec>` | spec | engine | runs the spec |

**`pierogi wrap [flags] -- <cmd>`** — the argv-first, zero-config "just works"
surface. Not a separate engine path: the bridge **synthesizes a spec** from
`argv` + strict defaults + the default base rootfs, then feeds the same
`resolve → spec → engine` tail. `wrap --dry-run` stops after the bridge and
prints that synthesized spec **unresolved** (sources only, no `closure:`) — the
editable *starting point* for hand-authoring. It is a flag on `wrap` because
`wrap` owns the argv front-end; `--dry-run` just says "stop after synthesizing,
don't run."

**`pierogi resolve <spec>`** — takes an existing pierogi spec and emits it
**resolved: spec + `closure:`**. This is exactly the resolution step `run` runs
internally (`closure_of()` → staple the result on), exposed standalone — the
**pre-bake / freeze** operation that produces the sealed nix-mode artifact. The
Nix builder shells it (sibling to `pierogi synth-etc`). Input is a spec, not
argv, so it is a verb, not a flag.

**`pierogi run [overrides] <spec>`** — runs a spec. Branches on `closure:`
presence: **absent** → dynamic config, resolved live at launch; **present** →
pre-baked, sealed, run as-is. The distinction is invisible at the CLI — just "is
this spec already resolved."

Because each verb is one layer, they **compose**:

```
pierogi wrap --dry-run -- bash     # argv → unresolved spec   (bridge)
  │  > sandbox.json ; edit
  ▼
pierogi resolve sandbox.json       # spec → spec + closure     (resolve)
  │  > sandbox.frozen.json
  ▼
pierogi run sandbox.frozen.json    # spec → running process    (engine)
```

`pierogi wrap -- bash` is all three fused; `run <unresolved-spec>` is `resolve`
then run internally.

## Config vs Spec: one schema, three provenances

There is a **`Config`** (declarative sources) distinct from the **`Spec`**
(resolved). The same schema appears at three points along resolution — the three
run modes are one `Config` at different stages, not three schemas:

| | `pierogi wrap` | `pierogi run <config>` | `pierogi run <store-path>` |
|---|---|---|---|
| Provenance | **synthesized** from argv+defaults | **authored** by hand | **frozen** (resolve output stapled on) |
| `root` | injected base | declared | declared (kept for audit) |
| `closure:` key | absent | absent | **present** |
| Resolution | live query over declared seeds | live query over declared seeds | **short-circuit — no query** |
| Reproducible? | no (tracks host store) | contents declared, resolved live | **fully sealed** |
| Speed | pays the query | pays the query | **fast-path, no query** |
| Position | ◄ just works | ── middle ── | works consistently ► |

The three modes are ergonomic **bundles** of two underlying axes — *provenance*
× *resolution time* (live/sealed) — not walls. Off-menu combinations are legal:
an **authored config carrying a `closure:` key** is a "sealed dynamic config" —
hand-frozen reproducibility with no Nix involved.

`pre-baked = dynamic config, frozen`: the Nix builder serializes a resolved
config with `closure:` stapled on. `run <unresolved-spec>` and the build path
share the **stamping/serialization** codepath — *not* the query: a build sandbox
has no daemon socket or Nix DB, so `nix-store --query --requisites` cannot run in
it. Instead the closure is **supplied** by Nix (`closureInfo` / the reference
graph — see [Nix-mode closure source](spec.md#nix-mode-closure-source-build-time)),
and `resolve` there is pure data-shuffling. The `nix-store` query is one
*pluggable source* of the closure (live wrap on a real host); `closureInfo`
output and the `PIEROGI_CLOSURE` stub are the others.

## Resolution rules

Today `src/run.rs` fuses "what to exec" and "what the rootfs is" into
`argv[0] → closure → rootfs`. The core of this design is **splitting them**: the
closure comes from **declared sources only** (`root` + store mounts); the exec
target is *resolved against* that closure, never a source for it. There is no
exe-derived closure — `argv[0]` is looked up, not scanned.

The closure has exactly two seeds, both declared:

1. **`root`** (required) → the base closure seed: `closure(root)`, with PATH from
   its bin dirs. `argv[0]` is resolved **within** the closure and is an **error if
   absent** — exec never expands the closure (Docker semantics; maximally
   auditable). This holds in *every* mode; there is no exe-derived fallback and no
   `root: absent` case.
2. **store mounts** → additive seeds, each dragging its own closure, unioned onto
   root's. The full mount taxonomy by closure cost:

| Mount kind | Has closure? | Query? |
|---|---|---|
| host bind | no (not a store path) | never — ro-default, narrow |
| tmpfs / state | no | never — writable scratch |
| store-path, declared at build | yes | no — folded into the resolved set |
| store-path, added at runtime | yes | **yes — the only query trigger** |

`closure = closure_of(root) ∪ ⋃ closure_of(store mount)` — nothing is inferred
from the command line (see [Declared-only closure](#declared-only-closure-no-discovery)).

## `closure:` — memoized resolution

`closure:` is not a new source; it is the **cached output** of resolving the
static sources. Its presence means "resolution already happened — do not re-run
`closure_of()`." This is the production form of the `PIEROGI_CLOSURE` bench stub
in `src/closure.rs`. `closure_of()` is a **pluggable source**: a live `nix-store
--query --requisites` (wrap, on a real host) *or* a supplied list. A Nix build
takes the supplied path — it knows the closure for free (`closureInfo` / the
reference graph) and could not query anyway (no daemon in the sandbox), so baking
it into the emitted `Config` is just not discarding that information. Reuse the
stub's validation (all under `/nix/store`, non-empty, fail loud) so a malformed
`closure:` aborts rather than silently falling back to a query.

When both `root` and `closure:` are present, `closure:` drives execution and
`root` is verifiable provenance — a test can assert `closure == resolve(root)`.

**Representation is swappable.** `closure:` as a flat list (per-path binds — the
current engine, one `mount` syscall per path) and nix-mode's `rootfs -> <merged
tree>` symlink are two serializations of the same "resolution done statically"
idea. Both set the no-query flag; the choice between them is the mount-count
lever (see startup-performance.md, lever 6) and lives entirely behind the
`{spec, rootfs}` boundary. Per-path binds for now.

## Declared-only closure (no discovery)

The closure is **exactly what's declared** — `root` + store mounts (+ `closure:`
if present). Nothing is inferred from `argv`, program arguments, or mount
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

## `wrap`'s default base rootfs — baked, with a live opt-in

`wrap` ships a small default base env so `wrap -- bash` yields a usable shell
(coreutils, etc.), not just bash's bare closure.

- **Baked default:** the base is pinned as a **runtime dependency of the pierogi
  binary** — its closure is sealed, present by construction (offline,
  reproducible), and known at build. Because bash resolves *within* that sealed
  base (rule 1, Docker-within-root), `wrap -- bash` pays **no query** for the
  common case; you pay a marginal delta only when wrapping something *outside* the
  base. This turns the "just works" verb into a fast path.
- **`--base <profile|pkgs>` live opt-in:** bootstrap a base at launch when you
  want to track current nixpkgs, at the cost of a query and needing Nix at
  runtime. The sealed-default / explicit-live split, applied to the base rootfs.

Caveat: baked assumes pierogi was Nix-installed (its runtime closure carries the
base); `--base` needs Nix available at launch.

## `run` overrides — rewrites, and when they recompute

Overrides on `run` are the bridge overlaying CLI input onto the loaded spec. The
rule follows from the mount taxonomy: **only closure-touching overrides
recompute; runtime-only overrides just rewrite.**

| Override | Closure cost? | Effect |
|---|---|---|
| `--env`, `--hostname`, `--net`, `--cwd` | none | rewrite spec field |
| `--exec` / argv | none | rewrite exec — **within** the closure (Docker-within-root), else **error** |
| `--mount host:/x`, tmpfs, state | none | layer on |
| `--mount store:/…` | **yes** | **delta recompute** over the existing set |

Seal interaction: when `<spec>` is pre-baked (`closure:`) and an override adds a
store path, that is the exact — and only — point where the seal breaks, gated by
an explicit override. `run` overrides are where the deferred sealed/delta policy
will eventually be answered.

## Roadmap fit

- **`wrap`** exists today (M0/M1) in an exe-derived form; the redesign relocates
  it into the bridge and swaps exe-derivation for a **baked base `root`** (exe
  resolved within it). Exe-derivation is dropped outright — there is no
  command-line closure discovery.
- **dynamic config** is the new middle: needs the `Config` schema + file loader +
  the authored bridge path. Lands with M1's spec work.
- **pre-baked** is M3 — cheap once dynamic config + `resolve()` exist (it is just
  resolve-then-freeze).
- The **Nix module layer (M7)** authors one of these (almost certainly emits
  pre-baked).

## Open / deferred

- **Sealed vs delta default** — with `--auto` dropped, the only way a store path
  enters a sealed spec at launch is an explicit `--mount store:` override on
  `run`. The open question narrows to just that: when an override adds a store
  path to a pre-baked spec, does the seal break silently, warn, or require a flag?
- **Discovered closure (`--auto`)** — parked, not part of v0 (see [Declared-only
  closure](#declared-only-closure-no-discovery)); revisit only if declared-only
  proves too rigid.
- **Rootfs representation** — per-path binds now; merged tree is the lever-6
  perf question, behind the `{spec, rootfs}` boundary.
