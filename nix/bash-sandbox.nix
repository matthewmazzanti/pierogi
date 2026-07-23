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
# as the internal input to `resolve --closure`, not as a runnable output.
#
# The pipeline it exercises:
#
#   deps (bash, coreutils, /etc files)            the runtime closure seeds
#     │
#     ├─ closureInfo { rootPaths = deps; }  →  store-paths (the bind set)
#     │      │
#     │      └─ used *twice*: to bake the /nix/store mountpoint tree into root,
#     │         and (re-sorted) as the supplied closure the spec stamps.
#     │
#     ├─ root (symlink farm + baked /etc + baked mountpoints)   the sealed rootfs
#     │
#     ├─ unresolved spec.json  (exec/root/mounts/env, no closure)
#     └─ pierogi resolve <spec> --closure store-paths  →  $out  (resolved spec.json)
#
# The closure is seeded from the **deps, not `root`** — deliberately. `root`'s
# content (the mountpoint dirs) depends on the closure, so seeding the closure
# from `root` would be circular. deps don't depend on `root`, so `closureInfo`
# resolves fine before `root` is built, and `root` itself is *excluded* from the
# bind set: it's the `/` mount, not a `/nix/store` entry (`oci::build_spec` skips
# it). `PATH` points at `/bin` — `root`'s own `bin/`, surfaced at `/` — so
# nothing references `/nix/store/<root-hash>` and there is no self-mountpoint.
#
# No `nix-store` query runs at build: the build sandbox has no daemon, so the
# closure is *supplied* by `closureInfo`.
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

  # The runtime deps — everything the sandbox binds under /nix/store. `root`
  # itself is NOT here (it's the / mount, not a bind), which is also what keeps
  # the closure non-circular: these are all buildable before `root` exists.
  deps = [ pkgs.bash pkgs.coreutils etcPasswd etcGroup etcHostname ];

  # The supplied closure of the deps. Seeded from deps (not root) — see header.
  # `store-paths` drives both the baked mountpoint tree and the stamped closure.
  closure = pkgs.closureInfo { rootPaths = deps; };

  # root: the sealed rootfs. A symlink farm for /bin, hand-rolled /etc, and every
  # mountpoint baked in — no launch-time scaffold.
  root = pkgs.runCommand "pierogi-bash-root" { } ''
    mkdir -p $out/bin $out/etc

    # /bin symlink farm (buildEnv-style, but we need the same derivation to bake
    # mountpoints, so it's hand-rolled). Absolute targets into /nix/store, which
    # resolve inside the sandbox because the closure is bound there.
    for pkg in ${pkgs.bash} ${pkgs.coreutils}; do
      for f in "$pkg"/bin/*; do
        ln -sf "$f" "$out/bin/$(basename "$f")"
      done
    done

    # Hand-rolled /etc — symlinks into the store (each is a closure member, so it
    # resolves inside the sandbox). A symlink, not a copy: a copy would bake
    # opaque bytes with no store ref; the symlink makes /etc/passwd a real store
    # path that `root` references and the closure folds in.
    ln -s ${etcPasswd}   $out/etc/passwd
    ln -s ${etcGroup}    $out/etc/group
    ln -s ${etcHostname} $out/etc/hostname

    # Baked mountpoints — the skeleton the runtime mounts onto. Directories only;
    # the runtime overlays proc/tmpfs/binds here at launch. A read-only rootfs
    # cannot grow these at runtime, which is the whole point: the mount topology
    # is frozen at build time.
    mkdir -p $out/proc $out/dev $out/tmp $out/home/pierogi $out/root

    # One /nix/store/<hash> mountpoint per closure member, for the identity
    # binds. The mountpoint kind must match the source: a *file* store path
    # (e.g. a `writeText` /etc file) binds onto a file, a directory onto a
    # directory — this is the file-vs-dir logic that used to run at launch in
    # `scaffold_mountpoints`, now frozen into the image. The closure paths are
    # realized in the build sandbox (closureInfo pulls them in), so we can stat
    # them here. (Directory/file *names* aren't scanned as store references, so
    # these add no closure refs to `root` — its refs come from the /bin and /etc
    # symlink targets, which are exactly `deps`.)
    mkdir -p $out/nix/store
    while read -r p; do
      if [ -d "$p" ]; then
        mkdir -p "$out$p"
      else
        mkdir -p "$(dirname "$out$p")"
        touch "$out$p"
      fi
    done < ${closure}/store-paths
  '';

  # The sources spec: no `closure:` (resolve --closure stamps it). Internal
  # input only — not runnable, since there is no live-query path.
  unresolvedSpec = {
    exec = {
      executable = "bash";
      args = [ "--norc" ];
      terminal = true;
    };
    root = "${root}";
    mounts = [
      # Writable scratch, onto the baked /tmp mountpoint.
      { type = "tmpfs"; dest = "/tmp"; }
    ];
    env = {
      # /bin is root's own bin/, surfaced at / — no /nix/store/<root-hash> ref.
      PATH = "/bin";
      HOME = "/home/pierogi";
      TERM = "xterm-256color";
    };
  };
  unresolvedSpecFile =
    pkgs.writeText "spec.unresolved.json" (builtins.toJSON unresolvedSpec);

in
{
  # Stamp: supplied closure, no query. $out is the built spec.json itself. The
  # supplied closure omits `root` (it's the / mount, excluded from binds);
  # `resolve` stamps `closure = { seeds = [root], resolved = <deps closure> }`.
  # Building this also realizes the rootfs and its whole closure into the store,
  # so every bind the spec names is present.
  resolved = pkgs.runCommand "pierogi-bash-sandbox.json" { } ''
    ${pierogi}/bin/pierogi resolve ${unresolvedSpecFile} \
      --closure ${closure}/store-paths > $out
  '';
}
