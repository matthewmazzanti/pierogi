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
# (`resolved`): closure stamped in at build. The sources-only spec exists only
# as the internal input to `resolve --graph`, not as a runnable output.
#
# The pipeline it exercises:
#
#   deps (bash, coreutils, /etc files)            the runtime deps
#     │
#     ├─ closureInfo { rootPaths = deps; }  →  store-paths   (bakes dep mountpoints)
#     │
#     ├─ root (symlink farm + baked /etc + baked mountpoints, incl. its OWN)
#     │     │
#     │     └─ __structuredAttrs + exportReferencesGraph.closure = [root]
#     │            │  (Nix expands the graph as JSON in $NIX_ATTRS_JSON_FILE)
#     │            └─ pierogi traverses it from the spec's seeds → `resolved`
#     │
#     ├─ unresolved spec.json  (root/executable/args/env/mounts + closure.seeds,
#     │                          no closure.resolved)
#     └─ pierogi resolve <spec> --graph $NIX_ATTRS_JSON_FILE  →  built spec.json
#
# Two different closure mechanisms, because baking and the spec-stamp have
# different constraints:
#
#   * **Baking dep mountpoints** just needs a flat path list, and must not depend
#     on `root` (its content is what we're building). `closureInfo { rootPaths =
#     deps }` gives exactly that — buildable before `root` exists. `root`'s *own*
#     /nix/store/<self> mountpoint is baked from `$out`, which nix fixes before
#     the builder runs (input-addressed), so there's no circularity.
#   * **The spec's `resolved`** is computed by pierogi itself: Nix hands it the
#     `exportReferencesGraph` of `root` as JSON (deps + root) and `pierogi
#     resolve` *traverses* it from the spec's seeds. So the bind set is provably
#     the closure of the declared seeds, not a list taken on faith. `root` IS in
#     it — identity-bound at /nix/store/<self> and mounted at `/`.
#
# No `nix-store` query runs at build: the build sandbox has no daemon. The dep
# list comes from `closureInfo`; the graph from the `exportReferencesGraph`
# derivation parameter, delivered as JSON via `$NIX_ATTRS_JSON_FILE`.
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

  # Light-touch resolution utilities (store-mount baking shell + `resolveSpec`).
  lib = import ./lib.nix { inherit pkgs pierogi; };

  # The runtime deps — root's references. `root` itself is NOT here: it doesn't
  # exist yet, and seeding from it would be circular. These are all buildable
  # before `root`, so they drive the baked dep-mountpoint tree.
  deps = terminalTools ++ [ etcPasswd etcGroup etcHostname etcBashrc ];

  # root: the sealed rootfs. A symlink farm for /bin, hand-rolled /etc, and every
  # mountpoint baked in — no launch-time scaffold.
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
    # path that `root` references and the closure folds in.
    ln -s ${etcPasswd}   $out/etc/passwd
    ln -s ${etcGroup}    $out/etc/group
    ln -s ${etcHostname} $out/etc/hostname
    ln -s ${etcBashrc}   $out/etc/bashrc
    # The terminfo DB, so TERM (xterm-256color) resolves for less/clear/tput/etc.
    ln -s ${pkgs.ncurses}/share/terminfo $out/etc/terminfo

    # Baked mountpoints — the base trio's dirs (proc/dev/tmp) + this sandbox's
    # own (/home/pierogi, /root). See `bake_dirs` in build-lib.sh; the read-only
    # rootfs can't grow these at launch, so the mount topology is frozen here.
    bake_dirs ${pkgs.lib.escapeShellArgs lib.defaultMountDirs} /home/pierogi /root

    # The /nix/store/<hash> mountpoints (one per closure member, kind matching
    # the source) + root's own /nix/store/<self> identity mountpoint. See
    # `bake_store_mounts` in build-lib.sh. `deps` is root's references (not root
    # itself — circular), so their closure is exactly what root needs bound.
    bake_store_mounts $(cat ${lib.storePaths deps})
  '';

  # The default-closed floor, as spec fields — the engine holds no policy, so
  # the sandbox is hardened *because* it composes this (see nix/profiles.nix).
  profiles = import ./profiles.nix { };

  # The sources spec: `profiles.base` (the floor) merged with this sandbox's
  # own fields. `closure.seeds` set (the build-time input), but no
  # `closure.resolved` yet — `resolve --graph` stamps that. Internal input only,
  # not runnable, since there is no live-query path.
  #
  # The process fields track docker-compose, flat: `executable`/`args` (execve),
  # `tty`, `environment`. The rest are faithful OCI pass-throughs (from `base`).
  # `root`/`closure` stay Nix-shaped. See `docs/spec.md`.
  unresolvedSpec = pkgs.lib.recursiveUpdate profiles.base {
    root.path = "${root}";
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
    # `closure.seeds` is injected by `resolveSpec` from its `seeds` arg (below),
    # so the graphed roots and the spec's seeds can't drift.
  };

  # Build: traverse root's reference graph from the seeds and stamp
  # `closure.resolved`. `seeds = [ root ]` (its closure covers bash/coreutils/etc);
  # an overlay lower would be added alongside. See `lib.resolveSpec`.
  resolved = lib.resolveSpec {
    name = "pierogi-bash-sandbox";
    spec = unresolvedSpec;
    seeds = [ root ];
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
