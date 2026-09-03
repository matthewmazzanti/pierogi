//! Pierogi — a Nix-focused namespace runtime.
//!
//! Drives an OCI runtime (the `crun` binary) from Rust to namespace + `exec` a
//! process over a **sealed `/nix/store` rootfs** (see `docs/spec.md`). One
//! backend: pierogi writes the OCI spec to a `config.json` and execs `crun`
//! (`src/crun.rs`). youki was evaluated and dropped — it can't RO-remount
//! a store rootfs; an earlier design linked libcrun in-process to keep the spec
//! in memory, dropped once benchmarking showed the process boundary costs only a
//! few milliseconds (see `docs/startup-performance.md`).
//!
//! Spike findings still load-bearing:
//! * **devices.** Don't put device nodes in `mounts[]` (bind of a char device
//!   via the generic mount path → EINVAL). We mount a `/dev` tmpfs and let the
//!   runtime populate its default device nodes.
//! * **cgroups.** Work via systemd user-session delegation (rootless).

mod crun;
mod dbus;
mod oci;
mod overrides;
mod resolve;
mod run;
mod spec;
mod tty;
mod net;
mod unshare;

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
    /// spec must carry its built `closure.resolved` — there is no runtime Nix
    /// query. Build one first with `pierogi resolve … --graph` (or a Nix builder).
    ///
    /// CLI overrides overlay onto the loaded spec before it runs (the `run`
    /// bridge): env, hostname, process args, and additive host/tmpfs mounts —
    /// all runtime-only. The executable is not overridable (the image seals the
    /// entrypoint). The closure is sealed at build time, so there is no
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

        /// Override the process args: everything after `--` replaces the spec's
        /// `process.args`. The executable stays the spec's — sealed by the image.
        #[arg(last = true, value_name = "ARG")]
        argv: Vec<String>,
    },

    /// Run a command inside pierogi's persistent **user + mount namespace**,
    /// starting the pause process that holds it if one isn't live
    /// (`pierogi unshare [cmd…]`, default `$SHELL`).
    ///
    /// The namespace carries the invoker's `/etc/subuid` range, mapped `0 →
    /// <uid>` plus the subordinate block, so a joined command runs as uid 0 with
    /// a full capability set *inside* it — and none outside.
    ///
    /// A rootless user namespace has no filesystem identity and cannot be pinned
    /// by a path, so a live process holding it is the only persistence mechanism
    /// available. The pause process is exactly that and nothing more.
    Unshare {
        /// The command to run inside the namespace; defaults to `$SHELL`.
        #[arg(
            trailing_var_arg = true,
            allow_hyphen_values = true,
            value_name = "CMD"
        )]
        argv: Vec<String>,
    },

    /// Manage pinned network namespaces in the pause process's registry.
    Net {
        #[command(subcommand)]
        cmd: NetCmd,
    },

    /// (internal) Join the pause namespace and pin a network there. Never
    /// user-typed: it exists so the join can happen at the top of `main`, while
    /// the process is still single-threaded, as `setns(CLONE_NEWUSER)` requires.
    #[command(hide = true, name = "__net-create")]
    NetCreate {
        /// The pause process to join.
        pin_pid: i32,
        /// Absolute path of the pin, inside the registry.
        path: PathBuf,
    },

    /// Build a spec: traverse the reference graph from the spec's
    /// `closure.seeds` and stamp the resulting `closure.resolved`, then print the
    /// frozen spec. No live `nix-store` query — the graph is supplied. Driven by
    /// the Nix builder that emits the spec (via `exportReferencesGraph`).
    Resolve {
        /// Path to a pierogi `spec.json`.
        spec: PathBuf,

        /// A Nix `exportReferencesGraph` dump of the seeds' reference graph.
        /// pierogi traverses it from the spec's `closure.seeds`. Required.
        #[arg(long)]
        graph: PathBuf,
    },
}

#[derive(Subcommand)]
enum NetCmd {
    /// Pin a network namespace at `name`. Idempotent: an existing network is
    /// reported and left alone, never replaced — replacing would strand any
    /// sandbox already attached to it.
    Create {
        /// The network's name; becomes a path component in the registry.
        name: String,
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
        Cmd::Unshare { argv } => unshare::unshare_cmd(argv),
        Cmd::Net { cmd } => match cmd {
            NetCmd::Create { name } => net::create(&name),
        },
        Cmd::NetCreate { pin_pid, path } => net::helper_create(pin_pid, path),
        Cmd::Resolve { spec, graph } => run::resolve_cmd(spec, graph),
    }
}
