//! Command-line surface for Breeze.
//!
//! Phase 0 defines the whole surface so later phases only fill in
//! implementations instead of reshaping the parser.

use std::ffi::OsString;
use std::io::IsTerminal;
use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

#[derive(Debug, Parser)]
#[command(
    name = "breeze",
    version,
    about = "Local-only file overlays for Git worktrees",
    long_about = None
)]
pub struct Cli {
    #[command(flatten)]
    pub common: CommonArgs,

    #[command(subcommand)]
    pub command: Command,
}

/// Flags accepted by every subcommand.
#[derive(Debug, Clone, Args)]
pub struct CommonArgs {
    /// Emit machine-readable JSON.
    #[arg(long, global = true)]
    pub json: bool,

    /// Suppress informational output.
    #[arg(long, global = true)]
    pub quiet: bool,

    /// When to color diagnostics.
    #[arg(
        long,
        global = true,
        value_enum,
        default_value_t = ColorChoice::Auto,
        value_name = "WHEN"
    )]
    pub color: ColorChoice,

    /// Path inside the target Git repository (defaults to the current directory).
    #[arg(long, global = true, value_name = "PATH")]
    pub repo: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ColorChoice {
    Auto,
    Always,
    Never,
}

impl ColorChoice {
    /// Whether diagnostics should use ANSI color.
    pub fn should_color(self) -> bool {
        match self {
            Self::Always => true,
            Self::Never => false,
            Self::Auto => std::io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none(),
        }
    }
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Create the Breeze store for this repository.
    Init,

    /// Show repository, worktree and overlay status.
    Status,

    /// Link a file into an overlay.
    Link(LinkArgs),

    /// Remove a linked file from an overlay.
    Unlink(UnlinkArgs),

    /// Edit the overlay copy of a file.
    Edit(EditArgs),

    /// Apply overlays to the working tree.
    Sync(SyncArgs),

    /// Show differences between overlays and the working tree.
    Diff(DiffArgs),

    /// Revert a file to its tracked content.
    Revert(RevertArgs),

    /// Promote an overlay file into tracked content.
    Promote(PromoteArgs),

    /// Print or write resolved environment variables.
    Env(EnvArgs),

    /// Run a command with the resolved environment.
    Run(RunArgs),

    /// Forward arguments to fog.
    Fog(FogArgs),

    /// Manage the Git hook guard.
    Guard(GuardArgs),

    /// Check the store and environment for problems.
    Doctor(DoctorArgs),

    /// Remove unused store data.
    Gc,

    /// Print version information.
    Version,
}

#[derive(Debug, Args)]
#[allow(dead_code)] // Consumed by Phase 1+ implementations.
pub struct LinkArgs {
    /// Repository-relative path to link.
    #[arg(value_name = "PATH")]
    pub path: PathBuf,

    /// Overlay layer to write to.
    #[arg(long, value_name = "LAYER")]
    pub layer: Option<String>,

    /// Apply the link to every worktree.
    #[arg(long)]
    pub all_worktrees: bool,
}

#[derive(Debug, Args)]
#[allow(dead_code)] // Consumed by Phase 1+ implementations.
pub struct UnlinkArgs {
    /// Repository-relative path to unlink.
    #[arg(value_name = "PATH")]
    pub path: PathBuf,

    /// Remove the link even if it was modified locally.
    #[arg(long)]
    pub force: bool,
}

#[derive(Debug, Args)]
#[allow(dead_code)] // Consumed by Phase 1+ implementations.
pub struct EditArgs {
    /// Repository-relative path to edit.
    #[arg(value_name = "PATH")]
    pub path: PathBuf,

    /// Overlay layer to edit.
    #[arg(long, value_name = "LAYER")]
    pub layer: Option<String>,

    /// Editor command to use instead of $EDITOR.
    #[arg(long, value_name = "CMD")]
    pub editor: Option<String>,
}

#[derive(Debug, Args)]
#[allow(dead_code)] // Consumed by Phase 1+ implementations.
pub struct SyncArgs {
    /// Sync every known link.
    #[arg(long)]
    pub all: bool,

    /// Sync only this path.
    #[arg(long, value_name = "PATH")]
    pub path: Option<PathBuf>,

    /// Report what would change without writing anything.
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Debug, Args)]
#[allow(dead_code)] // Consumed by Phase 1+ implementations.
pub struct DiffArgs {
    /// Limit the diff to this path.
    #[arg(value_name = "PATH")]
    pub path: Option<PathBuf>,

    /// Show a diffstat instead of a full diff.
    #[arg(long)]
    pub stat: bool,
}

#[derive(Debug, Args)]
#[allow(dead_code)] // Consumed by Phase 1+ implementations.
pub struct RevertArgs {
    /// Repository-relative path to revert.
    #[arg(value_name = "PATH")]
    pub path: PathBuf,

    /// Discard local modifications without prompting.
    #[arg(long)]
    pub force: bool,
}

#[derive(Debug, Args)]
#[allow(dead_code)] // Consumed by Phase 1+ implementations.
pub struct PromoteArgs {
    /// Repository-relative path to promote.
    #[arg(value_name = "PATH")]
    pub path: PathBuf,
}

#[derive(Debug, Args)]
#[allow(dead_code)] // Consumed by Phase 1+ implementations.
pub struct EnvArgs {
    /// Output format.
    #[arg(
        long,
        value_enum,
        default_value_t = EnvFormat::Dotenv,
        value_name = "FORMAT"
    )]
    pub format: EnvFormat,

    /// Write the resolved environment to the overlay env file.
    #[arg(long)]
    pub write: bool,

    /// Show values without masking secrets.
    #[arg(long)]
    pub show: bool,

    /// Reveal secret values.
    #[arg(long)]
    pub reveal: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum EnvFormat {
    Dotenv,
    Shell,
    Json,
}

#[derive(Debug, Args)]
#[allow(dead_code)] // Consumed by Phase 1+ implementations.
pub struct RunArgs {
    /// Command and arguments, after `--`.
    #[arg(last = true, required = true, num_args = 1.., value_name = "CMD")]
    pub cmd: Vec<OsString>,
}

#[derive(Debug, Args)]
#[allow(dead_code)] // Consumed by Phase 1+ implementations.
pub struct FogArgs {
    /// Arguments passed through to fog.
    #[arg(
        trailing_var_arg = true,
        allow_hyphen_values = true,
        num_args = 0..,
        value_name = "ARGS"
    )]
    pub args: Vec<OsString>,
}

#[derive(Debug, Args)]
#[allow(dead_code)] // Consumed by Phase 1+ implementations.
pub struct GuardArgs {
    #[command(subcommand)]
    pub command: GuardCommand,
}

#[derive(Debug, Subcommand)]
#[allow(dead_code)] // Consumed by Phase 1+ implementations.
pub enum GuardCommand {
    /// Install the guard hooks.
    Install,

    /// Remove the guard hooks.
    Uninstall,

    /// Show guard status.
    Status,
}

#[derive(Debug, Args)]
#[allow(dead_code)] // Consumed by Phase 1+ implementations.
pub struct DoctorArgs {
    /// Attempt to repair detected problems.
    #[arg(long)]
    pub fix: bool,
}
