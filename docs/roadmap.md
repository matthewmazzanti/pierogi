# Pierogi — Roadmap & Milestones

Milestones are ordered by increasing sandbox demands, after a deliberate
de-risking spike. Each one is a *forcing function* for a chunk of the
architecture: the shell-run spike forces the in-process runtime embedding, the
proper shell forces synth + strict confinement, the browser forces GUI + D-Bus +
portals, Steam forces the messy real-world long tail. See
[architecture.md](architecture.md) for components.

Each milestone lists **acceptance criteria** (how we know it's done) and the
**architecture it forces** (what must exist to get there).

---

## M0 — Skeleton & shell-run spike  ⭐ de-risk first

Prove the single biggest unknown **before** building anything else: that we can
embed an OCI runtime in-process and actually namespace + `exec` a real shell.
Deliberately crude — no synth, no strict defaults, no nice spec — just a shell in
a namespace, driven from Rust.

**Forces:** the in-process runtime embedding, the backend trait, the
spec → OCI-spec translation in its thinnest form, project plumbing.

- [ ] Cargo workspace: `pierogi-engine` (lib), `pierogi-cli` (bin). (Add
      `pierogi-synth` in M1.)
- [ ] Minimal in-memory spec struct — only the fields the spike needs. (Full
      serialized format stays open; don't over-design it.)
- [ ] Backend trait + a thin spec → in-memory `oci-spec` translation.
- [ ] **Drive `youki` `libcontainer` in-process** (no bundle dir, rootless) to
      `create → start → wait → teardown` a `bash` in user + mount + pid
      namespaces, over a throwaway/minimal rootfs. `/etc` may be empty or a
      crude stub; ids need not be fully mapped yet.
- [ ] Answer the embedding questions: backend choice (youki vs needing libcrun
      FFI), the runtime's process-model cost against the cold-start budget,
      pid-1/reaper behavior. Record a first cold-start number.
- [ ] Nix flake that builds the binary; `nix run` works. CI: build/test/clippy/
      rustfmt.

**Acceptance:** `pierogi run -- bash` (spike form) drops into an interactive
shell that is visibly in its own namespaces (`echo $$` → low pid; a fresh mount
view), launched by driving the runtime library from Rust — even though `/etc` is
crude and confinement isn't strict yet. No leaked mounts after exit.

**Explicitly deferred to M1:** synth `/etc`, read-only root, id mapping, strict
defaults — none of it should block proving the embedding works.

---

## M1 — Run a shell, properly  (synth + strict)

Turn M0's crude shell into the *real* confined shell: read-only root, id
mapping, and synthetic `/etc` — and build the synth library that produces it.
This is the "is the core actually working as designed" checkpoint.

**Forces:** the `pierogi-synth` builders, the full spec → OCI-spec translation,
strict-by-default wrap mode.

- [ ] `pierogi-synth` crate: pure builders for `passwd`/`group`/`hostname`/
      `hosts`/`resolv.conf`/`machine-id`/`nsswitch`, consuming the
      synthetic-identity JSON slice (`serde`). Unit-tested against golden files.
- [ ] `pierogi synth-etc <spec.json> <outdir>` subcommand (the build-time
      entrypoint Nix will call in M3).
- [ ] Wire `pierogi-synth` into wrap mode: render `/etc` at runtime into the
      staging dir, bound read-only into the spec.
- [ ] User namespace + uid/gid map from subuid/subgid ranges (via OCI
      `uidMappings`/`gidMappings`).
- [ ] Rootfs from store closure, `pivot_root`, read-only root (via OCI
      `root.readonly` + `mounts`); tmpfs for `/tmp` and runtime dir.
- [ ] UTS + IPC namespaces; finalize pid-1/init strategy from M0's findings.
- [ ] Capability drop to empty set + `noNewPrivileges`.
- [ ] A baseline seccomp profile in the OCI spec (permissive-ish; tighten in M2).
- [ ] Clean Pierogi-side teardown on failure (staging dir).
- [ ] `pierogi run --dry-run -- bash` prints the OCI spec it *would* hand the
      runtime.

**Acceptance:**
- `pierogi run -- bash` gives an interactive shell where:
  - `id` shows the mapped uid; `cat /etc/hostname` shows the synthetic name.
  - `/` is read-only (`touch /foo` fails); `/tmp` is writable.
  - host `/etc/passwd`, host `/home`, host `/proc/1` are **not** visible.
  - `whoami` resolves via synthetic `passwd`.
- `pierogi synth-etc` produces correct `/etc` files from an identity JSON.
- Cold-start overhead measured and recorded (baseline for the perf budget).
- No leaked mounts in the host namespace after exit.

**Explicitly deferred:** network, D-Bus, GUI, fine-grained seccomp profiles.

---

## M2 — Strictness, seccomp & hardening

Turn M1's "it works" into "it's actually tight." Done before adding surface
area, so later milestones inherit a hardened base.

**Forces:** the seccomp profile authoring pipeline, default-deny posture, perf
budget.

- [ ] Default-deny seccomp profile + per-profile allowlist, expressed in the
      OCI `linux.seccomp` schema (custom `seccompiler`-style filter only if the
      schema can't express what we need).
- [ ] No-new-privs, tightened capability/ambient handling.
- [ ] Network namespace default-off; loopback-only mode.
- [ ] Perf pass: hit the single-digit-ms cold-start target; document overhead.
- [ ] Threat-model doc: enumerate what's confined and known gaps.
- [ ] Fuzz/round-trip the spec parser + synth-identity JSON (attacker-adjacent
      input).

**Acceptance:** the shell from M1 runs under default-deny seccomp; a documented
benchmark shows cold-start within budget; threat model written.

---

## M3 — Nix mode: run a prebuilt store path

Deliver the *second* run-mode promised in the vision, while it's still cheap.
This is **not** the config/module layer (that's last) — just teaching the engine
to consume a prebuilt `{spec, rootfs}` store path, plus a minimal builder.

**Forces:** the build-time half of synthetic generation, the store-path input
shape, parity between the two run-modes.

- [ ] `pierogi run /nix/store/…-sandbox` reads `./spec` + resolves `./rootfs`.
- [ ] A minimal Nix builder helper: write the spec, call `pierogi synth-etc` to
      bake `/etc` into the rootfs closure, link it. No options schema yet.
- [ ] Confirm a nix-mode sandbox and the wrap-mode equivalent behave identically
      (same engine path past "obtain spec & rootfs").

**Acceptance:** a `nix build` produces a sandbox store path that
`pierogi run <path>` launches into a confined shell matching M1's behavior — with
`/etc` baked at build time, not generated at runtime.

---

## M4 — D-Bus proxy layer

The first managed proxy and the first "talks to the outside world through a
filter" feature.

**Forces:** the proxy supervisor, socket-binding-into-sandbox, the
`dbus` section of the spec, ready-synchronization before `exec`.

- [ ] Supervise a filtering D-Bus proxy (xdg-dbus-proxy-style policy).
- [ ] Session + system bus, independent allowlists, deny-all default.
- [ ] Proxy lifecycle tied to sandbox lifecycle (no orphans).
- [ ] Resolve "where the supervisor lives after the Engine hands off."
- [ ] Spec `dbus` rules → proxy policy translation + tests.

**Acceptance:** a sandboxed app can send a desktop notification *only* because
an explicit `talk` rule allows it; with the rule removed, the call is denied at
the proxy, and the raw bus socket is never present in the sandbox.

---

## M5 — Sandboxed browser  🦊

First heavyweight real-world target. GUI + audio + portals + D-Bus all at once.

**Forces:** GUI socket proxying (Wayland/X), PipeWire, portal D-Bus rules, and
a reusable "browser" bundle of spec defaults.

- [ ] Wayland (and/or Xwayland) socket exposure via a filtered/proxied socket,
      not a raw bind.
- [ ] PipeWire / audio access as a declared capability.
- [ ] xdg-desktop-portal D-Bus rules for the browser.
- [ ] GPU device access (`/dev/dri`) as a narrow, explicit device allowance.
- [ ] Persistent profile/state dir as an explicit writable mount.
- [ ] A reusable browser spec bundle — expressed as wrap-mode flags/defaults or
      a hand-written nix-mode store path. (Promoting this into a tidy
      `profiles.browser` *module* waits for the deferred config layer; the
      sandbox itself does not.)

**Acceptance:** Firefox (or Chromium) launches confined, renders accelerated,
plays audio, file dialogs work via portals — with no raw host socket, no host
`/etc`, no host `/home` beyond the declared profile dir.

---

## M6 — Steam & the messy long tail  🎮

The stress test. Steam wants a lot, behaves badly, and runs arbitrary
downloaded code — the ideal adversary for the "no escape hatches" thesis.

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

> **Status: intentionally last and left open.** Not part of the core
> deliverable, which is the runnable binary + its two run-modes. Shape this only
> once real usage (M1–M6) has shown what the sugar should crystallize.

Promote the hand-written nix-mode store paths into ergonomic, declarative,
composable modules — pure sugar over the `{spec, rootfs}` path M3 already runs.

**Forces:** options schema, `module → {spec, rootfs}` evaluation, profile
composition, OS integration.

- [ ] `pierogi.sandboxes.<name>` options schema evaluating to a nix-mode
      store path.
- [ ] Composable profiles (`base`, `network`, `gui`, `browser`, …), each with
      strict defaults, factoring out the bundles M5/M6 built by hand.
- [ ] NixOS / home-manager module: emit wrapper scripts and/or `systemd` units;
      lifecycle stays with `systemd`.
- [ ] Golden tests: `module eval → spec` matches the wrap-mode equivalent.

**Acceptance:** a checked-in `.nix` file defines a named sandbox whose entire
system view is auditable from that file; building it yields the same kind of
store path M3 runs, and the emitted spec matches the hand-written equivalent.

---

## Cross-cutting tracks (run alongside)

- **Performance budget.** Re-measure cold start each milestone; regressions are
  bugs. Target stays single-digit ms of engine overhead.
- **Threat model.** Living doc; updated whenever a milestone adds surface area.
- **Testing.** Synth-builder golden tests + spec/translation tests + namespace
  integration tests (in a VM/CI sandbox) + proxy policy tests.
- **Docs.** Keep `vision`/`architecture`/`roadmap` honest as reality diverges
  from plan.

## Sequencing rationale

Shell-run spike → proper shell → harden → nix-mode → D-Bus → browser → Steam →
config layer is deliberate:

- **The spike (M0) leads** because in-process runtime embedding is the biggest
  unknown: if we can't drive youki/libcrun to namespace + `exec` a shell from
  Rust, every later milestone is built on sand. Prove it crudely before
  investing in synth or strict defaults.
- Synth + strictness (M1) come *after* the spike, not before — generating a
  perfect `/etc` is pointless until a shell actually launches.
- The hard security work (M2) lands **before** we widen the attack surface
  (M5/M6), so every later target inherits a hardened base.
- Nix-mode (M3) comes early and cheap because it completes the "two run-modes"
  promise and exercises the build-time half of synthetic generation — but it's
  just *running* a store path, not the module sugar.
- The **config/module layer is last (M7) and open** on purpose: it's
  convenience over a mechanism that already works, so it should be shaped by
  what M1–M6 actually needed, not guessed up front. The deliverable is a
  runnable binary long before M7.
