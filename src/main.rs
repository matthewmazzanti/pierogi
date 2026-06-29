//! Pierogi — **M0 shell-run spike**.
//!
//! Goal: prove we can embed an OCI runtime in-process and drive it from Rust to
//! namespace + `exec` a real shell, over a **read-only rootfs assembled from the
//! Nix closure** of the target binary. Deliberately crude — no synthetic `/etc`,
//! no strict defaults, no nice spec format. See `docs/roadmap.md` (M0).
//!
//! Status: **working** on youki (`libcontainer`); evaluating libcrun next.
//! Findings from getting the youki path working:
//!
//! * **config.json on disk.** youki's public API reads `config.json` from a
//!   bundle dir, so we serialize the in-memory spec out. libcrun's
//!   `load_from_memory` avoids this — a reason we're evaluating it.
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
        Cmd::Run { argv } => run::run(argv),
    }
}
