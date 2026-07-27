#!/usr/bin/env bash
#
# bench.sh — measure sealed-rootfs container startup time with hyperfine.
#
# Runs a trivial target (`true`) so wall time is dominated by namespace/rootfs
# setup + teardown — the thing pierogi exists to make fast. The sandbox is the
# pre-baked bash-sandbox (a sealed store-path rootfs); there is no live closure
# query to skip anymore — the closure is stamped in at build time, so what's
# left is exactly the runtime backend cost.
#
# Head-to-head: the same pierogi run under both crun backends, flipped by
# $PIEROGI_BACKEND — everything upstream (spec build, store scaffold, state dir)
# is identical, so the delta is exactly the process boundary:
#   * lib  — libcrun linked in-process (the shim; spec stays in memory)
#   * exec — fork+exec the crun *binary* ($CRUN_DEV/bin/crun, the same build),
#            which reads the spec back off a materialized config.json
# hyperfine's closing "N ± M times faster" line is the cost of dropping the
# embedded libcrun.
#
# youki isn't in the mix: it cannot RO-remount a /nix/store rootfs (EPERM — see
# todo.md), so both arms here are crun.
#
# Run inside `nix develop` (crun needs $CRUN_DEV + a C toolchain; hyperfine and
# nix come from the devShell):
#
#   ./bench.sh            # 50 runs
#   ./bench.sh 200        # 200 runs
#
set -euo pipefail

RUNS=${1:-50}

# Build the runtime binary once, release: debug + per-invocation `cargo run`
# overhead would swamp the millisecond-scale startup we're measuring.
cargo build --release >/dev/null
BIN=$PWD/target/release/pierogi

# The pre-baked sealed spec (rootfs + stamped closure). Built via Nix, which
# realizes the whole rootfs closure into the store so every bind is present.
SEALED=$(nix build .#bash-sandbox-spec --no-link --print-out-paths)

# Bench a non-interactive `true`: flip the baked spec's entrypoint to a trivial,
# no-terminal target so the timed loop is pure sandbox setup/teardown (no tty
# relay, no shell). The rest of process (env / user / caps) is left intact.
SPEC=$(mktemp --suffix=.pierogi.json)
trap 'rm -f "$SPEC"' EXIT
python3 - "$SEALED" "$SPEC" <<'PY'
import json, sys
spec = json.load(open(sys.argv[1]))
spec["process"].update(executable="true", args=[], terminal=False)
json.dump(spec, open(sys.argv[2], "w"))
PY

echo "rootfs:  $(python3 -c "import json;print(json.load(open('$SEALED'))['root']['path'])")"
echo "closure: $(python3 -c "import json;print(len(json.load(open('$SEALED'))['closure']['resolved']))") binds (stamped at build)"
echo

hyperfine \
  --warmup 5 \
  --runs "$RUNS" \
  -n 'lib  (embedded libcrun)' "PIEROGI_BACKEND=lib  $BIN run $SPEC" \
  -n 'exec (crun binary)'      "PIEROGI_BACKEND=exec $BIN run $SPEC"
