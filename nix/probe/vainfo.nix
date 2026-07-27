# M1 nix-mode artifact — a VA-API (hardware video) probe (sealed-rootfs).
#
# The third GPU-userspace probe, alongside `egl.nix` (GL) and `vulkan.nix`
# (Vulkan). Same render node + `/sys` + Mesa, **no Wayland** — but it exercises
# the **media codec** path, a discovery mechanism distinct from both siblings:
# `vainfo` (libva-utils) opens the render node via libva, which `dlopen`s Mesa's
# `radeonsi_drv_video.so` from `LIBVA_DRIVERS_PATH`. The dump lists the driver
# version and the supported entrypoints (H.264/HEVC/AV1 decode/encode …) — the
# pass/fail is whether it reports the real VA driver versus failing to load one.
#
# So the trilogy hits the *same* exposed GPU three ways — glvnd vendor JSON (GL),
# ICD manifest (Vulkan), libva driver `.so` (video) — which is the point: each
# isolates one userspace-driver-resolution axis. See `lib.gpu` (nix/lib.nix) for the shared
# plumbing and `bash.nix` for the sealed-rootfs pipeline.
{ pkgs, pierogi }:

let
  etcHostname = pkgs.writeText "hostname" "pierogi\n";

  lib = import ../lib.nix { inherit pkgs pierogi; };
  gpu = lib.gpu;

  # root: /bin farm (libva-utils → `vainfo`), hand-rolled /etc, base + GPU
  # mountpoint dirs. Mesa (`gpu.seed`) is an extraSeed — libva `dlopen`s the VA
  # driver at runtime, so libva-utils doesn't reference it.
  root = pkgs.runCommand "pierogi-vainfo-root" { } ''
    source ${lib.buildLib}
    mkdir -p $out/bin $out/etc

    bin_farm ${pkgs.lib.escapeShellArg pkgs.libva-utils}
    ln -s ${etcHostname} $out/etc/hostname

    bake_dirs ${pkgs.lib.escapeShellArgs (lib.defaultMountDirs ++ gpu.mountDirs)}
  '';

  profiles = import ../profiles.nix { };

  unresolvedSpec = pkgs.lib.recursiveUpdate profiles.base {
    process = {
      executable = "vainfo";
      # Pin the DRM backend at the render node — `vainfo` otherwise probes X11
      # first (no display here). `--display drm --device <node>` goes straight to
      # the exposed node.
      args = [ "--display" "drm" "--device" "/dev/dri/renderD128" ];
      terminal = false;
      env = {
        PATH = "/bin";
        # Route Mesa's shader cache to the writable /tmp tmpfs (default
        # $HOME/.cache is `//.cache` with HOME unset → fails on the RO root).
        XDG_CACHE_HOME = "/tmp";
      } // gpu.vaEnv;
    };
    mounts = lib.defaultMounts ++ gpu.mounts;
  };

  resolved = lib.resolveSpec {
    name = "pierogi-vainfo-probe";
    spec = unresolvedSpec;
    root = root;
    extraSeeds = [ gpu.seed ];
  };

# The runnable artifact — render-node pre-check, then `pierogi run vainfo`. No
# Wayland socket. Trailing `"$@"` overrides args (the sealed default targets the
# render node via DRM).
in
pkgs.writeShellScriptBin "pierogi-vainfo-probe" ''
  set -eu

  if [ ! -e /dev/dri/renderD128 ]; then
    echo "pierogi-vainfo-probe: no render node at /dev/dri/renderD128 — is this an AMD/Intel (Mesa) GPU host?" >&2
    exit 1
  fi

  exec ${pierogi}/bin/pierogi run ${resolved} -- "$@"
''
