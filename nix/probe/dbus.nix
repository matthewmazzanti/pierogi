# M2 nix-mode artifact — a D-Bus session-bus probe over the filtering proxy.
#
# The successor to this probe's earlier life as an honest FAILURE demonstrator.
# It used to route the host bus socket straight in (the Wayland trick: a baked
# mountpoint + spec env + a runtime host bind) and DIE at the SASL `EXTERNAL`
# handshake — the sandbox presents its mapped (container-0) uid, the host
# `dbus-daemon` reads the real peer uid, they mismatch, "Did not receive a
# reply". Routing the socket was necessary but never sufficient.
#
# The M2 proxy layer dissolves the UPSTREAM half of that wall. Instead of binding
# the raw socket, the spec now carries a `dbus` section (see src/spec.rs,
# src/dbus.rs): the engine supervises an `xdg-dbus-proxy` host-side — running as
# the REAL uid, so the proxy→bus handshake succeeds — applies the allowlist, and
# binds the proxy's FILTERED socket onto `/run/user/1000/bus`. The sandbox reaches
# the bus through a socket it's allowed to open; the host bus never appears inside.
#
# But the wall has a SECOND hop, and routing alone doesn't clear it: xdg-dbus-proxy
# is itself a D-Bus server to the client and re-checks the client's SASL EXTERNAL
# uid against the socket peer credentials. Under profiles.base's container-0 →
# host-invoker map the client runs as uid 0 and presents "0", while the proxy
# reads the real host uid via SO_PEERCRED — mismatch, the same "Did not receive a
# reply". So the client must present the uid the proxy sees. This probe does that
# with an IDENTITY map (the M4 identity work, previewed below): map the invoker
# 1:1 (container 1000 → host "self") and run the client as 1000, sending container
# 0 to a subuid so crun still has a root-in-ns for setup. Only with BOTH the proxy
# (upstream auth) AND the identity map (client-side auth) does the call complete —
# exactly how flatpak works (its apps run at the real uid, not remapped to 0).
#
# Two chores fall away with the proxy, both now the engine's job, not the
# wrapper's:
#   * socket discovery — the engine resolves the `session` sentinel to
#     `$DBUS_SESSION_BUS_ADDRESS` at launch (the `host: "self"` precedent), so
#     the wrapper no longer parses `unix:path=` or hand-binds with `--mount
#     host:`. The wrapper is now nearly empty.
#   * the abstract-socket limitation — the OLD probe rejected `unix:abstract=…`
#     buses outright (an abstract socket has no filesystem path to bind-mount).
#     The proxy is a CLIENT connecting to the host bus, so it dials an abstract
#     address fine and creates a fresh PATH socket for the sandbox. Abstract
#     session buses now Just Work — the distinction the old probe surfaced is
#     gone.
#
# The runnable executable is unchanged: `dbus-send … ListNames`, a one-shot that
# connects and prints the names registered on the bus. Through the proxy the call
# succeeds and the reply is FILTERED to the names the policy makes visible (plus
# the always-visible bus driver and the client's own unique name) — the pass
# signal flips from "uid-wall failure" to "a filtered ListNames reply".
{ pkgs, pierogi }:

let
  # Hand-rolled /etc — just a hostname. `dbus-send`'s `ListNames` needs no
  # /etc/machine-id (that's the `GetMachineId` peer call, which this doesn't
  # make), and the proxy authenticates host-side with the host's own machine-id —
  # so the sandbox still needs none. Kept minimal: the axis here is the proxy.
  etcHostname = pkgs.writeText "hostname" "pierogi\n";

  lib = import ../lib.nix { inherit pkgs pierogi; };

  # root: /bin farm (dbus → `dbus-send`, `dbus-monitor`, …), hand-rolled /etc,
  # base mountpoint dirs + the `/run/user/1000` bus-socket dir. The `bus` **file**
  # is the proxy socket's bind target (a socket binds onto a file), so it's
  # `touch`ed — the engine binds the xdg-dbus-proxy socket here.
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
    # Identity uid/gid map for the client (M4 preview): xdg-dbus-proxy verifies
    # the client's SASL EXTERNAL uid against the socket peer credentials, so the
    # in-sandbox client must present the SAME uid the proxy reads host-side.
    # profiles.base's container-0 → host-invoker map makes the client present "0"
    # while the proxy sees the real uid — the uid wall, one hop in. So we map the
    # invoker identically (container 1000 → host "self") and run the client as
    # 1000; container 0 goes to a subuid so crun still has a root-in-ns for setup.
    # (Assumes invoker uid 1000, as the /run/user/1000 paths already do.)
    linux = {
      uid_mappings = [
        { container = 0; host = 100000; size = 1; }
        { container = 1000; host = "self"; size = 1; }
      ];
      gid_mappings = [
        { container = 0; host = 100000; size = 1; }
        { container = 1000; host = "self"; size = 1; }
      ];
    };
    process = {
      user = { uid = 1000; gid = 1000; };
      executable = "dbus-send";
      # List the names registered on the session bus — the canonical "is the bus
      # alive and talking" call. Through the proxy the reply is filtered to the
      # visible set (see `dbus.policy` below).
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
        # The client looks here for the bus — the baked mountpoint the engine
        # binds the proxy's FILTERED socket onto (no longer the raw host socket).
        DBUS_SESSION_BUS_ADDRESS = "unix:path=/run/user/1000/bus";
        XDG_RUNTIME_DIR = "/run/user/1000";
      };
    };
    mounts = lib.defaultMounts;

    # The M2 carve-out: one filtered session-bus proxy. The engine spawns
    # xdg-dbus-proxy against the host session bus (`session` → resolved at launch
    # from `$DBUS_SESSION_BUS_ADDRESS`), applies this policy, and binds its socket
    # onto `socket` — the baked mountpoint the client's env points at.
    #
    # `--filter` is always engaged, so this is deny-all plus what's below.
    # `ListNames` is a call on the bus driver, which the proxy always permits and
    # answers with a view filtered to the names the client may SEE — so `see`ing
    # the freedesktop names makes the reply legibly filtered (freedesktop.* names
    # present on the host bus show; anything else is hidden) without granting any
    # talk/own. This is the policy the Nix layer owns; the engine only transcribes
    # it to `--see=…` flags and supervises the process.
    dbus = [
      {
        bus = "session";
        socket = "/run/user/1000/bus";
        policy = {
          see = [ "org.freedesktop.*" ];
        };
      }
    ];
  };

  resolved = lib.resolveSpec {
    name = "pierogi-dbus-probe";
    spec = unresolvedSpec;
    root = root;
  };

# The runnable artifact — now a thin wrapper. Socket discovery and binding moved
# into the engine (the `session` sentinel + the `dbus` section), so there is
# nothing left to parse or bind by hand: just exec `pierogi run`. The upfront
# check keeps the error legible when no session bus is present (the engine would
# otherwise fall back to `/run/user/<uid>/bus` and the proxy fail there);
# trailing `"$@"` overrides args (the executable stays the sealed `dbus-send`).
in
pkgs.writeShellScriptBin "pierogi-dbus-probe" ''
  set -eu

  : "''${DBUS_SESSION_BUS_ADDRESS:?not set — is a session bus running?}"

  exec ${pierogi}/bin/pierogi run ${resolved} -- "$@"
''
