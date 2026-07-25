# pierogi build-lib — sourceable shell helpers for the sealed-rootfs builders.
#
# `source ${./build-lib.sh}` inside a root `runCommand`, then call these. They
# operate on the ambient `$out` of the calling derivation (the non-structuredAttrs
# root builders expose it directly). No `set -e` here — nix builders already run
# under errexit, and a lib shouldn't impose shell options on its caller.

# bake_store_mounts <store-paths-file>
#
# Bake one `/nix/store/<hash>` mountpoint per closure member listed in
# <store-paths-file> (a `closureInfo`/`store-paths` file), matching the source
# kind — a directory store path gets a directory mountpoint, a file store path
# (e.g. a `writeText` /etc file) gets a file mountpoint — because a bind's target
# kind must match its source. Then bake root's OWN identity mountpoint at
# `/nix/store/<self>` (from `$out`, input-addressed before the builder runs), so
# root is identity-bound at its store path while also being the `/` mount.
#
# This is the file-vs-dir logic that used to run at launch in the engine's
# `scaffold_mountpoints`, now frozen into the image.
bake_store_mounts() {
  local store_paths="$1"
  mkdir -p "$out/nix/store"
  while read -r p; do
    if [ -d "$p" ]; then
      mkdir -p "$out$p"
    else
      mkdir -p "$(dirname "$out$p")"
      touch "$out$p"
    fi
  done < "$store_paths"
  mkdir -p "$out/nix/store/$(basename "$out")"
}
