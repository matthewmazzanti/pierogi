# M1 nix-mode artifact — a PipeWire probe (sealed-rootfs edition).
#
# Another `wayland.nix`-shaped socket router, for audio/video media. PipeWire's
# client socket is a local `AF_UNIX` socket at `$XDG_RUNTIME_DIR/pipewire-0` (not
# network) — so the routing is the familiar trio: a **baked mountpoint** at
# `/run/user/1000/pipewire-0`, **spec env** (`XDG_RUNTIME_DIR`) pointing the
# client at it, and a **runtime host bind** of the caller's real socket onto it
# (added by the wrapper). The host's runtime dir never leaks in.
#
# The runnable executable is `pw-dump` — a one-shot client that connects to the
# PipeWire daemon, dumps the full object graph (nodes, ports, devices) as JSON,
# and exits. That's the pass/fail for the media-routing axis: a graph means the
# socket routed and the client completed a round trip; a connection error means
# the plumbing is wrong. `/bin` also carries `pw-cli`, `pw-mon`, `pw-play`, etc.,
# for richer follow-on queries over the same route.
#
# Note the client connects to the regular `pipewire-0` socket, not the session
# manager's `pipewire-0-manager` — enumerating the graph as an ordinary client
# needs only the former. See `bash.nix` for the sealed-rootfs pipeline.
{ pkgs, pierogi }:

let
  etcHostname = pkgs.writeText "hostname" "pierogi\n";

  lib = import ../lib.nix { inherit pkgs pierogi; };

  # root: /bin farm (pipewire → `pw-dump`, `pw-cli`, …), hand-rolled /etc, base
  # mountpoint dirs + the `/run/user/1000` socket dir. The `pipewire-0` **file**
  # is the socket bind target, so it's `touch`ed (a socket binds onto a file).
  root = pkgs.runCommand "pierogi-pipewire-root" { } ''
    source ${lib.buildLib}
    mkdir -p $out/bin $out/etc

    bin_farm ${pkgs.lib.escapeShellArg pkgs.pipewire}
    ln -s ${etcHostname} $out/etc/hostname

    bake_dirs ${pkgs.lib.escapeShellArgs lib.defaultMountDirs} /run/user/1000
    touch $out/run/user/1000/pipewire-0
  '';

  profiles = import ../profiles.nix { };

  unresolvedSpec = pkgs.lib.recursiveUpdate profiles.base {
    process = {
      executable = "pw-dump";
      args = [ ];
      terminal = false;
      env = {
        PATH = "/bin";
        # The client resolves `pipewire-0` under here — the baked mountpoint dir.
        # The wrapper binds the host's real socket onto it.
        XDG_RUNTIME_DIR = "/run/user/1000";
      };
    };
    mounts = lib.defaultMounts;
  };

  resolved = lib.resolveSpec {
    name = "pierogi-pipewire-probe";
    spec = unresolvedSpec;
    root = root;
  };

# The runnable artifact — the Wayland-style discovery wrapper, for the PipeWire
# socket: find `$XDG_RUNTIME_DIR/pipewire-0`, bind it onto the baked mountpoint,
# exec `pierogi run pw-dump`. Trailing `"$@"` overrides args (the executable
# stays the sealed `pw-dump`).
in
pkgs.writeShellScriptBin "pierogi-pipewire-probe" ''
  set -eu

  : "''${XDG_RUNTIME_DIR:?not set — is a user session running?}"
  sock="$XDG_RUNTIME_DIR/pipewire-0"
  if [ ! -S "$sock" ]; then
    echo "pierogi-pipewire-probe: no PipeWire socket at $sock — is PipeWire running?" >&2
    exit 1
  fi

  exec ${pierogi}/bin/pierogi run ${resolved} \
    --mount "host:$sock:/run/user/1000/pipewire-0:rw" \
    -- "$@"
''
