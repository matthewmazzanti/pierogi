# Pierogi — The control surface

> **Status: converging draft.** How pierogi's controls (spec fields today, `wrap`
> flags later) map onto the OCI runtime spec, which *direction* each moves the
> sandbox boundary, and the discipline for growing the surface. Companion to
> [spec.md](spec.md) (the *shape* of the spec) and [vision.md](vision.md) (the
> *why*). This doc is the *why-this-shape* for everything the engine hands crun.

## The model in one screen

The engine is a **pure realizer**: it binds what the resolved spec says and
`exec`s (see [spec.md](spec.md#what-the-spec-is)). So the "control surface" is the
set of things a *producer* (the bridge, a Nix module, a hand-authored spec) can
put into a spec, and this doc is the map from each to its OCI realization.

Three load-bearing ideas govern the whole surface:

1. **Sealed base.** The default state is the most-closed reasonable sandbox —
   read-only root, private namespaces, dropped caps, default-deny seccomp, masked
   `/proc`, `nosuid,nodev` binds. Nothing in a bare spec loosens this.
2. **Directionality from the base.** Because the base is already closed, every
   *lever* points one of two ways: **NEUTRAL** (describe the workload — doesn't
   move the boundary) or **OPEN** (loosen — move the boundary outward from the
   base). There are essentially **no CLOSE levers**, because there is nothing left
   to close: the floor is already the floor. This is the exact inverse of
   docker/podman, whose base is *open* and whose flags mostly CLOSE (or, via
   `--privileged`, blow the base wide). See [Directionality](#directionality).
3. **Auditability is the invariant.** Every OPEN lands in the resolved spec. A
   user may open anything — bind `/`, join the host net namespace, drop seccomp —
   but only *deliberately* and *on the record*. What is refused is not the
   capability to loosen, but loosening that never appears in the artifact. See
   [Refused surface](#refused-surface).

## Directionality

Classify every control by how it moves the boundary relative to the sealed base:

| Direction | Meaning | Examples |
|---|---|---|
| **NEUTRAL** | Describes *what runs / who it is / what it sees by placement*; boundary unchanged | `env`, `cwd`, `user`, `hostname`, `exec`, `args`, `terminal` |
| **OPEN** | Loosens outward from the base; opt-in, explicit, recorded | a `Host` bind, a `--device`, `net: host`, `cap-add`, `readonly: false`, `seccomp: none` |
| **CLOSE** | Tightens below the base | *(empty — the base is the floor; see below)* |

The empty CLOSE column is the whole point. In docker the useful security work is
CLOSE (`--cap-drop`, `--read-only`, `--security-opt no-new-privileges`) because
its base is loose. Pierogi ships those *as the base*, so they are not levers —
they are the floor, and the only lever adjacent to a floor item is the OPEN that
turns it off. "Strict by default" restated mechanically: **the base is the most
closed state, so the surface offers only *describe* and *loosen*.**

## The four fates

Every OCI knob lands in exactly one fate. This is the operational form of
[the docker/podman flag triage](vision.md): steal the vocabulary, not the
leverage.

- **FLOOR** — part of the sealed base. Applied by default; not a lever. The only
  associated control is an explicit OPEN override that is recorded in the spec.
- **FIELD** — a spec field the bridge fills. Genuinely varies per sandbox.
  NEUTRAL (descriptive) or OPEN (exposure).
- **DESUGARED** — a CLI/bridge convenience that expands into explicit FIELD
  entries and FLOOR overrides *in the resolved spec*. Permitted precisely because
  the expansion is auditable (a pierogi `--privileged` that writes out its every
  loosening, unlike docker's runtime-layer omnibus).
- **REFUSED** — never representable: ambient or unauditable loosening.

## The producer layer — where defaults and shared policy live

The engine holds no policy; every default and every OPEN is written into the spec
by a **producer** before the engine sees it (spec.md's four-layer pipeline).
Producers form a hierarchy of increasing leverage, all legitimate:

- **Hand-authored spec** — the raw form. The author *is* the producer;
  `absent = absent`, foot-guns included.
- **`wrap`** — synthesizes a spec from a binary/store path: the sealed base plus
  minimal exposure. Zero-config safe defaults.
- **Nix modules / profiles** (M7) — the interesting one: **reusable, composable,
  shareable policy**.

The Nix library layer is where policy is *authored once and shared*. A profile is
a named bundle of the sealed base plus the narrow, vetted OPENs an app class needs
— `profiles.browser` = GPU device + Wayland proxy + D-Bus allowlist + a profile
dir, nothing more; `profiles.network` = loopback + `resolv.conf`; and so on.
Properties that matter:

- **Least-privilege by construction.** A profile encodes the *minimum*
  capabilities/exposure a class of workload needs, established and reviewed once,
  rather than each user over-provisioning by hand. Shared minimal policy beats
  hand-rolled policy — the "minimum capabilities" a wrapper library provides
  become a vetted default, not a guess.
- **Composable.** `base ∘ network ∘ gui ∘ browser` — profiles stack; the sandbox's
  exposure is the union of their OPENs. Composition widens *only* by OPEN; a
  profile cannot reach below the floor.
- **Shareable through the store.** Distributed as ordinary Nix libraries via
  substituters — consistent with "no registry; the Nix store is the distribution
  mechanism" ([vision.md](vision.md)). Someone vets `profiles.browser` once;
  everyone who composes it inherits the vetting.
- **Still fully auditable.** A profile is *sugar*: it evaluates to a resolved
  spec, so every default and OPEN it contributes is visible in the artifact. A
  shared policy loosens nothing you can't see — the auditability invariant holds
  through composition. This is the DESUGARED fate, lifted to the Nix layer and
  made shareable.

So "the bridge owns defaults" reads richer than "`wrap` sets some flags": the Nix
layer lets defaults themselves be **libraries** — authored, minimized, reviewed,
shared — while the engine stays a pure realizer and the resolved spec stays the
honest record. Implementation is M7 (deferred, last); the *expectation* is
captured here so the spec and engine layers leave room for it and don't
accidentally bake policy where a library should provide it.

## Mapping: pierogi control → OCI realization

### Descriptive fields (NEUTRAL, FIELD) — steal verbatim

The docker meaning is already correct; these carry no confinement semantics.

| Pierogi control | OCI field | Status |
|---|---|---|
| `exec.executable` / `args` | `process.args` | done (`oci.rs`) |
| `exec.terminal` | `process.terminal` + devpts mount | done |
| `env` | `process.env` | done |
| `hostname` | `hostname` | done |
| `exec.cwd` *(new)* | `process.cwd` | **gap** — hardcoded `/` today; trivial `Option<PathBuf>` |
| identity (uid/gid, groups) | `process.user.{uid,gid,additionalGids,umask}` | partial — uid/gid only (M4 synth) |

### The sealed base (FLOOR) — applied, not levered

The base's OCI realization. Each row is **on by default**; the "OPEN override"
column is the *only* control, and it is always recorded.

| Floor property | OCI field | Today | OPEN override |
|---|---|---|---|
| Read-only root | `root.readonly: true` | done (hardcoded) | `readonly: false` |
| Private namespaces | `linux.namespaces` (user/mount/pid/uts/ipc) | done | per-ns OPEN (below) |
| Masked `/proc`, `/sys` bits | `linux.maskedPaths` | **missing** | `unmask` list |
| Read-only `/proc` bits | `linux.readonlyPaths` | **missing** | — |
| No new privileges | `process.noNewPrivileges: true` | **missing** | rarely; explicit |
| Caps dropped | `process.capabilities` = ∅ | **missing** (M4) | `cap-add` list |
| Default-deny seccomp | `linux.seccomp` | **missing** (M4) | `seccomp: none` / profile |
| `nosuid,nodev` on binds | mount `options` | **missing** | per-mount opt-out |
| No core dumps | `RLIMIT_CORE = 0` | **missing** | explicit rlimit |
| Private mount propagation | `linux.rootfsPropagation: rprivate` | **missing** | — |
| cgroup namespace | `linux.namespaces` cgroup | **missing** | `cgroupns: host` |

### Exposure fields (OPEN, FIELD) — steal, narrow the grammar

Representable, off by default, explicit, recorded. The grammar admits the
foot-gun values (`host`, arbitrary ns path) — pierogi does not paternalize — but
they are never the default and always land in the spec.

| Pierogi control | OCI field | Notes |
|---|---|---|
| `Mount::Host { source, dest, rw }` | `mounts` (bind) | `source` may be `/` — the engine won't refuse it; the bridge just never defaults to it |
| `Mount::Tmpfs` | `mounts` (tmpfs) | done |
| `Mount::Overlay` | `mounts` (overlay) | deferred (`oci.rs` bails) |
| idmapped mount | `mounts[].uidMappings/gidMappings` | **new** — the clean answer to host-owned project/profile dirs under a subuid range (M4/M5) |
| device | `linux.devices` (+ bind in rootless) | **new** — narrow `/dev/dri/renderD128`, controllers (M3). *Not* a `--gpus all` omnibus |
| network | `linux.namespaces` network + `path` | **new** — default off; grammar: `none` \| loopback \| proxied ns \| `host` \| `ns:<path>` (M4) |
| join namespace | `linux.namespaces[].path` | **new** — any `/proc/<pid>/ns/*`, incl. host; explicit only |
| id mapping | `linux.{uid,gid}Mappings` | 1:1 today → subuid ranges (M4) |
| seccomp profile select | `linux.seccomp` | choose allowlist; `none` is an OPEN override of the floor |

### Resource controls (OPEN-ish, FIELD) — the conditional ones

Bounds are a loosening of the "unbounded" base only in the sense of being
*expressible*; a limit tightens actual usage. Two mechanisms, chosen by
availability:

| Pierogi control | OCI field | Note |
|---|---|---|
| rlimits (`NPROC`, `NOFILE`) | `process.rlimits` | **always available** (per-process, no cgroup). The rootless-friendly guard |
| pids / memory / cpu limits | `linux.resources.{pids,memory,cpu}` | **needs cgroup-v2 delegation** — conditional rootless |

**Rule: never silently drop a limit.** If a requested `memory`/`pids` limit can't
be enforced because the controller isn't delegated, fail loud (or `log`
prominently) — do not realize an unbounded sandbox that claims a bound. Consistent
with the repo's fail-loud principle.

### Refused surface (REFUSED)

Never representable, for one reason: they loosen without appearing in the
artifact, which breaks the auditability invariant.

| OCI feature | Why refused |
|---|---|
| `hooks` (all phases) | Arbitrary host-side code triggered by the config; the archetypal ambient escape hatch |
| `hooks.d`-style injection | Bypassed structurally — pierogi drives libcrun directly, no libpod layer scans hook dirs (see [architecture.md](architecture.md)) |
| a *non-desugaring* omnibus | A `--privileged` that flips confinement at the runtime layer without writing its expansion into the spec. (A **desugaring** one is fine — see DESUGARED) |

## Out of scope (decline, with reason)

`apparmorProfile` / `selinuxLabel` / `mountLabel` (distro-specific LSMs; seccomp
is the portable MAC — optional pass-through at most) · cgroup `resources.devices`
allowlist (moot rootless) · broad `sysctl` (only namespaced ones settable
rootless) · `intelRdt`, `blockIO`, `hugepageLimits`, `rdma`, `resources.network`,
`time` ns + `timeOffsets`, `domainname`, `consoleSize` (dynamic winsize instead —
see `src/tty.rs`), `scheduler`/`ioPriority`, `personality` (except `LINUX32` as an M6
Steam companion), `unified` cgroup-v2 passthrough, Windows/Solaris/vm/zos.

## Open decision — who applies the floor when the spec is silent?

The reconciliation sharpens one fork. Given default-safe + foot-shootable + pure
realizer, **when a spec omits a floor field, does the engine apply the safe
value, or realize the absence?**

- **Bridge-only (pure engine).** Engine realizes literally; `absent = absent`
  (spec.md's existing commitment). The bridge always writes the floor, so the
  resolved spec is the *complete* record. A raw hand-authored spec without seccomp
  gets none — visibly (the field is absent), power-user territory.
- **Engine-defaults (defense-in-depth).** Engine applies the floor when silent,
  spec can override. A forgotten field stays safe — but the engine now holds
  policy, and the resolved spec is no longer the whole story.

Today's `oci.rs` is *neither cleanly*: it hardcodes `root.readonly(true)` and the
namespace set (engine-defaults) but emits no masks/seccomp/caps at all. The
decision resolves that inconsistency.

**Recommendation:** bridge-writes-the-floor, engine stops hardcoding (even
`readonly`). It keeps the engine a true realizer and makes the resolved spec the
complete, honest audit surface — which is the invariant everything else here
leans on. The cost (a hand-authored spec that omits the floor is unhardened) is
acceptable *because it is visible*. **Status: OPEN — owner's call.**

## How to expand

Adding any new control — a flag, a spec field, a new OCI knob — runs the same
litmus, in order:

1. **Does it describe the workload** (what runs / who it is / a placement)? →
   **FIELD, NEUTRAL.** Steal the docker vocabulary verbatim.
2. **Is it a confinement property that should always hold?** → **FLOOR.** Add it
   to the base; not a lever. Its only control is an explicit, recorded OPEN
   override.
3. **Is it an exposure** (add a device/mount/namespace/cap)? → **FIELD, OPEN.**
   Off by default, explicit, recorded. Admit the foot-gun values in the grammar;
   do not default to them and do not forbid them.
4. **Is it a convenience bundling several of the above?** → **DESUGARED.** Allowed
   only if it expands into explicit FIELD/FLOOR-override entries in the resolved
   spec.
5. **Does it loosen without appearing in the artifact?** → **REFUSED.**

Two standing rules over the whole surface:

- **No ambient loosening.** If it widens the boundary, it is in the spec — no
  exceptions, no hook dirs, no runtime-layer omnibus.
- **No silent tightening failure.** If a requested bound can't be enforced, say
  so loudly; never realize a sandbox that claims a limit it doesn't have.
