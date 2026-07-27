# M1 nix-mode artifact — a Wayland screenshot probe (sealed-rootfs).
#
# Another Wayland-protocol probe over `wayland.nix`'s socket routing (baked
# `/run/user/1000/wayland-0` mountpoint + spec env + a runtime host bind in the
# wrapper). Where `clipboard.nix` exercises the selection protocol, this exercises
# **`wlr-screencopy`**: `grim` connects, copies the compositor's framebuffer, and
# writes a PNG. Beyond a routed socket the axis needs one thing: a **writable
# output path** — a host `state/` dir bound at `/root` (the cwd), so the captured
# PNG lands on the host and survives the container (like the bash/foot state bind).
#
# Compositor caveat, surfaced in the wrapper: `grim` is a **wlroots** tool
# (`wlr-screencopy`). On a wlroots compositor (Sway, Hyprland, river, …) it works;
# on GNOME/KDE, which don't implement that protocol, `grim` fails and the portal
# path (`xdg-desktop-portal`) would be needed instead — out of scope for this
# minimal probe. See `bash.nix` for the sealed-rootfs pipeline.
{ pkgs, pierogi }:

let
  etcHostname = pkgs.writeText "hostname" "pierogi\n";

  lib = import ../lib.nix { inherit pkgs pierogi; };

  # root: /bin farm (grim), hand-rolled /etc, base mountpoint dirs + the
  # `/run/user/1000` Wayland socket dir + `/root` (the writable output bind
  # target). The `wayland-0` **file** is the socket bind target, so it's `touch`ed.
  root = pkgs.runCommand "pierogi-screenshot-root" { } ''
    source ${lib.buildLib}
    mkdir -p $out/bin $out/etc

    bin_farm ${pkgs.lib.escapeShellArg pkgs.grim}
    ln -s ${etcHostname} $out/etc/hostname

    bake_dirs ${pkgs.lib.escapeShellArgs lib.defaultMountDirs} /root /run/user/1000
    touch $out/run/user/1000/wayland-0
  '';

  profiles = import ../profiles.nix { };

  unresolvedSpec = pkgs.lib.recursiveUpdate profiles.base {
    process = {
      executable = "grim";
      # Capture to a file relative to the cwd (/root, the persistent host bind),
      # so the PNG lands on the host.
      args = [ "screenshot.png" ];
      terminal = false;
      cwd = "/root";
      env = {
        PATH = "/bin";
        HOME = "/root";
        # Wayland socket routing — the baked mountpoint path (wrapper binds the
        # host's real socket onto it).
        XDG_RUNTIME_DIR = "/run/user/1000";
        WAYLAND_DISPLAY = "wayland-0";
      };
    };
    mounts = lib.defaultMounts;
  };

  resolved = lib.resolveSpec {
    name = "pierogi-screenshot-probe";
    spec = unresolvedSpec;
    root = root;
  };

# The runnable artifact — the `wayland.nix` socket-discovery wrapper plus a
# persistent output bind: find the host's Wayland socket, bind it onto the baked
# mountpoint, bind a local `state/` dir to `/root` for the PNG, exec `pierogi run
# grim`. Trailing `"$@"` overrides args (e.g. `-- -g "0,0 100x100" crop.png`).
in
pkgs.writeShellScriptBin "pierogi-screenshot-probe" ''
  set -eu

  : "''${XDG_RUNTIME_DIR:?not set — is a Wayland session running?}"
  display="''${WAYLAND_DISPLAY:-wayland-0}"
  case "$display" in
    /*) sock="$display" ;;
    *)  sock="$XDG_RUNTIME_DIR/$display" ;;
  esac
  if [ ! -S "$sock" ]; then
    echo "pierogi-screenshot-probe: no Wayland socket at $sock" >&2
    exit 1
  fi

  # Persistent output: a local (gitignored) dir bound to /root (the cwd), where
  # grim writes the PNG. Override the location with $PIEROGI_STATE.
  state="''${PIEROGI_STATE:-$PWD/state/screenshot-probe}"
  mkdir -p "$state"

  exec ${pierogi}/bin/pierogi run ${resolved} \
    --mount "host:$sock:/run/user/1000/wayland-0:rw" \
    --mount "host:$state:/root:rw" \
    -- "$@"
''
