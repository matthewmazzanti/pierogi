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
#   * the shared GPU-userspace exposure (`gpu`): render-node + `/sys` + Mesa
#     plumbing the egl / vulkan / vainfo probes share.
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
    proc = {
      destination = "/proc";
      type = "proc";
      source = "proc";
    };
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
    ${pierogi}/bin/pierogi resolve ${file} \
      --graph "$NIX_ATTRS_JSON_FILE" \
      > "''${outputs[out]}"
  '';

  # Shared GPU exposure for the "GPU userspace" probe trilogy (egl / vulkan /
  # vainfo) — the render-node + `/sys` + Mesa plumbing a hardware-accelerated
  # client needs, minus any display. A probe binds `gpu = lib.gpu;` and appends
  # `gpu.mounts` / `gpu.mountDirs` to the `default*` bundles above.
  #
  # (ghostty.nix predates this and inlines equivalent `renderNode` / `sysfs` /
  # `devShm` definitions plus a Mesa EGL env; it could adopt this later.)
  #
  # The three probes all run WITHOUT a Wayland socket — they open the render node
  # directly, so they isolate "does the GPU userspace resolve the vendor driver"
  # from "does display routing work" (which `wayland.nix` already covers). Each API
  # discovers Mesa's driver a DIFFERENT way — that difference IS the axis each
  # probe exercises:
  #   * EGL/GL — libglvnd reads the EGL vendor JSON (`__EGL_VENDOR_LIBRARY_DIRS`);
  #   * Vulkan — the loader reads an ICD manifest (`VK_ICD_FILENAMES`);
  #   * VA-API — libva `dlopen`s `<driver>_drv_video.so` (`LIBVA_DRIVERS_PATH`).
  # Mesa is therefore never in a client's build closure — it's an `extraSeed`.
  #
  # AMD-specific (renderD128 + radeonsi / radeon_icd), matching ghostty's existing
  # assumption. An Intel/other host would point these at its own node + driver.
  gpu = let mesa = pkgs.mesa; in rec {
    # Mesa — the GL/EGL/Vulkan/VA userspace driver, seeded explicitly (a client
    # `dlopen`s it at runtime via the manifests below, so it's NOT a build-closure
    # reference). Pass as `extraSeeds = [ gpu.seed ]` to `resolveSpec`.
    seed = mesa;

    # The AMD render node, bound in as a **device**. Options omit `nodev` — a
    # device node bound MS_NODEV is inert. Lands on the runtime `/dev` tmpfs, so
    # crun creates the target at mount time (no baked dir). The node is mode 0666
    # on the host, so the 1:1-mapped invoker opens it with no capability juggling.
    renderNode = {
      destination = "/dev/dri/renderD128";
      source = "/dev/dri/renderD128";
      type = "bind";
      options = [ "bind" "rw" "nosuid" ];
    };

    # A `/dev/shm` tmpfs — GPU stacks commonly `shm_open`. Mounts onto the `/dev`
    # tmpfs at runtime, so it needs no baked dir.
    devShm = {
      destination = "/dev/shm";
      type = "tmpfs";
      source = "tmpfs";
      options = [ "nosuid" "nodev" "mode=1777" ];
    };

    # A read-only sysfs — Mesa's loader resolves the render node to its driver via
    # libdrm, which reads the node's PCI id under `/sys`. Without it, GL/VA fall
    # back to software (llvmpipe) or fail outright. `profiles.base` already masks
    # the sensitive `/sys` bits, which activate once `/sys` is mounted. Needs a
    # baked mountpoint (see `mountDirs`).
    sysfs = {
      destination = "/sys";
      type = "sysfs";
      source = "sysfs";
      options = [ "nosuid" "noexec" "nodev" "ro" ];
    };

    # The mount bundle, ordered so the `/dev`-nested pair follow their parent (the
    # `/dev` tmpfs in `defaultMounts`). Append to `defaultMounts`.
    mounts = [ sysfs renderNode devShm ];

    # The mountpoint dirs the sealed root must bake for these: just `/sys`
    # (renderNode + devShm mount onto the runtime `/dev` tmpfs, so no baked dir).
    # Append to `defaultMountDirs` in the root's `bake_dirs` call.
    mountDirs = [ "/sys" ];

    # Per-API driver-discovery env fragments — merge into `process.env`. Each
    # names Mesa by absolute store path, which is why Mesa must be seeded.
    eglEnv = {
      __EGL_VENDOR_LIBRARY_DIRS = "${mesa}/share/glvnd/egl_vendor.d";
      LIBGL_DRIVERS_PATH = "${mesa}/lib/dri";
    };
    # The AMD ICD **only**. Pointing at the whole `icd.d` would let lavapipe
    # (`lvp`, software) enumerate a device and mask a hardware failure — defeating
    # the "did we get the GPU" pass/fail signal.
    vulkanEnv = {
      VK_ICD_FILENAMES = "${mesa}/share/vulkan/icd.d/radeon_icd.x86_64.json";
    };
    vaEnv = {
      LIBVA_DRIVERS_PATH = "${mesa}/lib/dri";
      LIBVA_DRIVER_NAME = "radeonsi";
    };
  };
}
