/* Link smoke test for libcrun.
 *
 * Proves we can #include <crun/container.h> and link libcrun.a + libocispec.a
 * + transitive deps. Calls libcrun_container_spec(), which writes a default OCI
 * config to a FILE* and needs zero setup — the cheapest real symbol to call.
 *
 * Build (inside `nix develop`, with $CRUN_DEV from the flake):
 *
 *   cc csrc/smoke.c -I"$CRUN_DEV/include/crun" -lcrun -o /tmp/crun-smoke
 *
 * crunDev builds libcrun with --enable-shared, so libcrun.so carries its deps
 * (yajl/seccomp/cap/criu/systemd + folded-in libocispec) as DT_NEEDED with a
 * nix RUNPATH. So a consumer links just `-lcrun`; everything else resolves
 * automatically at link and run time. (The one explicit -I is for crun's
 * internal `#include <config.h>`; the crun + yajl include roots come from the
 * wrapper via crunDev's buildInputs.) This is what build.rs will emit. */
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
