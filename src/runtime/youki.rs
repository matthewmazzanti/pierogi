//! youki (`libcontainer`) backend: serialize the spec to `config.json`, then
//! create -> start -> wait in-process. See the module docs in `main.rs` for the
//! spike findings that shaped this (devices, RO-root remount, cgroups).

use std::path::Path;

use anyhow::{Context, Result};
use oci_spec::runtime::Spec;

/// Write `config.json`, build + start the container, block until it exits.
pub fn run(
    spec: &Spec,
    id: &str,
    bundle: &Path,
    state: &Path,
    console_socket: Option<&Path>,
) -> Result<i32> {
    use libcontainer::container::builder::ContainerBuilder;
    use libcontainer::syscall::syscall::SyscallType;
    use libcontainer::workload::default::DefaultExecutor;

    // youki's public builder reads config.json from the bundle, so we serialize
    // the in-memory spec here. (The crun backend skips this round-trip.)
    spec.save(bundle.join("config.json"))
        .context("writing config.json")?;

    let mut container = ContainerBuilder::new(id.to_string(), SyscallType::default())
        .with_executor(DefaultExecutor {})
        // When the spec has `terminal:true`, youki connects here and sends the
        // pty master; our `tty` relay is listening on the other end.
        .with_console_socket(console_socket)
        .with_root_path(state.to_path_buf())
        .context("setting container state path")?
        .as_init(bundle.to_path_buf())
        .with_systemd(false)
        .with_detach(false)
        .build()
        .context("creating container (namespaces, rootfs, mounts)")?;

    container.start().context("starting container")?;

    let pid = container
        .pid()
        .context("container reported no pid after start")?
        .as_raw();

    let mut status: libc::c_int = 0;
    let rc = unsafe { libc::waitpid(pid, &mut status, 0) };
    if rc < 0 {
        return Err(std::io::Error::last_os_error()).context("waitpid on container init");
    }

    // Clean up container state (ignore errors — process already exited).
    let _ = container.delete(true);

    Ok(if libc::WIFEXITED(status) {
        libc::WEXITSTATUS(status)
    } else {
        0
    })
}
