//! libcrun backend: hand the spec to `csrc/shim.c` as an in-memory JSON string.
//!
//! No `config.json` on disk — `libcrun_container_load_from_memory` parses the
//! JSON directly. `root.path` is the **absolute** `root` store path (set by
//! `oci::build_spec`), so it's cwd-independent and needs no rewrite here — the
//! historical `bundle/rootfs` override is gone. The shim's `libcrun_container_
//! run` blocks (detach=false) and returns the exit code.

use std::ffi::{CStr, CString};
use std::os::raw::c_char;
use std::path::Path;

use anyhow::{bail, Context, Result};
use oci_spec::runtime::Spec;

extern "C" {
    /// Returns the container exit code (>= 0) or a negative error; on error,
    /// `*err_out` is a malloc'd message we must `libc::free`. `console_socket`
    /// is NULL unless the spec requests a terminal.
    fn pierogi_crun_run(
        json: *const c_char,
        state_root: *const c_char,
        bundle: *const c_char,
        id: *const c_char,
        console_socket: *const c_char,
        err_out: *mut *mut c_char,
    ) -> i32;
}

pub fn run(
    spec: &Spec,
    id: &str,
    bundle: &Path,
    state: &Path,
    console_socket: Option<&Path>,
) -> Result<i32> {
    // `root.path` is already the absolute `root` store path (set in
    // `oci::build_spec`), so it doesn't depend on cwd — no rewrite needed.
    let json = serde_json::to_string(spec).context("serializing spec to JSON")?;

    let c_json = CString::new(json).context("spec JSON had an interior NUL")?;
    let c_state = CString::new(state.to_string_lossy().as_bytes())?;
    let c_bundle = CString::new(bundle.to_string_lossy().as_bytes())?;
    let c_id = CString::new(id)?;
    // NULL → no terminal; a path → crun connects and sends the pty master there.
    let c_console = console_socket
        .map(|p| CString::new(p.to_string_lossy().as_bytes()))
        .transpose()?;
    let console_ptr = c_console.as_ref().map_or(std::ptr::null(), |c| c.as_ptr());

    let mut err: *mut c_char = std::ptr::null_mut();
    let ret = unsafe {
        pierogi_crun_run(
            c_json.as_ptr(),
            c_state.as_ptr(),
            c_bundle.as_ptr(),
            c_id.as_ptr(),
            console_ptr,
            &mut err,
        )
    };

    if !err.is_null() {
        let msg = unsafe { CStr::from_ptr(err) }
            .to_string_lossy()
            .into_owned();
        unsafe { libc::free(err as *mut libc::c_void) };
        bail!("libcrun: {msg}");
    }

    Ok(ret)
}
