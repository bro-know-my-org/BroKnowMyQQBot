//! `BroKnowMyQQBot` process entry point.

#![forbid(unsafe_code)]

mod bootstrap;
mod browser;
mod cli;
mod config;
mod logging;
mod management;
mod plugin_configuration;
mod plugin_dev;
mod plugin_installer;
mod plugin_management;
mod plugin_marketplace;
mod plugins;
mod restart;
mod version;

use std::process::ExitCode;

fn main() -> ExitCode {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("bkmqb runtime failed: {error}");
            return ExitCode::FAILURE;
        }
    };
    let restart = restart::Controller::new();
    let outcome = runtime.block_on(run(restart.clone()));
    drop(runtime);
    match outcome {
        bootstrap::Outcome::Exit(code) => code,
        bootstrap::Outcome::Restart => restart.exec(),
    }
}

async fn run(restart: restart::Controller) -> bootstrap::Outcome {
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
            return bootstrap::Outcome::Exit(ExitCode::FAILURE);
        }
    };
    match cli::run(arguments).await {
        Ok(cli::RunOutcome::Complete) => bootstrap::Outcome::Exit(ExitCode::SUCCESS),
        Ok(cli::RunOutcome::StartBot { management_auth }) => {
            bootstrap::run(management_auth, restart).await
        }
        Err(error) => {
            eprintln!("bkmqb failed: {}", cli::terminal_safe(&error.to_string()));
            bootstrap::Outcome::Exit(ExitCode::FAILURE)
        }
    }
}
