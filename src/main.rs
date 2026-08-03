//! `BroKnowMyQQBot` process entry point.

#![forbid(unsafe_code)]

mod bootstrap;
mod browser;
mod cli;
mod config;
mod logging;
mod management;
mod plugin_dev;
mod plugins;

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
    if arguments.is_empty() {
        return bootstrap::run().await;
    }
    match cli::run(arguments).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("bkmqb failed: {error}");
            ExitCode::FAILURE
        }
    }
}
