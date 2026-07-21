# M1 nix-mode artifact — the tire-kicking version.
#
# Returns two spec.json store paths for a static bash + coreutils sandbox with a
# hand-rolled `/etc`:
#
#   * `resolved`   — the **self-contained sealed** spec (closure stamped in, no
#                    runtime query). The real nix-mode artifact.
#   * `unresolved` — the sources-only spec (no `closure:`). For shell iteration:
#                    `pierogi run` on it resolves the closure with a live
#                    `nix-store --query --requisites` at run time, so you can
#                    tweak and re-run without rebuilding the sealed artifact.
#
# Deliberately explicit and hardcoded — no `mkSandbox` abstraction yet; that
# extraction comes once this pipeline is proven. The pipeline it exercises:
#
#   root (buildEnv + baked /etc)                  the filesystem seed
#     │
#     ├─ unresolved spec.json  (exec/root/mounts/env, no closure)
#     ├─ closureInfo { rootPaths = [root]; }  →  store-paths (the supplied closure)
#     │
#     └─ pierogi resolve <spec> --closure store-paths  →  $out  (resolved spec.json)
#
# No `nix-store` query runs: the build sandbox has no daemon, so the closure is
# *supplied* by `closureInfo` and `resolve` just stamps it. Nix pins the whole
# closure as this artifact's references by scanning the emitted JSON for
# `/nix/store/…` strings, so the lone spec file is self-contained.
{ pkgs, pierogi }:

let
  # The login shell we bake into /etc/passwd. A real store path (bash is in the
  # closure), not `/bin/sh` — the sandbox's `/` is an empty scaffold.
  loginShell = "${pkgs.bash}/bin/bash";

  # Hand-rolled /etc — fixed files authored as Nix strings (M1). The same files
  # become `pierogi-synth` output at M4; here they are real, hand-written bytes.
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

  # 1. root: the static target as a buildEnv, with the hand-rolled /etc baked in.
  #    `/etc` lives *inside* root; the spec exposes it at the sandbox's /etc via
  #    a host bind of `${root}/etc` (below).
  root = pkgs.buildEnv {
    name = "pierogi-bash-root";
    paths = [ pkgs.bash pkgs.coreutils ];
    # **Symlink**, don't copy. A copy bakes opaque bytes into root (no store
    # refs); a symlink makes each /etc file a real store path that root
    # *references* — so Nix folds the three writeText paths into root's closure,
    # closureInfo picks them up, and they get identity-bound under /nix/store.
    # This exercises closure resolution: /etc/passwd is a symlink into the store,
    # and it only resolves inside the sandbox *because* its target is in the
    # closure (the whole point of the store fold).
    postBuild = ''
      mkdir -p $out/etc
      ln -s ${etcPasswd}   $out/etc/passwd
      ln -s ${etcGroup}    $out/etc/group
      ln -s ${etcHostname} $out/etc/hostname
    '';
  };

  # 2. The unresolved spec: sources only, no `closure:` (resolve stamps it).
  unresolvedSpec = {
    exec = {
      executable = "bash";
      args = [ "--norc" ];
      terminal = true;
    };
    root = "${root}";
    mounts = [
      # Baked /etc, exposed at /etc. Host bind (opaque, no fold) of a store
      # subpath — legal and deliberate: root is already folded as the seed, this
      # just surfaces its /etc at the FHS location.
      { type = "host"; source = "${root}/etc"; dest = "/etc"; }
      # Writable scratch.
      { type = "tmpfs"; dest = "/tmp"; }
    ];
    env = {
      PATH = "${root}/bin";
      HOME = "/home/pierogi";
      TERM = "xterm-256color";
    };
  };
  unresolvedSpecFile =
    pkgs.writeText "spec.unresolved.json" (builtins.toJSON unresolvedSpec);

  # 3. The supplied closure of the seed(s). rootPaths must match the spec's
  #    seeds (here just `root`).
  closure = pkgs.closureInfo { rootPaths = [ root ]; };

in
{
  # The sources-only spec. Its `root` is a reference, so building this realizes
  # the closure into the store — the live `nix-store` query at run time then has
  # everything to resolve against.
  unresolved = unresolvedSpecFile;

  # 4. Stamp: supplied closure, no query. $out is the resolved spec.json itself.
  resolved = pkgs.runCommand "pierogi-bash-sandbox.json" { } ''
    ${pierogi}/bin/pierogi resolve ${unresolvedSpecFile} \
      --closure ${closure}/store-paths > $out
  '';
}
