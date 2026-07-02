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
| `root` | none → exe-derived | declared | declared (kept for audit) |
| `closure:` key | absent | absent | **present** |
| Resolution | live query, scan=argv[0] (+`--auto`) | live query, scan=declared | **short-circuit — no query** |
| Reproducible? | no (tracks host store) | contents declared, resolved live | **fully sealed** |
| Speed | pays the query | pays the query | **fast-path, no query** |
| Position | ◄ just works | ── middle ── | works consistently ► |

The three modes are ergonomic **bundles** of three underlying axes — *provenance*
× *resolution time* (live/sealed) × *scan scope* — not walls. Off-menu
combinations are legal: an **authored config carrying a `closure:` key** is a
"sealed dynamic config" — hand-frozen reproducibility with no Nix involved.

`pre-baked = dynamic config, frozen`: the Nix builder is literally "run the
bridge + resolve() on a dynamic config, then serialize the result with `closure:`
stapled on." So `run <unresolved-spec>` and the build path share a codepath;
build just runs resolve early (via `pierogi resolve`).

## Resolution rules

Today `src/run.rs` fuses "what to exec" and "what the rootfs is" into
`argv[0] → closure → rootfs`. The core of this design is **splitting that**: the
rootfs source and the exec target are resolved separately.

1. **`root` declared** → rootfs = `closure(root)`; PATH from the root's bin dirs;
   `argv[0]` resolved **within** that closure and an **error if absent**. The root
   closure is fixed by the declaration — exec does not expand it. (Docker
   semantics; maximally auditable.)
2. **`root` absent** → rootfs = `closure(argv[0])`; PATH = the target's bin dir.
   Today's exe-derived behavior, unchanged — the `wrap` fallback.
3. **mounts** layer additively onto either, taxonomized by closure cost:

| Mount kind | Has closure? | Query? |
|---|---|---|
| host bind | no (not a store path) | never — ro-default, narrow |
| tmpfs / state | no | never — writable scratch |
| store-path, declared at build | yes | no — folded into the resolved set |
| store-path, added at runtime | yes | **yes — the only query trigger** |

## `closure:` — memoized resolution

`closure:` is not a new source; it is the **cached output** of resolving the
static sources. Its presence means "resolution already happened — do not re-run
`closure_of()`." This is the production form of the `PIEROGI_CLOSURE` bench stub
in `src/closure.rs`: a Nix build knows the closure for free (it *is* what computes
requisites), so baking it into the emitted `Config` is just not discarding that
information. Reuse the stub's validation (all under `/nix/store`, non-empty, fail
loud) so a malformed `closure:` aborts rather than silently falling back to a
query.

When both `root` and `closure:` are present, `closure:` drives execution and
`root` is verifiable provenance — a test can assert `closure == resolve(root)`.

**Representation is swappable.** `closure:` as a flat list (per-path binds — the
current engine, one `mount` syscall per path) and nix-mode's `rootfs -> <merged
tree>` symlink are two serializations of the same "resolution done statically"
idea. Both set the no-query flag; the choice between them is the mount-count
lever (see startup-performance.md, lever 6) and lives entirely behind the
`{spec, rootfs}` boundary. Per-path binds for now.

## `--auto`: declared vs discovered

The real tension is **declared** (everything the sandbox sees is written
down → auditable, reproducible) vs **discovered** (the tool infers closure from
what you *mention* → convenient, occasionally surprising). `--auto` is the dial,
so neither has to be a silent default:

| Scope | What gets closure-expanded | Mode |
|---|---|---|
| static-only | just `closure:` / declared store mounts — **no query** | sealed / fast-path |
| argv[0] | + the executable's closure | **always-on baseline** (the wrap promise) |
| `--auto` | + every cli arg / mount source that resolves into the store | discovered |

- argv[0] resolution is the irreducible wrap promise; `--auto` widens the net
  *past* it. A boolean suffices to start; it can grade later
  (`--auto=args`, `--auto=args+mounts`).
- **Scan predicate:** an arg is discoverable iff it canonicalizes to a path under
  `/nix/store/` (reuse `store_root` / `canonicalize` from `src/closure.rs`).
  Non-store args ignored.
- `--auto` is a heuristic, and that is *why* it is opt-in: it can't be complete
  (a `dlopen`ed store path never on the command line is missed), and it lets
  program arguments widen the visible store — a mild surface a hardened/pre-baked
  sandbox should not have. The declared path is the one you trust.
- Needs a `Config` representation, not just a flag (`resolve: { scan: declared |
  auto }`), so the one-schema property holds; `--auto` is CLI sugar over it.

`closure:` and `--auto` are orthogonal: `closure:` memoizes the *declared*
portion; `--auto` governs whether the *discovered* portion is pulled on top.

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
  runtime. Mirrors the sealed-default / explicit-live pattern used for `--auto`.

Caveat: baked assumes pierogi was Nix-installed (its runtime closure carries the
base); `--base` needs Nix available at launch.

## `run` overrides — rewrites, and when they recompute

Overrides on `run` are the bridge overlaying CLI input onto the loaded spec. The
rule follows from the mount taxonomy: **only closure-touching overrides
recompute; runtime-only overrides just rewrite.**

| Override | Closure cost? | Effect |
|---|---|---|
| `--env`, `--hostname`, `--net`, `--cwd` | none | rewrite spec field |
| `--exec` / argv, argv[0] **within** the closure | none | rewrite exec (Docker-within-root) |
| `--mount host:/x`, tmpfs, state | none | layer on |
| `--mount store:/…` (or exec **outside** the closure) | **yes** | **delta recompute** over the existing set |

Seal interaction: when `<spec>` is pre-baked (`closure:`) and an override adds a
store path, that is the exact — and only — point where the seal breaks, gated by
an explicit override. `run` overrides are where the deferred sealed/delta policy
will eventually be answered.

## Roadmap fit

- **`wrap` / exe-derived** exists today (M0/M1); rule 2 is the current code
  relocated into the bridge.
- **dynamic config** is the new middle: needs the `Config` schema + file loader +
  the authored bridge path. Lands with M1's spec work.
- **pre-baked** is M3 — cheap once dynamic config + `resolve()` exist (it is just
  resolve-then-freeze).
- The **Nix module layer (M7)** authors one of these (almost certainly emits
  pre-baked).

## Open / deferred

- **Sealed vs delta default** — currently expressed as "did an override / `--auto`
  ask for it." Whether `--auto` is ever implicit per surface stays open.
- **Scan-scope grading** — boolean `--auto` now; graded scopes later.
- **Rootfs representation** — per-path binds now; merged tree is the lever-6
  perf question, behind the `{spec, rootfs}` boundary.
