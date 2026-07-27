//! pierogi links no C now — it drives the `crun` *binary* as a subprocess (see
//! `src/runtime/crun.rs`). The one build-time input is `$CRUN_BIN`, the absolute
//! path to that binary, which `crun.rs` bakes in via `env!`. Nix sets it
//! (`CRUN_BIN=${pkgs.crun}/bin/crun`); the devShell sets it too. Declaring the
//! dependency here makes cargo rebuild if the pinned path changes.

fn main() {
    std::env::var("CRUN_BIN")
        .expect("CRUN_BIN not set — build inside `nix develop` (it pins the crun binary path)");
    println!("cargo:rerun-if-env-changed=CRUN_BIN");
}
