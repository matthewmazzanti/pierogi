# M1 nix-mode artifact — a Wayland-routing sandbox (sealed-rootfs edition).
#
# Sibling of `bash-sandbox.nix`: same "input derivation" pipeline (explicit
# `rootTree` build → `lib.sealAndResolve` bakes the /nix/store topology from
# rootTree's references and stamps `closure.resolved`), so see that file for the
# full rationale. What's different here is the *point*: it tests **routing a host
# Wayland socket into the container**.
#
# The runnable executable is `wayland-info` (from wayland-utils) — a one-shot,
# non-graphical client that connects to `$WAYLAND_DISPLAY`, dumps the
# compositor's globals, and exits 0. It's the cleanest pass/fail signal that the
# socket + env plumbing is correct; a graphical client (foot, weston-flower) is
# a heavier follow-on that reuses the exact same routing.
#
# Routing, under the sealed model, is three pieces:
#   * a **baked mountpoint** file at `/run/user/1000/wayland-0` (+ its parent
#     dir) — the read-only rootfs can't create it at launch, so it's frozen in;
#   * **spec env** `XDG_RUNTIME_DIR=/run/user/1000` + `WAYLAND_DISPLAY=wayland-0`,
#     so the client looks for the socket at exactly that baked path;
#   * a **runtime host bind** of the caller's real socket onto that mountpoint,
#     added by the wrapper (`--mount host:…:/run/user/1000/wayland-0:rw`). The
#     host's display number never leaks in — inside it's always `wayland-0`.
#
# The private network namespace from `profiles.base` is fine: Wayland is an
# AF_UNIX socket routed via the filesystem, not the network.
{ pkgs, pierogi }:

let
  # Hand-rolled /etc — just a hostname. `wayland-info` needs no login identity
  # (libwayland finds the socket via $XDG_RUNTIME_DIR, set below), so there's no
  # passwd/group here, unlike the interactive bash sandbox.
  etcHostname = pkgs.writeText "hostname" ''
    pierogi
  '';

  # Light-touch resolution utilities (store-mount baking shell + seal/resolve).
  lib = import ./lib.nix { inherit pkgs pierogi; };

  # rootTree: the tree the author designs — a /bin farm, hand-rolled /etc, base
  # mountpoint dirs (incl. the Wayland socket file). `lib.sealRoot` bakes the
  # `/nix/store` topology from rootTree's actual references (wayland-utils + its
  # closure), so there is no dep list.
  rootTree = pkgs.runCommand "pierogi-wayland-root-tree" { } ''
    source ${lib.buildLib}
    mkdir -p $out/bin $out/etc

    # /bin symlink farm — see `bin_farm` in build-lib.sh. Absolute targets into
    # /nix/store, resolved inside the sandbox because the closure is bound there.
    bin_farm ${pkgs.lib.escapeShellArg pkgs.wayland-utils}

    # Hand-rolled /etc — a symlink into the store (a closure member, so it
    # resolves in-sandbox and folds into `root`'s references).
    ln -s ${etcHostname} $out/etc/hostname

    # Baked mountpoints — the base trio's dirs (proc/dev/tmp) + the
    # `/run/user/1000` Wayland socket dir. The `wayland-0` **file** is the
    # socket's bind target (a socket binds onto a file), so it's `touch`ed, not a
    # dir. The read-only rootfs can't grow these at launch — hence baking them.
    bake_dirs ${pkgs.lib.escapeShellArgs lib.defaultMountDirs} /run/user/1000
    touch $out/run/user/1000/wayland-0
    # The `/nix/store` mountpoints are NOT baked here — `lib.sealRoot` owns those.
  '';

  # The default-closed floor, as spec fields (see nix/profiles.nix).
  profiles = import ./profiles.nix { };

  # The sources spec: `profiles.base` merged with this sandbox's fields.
  # `root.path` + `closure` are injected by `lib.sealAndResolve`.
  unresolvedSpec = pkgs.lib.recursiveUpdate profiles.base {
    process = {
      executable = "wayland-info";
      args = [ ];
      # One-shot: it prints and exits, so no tty relay.
      terminal = false;
      env = {
        PATH = "/bin";
        # The client looks here for the socket — the baked mountpoint path. The
        # wrapper binds the host's real socket onto it (see `runnable`).
        XDG_RUNTIME_DIR = "/run/user/1000";
        WAYLAND_DISPLAY = "wayland-0";
      };
    };
    # Placements — the common base trio (proc/dev/tmp), lifted out of the engine
    # into the producer layer (see nix/lib.nix). Verbatim OCI mounts.
    mounts = lib.defaultMounts;
    # `root.path` + `closure` are injected by `lib.sealAndResolve` (below).
  };

  # Seal `rootTree` into the rootfs and resolve against it — bakes the /nix/store
  # topology from rootTree's actual references, no dep list. See bash-sandbox.nix.
  resolved = lib.sealAndResolve {
    name = "pierogi-wayland-sandbox";
    spec = unresolvedSpec;
    rootTree = rootTree;
  };

in
{
  # The built spec (closure stamped in). Internal: the input the runner execs
  # against.
  inherit resolved;

  # The runnable artifact — a wrapper that discovers the host Wayland socket,
  # binds it into the sandbox at the baked mountpoint, then execs `pierogi run`.
  # The compositor's socket is the host unix socket at
  # `$XDG_RUNTIME_DIR/$WAYLAND_DISPLAY`; inside it's always `/run/user/1000/
  # wayland-0` (the baked path + spec env), so the host's display name never
  # leaks into the image. Trailing `"$@"` is the args-only override, forwarded to
  # `wayland-info` (e.g. `--tag` / a specific interface); the executable is
  # sealed by the spec.
  runnable = pkgs.writeShellScriptBin "pierogi-wayland-sandbox" ''
    set -eu

    # Fail loud if there's no session to route in — a clearer message than the
    # bind failing deeper in crun.
    : "''${XDG_RUNTIME_DIR:?not set — is a Wayland session running?}"
    display="''${WAYLAND_DISPLAY:-wayland-0}"
    case "$display" in
      /*) sock="$display" ;;
      *)  sock="$XDG_RUNTIME_DIR/$display" ;;
    esac
    if [ ! -S "$sock" ]; then
      echo "pierogi-wayland-sandbox: no Wayland socket at $sock" >&2
      exit 1
    fi

    exec ${pierogi}/bin/pierogi run ${resolved} \
      --mount "host:$sock:/run/user/1000/wayland-0:rw" \
      -- "$@"
  '';
}
