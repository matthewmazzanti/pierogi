# M1 nix-mode artifact — the tire-kicking version, sealed-rootfs edition.
#
# Returns the built spec.json for a static bash + coreutils sandbox whose `root`
# is a **complete, sealed rootfs**: `/etc`, a `/bin` symlink farm, and every
# mountpoint the runtime needs (`/proc`, `/dev`, `/tmp`, and one
# `/nix/store/<hash>` per closure member) baked in. Nothing is scaffolded at
# launch — `pierogi` mounts `root` at `/`, remounts it read-only, and binds the
# closure onto the pre-baked mountpoints.
#
# There is no runtime query, so the only runnable artifact is the **built** spec
# (`resolved`): closure stamped in at build.
#
# The pipeline it exercises (the "input derivation" model — see nix/lib.nix):
#
#   content = runCommand …           the rootfs the author designs, explicitly:
#     │                                a /bin farm, hand-rolled /etc, base
#     │                                mountpoint dirs. NOT the /nix/store tree.
#     │
#     └─ lib.sealAndResolve { spec; content }:
#          ├─ sealRoot content        cp content's tree, then bake the
#          │    │                       /nix/store/<hash> mountpoints from
#          │    │                       content's OWN references (closureInfo),
#          │    │                       + root's /nix/store/<self>. → sealed root
#          │    └─ (fails loud if content already carries /nix)
#          └─ resolveSpec { spec + root.path; seeds = [root] }
#               └─ __structuredAttrs + exportReferencesGraph.closure = [root];
#                  pierogi resolve --graph → `resolved` (closure stamped in)
#
# The point of the split: there is **no hand-maintained dep list**. The closure
# to bake is derived from what `content` *actually references* (its /bin and /etc
# symlink targets), so adding an /etc file or a tool is a single edit to `content`
# — never a parallel `deps` entry. `content` is build scaffold: the sealed root
# copies its symlink tree (pointing at the tools), so it references the tools
# directly, not `content`, which drops out of the closure.
#
# No `nix-store` query runs at build: the build sandbox has no daemon. The bake
# list comes from `closureInfo`; `resolved`'s graph from `exportReferencesGraph`,
# delivered as JSON via `$NIX_ATTRS_JSON_FILE`.
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
  terminalTools = import ./terminal-tools.nix { inherit pkgs; };

  # Light-touch resolution utilities (store-mount baking shell + seal/resolve).
  lib = import ./lib.nix { inherit pkgs pierogi; };

  # content: the rootfs the author designs — a /bin symlink farm, hand-rolled
  # /etc, and the base mountpoint dirs. Deliberately NOT the `/nix/store`
  # topology: `lib.sealRoot` bakes that from content's *actual references*, so
  # there is no separate dep list to keep in sync. content is build scaffold —
  # the sealed root copies its tree and references the tools directly, so content
  # itself drops out of the closure.
  content = pkgs.runCommand "pierogi-bash-content" { } ''
    source ${lib.buildLib}
    mkdir -p $out/bin $out/etc

    # /bin symlink farm — see `bin_farm` in build-lib.sh. Absolute targets into
    # /nix/store (resolved inside the sandbox because the closure is bound there);
    # fails loud on a command-name collision, buildEnv-style.
    bin_farm ${pkgs.lib.escapeShellArgs terminalTools}

    # Hand-rolled /etc — symlinks into the store (each is a closure member, so it
    # resolves inside the sandbox). A symlink, not a copy: a copy would bake
    # opaque bytes with no store ref; the symlink makes /etc/passwd a real store
    # path that content references and `sealRoot` folds into the closure.
    ln -s ${etcPasswd}   $out/etc/passwd
    ln -s ${etcGroup}    $out/etc/group
    ln -s ${etcHostname} $out/etc/hostname
    ln -s ${etcBashrc}   $out/etc/bashrc
    # The terminfo DB, so TERM (xterm-256color) resolves for less/clear/tput/etc.
    ln -s ${pkgs.ncurses}/share/terminfo $out/etc/terminfo

    # Baked mountpoints — the base trio's dirs (proc/dev/tmp) + this sandbox's
    # own (/home/pierogi, /root). See `bake_dirs` in build-lib.sh; the read-only
    # rootfs can't grow these at launch, so the mount topology is frozen here. The
    # `/nix/store` mountpoints are NOT baked here — `lib.sealRoot` owns those.
    bake_dirs ${pkgs.lib.escapeShellArgs lib.defaultMountDirs} /home/pierogi /root
  '';

  # The default-closed floor, as spec fields — the engine holds no policy, so
  # the sandbox is hardened *because* it composes this (see nix/profiles.nix).
  profiles = import ./profiles.nix { };

  # The sources spec: `profiles.base` (the floor) merged with this sandbox's own
  # fields. `root.path` and `closure` are injected by `lib.sealAndResolve` (it
  # seals `content`, then stamps them), so they're absent here.
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
    # `root.path` + `closure` are injected by `lib.sealAndResolve` (below).
  };

  # Seal `content` into the rootfs and resolve against it: `sealAndResolve` bakes
  # the /nix/store topology from content's actual references, injects `root.path`
  # + seeds, and stamps `closure.resolved`. No dep list — the closure is whatever
  # content references. See `lib.sealAndResolve`.
  resolved = lib.sealAndResolve {
    name = "pierogi-bash-sandbox";
    spec = unresolvedSpec;
    content = content;
  };

in
{
  # The built spec (closure stamped in). Internal: the input the runner execs
  # against, and the artifact `bench.sh` / `resolve --graph` inspection consume.
  inherit resolved;

  # The runnable artifact — a wrapper that execs `pierogi run` against the built
  # spec and hands the script's own argv to **bash** inside the sandbox. `"$@"`
  # sits after `--`, so it becomes clap's `last = true` args override — which now
  # replaces `process.args` *only*, keeping the spec's sealed `bash` executable.
  # So `nix run .#bash-sandbox -- -c 'echo hello'` runs `bash -c 'echo hello'`,
  # and a bare `nix run .#bash-sandbox` (empty `$@`) leaves the spec's args as-is
  # and launches the default interactive bash (sourcing /etc/bashrc). The closure
  # is sealed into `resolved` at build time; the wrapper adds no policy of its own.
  runnable = pkgs.writeShellScriptBin "pierogi-bash-sandbox" ''
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
  '';
}
