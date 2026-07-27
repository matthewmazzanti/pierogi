# Pierogi — The control surface

> **Status: converging draft.** How pierogi's controls (spec fields today, `wrap`
> flags later) map onto the OCI runtime spec, which *direction* each moves the
> sandbox boundary, and the discipline for growing the surface. Companion to
> [spec.md](spec.md) (the *shape* of the spec) and [vision.md](vision.md) (the
> *why*). This doc is the *why-this-shape* for everything the engine hands crun.

## The model in one screen

> **This doc maps the *producer* layer, not the engine.** The engine
> (pierogi-the-binary) is a **pure realizer** — a lightly-extended OCI runtime
> (runc's role) plus nix carve-outs (sealed rootfs, closure binds). It holds
> **zero policy**: it faithfully projects whatever the resolved spec says into the
> OCI spec and `exec`s, `absent = absent`. Every default, every floor value, every
> OPEN is written into the spec by a **producer** *before* the engine sees it
> (podman's role, here played by the Nix builder). So "the control surface" is
> what a producer can put in a spec, and this doc is the map from each control to
> its OCI realization — a spec for the **Nix profile layer**, not a description of
> engine behavior.

Three load-bearing ideas govern the whole surface:

1. **The sealed base is a *composed profile*, not an engine default.** The
   most-closed reasonable sandbox — read-only root, private namespaces, dropped
   caps, default-deny seccomp, masked `/proc`, `nosuid,nodev` binds — is
   `profiles.base` in the Nix layer, and it applies *because a producer composed
   it*, not because the engine imposes it. A genuinely bare spec (no profile) is
   **not** sealed — it's whatever the OCI runtime does with the fields present.
   Safety is one `import` away, vetted once, recorded in the artifact — never
   ambient in the engine. This is where curated defaults live, so masking /
   network / resources can each get bespoke defaults instead of one uniform floor.
2. **Directionality from the base.** Relative to `profiles.base`, every *lever*
   points one of two ways: **NEUTRAL** (describe the workload — doesn't move the
   boundary) or **OPEN** (loosen — move the boundary outward from the base). There
   are essentially **no CLOSE levers**, because the base is already the floor. This
   is the exact inverse of docker/podman, whose base is *open* and whose flags
   mostly CLOSE. (The exceptions — masking, network, resources — are catalogued in
   [How to expand](#how-to-expand); they refine, not break, this.) See
   [Directionality](#directionality).
3. **Auditability is the invariant.** Every default *and* every OPEN lands in the
   resolved spec — the floor is *in* the artifact (a producer wrote it), not applied
   invisibly by the engine. A user may open anything — bind `/`, join the host net
   namespace, drop seccomp — but only *deliberately* and *on the record*. What is
   refused is not the capability to loosen, but loosening that never appears in the
   artifact. See [Refused surface](#refused-surface).

## Directionality

Classify every control by how it moves the boundary relative to the sealed base:

| Direction | Meaning | Examples |
|---|---|---|
| **NEUTRAL** | Describes *what runs / who it is / what it sees by placement*; boundary unchanged | `environment`, `working_dir`, `user`, `hostname`, `executable`, `args`, `tty` |
| **OPEN** | Loosens outward from the base; opt-in, explicit, recorded | a `Host` bind, a `--device`, `net: host`, `cap-add`, `read_only: false`, `seccomp: none` |
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

### Descriptive fields (NEUTRAL, FIELD) — steal the compose vocabulary

The compose meaning is already correct; these carry no confinement semantics. The
spec is laid out **flat**, like a single compose service — no `exec {}` grouping.
Argv is the one deliberate divergence: it stays **execve** (one `executable` +
`args`), not compose's ENTRYPOINT/CMD split.

| Pierogi control | Compose | OCI field | Status |
|---|---|---|---|
| `executable` / `args` | ~`command` (execve, not entrypoint/command) | `process.args` | done (`oci.rs`) |
| `tty` | `tty` | `process.terminal` + devpts mount | done |
| `environment` | `environment` | `process.env` | done |
| `hostname` | `hostname` | `hostname` | done |
| `working_dir` | `working_dir` | `process.cwd` | done — `Option<PathBuf>`, absent → `/` |
| identity (`user`) | `user` | `process.user.{uid,gid,additionalGids,umask}` | partial — host uid/gid only (M4 synth) |

### The sealed base (FLOOR) — a composed profile, not an engine default

The OCI realization of `profiles.base`. Each row is **on when a producer composes
`base`** (not applied by the engine); the value lands in the resolved spec, and
the "OPEN override" column is how a producer relaxes it. The "Today" column is the
current *field/pass-through* status in `oci.rs` — the values themselves come from
the producer, `absent = absent`.

All engine support is **pass-through as of M1.5** — the values below come from
`profiles.base`, not the engine. "Curated in `base`" = the field exists and the
base sets the floor value; the *curated content* (the seccomp profile, subuid
ranges) is the M4 producer work.

| Floor property | OCI field | Today | OPEN override |
|---|---|---|---|
| Read-only root | `root.readonly` | pass-through; `base` sets `true` | `read_only: false` |
| Private namespaces | `linux.namespaces` | pass-through; `base` sets the set | omit a ns / join via `path` |
| Masked `/proc`, `/sys` bits | `linux.maskedPaths` | pass-through; `base` sets runc's list | `unmask` list (see note) |
| Read-only `/proc` bits | `linux.readonlyPaths` | pass-through; `base` sets the list | — (see note) |
| No new privileges | `process.noNewPrivileges` | pass-through; `base` sets `true` | rarely; explicit |
| Caps dropped | `process.capabilities` = ∅ | pass-through; `base` sets `[]` | `cap_add` list |
| Default-deny seccomp | `linux.seccomp` | pass-through; profile curated in **M4** | `seccomp: none` / profile (see note) |
| `nosuid,nodev` on mounts | mount `options` | producer-set (CLI/`base`); closure binds implicit | per-mount (producer) |
| No core dumps | `RLIMIT_CORE = 0` | pass-through; `base` sets it | explicit rlimit |
| Private mount propagation | `linux.rootfsPropagation` | pass-through; `base` sets `rprivate` | — |
| cgroup namespace | `linux.namespaces` cgroup | pass-through; not in `base` yet | `cgroupns: host` |

> **TODO — masking doesn't fit the FLOOR shape; treat it separately (shortly).**
> `maskedPaths`/`readonlyPaths` are the two rows above that are *not* clean floors.
> The other floor items have a bounded/binary bottom (RO is binary; caps floor at
> ∅; seccomp at deny-all), so "base = the maximum-closed state, only OPEN levers"
> holds. Masking's target space is **unbounded**, and "mask everything" is both
> inexpressible and useless — so any default is a *curated list* (a policy), not a
> mechanical maximum, and `unmask` inverts against an arbitrary choice.
>
> The resolution: masking is **not a floor** but the *narrowing parameter of an
> exposure*. The real floor for `/proc` is "not mounted"; we take an OPEN (mount
> it, apps need it), and the mask list is how far we narrow that opening back down.
> So it's a curated-default FIELD that is legitimately **bidirectional**: `unmask`
> widens (OPEN, away from floor), `mask` narrows *toward* the floor (allowed —
> re-closing an exposure, never below the floor; "mask all of `/proc`" converges on
> "don't mount it"). Grammar when we build it: a vetted baseline (runc's list —
> `/proc/kcore`, `/proc/keys`, `/sys/firmware`, `/sys/devices/virtual/powercap`, …)
> as the engine default, with `unmask`/`mask` **deltas** against it rather than a
> single overridable list (a full override silently drops `kcore` masking — a
> foot-gun). This refines, not breaks, the directionality thesis: the "base =
> mechanical maximum" claim holds only for bounded controls; unbounded list-valued
> exposures are "narrow-able," and add-from-∅ exposures (`mounts`, `devices`) are
> OPEN-only. Two shapes; fold this in when masking lands.

> **TODO — seccomp is masking's bounded cousin; treat carefully (M4).** The
> *mechanism* floors cleanly at deny-all — but "deny all" can't even `exit()`, so
> any usable default is a **curated allowlist** (the runc/Docker ~300-syscall
> profile). So, like masking, its default is a *policy, not a mechanical maximum*.
> The difference that makes it tractable: the syscall universe is **bounded**, so
> `seccomp: none`, profile-select, and allow-deltas are all expressible cleanly
> (masking's list has no such ceiling). Open calls when it lands: ship runc's
> profile verbatim vs. author our own (the same "track vs. freeze" sync question as
> masking), and how architecture-specific syscall tables are handled. Deferred to
> M4 with caps/identity; pinned here so the shape is on the record.

### Exposure fields (OPEN, FIELD) — steal, narrow the grammar

Representable, off by default, explicit, recorded. The grammar admits the
foot-gun values (`host`, arbitrary ns path) — pierogi does not paternalize — but
they are never the default and always land in the spec.

All are **pass-through fields** (M1.5) — engine support exists; what's noted as
M3/M4 is the *producer curation* (which device, which subuid range, the profile).

| Pierogi control | OCI field | Notes |
|---|---|---|
| bind mount | `mounts` (verbatim `oci_spec::Mount`, `type: bind`) | `source` may be `/` — the engine won't refuse it; a producer just never defaults to it |
| tmpfs | `mounts` (`type: tmpfs`) | done |
| overlay | `mounts` (`type: overlay`) | verbatim OCI mount; realization is crun's (rootless overlayfs/fuse) |
| idmapped mount | `mounts[].uidMappings/gidMappings` | free with verbatim mounts — the clean answer to host-owned dirs under a subuid range (M4/M5) |
| device | `linux.devices` (rootless binds the node) | pass-through; narrow `/dev/dri/renderD128`, controllers (curation M3). *Not* a `--gpus all` omnibus |
| network | `linux.namespaces` network + `path` | pass-through; grammar `none` \| join a `pasta`/`slirp` netns via `path` \| `host` (curation M4) |
| join namespace | `linux.namespaces[].path` | pass-through; any `/proc/<pid>/ns/*`, incl. host; explicit only |
| id mapping | `linux.{uid,gid}Mappings` | pass-through; `host: "self"` (invoker) today → subuid ranges (M4) |
| seccomp profile select | `linux.seccomp` | pass-through (verbatim OCI); `none` is an OPEN, the curated profile is M4 |

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
| `hooks.d`-style injection | Bypassed structurally — pierogi drives crun directly (no libpod/podman layer), so nothing scans hook dirs (see [architecture.md](architecture.md)) |
| a *non-desugaring* omnibus | A `--privileged` that flips confinement at the runtime layer without writing its expansion into the spec. (A **desugaring** one is fine — see DESUGARED) |

## Out of scope (decline, with reason)

`apparmorProfile` / `selinuxLabel` / `mountLabel` (distro-specific LSMs; seccomp
is the portable MAC — optional pass-through at most) · cgroup `resources.devices`
allowlist (moot rootless) · broad `sysctl` (only namespaced ones settable
rootless) · `intelRdt`, `blockIO`, `hugepageLimits`, `rdma`, `resources.network`,
`time` ns + `timeOffsets`, `domainname`, `consoleSize` (dynamic winsize instead —
see `src/tty.rs`), `scheduler`/`ioPriority`, `personality` (except `LINUX32` as an M6
Steam companion), `unified` cgroup-v2 passthrough, Windows/Solaris/vm/zos.

## DECIDED — the producer writes the floor; the engine holds no policy

**When a spec omits a floor field, the engine realizes the absence** — `absent =
absent`. It never applies a safe value of its own. The floor is written into the
spec by a producer (`profiles.base`) before the engine sees it, so the resolved
spec is the *complete, honest record* and the engine stays a true realizer. This
is the **bridge-writes-the-floor / runc model**, chosen over engine-defaults for
three reasons that the design surfaced:

- **The floor can't be uniform anyway.** Masking, network, and resources have no
  clean mechanical bottom (see [How to expand](#how-to-expand)), so an
  engine-applied floor could never be *correct* across them. Curated per-domain
  defaults only make sense in a profile.
- **A finishable engine.** A pure OCI+nix realizer has a bounded surface; a
  policy-embedding engine grows forever. Policy churn belongs in Nix, where
  iteration is cheap.
- **Auditability is stronger, not weaker.** The floor lands *in* the artifact
  (a producer wrote every value) rather than being applied invisibly.

The cost — a genuinely bare, profile-less spec is unhardened — is the runc
contract, and acceptable *because it is visible* and one `import profiles.base`
away. Consequence for `oci.rs`: it stops hardcoding anything (the namespace set,
the 1:1 mapping, `readonly`), and every OCI knob becomes a faithful pass-through
of a spec field, emitting nothing when the field is absent. **Status: DECIDED.**

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
