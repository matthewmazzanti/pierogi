# M1 nix-mode artifact — a D-Bus session-bus probe (sealed-rootfs).
#
# The direct sibling of `wayland.nix`: the **same socket-routing pattern**, for a
# different local `AF_UNIX` service. The session bus is not network — it's a unix
# socket at `$DBUS_SESSION_BUS_ADDRESS` (typically `unix:path=/run/user/1000/
# bus`) — so routing it is exactly the Wayland trick: a **baked mountpoint** at
# `/run/user/1000/bus`, **spec env** pointing the client there, and a **runtime
# host bind** of the caller's real bus socket onto it (added by the wrapper). The
# host's socket path never leaks in — inside it's always `/run/user/1000/bus`.
#
# The runnable executable is `dbus-send` calling `org.freedesktop.DBus.ListNames`
# — a one-shot client that connects to the bus, prints the list of registered
# names, and exits. `busctl`-style richer queries are a follow-on over the same
# route.
#
# KNOWN LIMITATION — the credential wrinkle (this probe SURFACES it, doesn't
# solve it). Routing the socket is necessary but NOT sufficient: D-Bus's SASL
# `EXTERNAL` auth keys on the connecting uid, and `profiles.base` maps
# **container-0 → host-invoker** (rootless 1:1). So the client presents uid 0
# while the host `dbus-daemon` reads the socket peer as the real backing uid
# (e.g. 1000) — the handshake is rejected with "Did not receive a reply". This
# reproduces in a bare `unshare -Ur` with no pierogi involved, and flips to
# success under an *identity* map (`--map-users=1000:1000:1`), proving uid — not
# a missing `/etc/machine-id` — is the cause (machine-id only matters later, for
# the `GetMachineId` peer call, well after the connection dies). The two real
# fixes, both bigger than a minimal probe: an `xdg-dbus-proxy` on the host that
# authenticates as the real uid and relays (what flatpak/Firejail do), or subuid
# ranges to map both 0 and the invoker so the process can run as the matching uid
# (the M4 identity work). Until then this probe is an honest demonstrator of
# *where* naive bus routing hits the wall — the same "think it through" character
# as host networking.
#
# Abstract-socket addresses (`unix:abstract=…`) are deliberately unsupported: an
# abstract socket lives in the net namespace's abstract namespace, has no
# filesystem path, and so **cannot be bind-mounted** — a real distinction from a
# path socket, surfaced as a clear error rather than a deep bind failure.
{ pkgs, pierogi }:

let
  # Hand-rolled /etc — just a hostname. `dbus-send` to an explicit bus address
  # needs no /etc/machine-id (that's only for the GetMachineId peer call); kept
  # minimal so the probe shows the ONE thing the axis needs: the socket.
  etcHostname = pkgs.writeText "hostname" "pierogi\n";

  lib = import ../lib.nix { inherit pkgs pierogi; };

  # root: /bin farm (dbus → `dbus-send`, `dbus-monitor`, …), hand-rolled /etc,
  # base mountpoint dirs + the `/run/user/1000` bus-socket dir. The `bus` **file**
  # is the socket's bind target (a socket binds onto a file), so it's `touch`ed.
  root = pkgs.runCommand "pierogi-dbus-root" { } ''
    source ${lib.buildLib}
    mkdir -p $out/bin $out/etc

    bin_farm ${pkgs.lib.escapeShellArg pkgs.dbus}
    ln -s ${etcHostname} $out/etc/hostname

    bake_dirs ${pkgs.lib.escapeShellArgs lib.defaultMountDirs} /run/user/1000
    touch $out/run/user/1000/bus
  '';

  profiles = import ../profiles.nix { };

  unresolvedSpec = pkgs.lib.recursiveUpdate profiles.base {
    process = {
      executable = "dbus-send";
      # List the names registered on the session bus — the canonical "is the bus
      # alive and talking" call.
      args = [
        "--session"
        "--print-reply"
        "--dest=org.freedesktop.DBus"
        "/org/freedesktop/DBus"
        "org.freedesktop.DBus.ListNames"
      ];
      terminal = false;
      env = {
        PATH = "/bin";
        # The client looks here for the bus — the baked mountpoint path. The
        # wrapper binds the host's real bus socket onto it.
        DBUS_SESSION_BUS_ADDRESS = "unix:path=/run/user/1000/bus";
        XDG_RUNTIME_DIR = "/run/user/1000";
      };
    };
    mounts = lib.defaultMounts;
  };

  resolved = lib.resolveSpec {
    name = "pierogi-dbus-probe";
    spec = unresolvedSpec;
    root = root;
  };

# The runnable artifact — the Wayland-style discovery wrapper, for the bus: parse
# the host's `$DBUS_SESSION_BUS_ADDRESS`, extract the `unix:path=` socket, bind it
# onto the baked mountpoint, exec `pierogi run dbus-send`. Trailing `"$@"`
# overrides args (the executable stays the sealed `dbus-send`).
in
pkgs.writeShellScriptBin "pierogi-dbus-probe" ''
  set -eu

  : "''${DBUS_SESSION_BUS_ADDRESS:?not set — is a session bus running?}"
  addr="$DBUS_SESSION_BUS_ADDRESS"
  case "$addr" in
    unix:path=*)
      # Strip the `unix:path=` prefix and any trailing `,key=val` (e.g. guid).
      sock="''${addr#unix:path=}"
      sock="''${sock%%,*}"
      ;;
    *abstract=*)
      echo "pierogi-dbus-probe: session bus is an abstract socket ($addr) — no filesystem path to bind-mount" >&2
      exit 1
      ;;
    *)
      echo "pierogi-dbus-probe: unsupported bus address: $addr" >&2
      exit 1
      ;;
  esac
  if [ ! -S "$sock" ]; then
    echo "pierogi-dbus-probe: no bus socket at $sock" >&2
    exit 1
  fi

  exec ${pierogi}/bin/pierogi run ${resolved} \
    --mount "host:$sock:/run/user/1000/bus:rw" \
    -- "$@"
''
