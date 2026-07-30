//! Application composition root and supervised startup.

#![forbid(unsafe_code)]

use std::{env, process::ExitCode, sync::Arc, time::Duration};

use crate::{
    config::{self, BotConfig, load_secret_environment},
    logging,
    plugins::load_plugins,
};
use adapter_qqbot::{QqWebSocketAdapter, QqWebSocketConfig, QqWebhookAdapter, QqWebhookConfig};
use bot_core::{Adapter, RuntimeBuilder, shutdown_channel};
use plugin_host::{PluginStore, StaticPluginHost};
use qqbot_protocol::{Intents, OpenApiClient, OpenApiEnvironment, TokenManager};
use secrecy::SecretString;
use thiserror::Error;
use tracing::{info, warn};

#[derive(Debug, Error)]
enum AppError {
    #[error(
        "required environment variable `{0}` is not set; edit config/secrets.env or provide it through the process environment"
    )]
    MissingEnvironment(&'static str),
    #[error("QQ Webhook secret was not retained during adapter construction")]
    MissingWebhookSecret,
}

pub(crate) async fn run() -> ExitCode {
    match start().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("BroKnowMyQQBot failed: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn start() -> Result<(), Box<dyn std::error::Error>> {
    let mut config = BotConfig::load()?;
    let secrets_file = load_secret_environment()?;
    let (app_id, app_secret) = match configured_credentials() {
        Ok(credentials) => credentials,
        Err(_error) if config::setup::is_interactive() => {
            let path = secrets_file
                .as_deref()
                .ok_or("interactive setup requires a writable secrets file path")?;
            config::setup::run(&mut config, path)?
        }
        Err(error) => return Err(error.into()),
    };
    let _logging_guards = logging::init(&config.logging)?;
    if let Some(path) = secrets_file {
        info!(path = %path.display(), "loaded local secrets environment file");
    }

    let environment = match config.qq.environment.as_str() {
        "sandbox" => OpenApiEnvironment::Sandbox,
        "production" => OpenApiEnvironment::Production,
        _ => unreachable!("configuration validation only accepts known QQ environments"),
    };
    let mut intents = Intents::empty().with_group_and_c2c();
    if config.qq.public_guild_messages {
        intents = intents.with_public_guild_messages();
    }

    let webhook_secret = (config.qq.transport == "webhook")
        .then(|| SecretString::from(app_secret.clone().into_boxed_str()));
    let tokens = TokenManager::new(
        app_id.clone(),
        SecretString::from(app_secret.into_boxed_str()),
    )?;
    let api = OpenApiClient::new(environment, tokens)?;
    let adapter = build_qq_adapter(&config, app_id, webhook_secret, api.clone(), intents)?;
    let plugin_db = config.plugins.database.clone();
    if let Some(parent) = plugin_db.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut plugins =
        StaticPluginHost::new(PluginStore::open(plugin_db)?).with_adapter(adapter.clone());
    let installation_file = config.plugins.installations.clone();
    if let Err(error) = load_plugins(&mut plugins, installation_file.as_deref()).await {
        if let Err(shutdown_error) = plugins.shutdown().await {
            warn!(%shutdown_error, "plugin shutdown failed after plugin loading failure");
        }
        return Err(error);
    }
    if config.qq.check_only {
        let gateway = match api.gateway().await {
            Ok(gateway) => gateway,
            Err(error) => {
                if let Err(shutdown_error) = plugins.shutdown().await {
                    warn!(%shutdown_error, "plugin shutdown failed after Gateway preflight failure");
                }
                return Err(Box::new(error));
            }
        };
        info!(gateway_url = %gateway.url, "QQ credentials, Gateway endpoint, and plugins are available");
        plugins.shutdown().await?;
        return Ok(());
    }

    let (shutdown_handle, shutdown_signal) = shutdown_channel();
    let plugins = Arc::new(plugins);
    let runtime = RuntimeBuilder::new()
        .event_concurrency(config.runtime.event_concurrency)
        .adapter(adapter)
        .handler(plugins.clone())
        .build()?;
    info!(transport = %config.qq.transport, "starting BroKnowMyQQBot with QQ Official adapter");
    let runtime_run = runtime.run(shutdown_signal);
    tokio::pin!(runtime_run);
    let runtime_result = tokio::select! {
        result = &mut runtime_run => result,
        signal = wait_for_shutdown_signal() => {
            shutdown_handle.shutdown();
            let result = runtime_run.await;
            if let Err(error) = signal {
                if result.is_ok() {
                    return Err(error.into());
                }
            }
            result
        }
    };
    if let Err(error) = plugins.shutdown().await {
        if runtime_result.is_ok() {
            return Err(error.into());
        }
        warn!(%error, "plugin shutdown failed after runtime failure");
    }
    runtime_result?;
    info!("BroKnowMyQQBot stopped");
    Ok(())
}

fn build_qq_adapter(
    config: &BotConfig,
    app_id: String,
    app_secret: Option<SecretString>,
    api: OpenApiClient,
    intents: Intents,
) -> Result<Arc<dyn Adapter>, Box<dyn std::error::Error>> {
    let log_message_content =
        config.logging.console.message_content || config.logging.files.message_content;
    match config.qq.transport.as_str() {
        "websocket" => Ok(Arc::new(QqWebSocketAdapter::new(
            QqWebSocketConfig {
                intents,
                log_message_content,
                ..QqWebSocketConfig::default()
            },
            api,
        ))),
        "webhook" => {
            let app_secret = app_secret.ok_or(AppError::MissingWebhookSecret)?;
            Ok(Arc::new(QqWebhookAdapter::new(
                QqWebhookConfig {
                    timestamp_tolerance: Duration::from_secs(
                        config.qq.webhook.timestamp_tolerance_seconds,
                    ),
                    max_body_bytes: config.qq.webhook.max_body_bytes,
                    max_request_concurrency: config.qq.webhook.max_request_concurrency,
                    request_timeout: Duration::from_secs(config.qq.webhook.request_timeout_seconds),
                    log_message_content,
                    ..QqWebhookConfig::new(
                        config.qq.webhook.listen.parse()?,
                        config.qq.webhook.path.clone(),
                        app_id,
                        app_secret,
                    )
                },
                api,
            )?))
        }
        _ => unreachable!("configuration validation only accepts known QQ transports"),
    }
}

#[cfg(unix)]
async fn wait_for_shutdown_signal() -> std::io::Result<()> {
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        result = tokio::signal::ctrl_c() => result,
        _ = terminate.recv() => Ok(()),
    }
}

#[cfg(not(unix))]
async fn wait_for_shutdown_signal() -> std::io::Result<()> {
    tokio::signal::ctrl_c().await
}

fn required_env(name: &'static str) -> Result<String, AppError> {
    env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .ok_or(AppError::MissingEnvironment(name))
}

fn configured_credentials() -> Result<(String, String), AppError> {
    Ok((
        required_env("BKM_QQ_OFFICIAL_APP_ID")?,
        required_env("BKM_QQ_OFFICIAL_APP_SECRET")?,
    ))
}
