# M1 nix-mode artifact — a foot terminal sandbox (sealed-rootfs edition).
#
# The graphical follow-on to `wayland-sandbox.nix`: it reuses that file's Wayland
# socket routing verbatim (baked `/run/user/1000/wayland-0` mountpoint + spec env
# + a runtime host bind in the wrapper), but runs **foot** — a real Wayland
# terminal that opens a window. See `wayland-sandbox.nix` for the routing writeup
# and `bash-sandbox.nix` for the sealed-rootfs pipeline itself.
#
# foot is CPU/`wl_shm`-rendered, so — unlike an EGL client — it needs **no GPU /
# `/dev/dri`**. What it does need beyond a routed socket is a small userland:
#   * a **shell to spawn** — foot runs `$SHELL` in its window, so bash + coreutils
#     live in `/bin` and `SHELL=/bin/bash` is set (like `bash-sandbox`);
#   * a **font** — foot resolves `monospace` via fontconfig, so DejaVu Sans Mono
#     is baked in and pointed at by a hand-rolled `/etc/fonts.conf`
#     (`FONTCONFIG_FILE`), with a writable `/tmp/fontconfig` cachedir;
#   * **terminfo** — foot sets `TERM=foot` for its child, so ncurses' terminfo DB
#     (which carries the `foot` entry) is baked at `/etc/terminfo` (`TERMINFO`),
#     or programs in the shell would fail their `foot` lookup;
#   * a **`/dev/pts` devpts** — foot allocates its own pty for the child shell.
#     The engine only auto-mounts devpts for `terminal` (its console pump), which
#     foot doesn't use, so the spec declares the devpts as a plain OCI mount.
{ pkgs, pierogi }:

let
  # The login shell foot spawns, surfaced at /bin/bash.
  loginShell = "/bin/bash";

  # Hand-rolled /etc identity — a real login user, so foot's `getpwuid` /
  # `$SHELL` resolution lands on a valid shell.
  etcPasswd = pkgs.writeText "passwd" ''
    root:x:0:0:root:/root:${loginShell}
    pierogi:x:1000:100:pierogi:/home/pierogi:${loginShell}
  '';
  etcGroup = pkgs.writeText "group" ''
    root:x:0:
    users:x:100:
  '';
  etcHostname = pkgs.writeText "hostname" ''
    pierogi
  '';
  etcBashrc = pkgs.writeText "bashrc" ''
    # pierogi foot sandbox — minimal interactive rc.
    # Colored prompt: magenta label, blue cwd. `\[..\]` wraps non-printing bytes.
    PS1='\[\e[1;35m\][foot]\[\e[0m\] \[\e[1;34m\]\w\[\e[0m\] \$ '
    alias ls='ls --color=auto'
    alias grep='grep --color=auto'
    alias ll='ls -la'
    alias l='ls -CF'
  '';

  # A monospace font + a self-sufficient fontconfig config. The config carries
  # the `monospace` alias itself (no reliance on a /etc/fonts/conf.d that the
  # sealed rootfs doesn't ship), and a writable `/tmp/fontconfig` cachedir (the
  # baked `/tmp` tmpfs). Nix scans this file's *contents* for store refs, so it
  # references `dejavu_fonts` automatically — seeding `root` from it pulls the
  # font into the closure.
  dejavuFonts = pkgs.dejavu_fonts;
  fontsConf = pkgs.writeText "fonts.conf" ''
    <?xml version="1.0"?>
    <!DOCTYPE fontconfig SYSTEM "urn:fontconfig:fonts.dtd">
    <fontconfig>
      <dir>${dejavuFonts}/share/fonts</dir>
      <cachedir>/tmp/fontconfig</cachedir>
      <alias><family>monospace</family><prefer><family>DejaVu Sans Mono</family></prefer></alias>
      <alias><family>sans-serif</family><prefer><family>DejaVu Sans</family></prefer></alias>
      <alias><family>serif</family><prefer><family>DejaVu Serif</family></prefer></alias>
    </fontconfig>
  '';

  # The minimal-but-usable terminal userland (shell, coreutils, pager, text
  # tools, tree, ncurses+terminfo) — shared with the bash sandbox. ncurses'
  # terminfo DB carries the `foot` entry, so no separate `foot.terminfo` output
  # is needed.
  terminalTools = import ./terminal-tools.nix { inherit pkgs; };

  # The runtime deps — root's references. `root` itself is NOT here (circular).
  # closureInfo folds in each dep's own closure, so `fontsConf` drags in
  # `dejavuFonts` for the baked mountpoints even though it's also listed.
  deps = terminalTools ++ [
    pkgs.foot
    dejavuFonts
    fontsConf
    etcPasswd
    etcGroup
    etcHostname
    etcBashrc
  ];

  # The supplied closure of the deps — for baking dep mountpoints only.
  depsClosure = pkgs.closureInfo { rootPaths = deps; };

  # root: the sealed rootfs — /bin farm, hand-rolled /etc, baked mountpoints
  # (incl. the Wayland socket file). No launch-time scaffold.
  root = pkgs.runCommand "pierogi-foot-root" { } ''
    mkdir -p $out/bin $out/etc

    # /bin symlink farm — the shared terminal userland plus foot itself.
    for pkg in ${pkgs.lib.concatMapStringsSep " " (p: "${p}") (terminalTools ++ [ pkgs.foot ])}; do
      for f in "$pkg"/bin/*; do
        ln -sf "$f" "$out/bin/$(basename "$f")"
      done
    done

    # Hand-rolled /etc — store symlinks (closure members, resolve in-sandbox and
    # fold into root's references). `/etc/fonts.conf` → the fontconfig file (→
    # dejavu), `/etc/terminfo` → ncurses' terminfo DB (carries `foot` + the rest).
    ln -s ${etcPasswd}   $out/etc/passwd
    ln -s ${etcGroup}    $out/etc/group
    ln -s ${etcHostname} $out/etc/hostname
    ln -s ${etcBashrc}   $out/etc/bashrc
    ln -s ${fontsConf}   $out/etc/fonts.conf
    ln -s ${pkgs.ncurses}/share/terminfo $out/etc/terminfo

    # Baked mountpoints. `/proc`, `/dev`, and `/tmp` are all producer mounts
    # (spec.mounts; `/tmp` also fontconfig's cachedir); `/run/user/1000` + the
    # `wayland-0` **file** are the socket bind target (a socket binds onto a file).
    mkdir -p $out/proc $out/dev $out/tmp $out/home/pierogi $out/root $out/run/user/1000
    touch $out/run/user/1000/wayland-0

    # One /nix/store/<hash> mountpoint per closure member (kind matches source).
    mkdir -p $out/nix/store
    while read -r p; do
      if [ -d "$p" ]; then
        mkdir -p "$out$p"
      else
        mkdir -p "$(dirname "$out$p")"
        touch "$out$p"
      fi
    done < ${depsClosure}/store-paths

    # root's OWN identity mountpoint ($out is input-addressed — no circularity).
    mkdir -p "$out/nix/store/$(basename "$out")"
  '';

  # The default-closed floor, as spec fields (see nix/profiles.nix).
  profiles = import ./profiles.nix { };

  # The sources spec: `profiles.base` merged with this sandbox's fields.
  unresolvedSpec = pkgs.lib.recursiveUpdate profiles.base {
    root.path = "${root}";
    process = {
      executable = "foot";
      args = [ ];
      # foot opens its own Wayland window and forks a pty for the child shell —
      # it doesn't use pierogi's stdio as a tty, so no relay.
      terminal = false;
      # foot (and the shell it spawns) starts in /root — the persistent host dir
      # the wrapper binds there; HOME too, so state survives the container.
      cwd = "/root";
      env = {
        PATH = "/bin";
        HOME = "/root";
        # foot spawns this in its window.
        SHELL = "/bin/bash";
        # foot sets TERM=foot for the child; this is where its entry lives.
        TERMINFO = "/etc/terminfo";
        # A UTF-8 locale (foot warns otherwise; glibc's C.UTF-8 needs no archive).
        LANG = "C.UTF-8";
        # fontconfig: the self-sufficient config + a writable cache under /tmp.
        FONTCONFIG_FILE = "/etc/fonts.conf";
        XDG_CACHE_HOME = "/tmp";
        # Wayland socket routing — the baked mountpoint path (wrapper binds the
        # host's real socket onto it).
        XDG_RUNTIME_DIR = "/run/user/1000";
        WAYLAND_DISPLAY = "wayland-0";
      };
    };
    # Placements — verbatim OCI mounts, straight through. Writable /tmp scratch
    # (also the fontconfig cache), and a **devpts** so foot can allocate its own
    # pty for the child shell. The engine auto-mounts devpts only for `terminal`
    # (its console pump); foot runs the pump-less path (`terminal = false`), so it
    # declares the devpts itself — plain producer policy, no engine knob. crun
    # mounts it onto the /dev tmpfs and wires up /dev/ptmx (matches the options
    # the engine's own `devpts_mount()` uses).
    mounts = [
      # /proc + /dev tmpfs — lifted out of the engine's base mounts into the
      # producer layer (see bash-sandbox.nix / docs/control-surface.md). /dev
      # takes no `nodev` — device nodes live there — and precedes the devpts
      # below, which mounts onto it.
      { destination = "/proc"; type = "proc"; source = "proc"; }
      { destination = "/dev"; type = "tmpfs"; source = "tmpfs"; options = [ "nosuid" "strictatime" "mode=755" "size=65536k" ]; }
      { destination = "/tmp"; type = "tmpfs"; options = [ "nosuid" "nodev" "mode=1777" ]; }
      {
        destination = "/dev/pts";
        type = "devpts";
        source = "devpts";
        options = [ "nosuid" "noexec" "newinstance" "ptmxmode=0666" "mode=0620" ];
      }
    ];
    closure.seeds = [ "${root}" ];
  };
  unresolvedSpecFile =
    pkgs.writeText "spec.unresolved.json" (builtins.toJSON unresolvedSpec);

  # Build: traverse root's reference graph from the spec's seeds to stamp
  # `closure.resolved` (see bash-sandbox.nix for the structuredAttrs details).
  resolved = pkgs.runCommand "pierogi-foot-sandbox.json"
    {
      __structuredAttrs = true;
      exportReferencesGraph.closure = [ root ];
    } ''
    ${pierogi}/bin/pierogi resolve ${unresolvedSpecFile} \
      --graph "$NIX_ATTRS_JSON_FILE" > "''${outputs[out]}"
  '';

in
{
  inherit resolved;

  # The runnable artifact — same socket-discovery wrapper as `wayland-sandbox`:
  # find the host's Wayland socket, bind it onto the baked mountpoint, exec
  # `pierogi run foot`. Trailing `"$@"` is the args-only override, forwarded to
  # foot (options, or a command to run instead of the shell — e.g.
  # `nix run .#foot-sandbox -- bash -c 'echo hi'`).
  runnable = pkgs.writeShellScriptBin "pierogi-foot-sandbox" ''
    set -eu

    : "''${XDG_RUNTIME_DIR:?not set — is a Wayland session running?}"
    display="''${WAYLAND_DISPLAY:-wayland-0}"
    case "$display" in
      /*) sock="$display" ;;
      *)  sock="$XDG_RUNTIME_DIR/$display" ;;
    esac
    if [ ! -S "$sock" ]; then
      echo "pierogi-foot-sandbox: no Wayland socket at $sock" >&2
      exit 1
    fi

    # Persistent state: a local (gitignored) dir bound to /root — foot's cwd +
    # HOME. Override the location with $PIEROGI_STATE.
    state="''${PIEROGI_STATE:-$PWD/state/foot-sandbox}"
    mkdir -p "$state"

    exec ${pierogi}/bin/pierogi run ${resolved} \
      --mount "host:$sock:/run/user/1000/wayland-0:rw" \
      --mount "host:$state:/root:rw" \
      -- "$@"
  '';
}
