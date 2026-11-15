//! Command dispatch.

mod archive;
pub mod completions;
pub mod diff;
pub mod doctor;
pub mod edit;
pub mod env;
pub mod filter;
pub mod fog;
pub mod gc;
pub mod guard;
pub mod init;
pub mod link;
pub mod patches;
pub mod promote;
pub mod restore;
pub mod revert;
pub mod run;
pub mod share;
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
        Command::Link(args) => link::run(&ctx, &args),
        Command::Unlink(args) => unlink::run(&ctx, &args),
        Command::Edit(args) => edit::run(&ctx, &args),
        Command::Sync(args) => sync::run(&ctx, &args),
        Command::Diff(args) => diff::run(&ctx, &args),
        Command::Revert(args) => revert::run(&ctx, &args),
        Command::Restore(args) => restore::run(&ctx, &args),
        Command::Patches => patches::run(&ctx),
        Command::Promote(args) => promote::run(&ctx, &args),
        Command::Share(args) => share::run(&ctx, &args),
        Command::Env(args) => env::run(&ctx, &args),
        Command::Run(args) => run::run(&ctx, &args),
        Command::Fog(args) => fog::run(&ctx, &args),
        Command::Guard(args) => guard::run(&ctx, &args),
        Command::Filter(args) => filter::run(&ctx, &args),
        Command::Doctor(args) => doctor::run(&ctx, &args),
        Command::Gc(args) => gc::run(&ctx, &args),
        Command::Version => {
            println!("breeze {}", env!("CARGO_PKG_VERSION"));
            Ok(exit::SUCCESS)
        }
        Command::Completions(args) => completions::run(&args),
    }
}
