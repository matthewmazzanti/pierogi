/* Rust <-> libcrun bridge.
 *
 * One entry point: load an OCI spec from an in-memory JSON string and run it to
 * completion. This is the whole point of the libcrun pivot over youki — youki's
 * public builder reads `config.json` off disk, libcrun takes the JSON directly
 * via `libcrun_container_load_from_memory`, so no bundle round-trip.
 *
 * `libcrun_context_s` is a *transparent* public struct (see <crun/container.h>),
 * so we populate it field-by-field here rather than through any opaque init.
 * The one thing that isn't a plain field is the output/log handler — set it via
 * the public `libcrun_init_logging` (NULL log target → default stderr).
 *
 * `libcrun_container_run` with `detach=false` forks the container, waits, and
 * returns its exit status — so the caller does not waitpid; this blocks.
 *
 * Error contract: on failure we strdup/format crun's `err->msg` into `*err_out`
 * (caller frees with libc::free) and return < 0. On success `*err_out` is NULL.
 */
#include <crun/container.h>
#include <crun/error.h>

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

/* Format "<ctx>: <crun msg>" into *out (heap, caller frees), release err. */
static void
take_error (char **out, libcrun_error_t err, const char *ctx)
{
  if (err && err->msg)
    {
      int n = snprintf (NULL, 0, "%s: %s", ctx, err->msg);
      if (n > 0)
        {
          char *buf = malloc ((size_t) n + 1);
          if (buf)
            snprintf (buf, (size_t) n + 1, "%s: %s", ctx, err->msg);
          *out = buf;
        }
    }
  else
    {
      *out = strdup (ctx);
    }
  if (err)
    libcrun_error_release (&err);
}

int
pierogi_crun_run (const char *json, const char *state_root, const char *bundle,
                  const char *id, const char *console_socket, char **err_out)
{
  libcrun_error_t err = NULL;
  *err_out = NULL;

  libcrun_container_t *container = libcrun_container_load_from_memory (json, &err);
  if (container == NULL)
    {
      take_error (err_out, err, "libcrun_container_load_from_memory");
      return -1;
    }

  libcrun_context_t ctx;
  memset (&ctx, 0, sizeof (ctx));
  ctx.state_root = state_root;
  ctx.id = id;
  ctx.bundle = bundle; /* root.path in the spec is resolved relative to this */
  /* NULL unless the spec set process.terminal: then crun connects to this
   * AF_UNIX path and sends the pty master fd, which our Rust `tty` relay (the
   * listener at the other end) receives and pumps against the caller's stdio. */
  ctx.console_socket = console_socket;
  ctx.fifo_exec_wait_fd = -1;
  ctx.detach = false;
  ctx.systemd_cgroup = false;

  if (libcrun_init_logging (&ctx.output_handler, &ctx.output_handler_arg, id,
                            NULL, &err)
      < 0)
    {
      take_error (err_out, err, "libcrun_init_logging");
      return -1;
    }

  int ret = libcrun_container_run (&ctx, container, 0, &err);
  if (ret < 0)
    {
      take_error (err_out, err, "libcrun_container_run");
      return ret;
    }
  return ret;
}
