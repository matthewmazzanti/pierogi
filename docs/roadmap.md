# Pierogi — Roadmap & Milestones

After the de-risking spike (M0), milestones are ordered by **where the
uncertainty and interest are**, not by a generic harden-first sequence. The novel
integrations — nix-mode, the D-Bus proxy, GPU/Wayland — are the genuinely unknown
surface and are pulled forward.

**The engine holds no policy** (see [architecture.md](architecture.md)'s third
principle and [control-surface.md](control-surface.md)): pierogi-the-binary is a
*pure realizer* — a lightly-extended OCI runtime (runc's role) plus nix carve-outs
— and it projects whatever the resolved spec carries, `absent = absent`. So
"hardening" is **not an engine milestone**. The engine *supports* the full OCI
surface (caps, seccomp, masks, mappings, devices, rlimits) as **pass-through** once
[M1.5](#m15--engine-as-pure-realizer-passthrough--light-nix-mapping) lands; the
**default-closed posture is `profiles.base`** in the Nix layer (podman's role), and
what's genuinely deferred is only *producer curation*: the seccomp profile,
subuid-range resolution, and `pierogi-synth` `/etc` ([M4](#m4--curated-policy-profilesbase-synth--seccomp-profile)). Two engine-adjacent
mechanisms stay the engine's job because they're mechanism, not policy: it
**supervises host-side sidecars** (the D-Bus filter proxy, [M2](#m2--d-bus-proxy-layer)) and accepts **CLI overrides** that inject
runtime-only changes (a bind mount, an env var) as a runtime producer.

Each milestone lists **acceptance criteria** (how we know it's done) and the
**architecture it forces** (what must exist to get there). See
[architecture.md](architecture.md) for components and [spec.md](spec.md) for the
spec schema.

---

## M0 — Skeleton & shell-run spike  ⭐ de-risk first  ✅

Prove the single biggest unknown **before** building anything else: that we can
drive an OCI runtime and actually namespace + `exec` a real shell. Deliberately
crude — no synth, no strict defaults, no nice spec — just a shell in a namespace,
driven from Rust.

**Forces:** driving the runtime, the backend trait, the spec → OCI-spec
translation in its thinnest form, project plumbing.

- [x] Cargo workspace: `pierogi-engine` (lib), `pierogi-cli` (bin). (Add
      `pierogi-synth` in M4.)
- [x] Minimal in-memory spec struct — only the fields the spike needs.
- [x] Backend trait + a thin spec → in-memory `oci-spec` translation.
- [x] **Drive the `crun` binary** (as a subprocess, rootless) to
      `create → start → wait → teardown` a `bash` in user + mount + pid
      namespaces, over a minimal store-closure rootfs. *(Started on `youki`
      `libcontainer`, but switched to `crun`: youki couldn't RO-remount a
      `/nix/store` rootfs. An interim design linked `libcrun` in-process via an
      FFI shim, later dropped for the plain binary — the in-process path saved
      only ~a few milliseconds, within noise; see startup-performance.md.)*
- [x] Answer the embedding questions: backend choice, process-model cost, pid-1
      behavior. First cold-start number recorded (see startup-performance.md).
- [x] Nix flake builds the binary; CI: build/test/clippy/rustfmt.

**Acceptance (met):** `pierogi run -- bash` (spike form) drops into an interactive
shell visibly in its own namespaces, launched by driving the runtime library from
Rust — `/etc` crude, confinement not yet strict. No leaked mounts after exit.

**Deferred to M4 (hardening):** synth `/etc`, read-only root, id mapping, strict
defaults, seccomp.

---

## M1 — Nix mode: the reproducible core  (crude)

Pull the second run-mode forward and build the **spec → closure → engine** loop
on it, while keeping the crudeness of M0. This is where the schema, the supplied
(no-query) closure, and the self-contained resolved-spec artifact get proven end
to end — the parts this whole design hinges on. **Not** the module/config layer
(that's last); just the engine consuming a spec and a minimal builder producing
one.

**Forces:** the typed spec schema (`serde`), the mode-agnostic engine realization
tail, the in-process closure traversal, the self-contained spec artifact.

- [x] Typed `Spec` / `Mount` / `Closure` + `serde` (the v0 schema — see
      [spec.md](spec.md)), replacing M0's ad-hoc `SpecInputs`.
- [x] `resolve()`: `closure.seeds` is a **build-time input** (producer declares
      `root ∪ overlay lowers`); no live query — traverse Nix's
      `exportReferencesGraph` (JSON via `__structuredAttrs`) in-process from the
      seeds; stamp `closure.resolved` (fail loud if it doesn't cover `root`).
- [x] Refactor `build_spec` to consume a **resolved** `Spec`: `closure.resolved`
      → identity binds, `mounts` → OCI mounts (`Host`/`Tmpfs`; `Overlay` after —
      still stubbed), `environment` → `process.env`, `executable`/`args` → argv
      (flat compose-flavoured fields; `tty`/`working_dir`/`read_only` too).
- [x] `pierogi run <spec.json>` — load, closure present ⇒ no query, realize,
      launch. `pierogi run <store-path>` is the *same* path: the nix-mode
      artifact is just a resolved spec file (see the builder item), so a store
      path is a `spec.json` that happens to live in the store.
- [x] `pierogi resolve <spec>` (freeze → bake `closure:`) via `resolve --graph
      <exportReferencesGraph JSON>` for the build-time reference graph.
- [ ] Minimal Nix builder: `root = buildEnv/pkgs` **with a hand-rolled `/etc`**
      (fixed `passwd`/`group`/`hostname` written as Nix strings, baked into the
      root — real files, authored by hand, *not* synth-generated), feed the
      seeds' `exportReferencesGraph` via `__structuredAttrs`
      (`$NIX_ATTRS_JSON_FILE`), run `resolve --graph` → emit a
      **self-contained `spec.json` store path** (a single JSON file, not a
      directory: `closure.resolved` names the closure absolutely, so no `rootfs`
      entry is needed and Nix pins the closure via the path strings it contains).
      See [spec.md](spec.md#nix-mode-closure-source-build-time).

**Acceptance:**
- `pierogi run <hand-authored spec.json>` launches `bash` from its declared
  closure with **no live Nix query** (closure already resolved in the spec). ✅
- `nix build` produces a self-contained `spec.json` store path that `pierogi run
  <path>` launches into the same shell.
- inside it, `cat /etc/passwd` shows the **hand-rolled** file baked into the root
  — proving the baked-`/etc`-in-root path (synth automation is M4).

**Deferred:** the `pierogi-synth` builders (M1 hand-rolls `/etc` in Nix instead),
the curated closed defaults (`profiles.base`, M1.5/M4), subuid ranges + seccomp
*profile* (M4), and the `wrap` argv front-end (M4).

---

## M1.5 — Engine as pure realizer (passthrough + light nix mapping)

The pivot that makes everything after it cheap. **Reduce the engine to a faithful
OCI projection + nix carve-outs, and move all policy up into Nix.** Once this
lands, adding a control (caps, seccomp, devices, mappings) is a one-line
pass-through field, not an engine feature — so the "hardening milestone" dissolves
into a Nix profile.

**Forces:** the schema as an OCI projection (`absent = absent`), an `oci.rs` that
holds no defaults, and the first Nix policy library (`profiles.base`).

- [ ] **Strip engine policy.** `oci.rs` stops hardcoding the namespace set, the
      1:1 id map, and `root.readonly`; `spec.rs` drops the `read_only`/`hostname`
      serde defaults. Neutralize `oci-spec`'s inherited `Process::default()` (nnp,
      3 caps, nofile rlimit) — emit exactly what the spec carries, or nothing.
- [ ] **Pass-through fields** (all `absent = absent`): `no_new_privileges`,
      `capabilities` (`Some([])` = drop-all), `rlimits`, `masked_paths`,
      `readonly_paths`, `rootfs_propagation`, `namespaces` (untagged: `"user"` |
      `{type,path}`), `seccomp` and `devices` (reuse `oci-spec` types verbatim),
      and `uid/gid_mappings` with a `host: N | "self"` sentinel the engine resolves
      to `getuid()`/`getgid()` at launch.
- [ ] **`mounts` carry explicit `options`** (drop `rw`); the `:rw`/`:ro` sugar
      moves into the producers (CLI + Nix). Closure identity-binds keep implicit
      `ro,nosuid,nodev` (mechanism constant — an immutable store never varies).
- [ ] **`profiles.base` (Nix):** the curated default-closed floor as a composable
      attrset (read-only root, nnp, caps `[]`, curated masks/readonly paths,
      rprivate, `RLIMIT_CORE=0`, private namespaces incl. `network`, the
      `self` 1:1 map). `sandbox/bash.nix` composes it and comes out hardened —
      while the engine decided nothing.
- [ ] **CLI overrides stay** (`--env`/`--mount`/argv/`--hostname`): a *runtime
      producer* that injects runtime-only changes onto the resolved spec.
- [ ] First `oci.rs` tests, incl. the regression guard: a minimal spec yields
      **no** capabilities (proves the inherited 3-cap default is gone).

**Acceptance:** `pierogi run` on a `base`-composed bash-sandbox launches a shell
with empty caps, nnp, masked `/proc`, a private net ns, `nosuid,nodev` binds — and
every one of those values is **visible in the resolved spec**, written by
`profiles.base`, not by the engine. The same spec with `base` *not* composed runs
open (the runc contract), also visibly.

---

## M2 — D-Bus proxy layer

The first managed proxy and the first "talks to the outside world through a
filter" feature — pulled forward as one of the genuinely novel integrations.

**Forces:** the proxy supervisor, socket-binding-into-sandbox, the `dbus` section
of the spec, ready-synchronization before `exec`.

- [ ] Supervise a filtering D-Bus proxy (xdg-dbus-proxy-style policy).
- [ ] Session + system bus, independent allowlists, deny-all default.
- [ ] Proxy lifecycle tied to sandbox lifecycle (no orphans).
- [ ] Resolve "where the supervisor lives after the Engine hands off."
- [ ] Spec `dbus` rules → proxy policy translation + tests.
- [ ] **`/etc/machine-id`** — the bus requires it; add it to the root's
      hand-rolled `/etc` (M1's approach). Just one more baked file — no synth yet.

**Acceptance:** a sandboxed app sends a desktop notification *only* because an
explicit `talk` rule allows it; remove the rule and the call is denied at the
proxy; the raw bus socket is never present in the sandbox.

**Pure-realizer framing:** the proxy is a **host-side sidecar** the engine
*supervises* (spawn, wait-ready before `exec`, tear down with the sandbox) and
whose filtered socket it binds in — that's **mechanism**, legitimately the
engine's job, like the tty relay. The **filter policy is producer data**: `dbus`
rules travel in the resolved spec and the engine translates them to proxy config
without holding any default of its own (deny-all is a `profiles.base`/producer
choice, not an engine one).

**Note:** compose `profiles.base` for a confined run; without it this is a bare
*mechanism* proof. Re-validated under the full curated policy at M4.

---

## M3 — GPU & Wayland  🖥️

The compositor-socket + device-access surface, standing on its own as reusable
foundation before the browser consumes it. The other novel integration worth
proving early.

**Forces:** GUI socket proxying (Wayland/Xwayland), the spec's device + socket
surface, GPU device allowance, PipeWire.

- [ ] Wayland (and/or Xwayland) exposure via a **filtered/proxied** socket, not a
      raw bind.
- [ ] `/dev/dri` GPU access as a **narrow, explicit** device allowance — grows
      the spec's device section.
- [ ] PipeWire / audio access as a declared capability.
- [ ] Spec surface for devices + proxied sockets, shaped against these real
      clients.

**Acceptance:** a GPU-accelerated Wayland client (e.g. a demo app or terminal)
renders through the proxied compositor socket with no raw host socket and no host
`/dev` beyond the declared device.

**Note:** mechanism proof on the crude base; re-validated under confinement at M4.

---

## M4 — Curated policy: `profiles.base`, synth & the seccomp profile

Not "add hardening to the engine" — the engine already *supports* every knob as
pass-through (M1.5). This milestone is the **producer-side curation** the
pass-throughs were waiting for: fill `profiles.base` out to its final form, author
the seccomp profile, resolve subuid ranges, and generate `/etc` — done in one pass
now that the mechanisms (M1–M3) exist and have shaped what the profiles must carry,
then re-validated across the earlier integrations.

**Forces:** the Nix profile library, the `pierogi-synth` builders, subuid-range
resolution, the `wrap` argv front-end.

- [ ] **`pierogi-synth` crate:** pure builders for `passwd`/`group`/`hostname`/
      `hosts`/`resolv.conf`/`machine-id`/`nsswitch` from the synthetic-identity
      JSON slice (`serde`). Golden-tested. **Replaces M1/M2's hand-rolled `/etc`** —
      the same generator serves build *and* runtime.
- [ ] `pierogi synth-etc <spec.json> <outdir>` build-time entrypoint (the M1 Nix
      builder starts calling it to bake `/etc`).
- [ ] `pierogi wrap` argv front-end (a *runtime producer*) + baked base rootfs +
      `wrap --dry-run` (prints the *resolved* spec — never a raw OCI dump).
- [ ] **subuid/subgid range resolution:** read `/etc/subuid`, drive
      `newuidmap`/`newgidmap`, and emit concrete `uid/gid_mappings` — the runtime
      resolution the M1.5 `"self"` sentinel and numeric pass-through leave open.
- [ ] **The curated seccomp *profile*** (deny-all default + allowlist) authored as
      a `profiles.base` value the engine passes through — including per-app deltas
      for the M2/M3 proxies. (Custom `seccompiler`-style only if the OCI schema
      can't express it.)
- [ ] **Finish `profiles.base`** (started in M1.5): pin the final mask list, cap
      posture, rlimits, propagation, and the loopback-only `network` rung; profile
      composition (`base ∘ network ∘ gui …`). Finalize pid-1/init strategy.
- [ ] Perf pass: hit the single-digit-ms cold-start target; document overhead.
- [ ] Threat-model doc; fuzz/round-trip the spec + synth-identity parsers.
- [ ] **Re-validate M2/M3** composing the full curated profiles (allowlist what the
      proxies / GPU access legitimately need).

**Acceptance:**
- `pierogi wrap -- bash` (composing `profiles.base`) gives a confined shell: `id`
  shows the mapped uid; `cat /etc/hostname` shows the synthetic name; `/` is
  read-only, `/tmp` writable; host `/etc/passwd`/`/home`/`/proc/1` are **not**
  visible; `whoami` resolves via synthetic `passwd`; runs under the deny-all
  seccomp profile — and every value is visible in the resolved spec.
- `pierogi synth-etc` produces correct `/etc` from an identity JSON.
- The M2 D-Bus and M3 GPU/Wayland sandboxes still work composing the profiles.
- Cold-start within budget, benchmarked; threat model written.

---

## M5 — Sandboxed browser  🦊

First heavyweight real-world target — now largely **composition** of the proven
D-Bus (M2), GPU/Wayland (M3), and hardening (M4) pieces, plus browser-specific
glue.

**Forces:** xdg-desktop-portal rules, persistent profile state, a reusable
"browser" bundle of spec defaults.

- [ ] xdg-desktop-portal D-Bus rules for the browser (on the M2 proxy).
- [ ] Persistent profile/state dir as an explicit writable mount (`Overlay` or
      `Host { rw }`).
- [ ] A reusable browser spec bundle — wrap-mode flags/defaults or a hand-written
      nix-mode store path. (Promotion into a `profiles.browser` *module* waits for
      the config layer; the sandbox itself does not.)

**Acceptance:** Firefox/Chromium launches confined, renders accelerated, plays
audio, file dialogs work via portals — no raw host socket, no host `/etc`, no host
`/home` beyond the declared profile dir.

---

## M6 — Steam & the messy long tail  🎮

The stress test. Steam wants a lot, behaves badly, and runs arbitrary downloaded
code — the ideal adversary for the auditable-boundary thesis (default-safe,
every loosening explicit and on the record — see [vision.md](vision.md) and
[control-surface.md](control-surface.md)).

**Forces:** multi-process trees, 32-bit, broad GPU/controller device access,
larger writable state, performance under load.

- [ ] Multi-process / child-process handling under the PID namespace.
- [ ] 32-bit library closure handling.
- [ ] Controller / input device access as narrow declared allowances.
- [ ] Larger persistent state mounts (game installs) done explicitly.
- [ ] Validate the strict-defaults thesis survives a genuinely demanding,
      misbehaving target — document where (if anywhere) we had to loosen.

**Acceptance:** Steam installs and runs a game confined; any concessions are
explicit in the profile and justified in writing, not blanket host exposure.

---

## M8 — Networking: `pierogi net` + rootless attach  🌐

> **Design and mechanism are settled and verified; none of it is built.** The full
> rationale, the kernel rules, the hazards and the phase detail live in
> [network.md](network.md) — this block is the milestone view only.

A network is **pre-baked** by a privileged, short-lived verb and attached to
rootlessly at launch — invariant 2 applied to networking. `sudo pierogi net up`
does the rootful work and exits, leaving a **registry pin**: one unprivileged
process holding a user namespace and a mount namespace, with each network a netns
bind-mounted at a name inside it. `pierogi run --net <name>` joins the registry's
user namespace and hands crun the netns by path; crun joins it and then unshares a
**fresh** user namespace for the sandbox. The sandbox can *use* the network and
cannot *shape* it — enforced by the kernel's capability model, not by policy the
engine maintains.

**Forces:** a `setns` in `pre_exec`, the first privileged verb in a rootless
codebase, a second resolved-artifact kind (net-config → net-plan), synchronous
netlink, and a third pinned binary (`nft`).

- [ ] **N0 — attach spike.** Hand-made pin; `--net` override, inode verification,
      `setns(CLONE_NEWUSER)` in `pre_exec`, and the guard that rejects
      `{type: user, path}` alongside a network attach.
- [ ] **N1 — the `net` verb, empty networks.** `setresuid` before `unshare`, root
      writes the id slab, named netns bind-pinned, attachability self-check,
      artifact emitted, attach-or-mint semantics.
- [ ] **N2 — the lowering.** Net-config schema; `pierogi net plan` as a pure
      unprivileged verb; the composed-mapping and `self`-under-attach fixes.
- [ ] **N3 — links.** Sync netlink (veth, move-to-netns, address, route) plus the
      naming/`/32` lowering and the router topology.
- [ ] **N4 — policy.** nft ruleset + sysctl sets built in the lowering, pushed by
      `up` via a `setns` episode and the pinned `nft`.
- [ ] **N5 — host edge.** Uplink NAT/DNAT and links into the host LAN.
- [ ] **N6 — reconcile in place.** `readState` → classify → execute, so a re-plan
      preserves attached sandboxes.

**Acceptance:** two sandboxes attach to one pre-baked network by name; an allowed
flow connects and a denied one drops; a sandbox running as uid 0 with
`CAP_NET_ADMIN` effective cannot rename, re-address, or re-route anything; and
re-running `net up` leaves already-attached sandboxes in place.

**Pure-realizer framing:** the *policy* — who exists, who may talk to whom, what
crosses the edge — is producer data authored in Nix. The binary performs a
**mechanical derivation** (config → plan) and then realizes it, which is
`pierogi resolve`'s existing shape rather than a new kind of thing. The split into
an unprivileged `plan` and a privileged `up` keeps validation ahead of privilege:
`up` is total over a valid plan.

**Note:** `net up` is the first code that runs as root. It is a distinct verb in a
non-setuid binary, so the privileged path is compiled in but unreachable without an
explicit `sudo` — the posture runc and crun already have.

---

## M7 — Nix config / module layer  (DEFERRED / OPEN — last)

> **Status: intentionally last and left open.** Not part of the core deliverable
> (the runnable binary + its run-modes). Shape this only once real usage (M1–M6)
> has shown what the sugar should crystallize.

Promote the hand-written nix-mode store paths into ergonomic, declarative,
composable modules — pure sugar over the resolved-spec store path M1 already runs.

**Forces:** options schema, `module → resolved-spec` evaluation, profile
composition, OS integration.

- [ ] `pierogi.sandboxes.<name>` options schema evaluating to a nix-mode store
      path.
- [ ] Composable profiles (`base`, `network`, `gui`, `browser`, …), factoring out
      the bundles M5/M6 built by hand.
- [ ] NixOS / home-manager module: emit wrapper scripts and/or `systemd` units.
- [ ] Golden tests: `module eval → spec` matches the wrap-mode equivalent.

**Acceptance:** a checked-in `.nix` file defines a named sandbox whose entire
system view is auditable from that file; building it yields the same kind of store
path M1 runs, and the emitted spec matches the hand-written equivalent.

---

## Cross-cutting tracks (run alongside)

- **Performance budget.** Re-measure cold start each milestone; regressions are
  bugs. Target stays single-digit ms of engine overhead.
- **Threat model.** Living doc; updated whenever a milestone adds surface area.
  Note: M1–M3 run on a *partially*-curated `profiles.base` — track them as
  *lightly-hardened until the M4 profiles are authored* (the engine surface is
  complete at M1.5; what's missing is the curated policy values).
- **Testing.** Synth-builder golden tests + spec/translation tests + namespace
  integration tests (VM/CI sandbox) + proxy policy tests.
- **Docs.** Keep `vision`/`architecture`/`roadmap`/`spec` honest as reality
  diverges from plan.

## Sequencing rationale

M0 → nix-mode → D-Bus → GPU/Wayland → hardening → browser → Steam → networking →
config layer is **interest-and-uncertainty-driven**, a deliberate departure from
harden-first:

- **The spike (M0) still leads** — driving an OCI runtime to namespace + `exec`
  was the biggest unknown; everything else builds on it. That much is unchanged.
- **Nix-mode (M1) comes next** because the spec → supplied-closure → engine loop
  is the design's core bet, and proving it end-to-end (schema,
  `exportReferencesGraph` traversal, self-contained resolved-spec artifact)
  validates the whole model cheaply —
  crudely, reusing M0's lack of hardening.
- **D-Bus (M2) and GPU/Wayland (M3) are pulled forward** because they're the
  *novel, uncertain* integrations — proxying sockets and narrowing device access
  is where the real unknowns live. Building them early also lets them **shape the
  spec's `dbus`/device/socket surface against real clients** rather than guesses
  (the M7-is-last philosophy, one level down).
- **Networking (M8) is numbered after Steam but sits before M7** in this file,
  because ordering here is execution sequence and M7 is last *by status*, not by
  position (M1.5 already breaks numeric sequence for the same reason). It lands
  once the engine's pass-through surface exists, since the attach is mostly
  `linux.namespaces` plus one `setns`.
- **The engine is reduced to a pure realizer early (M1.5)**, because that's what
  makes the rest cheap: once the engine holds no policy and passes the full OCI
  surface through, every later control is a one-line field, and "hardening" stops
  being engine work.
- **Curation (M4) is deferred on purpose.** The floor *values* — the seccomp
  profile, subuid resolution, the final `profiles.base`, synth `/etc` — are
  well-understood, so they buy little as *proofs*. Better to author them once, in
  Nix, over proven mechanisms, and re-validate the earlier integrations composing
  them. Note this is *producer* work; the engine's support for the knobs already
  exists from M1.5.
- **The cost, accepted consciously:** M1–M3 are demonstrated on a *lightly*-curated
  base (hand-rolled `/etc`, a partial `profiles.base`, no seccomp profile yet), so
  they are *mechanism spikes, not shippable-secure*. "Browser launches" ≠ "browser
  is confined" until the M4 profiles are authored — but the gap is now a matter of
  *composing a richer profile*, not of engine changes. (D-Bus needs
  `/etc/machine-id`, but that's just another hand-written file in M1's root `/etc`
  — no synth pulled forward.)
- **The config/module layer (M7) is still last and open** — convenience over a
  mechanism that already works, shaped by what M1–M6 actually needed.
