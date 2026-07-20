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
mod oci;
mod resolve;
mod run;
mod runtime;
mod spec;

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "pierogi", about = "Nix-focused namespace runtime (M1)")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run a sandbox from a spec: `pierogi run <spec.json>`. If the spec carries
    /// a `closure:`, it launches with no Nix query.
    Run {
        /// Runtime backend: `crun` (default, needs the `crun` build feature) or
        /// `youki`. Omit to use the default.
        #[arg(long)]
        runtime: Option<String>,

        /// Path to a pierogi `spec.json`.
        spec: PathBuf,
    },

    /// Freeze a spec: fill `closure:` and print the resolved spec. With
    /// `--closure` the closure is *supplied* (build-time, no query); without
    /// it, a live `nix-store` query — the raw-spec resolve we keep for parity
    /// against the build-time closure.
    Resolve {
        /// Path to a pierogi `spec.json`.
        spec: PathBuf,

        /// A file of store paths (one per line, `closureInfo` `store-paths`
        /// shape) supplying the closure instead of a live `nix-store` query.
        #[arg(long)]
        closure: Option<PathBuf>,
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
        Cmd::Run { runtime, spec } => {
            let backend = runtime::Backend::select(runtime.as_deref())?;
            run::run_spec(spec, backend)
        }
        Cmd::Resolve { spec, closure } => run::resolve_cmd(spec, closure),
    }
}
