mod cmd;
mod config;
mod docker;
mod envfile;
mod files;
mod git;
mod herdr;
mod hooks;
mod ports;
mod project;
mod services;
mod sh;
mod state;

use std::process::ExitCode;

use clap::{Parser, Subcommand};

/// A git worktree with its own databases and web port, cloned from the main
/// checkout's development stack.
#[derive(Parser)]
#[command(name = "wt", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create a worktree with services and a web port of its own
    ///
    /// An existing branch is checked out; an unknown one is created.
    Up {
        branch: String,
        /// Where a new branch starts from (default: HEAD)
        #[arg(long, value_name = "REF")]
        base: Option<String>,
    },
    /// Remove a worktree's containers, volumes and checkout
    ///
    /// The branch is kept unless you ask for it, and without a slug it uses the
    /// worktree you are standing in, or asks.
    Down {
        slug: Option<String>,
        /// Remove only the containers and volumes
        #[arg(long)]
        keep_checkout: bool,
        /// Also delete the branch, once the checkout is gone
        #[arg(long)]
        delete_branch: bool,
        /// Skip the confirmation prompts
        #[arg(long)]
        yes: bool,
        /// Also proceed on a dirty checkout or unlabelled resources
        #[arg(long)]
        force: bool,
    },
    /// List every worktree environment of this project
    ///
    /// STATUS: running (all containers up), partial, stopped, no-db (checkout
    /// without containers), ready (project without services), orphan
    /// (resources left behind, checkout gone: wt down <slug>).
    #[command(visible_alias = "list")]
    Ls,
    /// Write a .wt.toml from the repo's compose containers and toolchain
    Init {
        /// Overwrite an existing .wt.toml
        #[arg(long)]
        force: bool,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    // A source stopped for a copy must come back even on Ctrl-C.
    let _ = ctrlc::set_handler(|| {
        services::restore_sources();
        std::process::exit(130);
    });

    let result = match cli.command {
        Command::Up { branch, base } => cmd::up::run(&branch, base.as_deref()),
        Command::Down {
            slug,
            keep_checkout,
            delete_branch,
            yes,
            force,
        } => cmd::down::run(
            slug.as_deref(),
            cmd::down::Options {
                keep_checkout,
                delete_branch,
                yes,
                force,
            },
        ),
        Command::Ls => cmd::ls::run(),
        Command::Init { force } => cmd::init::run(force),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}
