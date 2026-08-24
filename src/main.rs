//! `BroKnowMyQQBot` process entry point.

#![forbid(unsafe_code)]

mod bootstrap;
mod browser;
mod cli;
mod config;
mod logging;
mod management;
mod plugin_dev;
mod plugin_marketplace;
mod plugins;
mod version;

use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    let arguments = match std::env::args_os()
        .skip(1)
        .map(|argument| {
            argument
                .into_string()
                .map_err(|_| "bkmqb arguments must be valid UTF-8")
        })
        .collect::<Result<Vec<_>, _>>()
    {
        Ok(arguments) => arguments,
        Err(error) => {
            eprintln!("bkmqb failed: {error}");
            return ExitCode::FAILURE;
        }
    };
    match cli::run(arguments).await {
        Ok(cli::RunOutcome::Complete) => ExitCode::SUCCESS,
        Ok(cli::RunOutcome::StartBot) => bootstrap::run().await,
        Err(error) => {
            eprintln!("bkmqb failed: {}", cli::terminal_safe(&error.to_string()));
            ExitCode::FAILURE
        }
    }
}
