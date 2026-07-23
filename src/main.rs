//! Pierogi — a Nix-focused namespace runtime.
//!
//! Embeds an OCI runtime (libcrun) in-process and drives it from Rust to
//! namespace + `exec` a process over a **sealed `/nix/store` rootfs** (see
//! `docs/spec.md`). One backend, crun (via the FFI shim in `csrc/shim.c`):
//! `libcrun_container_load_from_memory` takes the spec as an in-memory JSON
//! string, so nothing touches disk — the core reason for the libcrun pivot, and
//! why youki (which reads `config.json` off disk and can't RO-remount a store
//! rootfs) was dropped.
//!
//! Spike findings still load-bearing:
//! * **devices.** Don't put device nodes in `mounts[]` (bind of a char device
//!   via the generic mount path → EINVAL). We mount a `/dev` tmpfs and let the
//!   runtime populate its default device nodes.
//! * **cgroups.** Work via systemd user-session delegation (rootless).

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
    /// first with `pierogi resolve … --graph` (or a Nix builder).
    ///
    /// CLI overrides overlay onto the loaded spec before it runs (the `run`
    /// bridge): env, hostname, argv, and additive host/tmpfs mounts — all
    /// runtime-only. The closure is sealed at build time, so there is no
    /// `--mount store:`: a different closure means a different image.
    Run {
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

    /// Build a spec: traverse the reference graph from the spec's seeds and
    /// stamp the resulting `closure:`, then print the frozen spec. No live
    /// `nix-store` query — the graph is supplied. Driven by the Nix builder that
    /// emits the spec (via `exportReferencesGraph`).
    Resolve {
        /// Path to a pierogi `spec.json`.
        spec: PathBuf,

        /// A Nix `exportReferencesGraph` dump of the seeds' reference graph.
        /// pierogi traverses it from the spec's seeds. Required.
        #[arg(long)]
        graph: PathBuf,
    },
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Run {
            env,
            hostname,
            mount,
            spec,
            argv,
        } => {
            let overrides = overrides::Overrides::parse(env, hostname, mount, argv)?;
            run::run_spec(spec, overrides)
        }
        Cmd::Resolve { spec, graph } => run::resolve_cmd(spec, graph),
    }
}
