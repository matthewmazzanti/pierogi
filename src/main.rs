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

mod oci;
mod overrides;
mod resolve;
mod run;
mod runtime;
mod spec;
mod tty;

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
    /// Run a sandbox from a **pre-baked** spec: `pierogi run <spec.json>`. The
    /// spec must carry its `closure:` — there is no runtime Nix query. Build one
    /// first with `pierogi resolve … --closure` (or a Nix builder).
    ///
    /// CLI overrides overlay onto the loaded spec before it runs (the `run`
    /// bridge): env, hostname, argv, and additive host/tmpfs mounts — all
    /// runtime-only. The closure is sealed at build time, so there is no
    /// `--mount store:`: a different closure means a different image.
    Run {
        /// Runtime backend: `crun` (default, needs the `crun` build feature) or
        /// `youki`. Omit to use the default.
        #[arg(long)]
        runtime: Option<String>,

        /// Override/insert an env var (repeatable): `--env KEY=VALUE`.
        #[arg(long = "env", value_name = "KEY=VALUE")]
        env: Vec<String>,

        /// Override the UTS hostname.
        #[arg(long, value_name = "NAME")]
        hostname: Option<String>,

        /// Add a mount (repeatable): `host:SRC:DEST[:rw|:ro]` or `tmpfs:DEST`.
        /// The dest must already be a baked mountpoint in the sealed `root`.
        /// `store:…` is not supported (the closure is frozen at build time).
        #[arg(long = "mount", value_name = "SPEC")]
        mount: Vec<String>,

        /// Path to a pierogi `spec.json`.
        spec: PathBuf,

        /// Override the exec: everything after `--` becomes the argv (the first
        /// element is the executable, resolved within the spec's closure).
        #[arg(last = true, value_name = "CMD")]
        argv: Vec<String>,
    },

    /// Build a spec: stamp the supplied `--closure` into `closure:` and print
    /// the frozen spec. Supplied only — no live `nix-store` query (pierogi is
    /// pre-bake). Driven by the Nix builder that emits the spec.
    Resolve {
        /// Path to a pierogi `spec.json`.
        spec: PathBuf,

        /// A file of store paths (one per line, `closureInfo` `store-paths`
        /// shape) supplying the closure. Required.
        #[arg(long)]
        closure: PathBuf,
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
        Cmd::Run {
            runtime,
            env,
            hostname,
            mount,
            spec,
            argv,
        } => {
            let backend = runtime::Backend::select(runtime.as_deref())?;
            let overrides = overrides::Overrides::parse(env, hostname, mount, argv)?;
            run::run_spec(spec, backend, overrides)
        }
        Cmd::Resolve { spec, closure } => run::resolve_cmd(spec, closure),
    }
}
