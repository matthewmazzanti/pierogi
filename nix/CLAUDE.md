# CLAUDE.md — nix/ (the producer)

**This is where policy lives.** The engine (`src/`) is a pure realizer that adds
nothing; every default, floor, and "just works" affordance is composed *into a
spec here* before the engine ever sees it. If you're adding a default, hardening,
or a capability affordance, it belongs in this directory — not in Rust.

Each file has a thorough header; this is the map, the build pipeline, and the
recipes.

## Layout

```
profiles.nix       profiles.base — the default-closed FLOOR as plain spec fields
                   (ro root, dropped caps, nnp, masks, private propagation).
                   A sandbox is hardened *because it merged this*, visibly, not
                   because the engine imposes it. Compose with recursiveUpdate.
lib.nix            light-touch utilities: base mounts (proc/dev/tmp) + the
                   mountpoint dirs to bake, `resolveSpec` (the build pipeline),
                   and shared `gpu` exposure (render node + /sys + Mesa seeding).
build-lib.sh       sourceable shell for root runCommands: `bin_farm` (/bin
                   symlink farm), `bake_dirs` (mountpoint dirs).
sandbox/           runnable sealed-rootfs sandboxes: bash, wayland, foot, ghostty.
                   Each evaluates to a writeShellScriptBin that execs `pierogi run`.
probe/             minimal single-axis testers — each isolates ONE config axis
                   with the smallest tool giving a clean pass/fail (egl, vulkan,
                   vainfo, fontconfig, dbus, pipewire, clipboard, screenshot).
terminal-tools.nix shared terminal userland bits.
```

## The build pipeline (`lib.resolveSpec`)

A sandbox author builds a **`root`** tree (a `/bin` farm, a hand-rolled `/etc`,
and the base mountpoint dirs incl. an empty `/nix/store`), then calls
`resolveSpec { name; spec; root; extraSeeds ? [] }`:

1. injects `root.path = "${root}"` and `closure.seeds = [root] ++ extraSeeds`;
2. builds under `__structuredAttrs` with `exportReferencesGraph.closure = seeds`;
3. runs `pierogi resolve --graph $NIX_ATTRS_JSON_FILE`, which traverses that
   graph and stamps `closure.resolved`.

Key consequences to preserve:

- **No hand-maintained dep list.** The closure is whatever `root` *actually
  references* (its `/bin` + `/etc` symlink targets). Adding a tool or an `/etc`
  file is a single edit to the `root` build — never a separate dependency list.
- **`seeds` drives both sides**, so the graph Nix expands and the seeds pierogi
  traverses can't drift. Every bound path is provably a member of `closure(root)`.
- **The store topology is a runtime tmpfs** (the engine mounts `/nix/store` and
  identity-binds the closure). Nothing about store layout is baked or filtered
  here — `root` is used verbatim as the `/` mount.
- **No `nix-store` query runs at build** (the build sandbox has no daemon); the
  graph comes entirely from `exportReferencesGraph`.

## Recipes

**Add a sandbox** — build a `root` (use `build-lib.sh`'s `bin_farm`/`bake_dirs`),
compose a spec from `profiles.base` via `recursiveUpdate`, pull the base mounts
from `lib.mounts`/`lib.defaultMounts` (and bake their `defaultMountDirs`), then
`lib.resolveSpec`. Wrap the result in a `writeShellScriptBin` that execs
`pierogi run`. Add it to `packages` in `flake.nix`. Copy the closest existing
`sandbox/*.nix` as the template.

**Add a probe** — same shape, kept minimal: one axis, the smallest tool with a
clean pass/fail. Say in the header *what would silently pass if the axis were
broken* (e.g. `vulkanEnv` names only the AMD ICD so lavapipe/software can't
enumerate a device and mask a hardware failure).

**Expose the GPU** — bind `gpu = lib.gpu`, append `gpu.mounts`/`gpu.mountDirs` to
the base bundles, merge the right `*Env` fragment, and pass
`extraSeeds = [ gpu.seed ]` (Mesa is `dlopen`'d at runtime via the manifests, so
it is NOT a build-closure reference — it must be seeded explicitly, or you get a
silent software fallback). See `lib.nix`'s `gpu` header for the per-API
discovery differences (EGL vendor JSON vs Vulkan ICD vs libva `.so`).

## Conventions

- Plain functions a sandbox imports and calls — deliberately **not** a module
  system.
- Compose onto `profiles.base` with `pkgs.lib.recursiveUpdate`; every value is a
  faithful OCI field the engine passes straight through.
- Derive placement sets from one source (e.g. `defaultMountDirs` is derived from
  the mounts) so they can't drift.
- Reference `docs/control-surface.md` for whether a new knob is NEUTRAL or OPEN,
  and keep every loosening explicit and on the record in the spec.
