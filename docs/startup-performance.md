# Startup performance

Pierogi's reason to exist is launching small, namespaced processes **very
fast**. This doc records where the startup time goes, what we measured, and
which levers actually move it — so we don't re-derive it.

Target: **< 10 ms** of sandbox overhead per launch (`pierogi wrap`/`run`) — the
cost on top of the workload itself.

## TL;DR

- Driving crun, from the read-only Nix store, with cgroups off, we land at
  **~13 ms best / ~24 ms mean** for a sealed `true` (17-path closure) —
  bubblewrap-class, an order of magnitude faster than Docker/podman. See
  [Where we sit](#where-we-sit).
- The biggest *real-world* cost used to be the runtime's least interesting part —
  a live `nix-store --query --requisites` closure calculation (**~35 ms**). That
  query no longer exists: the closure is now computed once at build time (by
  traversing `exportReferencesGraph`) and baked into the spec, so nothing queries
  the store at launch.
- crun is the only backend. youki was evaluated and dropped: it couldn't
  RO-remount a `/nix/store` rootfs and paid a ~20 ms intermediate-process sleep,
  landing at ~45 ms vs crun's ~13 ms (see [Where we sit](#where-we-sit)).

## How to measure

**Convention:** the **crun** figures here are hyperfine-measured. The
cross-tool comparison in [Where we sit](#where-we-sit) is order-of-magnitude
**estimate** for every tool *except* our own measured crun row. (The youki
row is a historical measurement from when youki was still a candidate backend.)

`bench.sh` (needs `nix develop` for hyperfine + the pinned crun binary):

- **Run a pre-baked spec.** The closure is baked into the spec at build time, so
  the timed loop measures the runtime backend directly — there is no launch-time
  store query to stub out. (Historically `bench.sh` set a `PIEROGI_CLOSURE`
  override to skip a live `nix-store` query; that query and its override are gone.)
- **Run from the read-only store.** `nix-store --add target/release/pierogi`
  copies the binary into `/nix/store` (read-only); run *that* path, not
  `./target/...`. This matches production and avoids the memfd self-copy (below).
- **Read wall vs CPU.** hyperfine prints `Time (mean)` (wall — what you wait)
  plus `[User, System]` (CPU, summed across the process tree). `wall − (User +
  System)` is **idle/blocked** time — waiting, not computing. Big idle = the
  runtime is stalling on something (a sleep, IPC), which is the most reclaimable
  kind of overhead.

## The levers (in the order we found them)

Numbers are crun, closure-query stubbed, from the read-only store unless noted.

| # | Lever | Cost | Status |
|---|-------|------|--------|
| 1 | `nix-store` closure query | ~35 ms | **Removed** — closure is pre-baked at build time; no launch-time query |
| 2 | cgroup setup | ~12.5 ms | **Fixed** — we request no limits, so no cgroup is created |
| 3 | memfd self-copy (CVE-2019-5736) | ~8 ms | **Fixed for free** — auto-skipped from a read-only FS |
| 4 | binary size | ~2–3 ms | Minor — only when the memfd copy fires (writable FS); irrelevant from the store |
| 5 | criu / systemd DSO load | ~0 ms | **No-op** — measured negligible; keep criu |
| 6 | mount syscalls (per closure path) | part of ~9 ms | **Next** — scales with closure size |

### 1. The closure query (~35 ms) — the elephant we removed

Early on, computing a target's runtime closure meant shelling out to `nix-store
--query --requisites` (one read-only bind mount per store path). At ~35 ms that
live query was **larger than the entire runtime** — the biggest end-to-end cost,
and a fixed one that would have hit any backend equally.

That query is gone. The closure is now computed once at build time, by traversing
`exportReferencesGraph`, and baked into the spec (see
[execution-modes.md](execution-modes.md)). Launch reads the pre-baked `closure:`
directly, so nothing queries the store at runtime. This is the only path: `run`
of an unbuilt spec fails loud rather than falling back to a live query.

### 2. cgroups (~12.5 ms) — pure waste today

Our spec requests zero resource limits, yet crun still created and populated a
cgroup. Suppressing it dropped crun from ~33 ms → ~20 ms — almost all **idle
time**, the cgroup machinery blocking rather than computing. (In the earlier
in-process design this was libcrun's `force_no_cgroup` / `no_new_keyring` context
flags; under the crun binary it follows from the config requesting no resources
and crun's rootless cgroup handling. Worth re-isolating under the binary if the
number regresses.)

This is the right default *for the spike*. When M4 (hardening) adds resource
limits it becomes conditional: skip the cgroup only when the spec declares no
limits.

### 3. The memfd self-copy (~8 ms) — free from the store

To mitigate CVE-2019-5736, crun copies its own executable into a sealed `memfd`
and re-execs it, so a malicious container can't overwrite the host runtime via
`/proc/self/exe`. In the earlier in-process design libcrun was linked into
pierogi, so "the runtime binary" *was* pierogi and crun copied our whole ~9.6 MB
binary every run — which the benchmark paid because `./target/...` sits on a
writable filesystem. Now that crun is a separate binary, it is the thing copied.

crun's `is_self_cloned()` skips this when `/proc/self/exe` is on a **read-only
filesystem** (an immutable binary can't be tampered with, so the protection is
unnecessary). The crun binary is pinned to a `/nix/store` path (`CRUN_BIN`),
which is always read-only — so the self-copy is skipped automatically, in the
benchmark and in production alike. No flag, no patch, no security downgrade.

(Setting the `_LIBCONTAINER_CLONED_BINARY=1` sentinel does *not* work: crun also
requires the file to have no hardlinks, which an on-disk binary has. The
read-only-FS path is the real mechanism.)

### 5. criu / systemd DSO load (~0 ms) — measured, not assumed

The `crun` binary eagerly loads `libcriu` + `libprotobuf-c` (and `libsystemd`)
as `DT_NEEDED` — they're in crun's startup DSO graph (paid on every exec) even
though we never checkpoint or use systemd. Hypothesis: dropping them speeds
startup. **Measured: it does not** — removing systemd entirely (and attempting to
drop criu) moved startup by ~0 ms (11.7 vs 11.5 ms). DSO mapping is microseconds
and PLT binding is lazy; the ~9 ms System time is mounts + clone + exec, not
library loading.

Consequences:

- **Keep criu** — it costs nothing at startup and preserves the checkpoint/
  restore path (fast-start via restore; browser/Steam session snapshots).
- **A statically-linked crun won't help startup** — its only speed benefit
  would be eliminating DSO-load cost, which is already free. (It would also force
  dropping systemd/criu, which lack practical static archives.) We depend on
  stock `pkgs.crun` and there's no performance reason to build a custom one. Not
  worth it.

## Where we sit

Order-of-magnitude — hardware/config varies, but the tiers hold. **Every row is
an estimate except the measured pierogi / crun row**, a hyperfine number from
our spike. (The youki row is a *historical* measurement — youki was evaluated and
dropped, so it's no longer a live backend option; see below.)

| Tool | startup | notes |
|------|---------|-------|
| raw `fork`+`exec` | ~1 ms (est.) | no sandbox |
| bubblewrap (Flatpak) | ~3–10 ms (est.) | light namespaces, no OCI/cgroups |
| **pierogi / crun, from store** | **~13 ms (measured)** | full OCI spec, RO nix-closure rootfs |
| ~~pierogi / youki, from store~~ | ~~~45 ms~~ (historical) | dropped: couldn't RO-remount a store rootfs; intermediate-process sleep + socket handshake |
| `crun`/`runc` CLI | ~20–50 ms (est.) | + cgroups, on-disk config, memfd copy |
| podman | ~150–400 ms (est.) | + image/storage plumbing |
| docker | ~200–800 ms (est.) | + daemon, networking |
| gVisor (runsc) | ~150 ms+ (est.) | userspace kernel |
| Firecracker | ~125 ms+ (est.) | boots a kernel |
| Kata / full VM | ~1 s+ (est.) | full VM |

We're in the **bubblewrap tier** while doing *more* than bwrap (full OCI spec,
structured rootfs). For interactive launches this is imperceptible. It only
becomes a constraint at per-request scale (thousands/sec), where the answer is
CRIU restore, not shaving milliseconds.

## What's left

- **Closure query** (lever 1) — *done.* The biggest end-to-end win, already
  landed: the closure is pre-baked at build time and there is no launch-time query.
- **Mount-count** (lever 6) — today one `mount` syscall per closure path (~39
  for bash). The System time lives here, and it scales badly: a browser's 1000+
  paths is 1000+ syscalls. Collapsing the per-path binds is the next runtime
  lever and the one that matters for large closures.
- **CRIU restore** — for extreme per-request use, restore-from-snapshot is
  sub-millisecond, skipping setup entirely. The capability we deliberately kept.
