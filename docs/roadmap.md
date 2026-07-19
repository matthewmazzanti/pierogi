# Pierogi — Roadmap & Milestones

After the de-risking spike (M0), milestones are ordered by **where the
uncertainty and interest are**, not by a generic harden-first sequence. The novel
integrations — nix-mode, the D-Bus proxy, GPU/Wayland — are the genuinely unknown
surface and are pulled forward; the well-understood hardening work (strict
defaults, seccomp) is deliberately deferred to a single later milestone. This
**trades away** the "later targets inherit a hardened base" property on purpose —
see [Sequencing rationale](#sequencing-rationale) for the consequences.

Each milestone lists **acceptance criteria** (how we know it's done) and the
**architecture it forces** (what must exist to get there). See
[architecture.md](architecture.md) for components and [spec.md](spec.md) for the
spec schema.

---

## M0 — Skeleton & shell-run spike  ⭐ de-risk first  ✅

Prove the single biggest unknown **before** building anything else: that we can
embed an OCI runtime in-process and actually namespace + `exec` a real shell.
Deliberately crude — no synth, no strict defaults, no nice spec — just a shell in
a namespace, driven from Rust.

**Forces:** the in-process runtime embedding, the backend trait, the
spec → OCI-spec translation in its thinnest form, project plumbing.

- [x] Cargo workspace: `pierogi-engine` (lib), `pierogi-cli` (bin). (Add
      `pierogi-synth` in M4.)
- [x] Minimal in-memory spec struct — only the fields the spike needs.
- [x] Backend trait + a thin spec → in-memory `oci-spec` translation.
- [x] **Drive `youki` `libcontainer` in-process** (rootless) to
      `create → start → wait → teardown` a `bash` in user + mount + pid
      namespaces, over a minimal store-closure rootfs.
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
tail, the pluggable (supplied) closure source, the self-contained spec artifact.

- [x] Typed `Spec` / `Mount` / `Resolved` + `serde` (the v0 schema — see
      [spec.md](spec.md)), replacing M0's ad-hoc `SpecInputs`.
- [x] `resolve()`: seeds = `root ∪ store_refs(mounts)`; `closure_of` **pluggable**
      — live `nix-store --query --requisites` (host) *or* a supplied list; stamp
      `{from, paths}`. Reuse the `PIEROGI_CLOSURE` validation.
- [x] Refactor `build_spec` to consume a **resolved** `Spec`: `closure.paths` →
      identity binds, `mounts` → OCI mounts (`Store`/`Host`/`Tmpfs`; `Overlay`
      after — still stubbed), `env` → `process.env`, `exec` → argv.
- [x] `pierogi run <spec.json>` — load, closure present ⇒ no query, realize,
      launch. `pierogi run <store-path>` is the *same* path: the nix-mode
      artifact is just a resolved spec file (see the builder item), so a store
      path is a `spec.json` that happens to live in the store.
- [x] `pierogi resolve <spec>` (freeze → bake `closure:`) incl. `resolve
      --closure <store-paths>` for the build-time supplied path.
- [ ] Minimal Nix builder: `root = buildEnv/pkgs` **with a hand-rolled `/etc`**
      (fixed `passwd`/`group`/`hostname` written as Nix strings, baked into the
      root — real files, authored by hand, *not* synth-generated), `closureInfo {
      rootPaths = seeds }` → `paths`, run `resolve --closure` → emit a
      **self-contained `spec.json` store path** (a single JSON file, not a
      directory: `closure.paths` name the closure absolutely, so no `rootfs`
      entry is needed and Nix pins the closure via the path strings it contains).
      See [spec.md](spec.md#nix-mode-closure-source-build-time).

**Acceptance:**
- `pierogi run <hand-authored spec.json>` launches `bash` from its declared
  closure with **no `nix-store` query** (closure supplied). ✅
- `nix build` produces a self-contained `spec.json` store path that `pierogi run
  <path>` launches into the same shell.
- inside it, `cat /etc/passwd` shows the **hand-rolled** file baked into the root
  — proving the baked-`/etc`-in-root path (synth automation is M4).

**Deferred:** the `pierogi-synth` builders (M1 hand-rolls `/etc` in Nix instead),
strict confinement + id mapping (1:1 as M0), seccomp, and the `wrap` argv
front-end — all to M4.

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

**Note:** runs on the M0/M1 crude (unhardened) base — this is a *mechanism* proof,
re-validated under confinement at M4.

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

## M4 — Hardening: synth, strict defaults & seccomp

The deferred security work, done in one pass now that the mechanisms (M1–M3)
exist and have shaped the spec. Well-understood, so sequenced late on purpose —
then applied *across* the earlier integrations, which get re-validated under it.

**Forces:** the `pierogi-synth` builders, strict-by-default wrap mode, the seccomp
pipeline, the perf budget, the `wrap` argv front-end.

- [ ] `pierogi-synth` crate: pure builders for `passwd`/`group`/`hostname`/
      `hosts`/`resolv.conf`/`machine-id`/`nsswitch`, consuming the
      synthetic-identity JSON slice (`serde`). Golden-tested. **Replaces M1/M2's
      hand-rolled `/etc`** — the bridge from an identity attrset to real files
      (streamlines authoring, and the same generator serves build *and* runtime).
- [ ] `pierogi synth-etc <spec.json> <outdir>` (the build-time entrypoint; the
      Nix builder from M1 starts calling it to bake `/etc`).
- [ ] `pierogi wrap` argv front-end + baked base rootfs + `wrap --dry-run`
      (prints the *unresolved* spec — never an OCI dump).
- [ ] User namespace + uid/gid map from subuid/subgid ranges (OCI
      `uidMappings`/`gidMappings`).
- [ ] Read-only root, caps drop to empty + `noNewPrivileges`, UTS + IPC
      namespaces; finalize pid-1/init strategy.
- [ ] Default-deny seccomp profile + per-profile allowlist (OCI `linux.seccomp`;
      custom `seccompiler`-style filter only if the schema can't express it).
- [ ] Network namespace default-off; loopback-only mode.
- [ ] Perf pass: hit the single-digit-ms cold-start target; document overhead.
- [ ] Threat-model doc; fuzz/round-trip the spec + synth-identity parsers.
- [ ] **Re-validate M2/M3** under strict confinement + seccomp (allowlist what the
      proxies / GPU access legitimately need).

**Acceptance:**
- `pierogi wrap -- bash` gives a confined shell: `id` shows the mapped uid;
  `cat /etc/hostname` shows the synthetic name; `/` is read-only, `/tmp` writable;
  host `/etc/passwd`/`/home`/`/proc/1` are **not** visible; `whoami` resolves via
  synthetic `passwd`; runs under default-deny seccomp.
- `pierogi synth-etc` produces correct `/etc` from an identity JSON.
- The M2 D-Bus and M3 GPU/Wayland sandboxes still work under confinement.
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
code — the ideal adversary for the "no escape hatches" thesis.

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
  Note: M1–M3 predate strict confinement — track them as *unhardened until M4*.
- **Testing.** Synth-builder golden tests + spec/translation tests + namespace
  integration tests (VM/CI sandbox) + proxy policy tests.
- **Docs.** Keep `vision`/`architecture`/`roadmap`/`spec` honest as reality
  diverges from plan.

## Sequencing rationale

M0 → nix-mode → D-Bus → GPU/Wayland → hardening → browser → Steam → config layer
is **interest-and-uncertainty-driven**, a deliberate departure from harden-first:

- **The spike (M0) still leads** — in-process runtime embedding was the biggest
  unknown; everything else builds on it. That much is unchanged.
- **Nix-mode (M1) comes next** because the spec → supplied-closure → engine loop
  is the design's core bet, and proving it end-to-end (schema, `closureInfo`,
  self-contained resolved-spec artifact) validates the whole model cheaply —
  crudely, reusing M0's lack of hardening.
- **D-Bus (M2) and GPU/Wayland (M3) are pulled forward** because they're the
  *novel, uncertain* integrations — proxying sockets and narrowing device access
  is where the real unknowns live. Building them early also lets them **shape the
  spec's `dbus`/device/socket surface against real clients** rather than guesses
  (the M7-is-last philosophy, one level down).
- **Hardening (M4) is deferred on purpose.** Strict confinement and seccomp are
  well-understood, so they buy little as *proofs* — better to land them once, over
  proven mechanisms, and re-validate the earlier integrations under them.
- **The cost, accepted consciously:** M1–M3 run on a crude base (hand-rolled
  `/etc`, no strict confinement or seccomp), so they are *mechanism spikes, not
  shippable-secure*. "Browser launches" ≠ "browser is confined" until M4. (D-Bus
  needs `/etc/machine-id`, but that's just another hand-written file in M1's root
  `/etc` — no synth pulled forward.)
- **The config/module layer (M7) is still last and open** — convenience over a
  mechanism that already works, shaped by what M1–M6 actually needed.
