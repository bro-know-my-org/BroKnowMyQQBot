//! Application composition root and supervised startup.

#![forbid(unsafe_code)]

use std::{
    env,
    process::ExitCode,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use crate::{
    browser::InstalledBrowserExecutor,
    config::{self, BotConfig, ManagementAuth, ManagementConfig, load_secret_environment},
    logging, management,
    plugin_management::PluginManager,
    plugins::load_plugins,
    version,
};
use adapter_onebot11::{OneBot11Adapter, OneBot11Config};
use adapter_qqbot::{QqWebSocketAdapter, QqWebSocketConfig, QqWebhookAdapter, QqWebhookConfig};
use bot_core::{
    Adapter, MemoryDedupStore, Runtime, RuntimeBuilder, RuntimeObserver, shutdown_channel,
};
use plugin_host::{PluginStore, SecureHttpExecutor, StaticPluginHost};
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

struct AdapterSet {
    adapters: Vec<Arc<dyn Adapter>>,
    qq_api: Option<OpenApiClient>,
}

pub(crate) enum Outcome {
    Exit(ExitCode),
    Restart,
}

pub(crate) async fn run(
    management_auth: Option<ManagementAuth>,
    restart: crate::restart::Controller,
) -> Outcome {
    match start(management_auth, restart).await {
        Ok(true) => Outcome::Restart,
        Ok(false) => Outcome::Exit(ExitCode::SUCCESS),
        Err(error) => {
            eprintln!("BroKnowMyQQBot failed: {error}");
            Outcome::Exit(ExitCode::FAILURE)
        }
    }
}

async fn start(
    management_auth: Option<ManagementAuth>,
    restart: crate::restart::Controller,
) -> Result<bool, Box<dyn std::error::Error>> {
    let mut config = BotConfig::load()?;
    let secrets_file = load_secret_environment()?;
    let qq_credentials = resolve_credentials(&mut config, secrets_file.as_deref())?;
    if let Some(auth) = management_auth {
        config.management.auth = auth;
    }
    let live_logs = logging::live::LogBuffer::default();
    let _logging_guards = logging::init(
        &config.logging,
        config.management.enabled.then(|| live_logs.clone()),
    )?;
    if let Some(path) = secrets_file {
        info!(path = %path.display(), "loaded local secrets environment file");
    }

    management::check_access(&config.management)?;
    let AdapterSet { adapters, qq_api } = build_adapters(&config, qq_credentials).await?;
    let plugin_db = config.plugins.database.clone();
    if let Some(parent) = plugin_db.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let plugin_store = PluginStore::open(plugin_db)?;
    let mut plugins = StaticPluginHost::new(plugin_store.clone())
        .with_command_prefix(&config.commands.prefix)?
        .with_http_executor(Arc::new(SecureHttpExecutor::from_environment()?));
    if let Some(executor) = discover_browser_executor().await {
        plugins = plugins.with_browser_executor(Arc::new(executor));
    }
    for adapter in &adapters {
        plugins = plugins.with_adapter(adapter.clone());
    }
    let installation_file = config.plugins.installations.clone();
    let loaded_packages =
        match load_plugins(&mut plugins, &plugin_store, installation_file.as_deref()).await {
            Ok(loaded) => loaded,
            Err(error) => {
                if let Err(shutdown_error) = plugins.shutdown().await {
                    warn!(%shutdown_error, "plugin shutdown failed after plugin loading failure");
                }
                return Err(error);
            }
        };
    if config.qq.enabled && config.qq.check_only {
        return preflight(&plugins, qq_api.as_ref()).await.map(|()| false);
    }

    let manager = PluginManager::new(
        plugins,
        plugin_store,
        loaded_packages.packages,
        loaded_packages.file_instances,
    )
    .with_marketplace(
        config.plugins.database.clone(),
        config.plugins.installations.clone(),
        config.plugins.marketplace_url.clone(),
    )?;
    let plugins = Arc::new(manager.clone());
    let observer = RuntimeObserver::new();
    let shutdown_timeout = Duration::from_secs(config.runtime.shutdown_timeout_seconds);
    let mut runtime_builder = RuntimeBuilder::new()
        .queue_capacity(config.runtime.queue_capacity)
        .event_concurrency(config.runtime.event_concurrency)
        .handler_timeout(Duration::from_secs(config.runtime.handler_timeout_seconds))
        .shutdown_timeout(shutdown_timeout)
        .dedup_store(Arc::new(MemoryDedupStore::try_new(
            config.runtime.dedup_capacity,
        )?))
        .observer(observer.clone())
        .handler(plugins.clone());
    for adapter in adapters {
        runtime_builder = runtime_builder.adapter(adapter);
    }
    let runtime = runtime_builder.build()?;
    info!(
        version = %version::display(),
        qq_enabled = config.qq.enabled,
        onebot11_enabled = config.onebot11.enabled,
        "starting BroKnowMyQQBot adapters"
    );
    let signal = wait_for_shutdown_signal();
    tokio::pin!(signal);
    let runtime_result = supervise_runtime(
        runtime,
        config.management,
        observer,
        manager,
        live_logs,
        shutdown_timeout,
        (restart, signal.as_mut()),
    )
    .await;
    finish_plugins(&plugins, runtime_result, signal.as_mut()).await
}

async fn finish_plugins(
    plugins: &PluginManager,
    runtime_result: Result<bool, Box<dyn std::error::Error>>,
    mut signal: std::pin::Pin<&mut impl std::future::Future<Output = std::io::Result<()>>>,
) -> Result<bool, Box<dyn std::error::Error>> {
    let mut restarting = *runtime_result.as_ref().unwrap_or(&false);
    let shutdown = plugins.shutdown();
    tokio::pin!(shutdown);
    let (shutdown_result, signal_result) = if restarting {
        let (result, decision) = crate::restart::drain(signal.as_mut(), shutdown).await;
        restarting = *decision.as_ref().unwrap_or(&false);
        (result, decision.map(|_| ()))
    } else {
        (shutdown.await, Ok(()))
    };
    if let Err(error) = shutdown_result {
        if runtime_result.is_ok() {
            return Err(error.into());
        }
        warn!(%error, "plugin shutdown failed after runtime failure");
    }
    runtime_result?;
    signal_result?;
    info!("BroKnowMyQQBot stopped");
    Ok(restarting)
}

fn resolve_credentials(
    config: &mut BotConfig,
    secrets_file: Option<&std::path::Path>,
) -> Result<Option<(String, String)>, Box<dyn std::error::Error>> {
    if !config.qq.enabled {
        return Ok(None);
    }
    match configured_credentials() {
        Ok(credentials) => Ok(Some(credentials)),
        Err(_error) if config::setup::is_interactive() => {
            let path =
                secrets_file.ok_or("interactive setup requires a writable secrets file path")?;
            Ok(Some(config::setup::run(config, path)?))
        }
        Err(error) => Err(error.into()),
    }
}

async fn preflight(
    plugins: &StaticPluginHost,
    qq_api: Option<&OpenApiClient>,
) -> Result<(), Box<dyn std::error::Error>> {
    let api = qq_api.ok_or("QQ preflight client is unavailable")?;
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
    Ok(())
}

async fn discover_browser_executor() -> Option<InstalledBrowserExecutor> {
    let cancel = Arc::new(AtomicBool::new(false));
    let worker_cancel = Arc::clone(&cancel);
    let mut task = tokio::task::spawn_blocking(move || {
        InstalledBrowserExecutor::discover_with_cancel(&worker_cancel)
            .map_err(|error| error.to_string())
    });
    let discovery = tokio::time::timeout(Duration::from_secs(5), &mut task).await;
    match discovery {
        Ok(Ok(Ok(Some(executor)))) => Some(executor),
        Ok(Ok(Ok(None))) => {
            info!("optional Browser Runtime is not installed");
            None
        }
        Ok(Ok(Err(error))) => {
            warn!(error, "optional Browser Runtime is unavailable");
            None
        }
        Ok(Err(error)) => {
            warn!(error = %error, "optional Browser Runtime discovery task failed");
            None
        }
        Err(_) => {
            cancel.store(true, Ordering::Relaxed);
            warn!("optional Browser Runtime discovery timed out");
            if let Err(error) = task.await {
                warn!(error = %error, "timed-out Browser Runtime discovery task failed to join");
            }
            None
        }
    }
}

async fn supervise_runtime(
    runtime: Runtime,
    management_config: ManagementConfig,
    observer: RuntimeObserver,
    manager: PluginManager,
    live_logs: logging::live::LogBuffer,
    shutdown_timeout: Duration,
    control: (
        crate::restart::Controller,
        std::pin::Pin<&mut impl std::future::Future<Output = std::io::Result<()>>>,
    ),
) -> Result<bool, Box<dyn std::error::Error>> {
    let (restart, mut signal) = control;
    let (shutdown_handle, shutdown_signal) = shutdown_channel();
    let runtime_run = runtime.run(shutdown_signal.clone());
    let management_run = management::serve(
        management_config,
        observer,
        manager,
        live_logs,
        shutdown_signal,
        shutdown_timeout,
        restart.clone(),
    );
    tokio::pin!(runtime_run);
    tokio::pin!(management_run);
    let mut restarting = false;
    let (runtime_result, management_result, signal_result) = tokio::select! {
        biased;
        result = &mut signal => {
            shutdown_handle.shutdown();
            let (runtime_result, management_result) = tokio::join!(&mut runtime_run, &mut management_run);
            (runtime_result, management_result, result)
        },
        result = &mut runtime_run => {
            shutdown_handle.shutdown();
            (result, management_run.await, Ok(()))
        },
        result = &mut management_run => {
            shutdown_handle.shutdown();
            (runtime_run.await, result, Ok(()))
        },
        () = restart.cancelled() => {
            shutdown_handle.shutdown();
            let ((runtime_result, management_result), decision) = crate::restart::drain(
                signal.as_mut(), async { tokio::join!(&mut runtime_run, &mut management_run) },
            ).await;
            restarting = *decision.as_ref().unwrap_or(&false);
            (runtime_result, management_result, decision.map(|_| ()))
        }
    };
    if runtime_result.is_ok() && management_result.is_ok() {
        if let Err(error) = signal_result {
            return Err(error.into());
        }
    }
    if let (Err(_runtime_error), Err(management_error)) = (&runtime_result, &management_result) {
        warn!(error = %management_error, "management service also failed during runtime shutdown");
    }
    if runtime_result.is_ok() {
        if let Err(error) = management_result {
            return Err(error.into());
        }
    }
    runtime_result?;
    Ok(restarting)
}

async fn build_adapters(
    config: &BotConfig,
    qq_credentials: Option<(String, String)>,
) -> Result<AdapterSet, Box<dyn std::error::Error>> {
    let mut adapters = Vec::<Arc<dyn Adapter>>::new();
    let mut qq_api = None;
    if let Some((app_id, app_secret)) = qq_credentials {
        let environment = match config.qq.environment.as_str() {
            "sandbox" => OpenApiEnvironment::Sandbox,
            "production" => OpenApiEnvironment::Production,
            _ => unreachable!("configuration validation only accepts known QQ environments"),
        };
        let intents = configured_qq_intents(config);
        let webhook_secret = (config.qq.transport == "webhook")
            .then(|| SecretString::from(app_secret.clone().into_boxed_str()));
        let tokens = TokenManager::new(
            app_id.clone(),
            SecretString::from(app_secret.into_boxed_str()),
        )?;
        let api = OpenApiClient::new(environment, tokens)?;
        adapters.push(build_qq_adapter(
            config,
            app_id,
            webhook_secret,
            api.clone(),
            intents,
        )?);
        qq_api = Some(api);
    }
    if config.onebot11.enabled {
        let token = required_env("BKMQB_ONEBOT_ACCESS_TOKEN")?;
        let adapter = OneBot11Adapter::bind(OneBot11Config {
            id: bot_core::AdapterId::new("onebot11-reverse"),
            listen: config.onebot11.listen.parse()?,
            access_token: SecretString::from(token),
            allow_insecure_remote: config.onebot11.allow_insecure_remote,
            action_timeout: Duration::from_secs(config.onebot11.action_timeout_seconds),
            max_message_bytes: config.onebot11.max_message_bytes,
            max_pending_actions: config.onebot11.max_pending_actions,
        })
        .await?;
        adapters.push(Arc::new(adapter));
    }
    Ok(AdapterSet { adapters, qq_api })
}

fn configured_qq_intents(config: &BotConfig) -> Intents {
    let mut intents = Intents::empty().with_group_and_c2c();
    if config.qq.public_guild_messages {
        intents = intents.with_public_guild_messages();
    }
    if config.qq.private_guild_messages {
        intents = intents.with_guild_messages();
    }
    if config.qq.direct_messages {
        intents = intents.with_direct_messages();
    }
    if config.qq.extended_events.is_enabled() {
        intents |= Intents::GUILDS
            | Intents::GUILD_MEMBERS
            | Intents::GUILD_MESSAGE_REACTIONS
            | Intents::GROUP_MEMBER_EVENT
            | Intents::INTERACTION
            | Intents::MESSAGE_AUDIT;
    }
    if config.qq.forum_events {
        intents = intents.with_forums();
    }
    if config.qq.audio_events {
        intents = intents.with_audio_actions();
    }
    if config.qq.open_forum_events {
        intents = intents.with_open_forums();
    }
    if config.qq.audio_live_member_events {
        intents = intents.with_audio_live_members();
    }
    intents
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
        required_env("BKMQB_QQ_OFFICIAL_APP_ID")?,
        required_env("BKMQB_QQ_OFFICIAL_APP_SECRET")?,
    ))
}

#[cfg(test)]
mod tests {
    use qqbot_protocol::Intents;

    use super::{BotConfig, configured_qq_intents};

    #[test]
    fn extended_events_control_optional_intents() {
        let enabled: BotConfig = toml::from_str("[qq]\nextended_events = true").unwrap();
        let enabled_intents = configured_qq_intents(&enabled);
        assert_eq!(
            enabled_intents,
            Intents::GROUP_AND_C2C_EVENT
                | Intents::GUILDS
                | Intents::GUILD_MEMBERS
                | Intents::GUILD_MESSAGE_REACTIONS
                | Intents::GROUP_MEMBER_EVENT
                | Intents::INTERACTION
                | Intents::MESSAGE_AUDIT
        );

        let disabled: BotConfig = toml::from_str("[qq]\nextended_events = false").unwrap();
        let disabled_intents = configured_qq_intents(&disabled);
        assert_eq!(disabled_intents, Intents::GROUP_AND_C2C_EVENT);
    }

    #[test]
    fn private_guild_messages_require_an_explicit_intent_switch() {
        let default = configured_qq_intents(&BotConfig::default());
        assert!(!default.contains(Intents::GUILD_MESSAGES));

        let enabled: BotConfig = toml::from_str("[qq]\nprivate_guild_messages = true").unwrap();
        assert!(configured_qq_intents(&enabled).contains(Intents::GUILD_MESSAGES));

        let extended: BotConfig = toml::from_str("[qq]\nextended_events = true").unwrap();
        assert!(!configured_qq_intents(&extended).contains(Intents::GUILD_MESSAGES));
    }

    #[test]
    fn special_audio_and_forum_intents_require_independent_switches() {
        let default = configured_qq_intents(&BotConfig::default());
        for intent in [
            Intents::FORUMS_EVENT,
            Intents::AUDIO_ACTION,
            Intents::OPEN_FORUMS_EVENT,
            Intents::AUDIO_OR_LIVE_CHANNEL_MEMBER,
        ] {
            assert!(!default.contains(intent));
        }

        let special_intents = Intents::FORUMS_EVENT
            | Intents::AUDIO_ACTION
            | Intents::OPEN_FORUMS_EVENT
            | Intents::AUDIO_OR_LIVE_CHANNEL_MEMBER;
        for (key, expected) in [
            ("forum_events", Intents::FORUMS_EVENT),
            ("audio_events", Intents::AUDIO_ACTION),
            ("open_forum_events", Intents::OPEN_FORUMS_EVENT),
            (
                "audio_live_member_events",
                Intents::AUDIO_OR_LIVE_CHANNEL_MEMBER,
            ),
        ] {
            let config: BotConfig = toml::from_str(&format!("[qq]\n{key}=true")).unwrap();
            assert_eq!(configured_qq_intents(&config) & special_intents, expected);
        }

        let extended: BotConfig = toml::from_str("[qq]\nextended_events=true").unwrap();
        let intents = configured_qq_intents(&extended);
        assert!(!intents.intersects(special_intents));
    }
}
