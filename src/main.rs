//! `BroKnowMyQQBot` process entry point.

#![forbid(unsafe_code)]

mod bootstrap;
mod config;
mod logging;
mod plugins;

use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    bootstrap::run().await
}
