/* Link smoke test for libcrun.
 *
 * Proves we can #include <crun/container.h> and link libcrun.a + libocispec.a
 * + transitive deps. Calls libcrun_container_spec(), which writes a default OCI
 * config to a FILE* and needs zero setup — the cheapest real symbol to call.
 *
 * Build (inside `nix develop`, with $CRUN_DEV from the flake):
 *
 *   cc csrc/smoke.c \
 *     -I"$CRUN_DEV/include" -I"$CRUN_DEV/include/crun" \
 *     -Wl,--start-group \
 *       "$CRUN_DEV/lib/libcrun.a" "$CRUN_DEV/lib/libocispec.a" \
 *       -lyajl -lseccomp -lcap -lcriu -lsystemd \
 *     -Wl,--end-group \
 *     -lm -lpthread -ldl \
 *     -o /tmp/crun-smoke
 *
 * This exact recipe (start-group around the two static libs, then the
 * yajl/seccomp/cap/criu/systemd chain) is what the build.rs will emit. */
#include <crun/container.h>

#include <stdbool.h>
#include <stdio.h>

int
main (void)
{
  libcrun_error_t err = NULL;
  int ret = libcrun_container_spec (true, stdout, &err);
  fprintf (stderr, "\n[smoke] libcrun_container_spec returned %d — libcrun linked OK\n", ret);
  return ret < 0 ? 1 : 0;
}
