//! Pierogi — **M0 shell-run spike**.
//!
//! Goal: prove we can embed an OCI runtime in-process and drive it from Rust to
//! namespace + `exec` a real shell, over a **read-only rootfs assembled from the
//! Nix closure** of the target binary. Deliberately crude — no synthetic `/etc`,
//! no strict defaults, no nice spec format. See `docs/roadmap.md` (M0).
//!
//! Status: **working** on both backends (`--runtime`): youki (`libcontainer`)
//! and crun (libcrun via the FFI shim in `csrc/shim.c`). Findings:
//!
//! * **config.json on disk.** youki's public API reads `config.json` from a
//!   bundle dir, so the youki backend serializes the in-memory spec out. The
//!   crun backend skips this — `libcrun_container_load_from_memory` takes the
//!   spec as a JSON string, so nothing touches disk. This was a core reason for
//!   the libcrun pivot.
//! * **rootfs path resolution differs.** youki resolves `root.path` against the
//!   bundle dir; libcrun resolves it against cwd. The crun backend rewrites
//!   `root.path` to an absolute path before handing the spec over.
//! * **devices.** Don't put device nodes in `mounts[]` (bind of a char device
//!   via the generic mount path → EINVAL). youki's `default_devices()`
//!   populates `/dev` itself (bind in rootless mode); we just mount `/dev`
//!   tmpfs and let it.
//! * **read-only root needs the bundle off a `nosuid,nodev` mount.** youki
//!   remounts `/` RO as a bare `MS_RDONLY|MS_REMOUNT|MS_BIND` without
//!   re-supplying the flags the kernel locks on a userns's mounts, so a bundle
//!   on `XDG_RUNTIME_DIR` (a `nosuid,nodev` tmpfs) fails with EPERM. We lay the
//!   bundle under `$TMPDIR` instead. See `run::runtime_dir`.
//! * **cgroups.** Worked via systemd user-session delegation (rootless).

mod closure;
mod run;
mod runtime;
mod spec;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "pierogi", about = "Nix-focused namespace runtime (M0 spike)")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run a command in a namespaced, read-only sandbox built from its Nix
    /// closure, e.g. `pierogi run -- bash`.
    Run {
        /// Runtime backend: `youki` (default) or `crun` (needs the `crun`
        /// build feature).
        #[arg(long, default_value = "youki")]
        runtime: String,

        /// Command and arguments (everything after `--`).
        #[arg(trailing_var_arg = true, required = true, allow_hyphen_values = true)]
        argv: Vec<String>,
    },
}

fn main() -> Result<()> {
    // Surface the runtime's `tracing` logs. Default to `warn`; override with
    // e.g. `RUST_LOG=libcontainer=debug` to see which mount fails.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();

    match Cli::parse().cmd {
        Cmd::Run { runtime, argv } => {
            let backend = runtime::Backend::parse(&runtime)?;
            run::run(argv, backend)
        }
    }
}
