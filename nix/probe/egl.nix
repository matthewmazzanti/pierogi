# M1 nix-mode artifact — an EGL/OpenGL probe (sealed-rootfs edition).
#
# The non-graphical GL isolator. It runs `eglinfo` (mesa-demos), which opens the
# GPU render node directly (the surfaceless / device EGL platforms — **no Wayland
# socket**) and dumps the EGL/GLES vendor + renderer strings. That renderer
# string is the pass/fail: a real driver (e.g. "AMD … (radeonsi)") means the GL
# userspace resolved the hardware; "llvmpipe" means it silently fell back to
# software.
#
# Where `ghostty.nix` exercises GL *through* a GTK Wayland window (display + GL
# entangled), this isolates the GL-driver axis alone — same render node + `/sys`
# + Mesa plumbing, no compositor. See `lib.gpu` (nix/lib.nix) for that shared plumbing and
# `bash.nix` for the sealed-rootfs pipeline itself.
{ pkgs, pierogi }:

let
  # Hand-rolled /etc — just a hostname. `eglinfo` needs no login identity.
  etcHostname = pkgs.writeText "hostname" "pierogi\n";

  # Light-touch resolution utilities (build shell + resolveSpec).
  lib = import ../lib.nix { inherit pkgs pierogi; };
  # Shared render-node / Mesa plumbing (mounts, seed, per-API driver env).
  gpu = lib.gpu;

  # root: the tree the author designs — a /bin farm (mesa-demos, carrying
  # `eglinfo`), a hand-rolled /etc, and the base + GPU mountpoint dirs. It IS the
  # `/` mount; `lib.resolveSpec` derives the closure from its references, plus
  # `gpu.seed` (Mesa) as an extraSeed — libglvnd `dlopen`s the vendor driver at
  # runtime via the EGL vendor JSON, so mesa-demos doesn't reference it.
  root = pkgs.runCommand "pierogi-egl-root" { } ''
    source ${lib.buildLib}
    mkdir -p $out/bin $out/etc

    bin_farm ${pkgs.lib.escapeShellArg pkgs.mesa-demos}
    ln -s ${etcHostname} $out/etc/hostname

    # Baked mountpoints — the base dirs (proc/dev/tmp + /nix/store) plus `/sys`
    # (lib.gpu's sysfs mountpoint). The render node and /dev/shm mount onto the
    # runtime /dev tmpfs, so they need no baked dir.
    bake_dirs ${pkgs.lib.escapeShellArgs (lib.defaultMountDirs ++ gpu.mountDirs)}
  '';

  # The default-closed floor, as spec fields (see nix/profiles.nix). GL on a
  # render node needs no capabilities and no network, so the base is untouched.
  profiles = import ../profiles.nix { };

  unresolvedSpec = pkgs.lib.recursiveUpdate profiles.base {
    process = {
      executable = "eglinfo";
      args = [ ];
      # One-shot: it probes each EGL platform, prints, and exits.
      terminal = false;
      env = {
        PATH = "/bin";
        # Mesa's on-disk shader cache defaults to $HOME/.cache — unset HOME makes
        # that `//.cache`, which fails on the read-only root. Route it to the
        # writable /tmp tmpfs (harmless for a one-shot probe, silences the error).
        XDG_CACHE_HOME = "/tmp";
      } // gpu.eglEnv;
    };
    # The base trio (proc/dev/tmp) plus the GPU mounts (/sys, render node,
    # /dev/shm) — the /dev-nested pair follow the /dev tmpfs.
    mounts = lib.defaultMounts ++ gpu.mounts;
  };

  resolved = lib.resolveSpec {
    name = "pierogi-egl-probe";
    spec = unresolvedSpec;
    root = root;
    extraSeeds = [ gpu.seed ];
  };

# The runnable artifact — a thin wrapper that pre-checks the render node (a
# sealed placement, so we only verify it exists for a clear error) then execs
# `pierogi run eglinfo`. No Wayland socket to discover. Trailing `"$@"` is the
# args-only override (e.g. `nix run .#egl-probe -- -B` for brief output).
in
pkgs.writeShellScriptBin "pierogi-egl-probe" ''
  set -eu

  if [ ! -e /dev/dri/renderD128 ]; then
    echo "pierogi-egl-probe: no render node at /dev/dri/renderD128 — is this an AMD/Intel (Mesa) GPU host?" >&2
    exit 1
  fi

  exec ${pierogi}/bin/pierogi run ${resolved} -- "$@"
''
