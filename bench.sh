#!/usr/bin/env bash
#
# bench.sh — compare container startup time between backends with hyperfine.
#
# Runs a trivial target (`true`) so wall time is dominated by namespace/rootfs
# setup + teardown — the thing pierogi exists to make fast. The target's closure
# is precomputed once and passed via $PIEROGI_CLOSURE, so the timed loop skips
# the `nix-store --query --requisites` call (tens of ms) and measures the
# runtime backend in isolation.
#
# Run inside `nix develop` (crun needs $CRUN_DEV + a C toolchain; hyperfine
# comes from the devShell):
#
#   ./bench.sh            # 50 runs per backend
#   ./bench.sh 200        # 200 runs per backend
#
set -euo pipefail

RUNS=${1:-50}

# Build both backends once, release: debug + per-invocation `cargo run` overhead
# would swamp the millisecond-scale startup we're measuring.
cargo build --release --features crun >/dev/null

BIN=$PWD/target/release/pierogi
# `type -P` finds the real coreutils binary, not bash's `true` builtin.
TARGET=$(type -P true)

# Precompute the closure once and stub it in via the env var. STORE_ROOT is the
# top-level store dir of the canonicalized target.
STORE_EXE=$(readlink -f "$TARGET")
STORE_ROOT=/nix/store/$(cut -d/ -f1 <<<"${STORE_EXE#/nix/store/}")
PIEROGI_CLOSURE=$(nix-store --query --requisites "$STORE_ROOT" | paste -sd:)
export PIEROGI_CLOSURE

echo "target:  $TARGET"
echo "closure: $(tr ':' '\n' <<<"$PIEROGI_CLOSURE" | wc -l) paths (precomputed; nix-store query skipped)"
echo

hyperfine \
  --warmup 5 \
  --runs "$RUNS" \
  --parameter-list runtime youki,crun \
  "$BIN run-argv --runtime={runtime} -- $TARGET"
