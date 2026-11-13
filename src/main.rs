mod cli;
mod commands;
mod envfile;
mod exit;
mod fog;
mod gitinfo;
mod store;

use clap::Parser;

fn main() {
    let cli = cli::Cli::parse();
    let color = cli.common.color.should_color();

    let code = match commands::dispatch(cli) {
        Ok(code) => code,
        Err(err) => {
            report_error(&err, color);
            match err.downcast_ref::<exit::CommandError>() {
                Some(err) => err.code,
                None => exit::GENERAL,
            }
        }
    };

    std::process::exit(code);
}

fn report_error(err: &anyhow::Error, color: bool) {
    if color {
        eprintln!("\x1b[31merror:\x1b[0m {err:#}");
    } else {
        eprintln!("error: {err:#}");
    }
}
