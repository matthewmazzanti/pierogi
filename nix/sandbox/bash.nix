# M1 nix-mode artifact — the tire-kicking version, sealed-rootfs edition.
#
# Returns the built spec.json for a static bash + coreutils sandbox whose `root`
# is a sealed rootfs: `/etc`, a `/bin` symlink farm, and the base mountpoint dirs
# (`/proc`, `/dev`, `/tmp`, `/nix/store`) baked in. `pierogi` mounts `root` at
# `/`, remounts it read-only, mounts a tmpfs at `/nix/store`, and binds the
# closure onto it (the runtime creates each `/nix/store/<hash>` target).
#
# There is no runtime query, so the only runnable artifact is the **built** spec
# (`resolved`): closure stamped in at build.
#
# The pipeline it exercises (see nix/lib.nix):
#
#   root = runCommand …               the tree the author designs, explicitly:
#     │                                a /bin farm, hand-rolled /etc, base
#     │                                mountpoint dirs (incl. an empty /nix/store).
#     │
#     └─ lib.resolveSpec { spec; root }:
#          └─ inject root.path + seeds = [root];
#             __structuredAttrs + exportReferencesGraph.closure = [root];
#             pierogi resolve --graph → `resolved` (closure stamped in)
#
# There is **no hand-maintained dep list**: the closure is whatever `root`
# *actually references* (its /bin and /etc symlink targets), so adding an /etc
# file or a tool is a single edit to the `root` build. The store *topology* is a
# runtime tmpfs, so nothing is baked or filtered — `root` is the `/` mount and
# references only its real content.
#
# No `nix-store` query runs at build: the build sandbox has no daemon. `resolved`'s
# graph comes from `exportReferencesGraph`, delivered as JSON via
# `$NIX_ATTRS_JSON_FILE`.
{ pkgs, pierogi }:

let
  # The login shell we bake into /etc/passwd — a real store path (bash is in the
  # closure), surfaced at /bin/bash inside the sandbox.
  loginShell = "/bin/bash";

  # Hand-rolled /etc — fixed files authored as Nix strings (M1).
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

  # Minimal system bashrc — nixpkgs bash is built with SYS_BASHRC = /etc/bashrc,
  # so an interactive (non-`--norc`) shell sources this before ~/.bashrc. Just a
  # prompt and a couple of aliases; the sealed rootfs carries no other rc files.
  etcBashrc = pkgs.writeText "bashrc" ''
    # pierogi bash sandbox — minimal interactive rc.
    # Colored prompt: green label, blue cwd. `\[..\]` wraps non-printing bytes so
    # bash keeps its line-length accounting correct.
    PS1='\[\e[1;32m\][pierogi]\[\e[0m\] \[\e[1;34m\]\w\[\e[0m\] \$ '
    alias ls='ls --color=auto'
    alias grep='grep --color=auto'
    alias ll='ls -la'
    alias l='ls -CF'
  '';

  # The minimal-but-usable terminal userland (shell, coreutils, pager, text
  # tools, tree, ncurses+terminfo) — shared with the foot sandbox.
  terminalTools = import ../terminal-tools.nix { inherit pkgs; };

  # Light-touch resolution utilities (build shell + resolveSpec).
  lib = import ../lib.nix { inherit pkgs pierogi; };

  # root: the tree the author designs — a /bin symlink farm, hand-rolled /etc,
  # and the base mountpoint dirs (incl. an empty /nix/store, the engine's store
  # tmpfs mountpoint). It IS the `/` mount; `lib.resolveSpec` derives the closure
  # from its *actual references*, so there is no separate dep list to keep in sync.
  root = pkgs.runCommand "pierogi-bash-root" { } ''
    source ${lib.buildLib}
    mkdir -p $out/bin $out/etc

    # /bin symlink farm — see `bin_farm` in build-lib.sh. Absolute targets into
    # /nix/store (resolved inside the sandbox because the closure is bound there);
    # fails loud on a command-name collision, buildEnv-style.
    bin_farm ${pkgs.lib.escapeShellArgs terminalTools}

    # Hand-rolled /etc — symlinks into the store (each is a closure member, so it
    # resolves inside the sandbox). A symlink, not a copy: a copy would bake
    # opaque bytes with no store ref; the symlink makes /etc/passwd a real store
    # path that `root` references and `resolveSpec` folds into the closure.
    ln -s ${etcPasswd}   $out/etc/passwd
    ln -s ${etcGroup}    $out/etc/group
    ln -s ${etcHostname} $out/etc/hostname
    ln -s ${etcBashrc}   $out/etc/bashrc
    # The terminfo DB, so TERM (xterm-256color) resolves for less/clear/tput/etc.
    ln -s ${pkgs.ncurses}/share/terminfo $out/etc/terminfo

    # Baked mountpoints — the base dirs (proc/dev/tmp + /nix/store, the engine's
    # store-tmpfs mountpoint) + this sandbox's own (/home/pierogi, /root). See
    # `bake_dirs` in build-lib.sh; the read-only rootfs can't grow these at
    # launch, so every mount target is frozen here.
    bake_dirs ${pkgs.lib.escapeShellArgs lib.defaultMountDirs} /home/pierogi /root
  '';

  # The default-closed floor, as spec fields — the engine holds no policy, so
  # the sandbox is hardened *because* it composes this (see nix/profiles.nix).
  profiles = import ../profiles.nix { };

  # The sources spec: `profiles.base` (the floor) merged with this sandbox's own
  # fields. `root.path` and `closure` are injected by `lib.resolveSpec`, so
  # they're absent here.
  #
  # The process fields track docker-compose, flat: `executable`/`args` (execve),
  # `tty`, `environment`. The rest are faithful OCI pass-throughs (from `base`).
  # See `docs/spec.md`.
  unresolvedSpec = pkgs.lib.recursiveUpdate profiles.base {
    process = {
      executable = "bash";
      # No `--norc`: an interactive shell sources /etc/bashrc (baked above).
      args = [ ];
      terminal = true;
      # Start in /root — the persistent host dir the wrapper binds there. HOME
      # points there too, so shell history / dotfiles survive the container.
      cwd = "/root";
      env = {
        # /bin is root's own bin/, surfaced at /. `/nix/store/<root>/bin` would
        # work too now that root is identity-bound, but /bin is shorter.
        PATH = "/bin";
        HOME = "/root";
        TERM = "xterm-256color";
        TERMINFO = "/etc/terminfo";
        LANG = "C.UTF-8";
      };
    };
    # Placements — the common base trio (proc/dev/tmp), lifted out of the engine
    # into the producer layer (see nix/lib.nix). Verbatim OCI mounts. The engine
    # appends the `terminal` devpts after these.
    mounts = lib.defaultMounts;
    # `root.path` + `closure` are injected by `lib.resolveSpec` (below).
  };

  # Resolve against `root`: `resolveSpec` injects `root.path` + seeds and stamps
  # `closure.resolved` from what `root` references. No dep list, no store baking —
  # the store topology is a runtime tmpfs. See `lib.resolveSpec`.
  resolved = lib.resolveSpec {
    name = "pierogi-bash-sandbox";
    spec = unresolvedSpec;
    root = root;
  };

# The runnable artifact (this file's sole output) — a wrapper that execs
# `pierogi run` against the built spec and hands the script's own argv to
# **bash** inside the sandbox. `"$@"` sits after `--`, so it becomes clap's
# `last = true` args override — which now replaces `process.args` *only*, keeping
# the spec's sealed `bash` executable. So `nix run .#bash-sandbox -- -c 'echo
# hello'` runs `bash -c 'echo hello'`, and a bare `nix run .#bash-sandbox` (empty
# `$@`) leaves the spec's args as-is and launches the default interactive bash
# (sourcing /etc/bashrc). The closure is sealed into `resolved` at build time;
# the wrapper adds no policy of its own.
in
pkgs.writeShellScriptBin "pierogi-bash-sandbox" ''
  set -eu

  # Persistent state: a local (gitignored) dir bound to /root — the sandbox's
  # cwd + HOME. Anything written there survives the container. Defaults under
  # the invocation dir (the repo's gitignored `state/`); override with
  # $PIEROGI_STATE.
  state="''${PIEROGI_STATE:-$PWD/state/bash-sandbox}"
  mkdir -p "$state"

  exec ${pierogi}/bin/pierogi run ${resolved} \
    --mount "host:$state:/root:rw" \
    -- "$@"
''
