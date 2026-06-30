//! Compile the libcrun FFI shim and link libcrun — only under the `crun`
//! feature, so the default (youki-only) build needs no C toolchain or CRUN_DEV.
//!
//! The harvested crun dev tree lives at $CRUN_DEV (exported by the flake's
//! devShell). We need two include roots from it:
//!   - include/crun     for <crun/container.h>, <config.h>
//!   - include          for <ocispec/...> that container.h pulls in
//! yajl's headers (container.h does `#include <yajl/yajl_tree.h>`) arrive via
//! NIX_CFLAGS_COMPILE, which crunDev propagates and the nix cc-wrapper injects —
//! so the `cc` crate's compiler picks them up without an explicit -I here.
//!
//! libcrun is built --enable-shared, so a single `-lcrun` resolves the rest
//! (seccomp/cap/criu/systemd/yajl + folded-in libocispec) via DT_NEEDED+RUNPATH.

fn main() {
    #[cfg(feature = "crun")]
    {
        let crun_dev = std::env::var("CRUN_DEV")
            .expect("CRUN_DEV not set — build the `crun` feature inside `nix develop`");

        cc::Build::new()
            .file("csrc/shim.c")
            .include(format!("{crun_dev}/include/crun"))
            .include(format!("{crun_dev}/include"))
            .compile("pierogi_crun_shim");

        println!("cargo:rustc-link-search=native={crun_dev}/lib");
        println!("cargo:rustc-link-lib=crun");
        println!("cargo:rerun-if-changed=csrc/shim.c");
        println!("cargo:rerun-if-env-changed=CRUN_DEV");
    }
}
