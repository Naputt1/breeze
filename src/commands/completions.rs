//! Hidden `breeze completions <shell>` command.

use std::io;

use anyhow::Result;
use clap::CommandFactory;
use clap_complete::generate;

use crate::cli::{Cli, CompletionsArgs};
use crate::exit;

pub fn run(args: &CompletionsArgs) -> Result<i32> {
    let mut command = Cli::command();
    generate(args.shell, &mut command, "breeze", &mut io::stdout());
    Ok(exit::SUCCESS)
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;
    use clap_complete::{generate, Shell};

    use crate::cli::Cli;

    #[test]
    fn generates_non_empty_completions_for_bash_and_zsh() {
        for shell in [Shell::Bash, Shell::Zsh] {
            let mut buffer = Vec::new();
            generate(shell, &mut Cli::command(), "breeze", &mut buffer);
            assert!(!buffer.is_empty(), "{shell:?} completions were empty");
            let text = String::from_utf8_lossy(&buffer);
            assert!(text.contains("breeze"), "{shell:?} output lacks the name");
        }
    }
}
