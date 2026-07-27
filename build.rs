//! pierogi links no C now — it drives external *binaries* as subprocesses: the
//! `crun` runtime (`src/crun.rs`) and the `xdg-dbus-proxy` filter proxy
//! (`src/dbus.rs`). Each is pinned by an absolute-path env var the code bakes in
//! via `env!` (`$CRUN_BIN`, `$XDG_DBUS_PROXY_BIN`); Nix sets them
//! (`${pkgs.crun}/bin/crun`, `${pkgs.xdg-dbus-proxy}/bin/xdg-dbus-proxy`), the
//! devShell too. Declaring the deps here makes cargo rebuild if a pinned path
//! changes, and Nix's reference scan turns them into runtime dependencies.

fn main() {
    for var in ["CRUN_BIN", "XDG_DBUS_PROXY_BIN"] {
        std::env::var(var).unwrap_or_else(|_| {
            panic!("{var} not set — build inside `nix develop` (it pins the binary path)")
        });
        println!("cargo:rerun-if-env-changed={var}");
    }
}
