# M1 nix-mode artifact — a Wayland clipboard probe (sealed-rootfs).
#
# A graphical-protocol follow-on to `wayland.nix`: it reuses that file's Wayland
# socket routing verbatim (baked `/run/user/1000/wayland-0` mountpoint + spec env
# + a runtime host bind in the wrapper), but exercises a specific Wayland
# **protocol** rather than just the socket — the **data-device / selection**
# machinery behind the clipboard. `wl-paste` (wl-clipboard) connects, reads the
# current selection's offered MIME types, and exits.
#
# `--list-types` is the probe: it prints the types on the clipboard (proving a
# completed selection round trip), or — if nothing is copied — exits with
# "Nothing is copied", which STILL proves the client connected and spoke the
# protocol (a socket-routing failure looks completely different: "failed to
# connect to display"). `/bin` also carries `wl-copy`, so a full round trip is
# `nix run .#clipboard-probe -- <flags>` after a `wl-copy` on the host.
#
# Caveat, like `grim`: `wl-paste` reads the selection via the `wlr-data-control`
# protocol where available (wlroots compositors); on GNOME/KDE, which lack it,
# clipboard access is focus-gated and this headless probe may see nothing.
{ pkgs, pierogi }:

let
  etcHostname = pkgs.writeText "hostname" "pierogi\n";

  lib = import ../lib.nix { inherit pkgs pierogi; };

  # root: /bin farm (wl-clipboard → `wl-paste`, `wl-copy`), hand-rolled /etc,
  # base mountpoint dirs + the `/run/user/1000` Wayland socket dir. The
  # `wayland-0` **file** is the socket bind target, so it's `touch`ed.
  root = pkgs.runCommand "pierogi-clipboard-root" { } ''
    source ${lib.buildLib}
    mkdir -p $out/bin $out/etc

    bin_farm ${pkgs.lib.escapeShellArgs [ pkgs.wl-clipboard pkgs.coreutils ]}
    ln -s ${etcHostname} $out/etc/hostname

    bake_dirs ${pkgs.lib.escapeShellArgs lib.defaultMountDirs} /run/user/1000
    touch $out/run/user/1000/wayland-0
  '';

  profiles = import ../profiles.nix { };

  unresolvedSpec = pkgs.lib.recursiveUpdate profiles.base {
    process = {
      executable = "wl-paste";
      # Print the MIME types offered by the current selection, then exit.
      # args = [ "--list-types" ];
      terminal = false;
      env = {
        PATH = "/bin";
        # Wayland socket routing — the baked mountpoint path (wrapper binds the
        # host's real socket onto it).
        XDG_RUNTIME_DIR = "/run/user/1000";
        WAYLAND_DISPLAY = "wayland-0";
      };
    };
    mounts = lib.defaultMounts;
  };

  resolved = lib.resolveSpec {
    name = "pierogi-clipboard-probe";
    spec = unresolvedSpec;
    root = root;
  };

# The runnable artifact — the same socket-discovery wrapper as `wayland.nix`:
# find the host's Wayland socket, bind it onto the baked mountpoint, exec
# `pierogi run wl-paste`. Trailing `"$@"` overrides args (the executable stays
# the sealed `wl-paste`).
in
pkgs.writeShellScriptBin "pierogi-clipboard-probe" ''
  set -eu

  : "''${XDG_RUNTIME_DIR:?not set — is a Wayland session running?}"
  display="''${WAYLAND_DISPLAY:-wayland-0}"
  case "$display" in
    /*) sock="$display" ;;
    *)  sock="$XDG_RUNTIME_DIR/$display" ;;
  esac
  if [ ! -S "$sock" ]; then
    echo "pierogi-clipboard-probe: no Wayland socket at $sock" >&2
    exit 1
  fi

  exec ${pierogi}/bin/pierogi run ${resolved} \
    --mount "host:$sock:/run/user/1000/wayland-0:rw" \
    -- "$@"
''
