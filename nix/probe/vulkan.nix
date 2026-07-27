# M1 nix-mode artifact — a Vulkan probe (sealed-rootfs edition).
#
# The GL sibling `egl.nix` in a different key: same render node + `/sys` + Mesa,
# **no Wayland**, but it exercises the **Vulkan** loader path, which is a wholly
# separate discovery mechanism. Where EGL finds its driver through libglvnd's
# vendor JSON, Vulkan's loader (`vulkan-loader`, pulled in by `vulkan-tools`)
# reads an **ICD manifest** named by `VK_ICD_FILENAMES` and `dlopen`s the
# `libvulkan_radeon.so` it points at. `vulkaninfo` enumerates the physical
# devices and exits — the pass/fail is whether it lists the real GPU (an AMD
# "Radeon …") versus nothing / a software device.
#
# It's headless by design: Vulkan needs no surface to enumerate devices, so this
# probes "the ICD resolves the GPU" with zero display plumbing. See `lib.gpu` (nix/lib.nix)
# for the shared render-node/Mesa pieces and `bash.nix` for the pipeline.
{ pkgs, pierogi }:

let
  etcHostname = pkgs.writeText "hostname" "pierogi\n";

  lib = import ../lib.nix { inherit pkgs pierogi; };
  gpu = lib.gpu;

  # root: /bin farm (vulkan-tools → `vulkaninfo`), hand-rolled /etc, base + GPU
  # mountpoint dirs. Mesa (`gpu.seed`) is an extraSeed — the loader `dlopen`s the
  # ICD at runtime, so vulkan-tools doesn't reference it.
  root = pkgs.runCommand "pierogi-vulkan-root" { } ''
    source ${lib.buildLib}
    mkdir -p $out/bin $out/etc

    bin_farm ${pkgs.lib.escapeShellArg pkgs.vulkan-tools}
    ln -s ${etcHostname} $out/etc/hostname

    bake_dirs ${pkgs.lib.escapeShellArgs (lib.defaultMountDirs ++ gpu.mountDirs)}
  '';

  profiles = import ../profiles.nix { };

  unresolvedSpec = pkgs.lib.recursiveUpdate profiles.base {
    process = {
      executable = "vulkaninfo";
      # `--summary` keeps the dump to the device list — the signal — rather than
      # every format/limit. Override with args (e.g. `-- ` bare for the full dump).
      args = [ "--summary" ];
      terminal = false;
      env = {
        PATH = "/bin";
        # Route Mesa's shader cache to the writable /tmp tmpfs (default
        # $HOME/.cache is `//.cache` with HOME unset → fails on the RO root).
        XDG_CACHE_HOME = "/tmp";
      } // gpu.vulkanEnv;
    };
    mounts = lib.defaultMounts ++ gpu.mounts;
  };

  resolved = lib.resolveSpec {
    name = "pierogi-vulkan-probe";
    spec = unresolvedSpec;
    root = root;
    extraSeeds = [ gpu.seed ];
  };

# The runnable artifact — render-node pre-check, then `pierogi run vulkaninfo`.
# No Wayland socket. Trailing `"$@"` overrides args (e.g. `-- --summary` is the
# default; `nix run .#vulkan-probe --` with no args gives the full dump).
in
pkgs.writeShellScriptBin "pierogi-vulkan-probe" ''
  set -eu

  if [ ! -e /dev/dri/renderD128 ]; then
    echo "pierogi-vulkan-probe: no render node at /dev/dri/renderD128 — is this an AMD/Intel (Mesa) GPU host?" >&2
    exit 1
  fi

  exec ${pierogi}/bin/pierogi run ${resolved} -- "$@"
''
