# Pierogi — Networking

> **Status: design, not implemented.** The engine's `linux.namespaces[].path`
> pass-through exists (M1.5); the `net` verb, the registry pin, and the attach
> sequence below are designed and *mechanism-verified* but unbuilt. Companion to
> [control-surface.md](control-surface.md) (the network rung),
> [architecture.md](architecture.md) (the engine/producer split), and
> [vision.md](vision.md) (the no-daemon, no-fabric non-goals).

Every kernel claim in this document was verified experimentally on Linux 6.18.44
driving crun 1.27.1, not derived from documentation. Several results contradicted
the obvious expectation; those are called out where they matter. Claims that were
*not* verified say so explicitly.

## The shape in one paragraph

A network is built **ahead of time** by a privileged, short-lived verb, then
attached to rootlessly at launch — the same pre-bake discipline the closure gets
(invariant 2). `sudo pierogi net <net-spec>` performs the rootful work (veth
pairs, bridges, addressing, firewall rules), leaves behind a **registry pin** —
one unprivileged process holding a user namespace and a mount namespace — and
exits. Each network is a netns bind-mounted at a name inside that mount
namespace. `pierogi run --net <name>` joins the registry's user namespace — the
only namespace the engine enters itself — and hands crun a spec naming the netns
by path, which crun joins before unsharing a **fresh** user namespace for the
sandbox. The result: the sandbox can *use* the network and cannot *shape* it, and
that is enforced by the kernel's capability model rather than by policy pierogi
has to maintain.

## Why this is not a ported network manager

A declarative network manager does not belong inside the engine, for three
reasons that are already commitments elsewhere in the tree:

- **It imports two explicit non-goals.** [vision.md](vision.md) declines a
  "daemon / management layer" (lifecycle is `systemd`'s job) and an
  "orchestration / networking fabric".
- **The lifetimes do not fit.** One engine process exists per launched sandbox
  and dies with it ([architecture.md](architecture.md)). A network that several
  sandboxes share must outlive all of them, so it cannot live inside the thing
  that exits first. This is the structural objection, not a stylistic one.
- **Pierogi already has a declarative layer, and it is not Rust.** The engine
  holds no policy (invariant 1); `nix/` is the policy engine. The declarative
  half of a network manager ports into Nix, not into `src/`.

What that leaves is a clean split:

| Concern | Home |
|---|---|
| declarative network model | `nix/` — the **net-spec** (the `net` verb's input), authored in Nix like `profiles.*`. Distinct from the sandbox spec, which records only the attachment |
| netns/userns realization, firewall, flow control | its own binary, pinned `NETNS_MGR_BIN` beside `CRUN_BIN` (`build.rs` already loops over those vars) |
| joining the namespaces at launch | the engine — mechanism, not policy |
| the schema both sides agree on | a shared Rust crate, so `deny_unknown_fields` catches drift instead of the two definitions diverging silently |

The `xdg-dbus-proxy` supervisor (`src/dbus.rs`) is the precedent for the third
row: the engine spawns and supervises a host-side helper whose *rules* travel in
the resolved spec.

## The kernel rules everything derives from

### Two different relationships, commonly conflated

**Hierarchy.** Only **user** and **PID** namespaces are hierarchical. Every user
namespace except `init_user_ns` has a parent — the userns of whoever created it.

**Ownership.** Every *non-user* namespace (net, mnt, uts, ipc, cgroup) carries a
pointer to an owning user namespace, `ns->user_ns`, fixed at creation and
**immutable**. "A netns belongs to a userns" is an ownership edge, not a parent
edge, and nothing relaunders it — including a bind mount.

### The capability walk

`cap_capable()` walks *up* from the target namespace toward `init_user_ns`,
asking at each level whether the caller holds the capability. Capabilities
therefore flow **downward only**: holding one in an ancestor grants it over every
descendant; being in a descendant grants nothing over an ancestor. One special
case makes rootless operation possible at all:

```c
/* The owner of the user namespace in the parent of the
 * user namespace has all caps. */
if ((ns->parent == cred->user_ns) && uid_eq(ns->owner, cred->euid))
```

This is why an unprivileged user can enter a userns they created — and why a
**root-created** one is a dead end for them, since `owner` is 0.

### `setns()` has two different contracts

For a **non-user** namespace it requires `CAP_SYS_ADMIN` in *both* the target's
owning userns *and* the caller's current userns. The second condition is the
non-obvious one: owning the target's userns is not enough if the caller sits in
`init_user_ns` as an unprivileged user with an empty effective set.

For a **user** namespace, entering *grants the full capability set in it*. That
is the bootstrap — the netns join is illegal until the userns join supplies the
capabilities that make it legal. Hence the order is forced, not stylistic:

```
init_user_ns, uid 1000, no caps
  └─ setns(userns U)         → cred->user_ns = U, full caps in U
       └─ setns(netns N)     → legal: caps in N->user_ns (= U) and in U
            └─ unshare(CLONE_NEWUSER) → child C; netns retained
```

`setns()` changes exactly one namespace per call; joining a netns does **not**
implicitly change the user namespace. `nsenter` appears to change both only
because the tool issues one `setns()` per flag.

### `unshare` does not evict

Creating a fresh user namespace leaves every *other* namespace held by the
process untouched. This is what makes private per-sandbox identity possible: the
sandbox keeps the network and loses authority over it in the same move, because C
is a descendant of U and the capability walk from `N->user_ns == U` finds nothing.

### `execve` drops capabilities

Unless `euid == 0` or the file carries capabilities, `execve` discards them. A
process may join a userns, gain full capabilities, and lose them at the very next
`exec` if its uid is not mapped to 0 there. This is why the registry's map must
include the invoking uid as `0 → <uid>`, and why podman's rootless map opens with
exactly that entry.

### `uid_map`

Write-once per userns. The writer needs `CAP_SETUID` in the *parent* userns to
map arbitrary ranges (otherwise only its own euid). Every id mapped must already
be mapped in the parent. Maximum **340 extents** (`UID_GID_MAP_MAX_EXTENTS`) —
340 succeeds, 341 fails `EINVAL`. Contiguous ranges cost one extent, so
millions of unique ids are cheap; only *scattered* ranges hit the cap.

### Lifetime

A namespace lives while anything references it: a member process, an open fd, a
bind mount, or — for a userns — any namespace it owns, since `net->user_ns` holds
a reference. **A bind-mounted netns outlives the process that created it**: with
the creating process fully reaped and zero processes in the namespace, attaching
through the bind mount still lands in the same netns with its interfaces intact.
(That the owned-namespace reference keeps a userns alive follows from the kernel's
reference model and is *not* separately verified here.)

## The two verbs

### `sudo pierogi net <net-spec>` — privileged, short-lived

Symmetric with `resolve`: a build verb that emits a durable, auditable artifact.
It boots, realizes, and exits; no daemon, no supervision, no ambient privilege.

1. **Fork a child, `setresuid()` to the invoking user, *then* `unshare`** the
   user and mount namespaces. This step is load-bearing: the userns `owner` must
   be the target uid. A registry created while still `euid 0` is owned by root,
   the capability walk terminates at `init_user_ns` against an empty effective
   set, and every rootless attach fails `EPERM` — permanently, since ownership
   cannot be changed after creation.
2. **The still-root parent `setns`es into each netns** and performs the rootful
   work — veth pairs, moving peers across, bridges, addresses, firewall rules.
   Root may do this: it holds `CAP_SYS_ADMIN` in `init_user_ns`, an ancestor.
   Each netns must be *created inside the registry's userns*, never in
   `init_user_ns`.
3. **Root writes the registry's `uid_map`/`gid_map`** with a generous contiguous
   range. Root is not bound by `/etc/subuid`, which is the central advantage of
   the privileged-setup model over a fully-rootless one.
4. **Bind-mount each netns at a name** inside the registry's mount namespace.
5. **Self-check before exiting**: drop to the invoking user and attempt the full
   `setns(user)` → `setns(net)` on every network just built, failing the whole
   invocation if any cannot be joined. A successful *mount* is not evidence of an
   attachable network (see Hazards), and invariant 5 says fail at build time
   rather than shipping a broken artifact.
6. **The child survives as the registry pin** — already unprivileged, so a normal
   user process: inspectable, killable, `systemd --user`-supervisable. Root exits.

The pin is not a daemon. It runs no event loop, holds no policy, and supervises
nothing; it exists because a rootless user namespace has no other way to stay
alive. Rootless podman reaches the same conclusion and calls it the pause process.

### `pierogi run --net <name>` — rootless attach

A netns path is a genuinely-runtime value, so it enters through the CLI override
channel beside `--env`/`--mount`/`--hostname`, never baked into a spec. See the
`todo.md` note on runtime-only values and the `self` sentinel — this is the same
seam and the same open question.

## The attach sequence

The engine joins **one** namespace itself — the registry's user namespace — and
lets crun do the rest:

```
1. open the netns fd: /proc/<pin>/root/<name>   (or /proc/<pin>/ns/net)
2. open the registry userns fd: /proc/<pin>/ns/user
3. setns(user)  → full capabilities in U        (a pre_exec in src/crun.rs)
4. exec crun with
     linux.namespaces = [ {type: user},                             ← fresh, NO path
                          {type: network, path: <the netns name>},
                          {type: mount}, {type: pid}, … ]
5. crun joins the netns while it still has authority in U,
   then unshares C — a child of U — and writes C's map
```

**Only the userns join is out-of-band**, because it is the engine's *exec
context*, not a namespace the sandbox joins. Everything the sandbox actually
receives — its fresh user namespace and the network by path — travels in
`linux.namespaces`, the M1.5 verbatim pass-through. So the attachment is recorded
in the resolved spec by the ordinary schema, and invariant 4 is satisfied without
a bespoke section to carry it.

**The precondition is "be inside U", not an OCI expressiveness limit.** Handing
crun `{type: user}` plus `{type: network, path: N}` from `init_user_ns` fails
(`setns: Operation not permitted`) — the caller has no capabilities in its own
user namespace. The identical spec run from inside U succeeds. Verified with the
netns named both ways: as `/proc/<pin>/ns/net`, and as a mount-pinned name
`/proc/<pin>/root/<name>` — in both cases the sandbox landed in a fresh userns
(`≠ U`) on the pinned netns (same inode, its interfaces present) with
`CAP_NET_ADMIN` inert. This is also how rootless podman does it: podman re-execs
into its own user namespace first, so crun inherits the authority it needs.

**A `path` on the user entry must be refused.** `{type: user, path: U}` alongside
a network attach puts the sandbox *in* the owning user namespace rather than
below it — the one configuration that makes `CAP_NET_ADMIN` live again (see
[What the sandbox can and cannot do](#what-the-sandbox-can-and-cannot-do)). It is
the OCI spelling of podman's `--userns=host`. The engine must reject the
combination outright rather than realize it — fail loud, per invariant 5. This is
the only way the guarantee is lost, so it is the only guard that has to exist.

**Ordering is still load-bearing for the fds.** Open them *before* `setns(user)`.
Once inside U, `/proc/<pin>/...` is no longer openable from a sibling user
namespace — `EACCES` on `open`, before any `setns` is attempted.

**Pierogi never joins the registry's mount namespace.** `/proc/<pin>/root/<name>`
reaches the pin's mount view from outside, so the sandbox's mount environment
stays the host's.

## Identity

The sandbox's userns C is necessarily a **descendant** of the registry's U, and
its map is bounded by U's. This is kernel-forced: joining the netns requires
entering U, and there is no way back out. A sibling userns with a larger range
cannot join the network at all.

Within that bound, subdivision is entirely free and may differ between runs —
verified with a single-uid map, a full 65535-uid map, and an arbitrary interior
slice, all against the same registry. A slice outside the range is refused by the
kernel (`write to gid_map: Operation not permitted`), so "stay inside your
network's range" needs no engine policy.

**Mappings compose, and `hostID` is a misnomer at depth.** C's map is written in
*U's* coordinates, not the host's:

```
C uid 1000  →  U uid 50000  →  host uid 149999
                               (U maps 1 → 100000, so 100000 + 50000 - 1)
```

Verified by creating a file from inside the sandbox and stating it on the host.
The same spec text therefore denotes different host identities depending on
whether `--net` was passed, which cuts against invariant 4. See Hazards.

**Nothing enforces disjointness between sandboxes.** Two slices that overlap land
on identical host uids and can read and write each other's files. Podman prevents
this with a runtime allocator; pierogi's answer is static allocation from a single
evaluation-time source in the Nix layer — which removes the allocator, the
database, and the runtime state, at the cost of making collisions a producer
responsibility.

**The range is the one irreversible decision.** `uid_map` is write-once, so a
registry whose range is too small can only be rebuilt, taking every attached
sandbox with it. Because the id-range authority and the network authority are
necessarily the same userns, the reservation must be sized for *everything* a
sandbox might want ids for, not just for networking. Podman's documented rootful
example reserves `containers:2147483647:2147483648` — half the uid space — which
is the right order of magnitude.

## What the sandbox can and cannot do

Verified with the sandbox running as uid 0 with `CAP_NET_ADMIN`, `CAP_NET_RAW`,
`CAP_NET_BIND_SERVICE` and `CAP_SYS_ADMIN` all effective (`CapEff=0x203400`):

```
rename interface   → Operation not permitted
set interface down → Operation not permitted
change MAC         → Operation not permitted
add address        → Operation not permitted
add route          → Operation not permitted
```

Container-root with `CAP_NET_ADMIN` cannot touch the network. The capability is
evaluated against `N->user_ns` (= U) and C is a descendant, so no cap set, uid, or
flag can grant it back. Interface-identity attacks are *unreachable*, not merely
disallowed — which means the property needs no per-sandbox vigilance to maintain.

The structure that produces it, with the two edges under U kept distinct:

```
init_user_ns
     │ child
     ▼
     U ───── owns ─────▶  N        (net->user_ns = U; set at creation, immutable)
     │ child
     ▼
     C   ← the sandbox lives here, and is a *member* of N
```

`N` is not a user namespace, so it is not `C`'s sibling; `U` relates to the two by
different edges — it **owns** `N` and **parents** `C`. (Reserve "sibling userns"
for two user namespaces sharing a parent — the arrangement that cannot even `open`
a foreign netns, let alone join it.) `cap_capable` starts at `N->user_ns` and walks
*upward*, so it never descends into `C`.

The one-line form: **membership is not authority.** The sandbox is a member of
`N` — same netns inode, full use of the network — while holding nothing over it.
Asked from inside, it cannot even *name* `N`'s owner (`NS_GET_USERNS` →
`Operation not permitted`): the owning namespace is outside its scope entirely,
so there is nothing there to address.

The same test run with the sandbox placed *at* U instead of below it — the only
other position available — inverts the result:

| sandbox's user namespace | `ip link add` in N |
|---|---|
| a fresh child of U (`≠ U`) | `Operation not permitted` |
| **U itself** | **SUCCEEDED** |

**Structurally unavailable, with no knob:**

```
AF_PACKET raw socket → Operation not permitted
SOCK_RAW    socket   → Operation not permitted
```

`tcpdump`, DHCP clients, raw-socket VPNs and raw-mode scanners cannot run in an
attached sandbox. Diagnostics must run from U, via a `pierogi net`-side tool.

**Available, but only if the network grants it.** Per-netns sysctls are the one
dial, set by `pierogi net` in U and unforgeable from inside:

| Grant | Sysctl | Default in a fresh netns |
|---|---|---|
| bind ports < 1024 | `net.ipv4.ip_unprivileged_port_start = 0` | 1024 (blocked) |
| ICMP datagram sockets (`ping`, `mtr`) | `net.ipv4.ping_group_range` | `65534 65534` (blocked) |

Both verified in both directions: refused before the grant, permitted after, with
reconfiguration still refused throughout. So "no raw sockets" does not imply "no
ping" — datagram ICMP is recoverable, raw is not.

**Isolation and raw-socket capability are the same lever, for a given netns.**
Both resolve through the same `ns_capable(net->user_ns, …)` check, so nothing
grants one without the other: a sandbox either descends from the netns's owner (no
reconfiguration, no raw sockets) or sits *in* the owner (raw sockets and full
`CAP_NET_ADMIN`, including every identity attack above). Neither has a relaxation
sysctl — the only raw-named per-netns knob is `raw_l3mdev_accept`, which governs
how an *already-created* raw socket binds against VRF devices, not whether one may
be created. That absence is coherent rather than an oversight: a raw socket is
direct access to the wire, which is exactly the authority the network retains.

TUN/TAP goes with it — creating a tun interface needs `CAP_NET_ADMIN` over the
netns, so userspace VPNs terminating on a tun device cannot run in a joined
sandbox. (Follows from the same rule rather than separately verified;
`profiles.base` bakes no `/dev/net/tun` in any case.)

### Delegated netns — trading the guarantee for capability

A sandbox that genuinely needs admin or raw sockets can be given them — not by
relaxing a capability, which is impossible, but by **inverting the ownership**.
Give the sandbox a netns owned by its *own* userns C, and wire it to the fabric
with a veth moved in from U. That move is legal because U is an ancestor of C and
so holds `CAP_NET_ADMIN` over C's namespaces; capabilities flowing downward is what
makes the delegation possible in one direction only. Verified: with `N_c` owned by
C, the sandbox gets `ip addr add`, `ip link up`, `AF_PACKET` and `SOCK_RAW` inside
its own netns, while U's router netns stays unreachable.

| shape | netns owned by | sandbox gets | wiring |
|---|---|---|---|
| **joined** | U | use only; admin/raw inert | pre-baked — sandbox joins by path |
| **delegated** | C | full admin/raw in its own netns | a U-side helper moves a veth in **at launch** |

What survives: the router netns, the flow matrix and `rp_filter` all stay beyond
reach, so a sandbox that rewrites its own address is still dropped at the router.
And the sandbox cannot opt itself in — it may mint netns in C freely, but attaching
one to the fabric means putting the peer end in the router netns, which it has no
authority to do. The network keeps the decision, which is what makes this a dial
rather than a hole.

What it costs:

- **Launch-time wiring.** C does not exist until the sandbox starts, so the veth
  cannot be pre-baked; something in U must wire it afterwards. That forfeits the
  "netns pre-exists, the sandbox just joins, no negotiation" property and
  reintroduces a live component at launch — the shape pasta/slirp4netns/CNI have.
- **A readiness sync**, so the sandbox does not run before its veth lands — the
  `src/dbus.rs` ready-fd pattern again.
- **The guarantee changes kind**, from "the capability is meaningless" to "the
  router is correct". Defensible — it is turnip's threat model — but it is
  per-network vigilance rather than a structural property.

Whether the net-spec carries this as a per-attachment mode is deferred. The joined
shape is the default, and the one the build plan below implements.

## Hazards

Each of these cost a test to establish; several contradicted the obvious
expectation.

1. **A root-created netns mounts fine and is permanently unattachable.** If root
   creates a netns as itself and binds it into the registry, the mount succeeds —
   root can open anything — and the network looks well-formed while every rootless
   attach fails `EPERM` forever. Ownership cannot be changed after creation. This
   is what the `net` self-check exists to catch.
2. **A pid is not a stable handle.** The attach path embeds the pin's pid. If the
   pin dies and the pid is recycled, `/proc/<pid>/root/<name>` resolves into an
   unrelated process's mount view and could plausibly succeed against the wrong
   namespace. Record each netns inode in the artifact and verify it after `setns`;
   reach the pin by pidfd where the lifetime spans a launch.
3. **`hostID` changes coordinate system under attachment.** See Identity. `run`
   should compose the chain and stamp *real host ids* into the resolved spec so
   the artifact means one thing regardless of how it was launched. The same
   hazard recurs in id-valued sysctls: `ping_group_range` rejects values outside
   U's id space (`0 2147483647` → `EINVAL`; `0 65535` succeeds), so net-spec
   values must be written in registry coordinates too. One shared answer is
   better than two local fixes.
4. **`self` resolves differently under attachment.** With a registry mapping
   `0 → 1000`, `getuid()` returns **0** after the join, not 1000. `profiles.base`
   resolves `host = "self"` via `getuid()` in the host process, which would stamp
   an id not mapped in the registry at all. `self` must be resolved after the
   join, or translated through the registry's map. This is the `todo.md` `self`
   wart with a concrete forcing case.
5. **`net.*` sysctls in the *sandbox* spec do not work.** crun lacks authority
   over U, so `spec.linux.sysctl` cannot set them; they belong in the net-spec.
   It fails loud rather than silently.
6. **Port space and loopback are shared by everything on a network.** Two
   sandboxes collide on a port (`EADDRINUSE`) and either can reach the other on
   `127.0.0.1`. This is the real remaining isolation cost of sharing a network —
   not the user namespace, which is fully separated. Sandboxes that should share
   egress but not each other need separate netns.
7. **Baked `/etc/resolv.conf` versus network-assigned DNS.** The synthetic-`/etc`
   principle bakes resolv.conf at build time, but the network is created later and
   independently. If the net-spec's DNS address moves, every image baked against
   the old one breaks silently. Either the address is a pinned constant images may
   bake against, or resolv.conf becomes a runtime-injected value through the same
   override channel as the other genuinely-runtime values.
8. **No rootless mount pin into the host.** A rootless process cannot publish a
   netns pin into the host's mount namespace: propagation into an unprivileged
   mount namespace is inbound-only (`shared:N` on the host becomes `master:N`
   inside), so the bind succeeds locally and the host sees nothing. `ip
   netns`-style path pinning is a root-only privilege, not a trick being missed.
   This is *why* the registry pin holds a mount namespace rather than publishing
   names to `/run`.

## How podman solves the same problems

Useful as a cross-check, since podman is the mainstream implementation of this
exact mechanism. Observed on podman 5.8.4.

- **It has the same pin.** A pause process holds the rootless user namespace,
  recorded at `$XDG_RUNTIME_DIR/libpod/tmp/pause.pid`.
- **Its map is the same two-entry shape** derived above: `0 → 1000` (own uid,
  supplying a mapped identity and capabilities) plus `1 → 100000` size 65536.
- **Its rootless netns is pinned by bind mount inside a mount namespace**, under
  `$XDG_RUNTIME_DIR/containers/networks/rootless-netns/` — the same construction
  as the registry, for the same reason (hazard 8).
- **One user namespace for the whole installation.** `--userns=host` is the
  default, and it means the *caller's* namespace — which for rootless podman is
  the pause process's. Three separate containers and the pause process all report
  the same inode. Containers are not isolated from each other by user namespace
  at all unless asked.
- **It is capped by `/etc/subuid`** because it is fully rootless — the ceiling
  `sudo pierogi net` escapes.

### Grant-based versus topology-based safety

The difference that matters is not that podman is unsafe by default. Its default
capability set omits `CAP_NET_ADMIN` entirely:

```
default CapEff:             0x800405fb
--cap-add=NET_ADMIN CapEff: 0x800415fb    ← delta 0x1000 = CAP_NET_ADMIN
```

So a stock container cannot reconfigure a network. The difference is **what the
safety rests on**:

- **Podman is grant-based.** Safe because the capability is withheld. Grant it —
  one flag, or a quadlet unit carrying it — and it is *real*, because the
  container sits in the owning user namespace. Confirmed against a real
  turnip-style pin: a container attached with `--network ns:<pin> --cap-add=NET_ADMIN`
  created an interface inside the network.
- **Pierogi is topology-based.** The capability may be granted freely and stays
  inert, because the sandbox is a descendant. There is no flag whose absence is
  load-bearing.

**Podman can reach the same topology**, and it is worth being precise about this
rather than claiming the shape is unique. `--userns=auto --network ns:<pin>`
produces a child user namespace with an allocated slice (`0 1 1024`), joined to
the pinned netns, with `CAP_NET_ADMIN` inert — pierogi's arrangement exactly. It
is simply opt-in per container, drawn from the `/etc/subuid` range, and needs a
stateful allocator: its documented failure mode reveals the bookkeeping — it
"does not work as long as any containers exist that were started with
`--userns=nomap` or `--userns=keep-id` without limiting the user namespace size",
because those consume the entire range. (That the tracking lives in podman's
database is inference from that constraint, not verified here.)

### Why the hardened mode is cheap here and expensive there

`--userns=auto` failed outright on a normal image:

```
Error: creating an ID-mapped copy of layer …: lchown etc/shadow: operation not permitted
```

Podman stores image layers **pre-translated** into one specific mapping — a
layer's root directory on disk is owned by host uid 1000, the host coordinate of
container-root under the default `0 → 1000`. Change the mapping and every stored
ownership is wrong, so the layer must be copied and `lchown`ed into the new range
(`storage-chown-by-maps`): a cost per *(layer × distinct mapping)*. That is what
makes per-container user namespaces expensive enough to be opt-in.

Pierogi has nothing to rewrite, for three independent reasons:

1. **Ownership carries no access semantics in the store.** Store paths are
   `root:root` mode `0555`; read and execute come from the *other* bits, so a
   sandbox never needs to own what it runs and the mapping is irrelevant to access.
2. **The root is read-only** (`profiles.base`), so there is no writable upper
   layer whose ownership must match the container's notion of root.
3. **Nothing is materialized per-sandbox.** One shared read-only bind serves every
   sandbox regardless of its slice, so no cost scales with the number of mappings.

Host binds are the one place ownership does matter — the `hostID` coordinate
hazard — and the general answer there is idmapped mounts
([control-surface.md](control-surface.md)), which remap at the mount layer with no
copy at all. (The mechanism above explains why *a* chown is required; the specific
`lchown etc/shadow` refusal was not isolated.)

The design here is podman's model with the `/etc/subuid` ceiling removed, the
allocator replaced by static Nix-time assignment, and the descendant user
namespace as the only shape the engine can emit rather than an opt-in flag.

## Build plan

> **Decisions settled:** one binary (`net` is a verb beside `run`/`resolve`); the
> lowering lives in Rust; Nix authors the declarative input; netlink is driven
> synchronously; `nft` is forked as a Nix-pinned binary.

### Shape: two verbs, mirroring `resolve` / `run`

The sandbox spec has an authored input form (`closure.seeds`) and a stamped
resolved form (`closure.resolved`), split across two verbs — `resolve` computes,
`run` asserts. Networking takes the same shape:

| | sandbox | network |
|---|---|---|
| authored input | spec carrying `closure.seeds` | net-config, authored in Nix |
| lowering verb | `pierogi resolve --graph` | `pierogi net plan` |
| stamped artifact | spec carrying `closure.resolved` | the resolved net-plan |
| realizing verb | `pierogi run` | `sudo pierogi net up` |

`net plan` is pure and **unprivileged**: config → fully-resolved plan (veth names,
`/32` assignments, sysctl sets, the nft ruleset), with everything that can *fail
to resolve* decided there. `net up` is then total over a valid plan, and is the
only part that needs root. So validation still happens before privilege is
acquired — that property comes from the verb split, not from which language holds
the lowering. It also mirrors turnip's existing `plan`/`up` split.

### Why a lowering in the binary is not policy

Invariant 1 says the engine holds no policy, and a lowering can look like a
violation. It is not: the **policy** is who may talk to whom, authored in Nix; the
**derivation** is therefore-these-veth-names, therefore-these-`/32`s,
therefore-this-ruleset. `pierogi resolve` is the standing precedent — it traverses
a supplied graph and stamps a derived closure while holding no policy of its own.
The test to keep applying: if a value could reasonably differ between two sites
with the same intent, it is policy and belongs in Nix.

### What Nix owns

The declarative input only — the container roster, networks, flow matrix,
uplinks, and links, authored as `profiles.network` the way sandbox specs are
authored from `profiles.base`. turnip's `nix/lib/turnip.nix` already prototypes
this shape (`turnipWithConfig`: a Nix attrset → `toJSON`).

### Phases

**N0 — attach spike.** No `net` verb; a hand-made registry pin. *Forces:* fd open
+ inode verification, `setns(CLONE_NEWUSER)` in a `Command::pre_exec`, the
`{type: user}` + `{type: network, path}` passthrough, and the guard rejecting
`{type: user, path}` alongside a network attach.
*Acceptance:* a sandbox lands on the pinned netns with `CAP_NET_ADMIN` inert,
while the tty relay and the D-Bus proxies still work.

The `pre_exec` placement is forced, not stylistic: `src/tty.rs` spawns the relay
thread, so the parent is multithreaded by launch time and `setns(CLONE_NEWUSER)`
requires a single-threaded caller. The post-fork child is the only legal window —
and the parent must stay in the host namespaces anyway to keep supervising the
relay and the proxies. This is the wall that forces turnip's `podman unshare`
re-exec in Go; Rust does not need an exec boundary.

**N1 — the `net` verb, empty networks.** *Forces:* fork → `setresuid` to the
invoker → `unshare(user+mount)`; the still-root parent writes the maps with the
reserved slab, creates and bind-pins named netns, runs the attachability
self-check, and emits the artifact. Attach-or-mint semantics from the start,
since sandboxes attach from here on.
*Acceptance:* two sandboxes on two named networks, surviving a re-run of `net up`.

**N2 — the lowering.** *Forces:* the net-config schema (`deny_unknown_fields`),
`pierogi net plan` as a pure unprivileged verb, golden tests over the resolved
plan. Also lands the two identity fixes: composed-mapping stamping and `self`
resolution under attach.
*Acceptance:* a plan is reproducible from a config with no root and no live state.

**N3 — links.** *Forces:* sync netlink — veth pair, move-an-end-into-netns,
address, route, loopback — plus the lowering half (veth naming with IFNAMSIZ
resolution, `/32` assignment, the router topology).
*Acceptance:* two sandboxes reach each other through the router netns.

**N4 — policy.** *Forces:* the nft ruleset builders and the sysctl sets in the
lowering; pushing them in `up` (a `setns` episode for sysctls, a pipe to the
pinned `nft -j -f -`).
*Acceptance:* directional default-deny flows enforced, plus the bad-actor cases —
source spoofing, out-of-state injection, ARP poisoning.

**N5 — host edge.** Uplink NAT/DNAT and container links into the host LAN — the
part that genuinely needs the init netns.

**N6 — reconcile in place.** `readState` → classify → execute over live state, so
a re-plan preserves attached sandboxes rather than stranding them.

### Dependency notes

- **No async runtime.** The rust-netlink stack splits into a sync core
  (`netlink-sys` / `netlink-packet-route`) and the async `rtnetlink` layer. Only
  the sync core goes in: one binary means a tokio dependency would land in the
  `run` path, whose whole claim is single-digit-ms cold start
  ([startup-performance.md](startup-performance.md)). The netlink surface is
  small — veth, move, address, route, link-up. Confirm the crate split at N3.
- **`NFT_BIN` joins the pinned-binary set** in `build.rs` and the flake, beside
  `CRUN_BIN` and `XDG_DBUS_PROXY_BIN` — same rationale, and it makes `nft` a
  runtime dependency by Nix's reference scan.
- **No feature gates.** The `crun` gate was removed for the same reason; the
  privileged code is compiled in but reachable only through an explicit verb under
  `sudo`. The binary is not setuid.

## Open questions

- **Net-spec range default.** `/etc/subuid` is the conventional answer and the
  wrong authority here, since root is not bound by it. Sizing is a judgment call
  about expected workload; podman's `2147483648` is the precedent for erring
  large.
- **One registry or several.** A single registry pin gives every network a shared
  fate (killing it drops the mount namespace and every name with it) and a shared
  id-range ceiling. Per-network registries restore independence at the cost of one
  process each. The `src/dbus.rs` lifecycle-isolation argument does *not* transfer
  directly: networks under a registry are inodes, not supervised processes, so
  there is no health to couple.
- **Where the pin is supervised.** `systemd --user` is the answer
  [vision.md](vision.md) prescribes for lifecycle; the unit shape is unspecified.
- **Whether the shared schema crate is worth the coupling** versus two
  definitions kept in sync by tests.
