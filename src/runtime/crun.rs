//! libcrun backend: hand the spec to `csrc/shim.c` as an in-memory JSON string.
//!
//! No `config.json` on disk — `libcrun_container_load_from_memory` parses the
//! JSON directly. `root.path` in the spec stays relative (`"rootfs"`); libcrun
//! resolves it against `bundle`, which we pass as the context bundle. The shim's
//! `libcrun_container_run` blocks (detach=false) and returns the exit code.

use std::ffi::{CStr, CString};
use std::os::raw::c_char;
use std::path::Path;

use anyhow::{bail, Context, Result};
use oci_spec::runtime::Spec;

extern "C" {
    /// Returns the container exit code (>= 0) or a negative error; on error,
    /// `*err_out` is a malloc'd message we must `libc::free`.
    fn pierogi_crun_run(
        json: *const c_char,
        state_root: *const c_char,
        bundle: *const c_char,
        id: *const c_char,
        err_out: *mut *mut c_char,
    ) -> i32;
}

pub fn run(spec: &Spec, id: &str, bundle: &Path, state: &Path) -> Result<i32> {
    // youki resolves `root.path` against the bundle dir; libcrun resolves it
    // against the *cwd* (the transparent `bundle` context field is not a
    // chdir). Give crun an absolute rootfs so it doesn't depend on cwd.
    let mut spec = spec.clone();
    let mut root = spec.root().clone().context("spec has no root")?;
    root.set_path(bundle.join("rootfs"));
    spec.set_root(Some(root));

    let json = serde_json::to_string(&spec).context("serializing spec to JSON")?;

    let c_json = CString::new(json).context("spec JSON had an interior NUL")?;
    let c_state = CString::new(state.to_string_lossy().as_bytes())?;
    let c_bundle = CString::new(bundle.to_string_lossy().as_bytes())?;
    let c_id = CString::new(id)?;

    let mut err: *mut c_char = std::ptr::null_mut();
    let ret = unsafe {
        pierogi_crun_run(
            c_json.as_ptr(),
            c_state.as_ptr(),
            c_bundle.as_ptr(),
            c_id.as_ptr(),
            &mut err,
        )
    };

    if !err.is_null() {
        let msg = unsafe { CStr::from_ptr(err) }.to_string_lossy().into_owned();
        unsafe { libc::free(err as *mut libc::c_void) };
        bail!("libcrun: {msg}");
    }

    Ok(ret)
}
