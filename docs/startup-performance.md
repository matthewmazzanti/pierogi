# Startup performance

Pierogi's reason to exist is launching small, namespaced processes **very
fast**. This doc records where the startup time goes, what we measured, and
which levers actually move it — so we don't re-derive it.

Target: **< 10 ms** of sandbox overhead per launch (`pierogi wrap`/`run`) — the
cost on top of the workload itself.

## TL;DR

- Driving crun, from the read-only Nix store, with cgroups off, we land at
  **~11.5 ms** (min observed **9.5 ms**) — bubblewrap-class, an order of
  magnitude faster than Docker/podman. See [Where we sit](#where-we-sit).
- The biggest *real-world* cost isn't the runtime at all — it's the
  `nix-store --query --requisites` closure calculation (**~35 ms**), which
  nix-mode erases by precomputing the closure at build time.
- crun beats youki ~2.3× (≈11.5 ms vs ≈45 ms): youki pays a 20 ms intermediate-
  process sleep + socket-handshake choreography that crun's design avoids.

## How to measure

**Convention:** the **libcrun** and **youki** figures here are
hyperfine-measured. The cross-tool comparison in [Where we sit](#where-we-sit)
is order-of-magnitude **estimate** for every tool *except* our own measured
libcrun/youki rows.

`bench.sh` (needs `nix develop` for hyperfine + `$CRUN_DEV`):

- **Stub the closure query.** `PIEROGI_CLOSURE` (a `:`-separated list of store
  paths) replaces the `nix-store` query, so the timed loop measures the runtime
  backend, not the ~35 ms query. Bench/test only — see `src/closure.rs`.
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
| 1 | `nix-store` closure query | ~35 ms | **Open** — erased by nix-mode (precompute closure at build) |
| 2 | cgroup setup | ~12.5 ms | **Fixed** — `force_no_cgroup` (we request no limits) |
| 3 | memfd self-copy (CVE-2019-5736) | ~8 ms | **Fixed for free** — auto-skipped from a read-only FS |
| 4 | binary size | ~2–3 ms | Minor — only when the memfd copy fires (writable FS); irrelevant from the store |
| 5 | criu / systemd DSO load | ~0 ms | **No-op** — measured negligible; keep criu |
| 6 | mount syscalls (per closure path) | part of ~9 ms | **Next** — scales with closure size |

### 1. The closure query (~35 ms) — the real elephant

Every *live* launch (`pierogi wrap`, or `run` of an unresolved spec) shells out
to `nix-store --query --requisites` to compute the target's runtime closure (one
read-only bind mount per store path) — a pre-baked spec carrying `closure:` skips
it (see [execution-modes.md](execution-modes.md)). At
~35 ms it's **larger than the entire runtime**. It's a fixed cost that hits both
backends equally, which is why `bench.sh` stubs it out to compare runtimes.

The fix is architectural, not a micro-optimization: **nix-mode** precomputes the
closure at build time and ships it in the store path, so the query never runs at
launch. That's the single biggest end-to-end win available.

### 2. cgroups (~12.5 ms) — pure waste today

Our spec requests zero resource limits, yet crun still created and populated a
cgroup. Setting `force_no_cgroup` (and `no_new_keyring`) in the shim dropped
crun from ~33 ms → ~20 ms. Almost all of it was **idle time** — the cgroup
machinery blocking, not computing.

This is the right default *for the spike*. When M2 adds resource limits it
becomes conditional: skip the cgroup only when the spec declares no limits.

### 3. The memfd self-copy (~8 ms) — free from the store

To mitigate CVE-2019-5736, crun copies its own executable into a sealed `memfd`
and re-execs it, so a malicious container can't overwrite the host runtime via
`/proc/self/exe`. Because libcrun is linked into pierogi, "the runtime binary"
*is* pierogi — so it was copying our whole ~9.6 MB binary every run.

crun's `is_self_cloned()` skips this when `/proc/self/exe` is on a **read-only
filesystem** (an immutable binary can't be tampered with, so the protection is
unnecessary). The Nix store is read-only — so in production, where pierogi runs
from the store, the copy is skipped automatically. No flag, no patch, no
security downgrade. The benchmark only paid it because `./target/...` is on a
writable filesystem.

(Setting the `_LIBCONTAINER_CLONED_BINARY=1` sentinel does *not* work: crun also
requires the file to have no hardlinks, which an on-disk binary has. The
read-only-FS path is the real mechanism.)

### 5. criu / systemd DSO load (~0 ms) — measured, not assumed

`libcrun.so` eagerly loads `libcriu` + `libprotobuf-c` (and `libsystemd`) as
`DT_NEEDED` — they're in pierogi's startup DSO graph even though we never
checkpoint or use systemd. Hypothesis: dropping them speeds startup. **Measured:
it does not** — removing systemd entirely (and attempting to drop criu) moved
startup by ~0 ms (11.7 vs 11.5 ms). DSO mapping is microseconds and PLT binding
is lazy; the ~9 ms System time is mounts + clone + exec, not library loading.

Consequences:

- **Keep criu** — it costs nothing at startup and preserves the checkpoint/
  restore path (fast-start via restore; browser/Steam session snapshots).
- **Static-linking the crun stack won't help startup** — its only speed benefit
  would be eliminating DSO-load cost, which is already free. (It would also force
  dropping systemd/criu, which lack practical static archives, and reopen the
  link-chain work we collapsed to `-lcrun` via `--enable-shared`.) Not worth it
  for performance.

## Where we sit

Order-of-magnitude — hardware/config varies, but the tiers hold. **Every row is
an estimate except the two measured pierogi rows (libcrun, youki)**, which are
hyperfine numbers from our spike:

| Tool | startup | notes |
|------|---------|-------|
| raw `fork`+`exec` | ~1 ms (est.) | no sandbox |
| bubblewrap (Flatpak) | ~3–10 ms (est.) | light namespaces, no OCI/cgroups |
| **pierogi / libcrun, from store** | **~11.5 ms (measured)** | full OCI spec, RO nix-closure rootfs |
| **pierogi / youki, from store** | **~45 ms (measured)** | intermediate-process sleep + socket handshake |
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

- **Kill the closure query in nix-mode** (lever 1) — the biggest end-to-end win;
  architectural, already planned.
- **Mount-count** (lever 6) — today one `mount` syscall per closure path (~39
  for bash). The System time lives here, and it scales badly: a browser's 1000+
  paths is 1000+ syscalls. Collapsing the per-path binds is the next runtime
  lever and the one that matters for large closures.
- **CRIU restore** — for extreme per-request use, restore-from-snapshot is
  sub-millisecond, skipping setup entirely. The capability we deliberately kept.
