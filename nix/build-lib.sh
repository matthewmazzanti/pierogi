# pierogi build-lib — sourceable shell helpers for the sealed-rootfs builders.
#
# `source ${./build-lib.sh}` inside a root `runCommand`, then call these. They
# operate on the ambient `$out` of the calling derivation (the non-structuredAttrs
# root builders expose it directly). No `set -e` here — nix builders already run
# under errexit, and a lib shouldn't impose shell options on its caller.

# bin_farm <pkg>...
#
# Symlink each package's `bin/*` into `$out/bin`, with absolute targets into
# /nix/store (they resolve inside the sandbox because the closure is bound
# there). Fail loud on a collision — two packages providing the same command
# name with *different* targets — the way `buildEnv` does, rather than silently
# overwriting (`ln -sf`, last wins) or skipping (`lndir -silent`, first wins). A
# curated tool list shouldn't collide; if it does, that's a bug to surface, not
# paper over. (The same target twice — a tool listed more than once — is
# idempotent, not a collision.)
bin_farm() {
  mkdir -p "$out/bin"
  local pkg f name dest have
  for pkg in "$@"; do
    for f in "$pkg"/bin/*; do
      [ -e "$f" ] || continue                 # empty/absent bin/ — glob didn't match
      name=$(basename "$f")
      dest="$out/bin/$name"
      if [ -L "$dest" ]; then
        have=$(readlink "$dest")
        [ "$have" = "$f" ] && continue         # same target — idempotent, fine
        echo "bin_farm: /bin/$name collision:" >&2
        echo "  have $have" >&2
        echo "  want $f" >&2
        exit 1
      fi
      ln -s "$f" "$dest"
    done
  done
}

# bake_dirs <dir>...
#
# Bake a mountpoint directory under `$out` for each arg (an absolute in-sandbox
# path like `/proc` or `/home/pierogi`). The read-only rootfs can't grow these
# at launch, so every mount's target has to be frozen into the image here. Pair
# this with the mounts the spec declares — a mount whose target isn't baked
# fails at launch. (A mount onto a writable tmpfs — e.g. `/dev/pts` onto the
# `/dev` tmpfs — is the exception: it's created at runtime, so it needs no dir.)
bake_dirs() {
  for d in "$@"; do
    mkdir -p "$out$d"
  done
}
