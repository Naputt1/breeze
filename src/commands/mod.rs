//! Command dispatch.

pub mod diff;
pub mod doctor;
pub mod edit;
pub mod env;
pub mod fog;
pub mod gc;
pub mod guard;
pub mod init;
pub mod link;
pub mod promote;
pub mod revert;
pub mod run;
pub mod status;
pub mod sync;
pub mod unlink;

use std::path::PathBuf;

use crate::cli::{Cli, Command};
use crate::exit;

/// Flags shared by every command implementation.
pub struct Ctx {
    pub json: bool,
    pub quiet: bool,
    pub repo: Option<PathBuf>,
}

pub fn dispatch(cli: Cli) -> anyhow::Result<i32> {
    let Cli { common, command } = cli;
    let ctx = Ctx {
        json: common.json,
        quiet: common.quiet,
        repo: common.repo,
    };

    match command {
        Command::Init => init::run(&ctx),
        Command::Status => status::run(&ctx),
        Command::Link(_) => link::run(&ctx),
        Command::Unlink(_) => unlink::run(&ctx),
        Command::Edit(_) => edit::run(&ctx),
        Command::Sync(_) => sync::run(&ctx),
        Command::Diff(_) => diff::run(&ctx),
        Command::Revert(_) => revert::run(&ctx),
        Command::Promote(_) => promote::run(&ctx),
        Command::Env(_) => env::run(&ctx),
        Command::Run(_) => run::run(&ctx),
        Command::Fog(args) => fog::run(&ctx, &args),
        Command::Guard(_) => guard::run(&ctx),
        Command::Doctor(_) => doctor::run(&ctx),
        Command::Gc => gc::run(&ctx),
        Command::Version => {
            println!("breeze {}", env!("CARGO_PKG_VERSION"));
            Ok(exit::SUCCESS)
        }
    }
}

/// Phase 0 placeholder for commands implemented in later phases.
pub fn not_implemented() -> anyhow::Result<i32> {
    Err(exit::CommandError::new(exit::USAGE, "not implemented yet").into())
}
