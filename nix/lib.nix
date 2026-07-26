# pierogi nix-lib — light-touch utilities for the sealed-rootfs "final
# resolution" layer. What a sandbox factors out here:
#
#   * the sourceable build shell (`buildLib` → build-lib.sh): `bin_farm` for the
#     `/bin` symlink farm, `bake_dirs` for mountpoint dirs. Sourced by a root
#     `runCommand`.
#   * the producer-lifted base mounts (`mounts` / `defaultMounts`) and the
#     mountpoint dirs they need baked (`defaultMountDirs`).
#   * resolving the spec — `exportReferencesGraph` + `pierogi resolve --graph`
#     (`resolveSpec`), byte-identical across sandboxes.
#
# What is deliberately NOT here anymore: baking a `/nix/store/<hash>` mountpoint
# tree into the root. The store topology is provided at *runtime* — the engine
# mounts a tmpfs at `/nix/store` and the closure identity-binds onto it (see
# src/oci.rs). So the root is just the tree the author builds (a `/bin` farm,
# `/etc`, base mountpoint dirs including an empty `/nix/store`); its Nix closure
# is exactly what it references, with no baked-in store mountpoints to scan,
# filter, or self-reference. `resolveSpec` takes that root directly.
#
# Deliberately not a module system: plain functions a sandbox imports and calls.
{ pkgs, pierogi }:

rec {
  # The sourceable shell library (`bin_farm`, `bake_dirs`). `source ${lib.buildLib}`
  # in a root `runCommand`, then call them. Referenced as a path → copied to the
  # store, zero Nix-string escaping.
  buildLib = ./build-lib.sh;

  # The producer-lifted base mounts, as individual named OCI mount entries —
  # decomposed so a sandbox takes exactly what it needs. `/proc`, `/dev`, and
  # `/tmp` were lifted out of the engine (see docs/control-surface.md); each is
  # a verbatim OCI mount the engine passes straight through. `/dev` carries no
  # `nodev` (device nodes live there); `devpts` is a foot-style extra (the engine
  # auto-adds one only for `terminal`).
  mounts = {
    proc = { destination = "/proc"; type = "proc"; source = "proc"; };
    dev = {
      destination = "/dev";
      type = "tmpfs";
      source = "tmpfs";
      options = [ "nosuid" "strictatime" "mode=755" "size=65536k" ];
    };
    tmp = {
      destination = "/tmp";
      type = "tmpfs";
      options = [ "nosuid" "nodev" "mode=1777" ];
    };
    devpts = {
      destination = "/dev/pts";
      type = "devpts";
      source = "devpts";
      options = [ "nosuid" "noexec" "newinstance" "ptmxmode=0666" "mode=0620" ];
    };
  };

  # The common trio every sandbox mounts, as a ready-to-use list (bundle sugar
  # over `mounts`). All three bind onto the read-only rootfs, so each needs a
  # baked mountpoint dir — see `defaultMountDirs`. (devpts is deliberately NOT
  # here: it mounts onto the /dev tmpfs at runtime, needs no baked dir, and only
  # some sandboxes want it.)
  defaultMounts = [ mounts.proc mounts.dev mounts.tmp ];

  # The mountpoint dirs a sealed root must bake: the `defaultMounts` destinations
  # (proc/dev/tmp) plus `/nix/store` — the mountpoint for the engine's store
  # tmpfs (src/oci.rs). `/nix/store` isn't a `defaultMounts` entry because the
  # engine injects that tmpfs itself, not the producer; but its mountpoint dir
  # still has to exist in the read-only root, so every root bakes it here. One
  # source of truth: derive from the mounts so the placement set can't drift.
  defaultMountDirs = (map (m: m.destination) defaultMounts) ++ [ "/nix/store" ];

  # Resolve a sandbox: given the `root` tree and its unresolved `spec`, inject
  # `root.path`, seed the closure from `[ root ] ++ extraSeeds`, and stamp
  # `closure.resolved` by traversing `exportReferencesGraph` with `pierogi resolve`.
  #
  # `seeds` drives BOTH sides so they can't drift: they become the
  # `exportReferencesGraph` roots Nix expands AND the spec's `closure.seeds` that
  # `pierogi resolve` traverses from — so every bound store path is exactly a
  # member of `closure(root)`. The closure comes from what `root` *actually
  # references* (its /bin + /etc symlink targets), so there is no hand-maintained
  # dep list: adding a tool or an /etc file is a single edit to the root build.
  # `extraSeeds` covers sources `root` doesn't itself reference (e.g. an overlay
  # lower). The store *topology* is a runtime tmpfs (src/oci.rs), so nothing is
  # baked or filtered here — `root` is used verbatim as the `/` mount.
  resolveSpec = { name, spec, root, extraSeeds ? [ ] }: let
    seeds = [ root ] ++ extraSeeds;
    spec' = pkgs.lib.recursiveUpdate spec {
      root.path = "${root}";
      closure.seeds = map toString seeds;
    };
    file = pkgs.writeText "${name}.unresolved.json" (builtins.toJSON spec');
  in pkgs.runCommand "${name}.json" {
    __structuredAttrs = true;
    exportReferencesGraph.closure = seeds;
  } ''
    ${pierogi}/bin/pierogi resolve ${file} --graph "$NIX_ATTRS_JSON_FILE" > "''${outputs[out]}"
  '';
}
