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
    // Surface youki/libcontainer's `tracing` logs. Default to `warn`; override
    // with e.g. `RUST_LOG=libcontainer=debug` to see which mount fails.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();

    match Cli::parse().cmd {
        Cmd::Run { argv } => pierogi_engine::run(argv),
    }
}
