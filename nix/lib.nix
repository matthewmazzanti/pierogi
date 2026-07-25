# pierogi nix-lib — light-touch utilities for the sealed-rootfs "final
# resolution" layer. Two mechanical concerns, factored out of every sandbox:
#
#   * store-mount baking — the `/nix/store/<hash>` mountpoint tree a sealed root
#     must carry. The shell of it lives in `build-lib.sh` (`bake_store_mounts`);
#     here we only supply the `closureInfo` store-paths file it reads. Baking
#     must run *inside* the root builder (root is bound at `/nix/store/<self>`,
#     so the mountpoints have to be in root's own tree), so it stays a shell
#     helper the builder sources — not a wrapping derivation.
#   * resolving the spec — the `exportReferencesGraph` + `pierogi resolve --graph`
#     derivation (`resolveSpec`), byte-identical across sandboxes today.
#
# Deliberately not a module system: plain functions a sandbox imports and calls.
{ pkgs, pierogi }:

rec {
  # The sourceable shell library (store-mount baking, later the /bin + /etc
  # farms). `source ${lib.buildLib}` in a root `runCommand`, then call e.g.
  # `bake_store_mounts`. Referenced as a path → copied to the store, zero
  # Nix-string escaping.
  buildLib = ./build-lib.sh;

  # The `store-paths` file for a dep set — the input to `bake_store_mounts`.
  # `closureInfo` folds each dep's own closure in, so this is the full set of
  # `/nix/store/<hash>` mountpoints the sealed root needs (its own `<self>` is
  # added by `bake_store_mounts` from `$out`). Takes a dep *list*; the caller
  # never touches `closureInfo`.
  storePaths = deps: "${pkgs.closureInfo { rootPaths = deps; }}/store-paths";

  # Resolve an unresolved spec attrset into the built `spec.json`.
  #
  # `seeds` (a list of derivations / store paths) drives BOTH sides of the
  # resolution so they can't drift: it becomes the `exportReferencesGraph` root
  # Nix expands, AND — injected here — the spec's `closure.seeds` that
  # `pierogi resolve` traverses from. The producer declares the seeds once
  # (conventionally `[ root ]`, plus any overlay-lower sources) and this guarantees
  # every baked mountpoint's path is exactly what gets resolved.
  resolveSpec = { name, spec, seeds }:
    let
      spec' = spec // {
        closure = (spec.closure or { }) // { seeds = map toString seeds; };
      };
      file = pkgs.writeText "${name}.unresolved.json" (builtins.toJSON spec');
    in
    pkgs.runCommand "${name}.json"
      {
        __structuredAttrs = true;
        exportReferencesGraph.closure = seeds;
      } ''
      ${pierogi}/bin/pierogi resolve ${file} --graph "$NIX_ATTRS_JSON_FILE" > "''${outputs[out]}"
    '';
}
