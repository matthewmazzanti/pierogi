//! Compile the libcrun FFI shim and link libcrun. crun is the only backend, so
//! this always runs — the build needs a C toolchain and the harvested libcrun
//! dev tree at $CRUN_DEV (both provided by `nix develop` / the flake).
//!
//! We need two include roots from $CRUN_DEV:
//!   - include/crun     for <crun/container.h>, <config.h>
//!   - include          for <ocispec/...> that container.h pulls in
//! yajl's headers (container.h does `#include <yajl/yajl_tree.h>`) arrive via
//! NIX_CFLAGS_COMPILE, which crunDev propagates and the nix cc-wrapper injects —
//! so the `cc` crate's compiler picks them up without an explicit -I here.
//!
//! libcrun is built --enable-shared, so a single `-lcrun` resolves the rest
//! (seccomp/cap/criu/systemd/yajl + folded-in libocispec) via DT_NEEDED+RUNPATH.

fn main() {
    let crun_dev = std::env::var("CRUN_DEV")
        .expect("CRUN_DEV not set — build inside `nix develop` (needs the libcrun dev tree)");

    cc::Build::new()
        .file("csrc/shim.c")
        // The nix cc-wrapper injects _FORTIFY_SOURCE via its hardening machinery
        // (NIX_HARDENING_ENABLE=…fortify…), but supplies no -O; FORTIFY requires
        // optimization, so the dev profile's OPT_LEVEL=0 otherwise warns. Pin -O2
        // for the shim so FORTIFY is active regardless of the Rust profile.
        .opt_level(2)
        .include(format!("{crun_dev}/include/crun"))
        .include(format!("{crun_dev}/include"))
        .compile("pierogi_crun_shim");

    println!("cargo:rustc-link-search=native={crun_dev}/lib");
    println!("cargo:rustc-link-lib=crun");
    println!("cargo:rerun-if-changed=csrc/shim.c");
    println!("cargo:rerun-if-env-changed=CRUN_DEV");
}
