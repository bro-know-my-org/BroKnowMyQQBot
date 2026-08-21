//! Interactive first-run setup for one QQ Official Bot instance.

use std::{env, fs, io::IsTerminal as _, path::Path};

use dialoguer::{Confirm, Input, Password, Select, theme::ColorfulTheme};
use fs2::FileExt as _;

use crate::config::{
    BotConfig, atomic_write, ensure_smoke_plugin_config, write_secret_environment,
};

pub(crate) fn is_interactive() -> bool {
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

#[allow(clippy::too_many_lines)]
pub(crate) fn run(
    config: &mut BotConfig,
    secrets_path: &Path,
) -> Result<(String, String), Box<dyn std::error::Error>> {
    let theme = ColorfulTheme::default();
    let languages = ["English (default)", "简体中文"];
    let language_default = usize::from(config.logging.console.language == "zh-CN");
    let language = Select::with_theme(&theme)
        .with_prompt("1/9 Language / 语言")
        .items(&languages)
        .default(language_default)
        .interact()?;
    config.logging.console.language = if language == 1 {
        "zh-CN".to_owned()
    } else {
        "en".to_owned()
    };
    let text = SetupText::for_language(&config.logging.console.language);
    println!("\n{}\n", text.title);

    let environment_default = usize::from(config.qq.environment == "sandbox");
    let environment = Select::with_theme(&theme)
        .with_prompt(text.environment_prompt)
        .items(&text.environments)
        .default(environment_default)
        .interact()?;
    config.qq.environment = if environment == 1 {
        "sandbox".to_owned()
    } else {
        "production".to_owned()
    };

    let mut app_id_prompt = Input::<String>::with_theme(&theme).with_prompt(text.app_id_prompt);
    if let Some(existing) = non_empty_environment("BKMQB_QQ_OFFICIAL_APP_ID") {
        app_id_prompt = app_id_prompt.with_initial_text(existing);
    }
    let app_id = app_id_prompt
        .validate_with(|value: &String| -> Result<(), &str> {
            (!value.trim().is_empty())
                .then_some(())
                .ok_or("AppID must not be empty / AppID 不能为空")
        })
        .interact_text()?;

    let app_secret = Password::with_theme(&theme)
        .with_prompt(text.app_secret_prompt)
        .validate_with(|value: &String| -> Result<(), &str> {
            (!value.trim().is_empty())
                .then_some(())
                .ok_or("AppSecret must not be empty / AppSecret 不能为空")
        })
        .interact()?;

    config.qq.public_guild_messages = Confirm::with_theme(&theme)
        .with_prompt(text.public_guild_prompt)
        .default(config.qq.public_guild_messages)
        .interact()?;
    if config.qq.environment == "production" {
        config.qq.direct_messages = Confirm::with_theme(&theme)
            .with_prompt(text.direct_messages_prompt)
            .default(config.qq.direct_messages)
            .interact()?;
    } else {
        config.qq.direct_messages = false;
        println!("{}", text.direct_messages_disabled);
    }
    config.qq.check_only = Confirm::with_theme(&theme)
        .with_prompt(text.check_only_prompt)
        .default(config.qq.check_only)
        .interact()?;

    let current_installations = config.plugins.installations.clone();
    let profile = Select::with_theme(&theme)
        .with_prompt(text.profile_prompt)
        .items(&text.profiles)
        .default(0)
        .interact()?;
    config.plugins.installations = if profile == 1 {
        Some(ensure_smoke_plugin_config()?)
    } else {
        current_installations
    };
    let log_message_content = Confirm::with_theme(&theme)
        .with_prompt(text.message_content_prompt)
        .default(false)
        .interact()?;
    config.logging.console.message_content = log_message_content;
    config.logging.files.message_content = log_message_content;

    let _setup_lock = lock_setup(secrets_path)?;
    let previous_secrets = match fs::read(secrets_path) {
        Ok(contents) => Some(contents),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(source) => {
            return Err(crate::config::ConfigError::Read {
                path: secrets_path.to_path_buf(),
                source,
            }
            .into());
        }
    };
    write_secret_environment(secrets_path, app_id.trim(), &app_secret)?;
    let config_path = match config.save() {
        Ok(path) => path,
        Err(error) => {
            let rollback = if let Some(previous) = previous_secrets {
                atomic_write(secrets_path, &previous, 0o600)
            } else {
                fs::remove_file(secrets_path).map_err(|source| crate::config::ConfigError::Write {
                    path: secrets_path.to_path_buf(),
                    source,
                })
            };
            if let Err(rollback_error) = rollback {
                return Err(std::io::Error::other(format!(
                    "configuration save failed ({error}); credential rollback failed ({rollback_error})"
                ))
                .into());
            }
            return Err(error.into());
        }
    };
    println!(
        "\n{}:\n- {}: {}\n- {}: {}{}\n",
        text.completed,
        text.configuration,
        config_path.display(),
        text.credentials,
        secrets_path.display(),
        credential_protection_label()
    );
    Ok((app_id.trim().to_owned(), app_secret))
}

fn lock_setup(secrets_path: &Path) -> std::io::Result<fs::File> {
    let parent = secrets_path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let lock_path = parent.join(".bkm-setup.lock");
    let mut options = fs::OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let lock = options.open(lock_path)?;
    lock.lock_exclusive()?;
    Ok(lock)
}

const fn credential_protection_label() -> &'static str {
    #[cfg(unix)]
    {
        " (0600)"
    }
    #[cfg(not(unix))]
    {
        ""
    }
}

struct SetupText {
    title: &'static str,
    environments: [&'static str; 2],
    environment_prompt: &'static str,
    app_id_prompt: &'static str,
    app_secret_prompt: &'static str,
    public_guild_prompt: &'static str,
    direct_messages_prompt: &'static str,
    direct_messages_disabled: &'static str,
    check_only_prompt: &'static str,
    profiles: [&'static str; 2],
    profile_prompt: &'static str,
    message_content_prompt: &'static str,
    completed: &'static str,
    configuration: &'static str,
    credentials: &'static str,
}

impl SetupText {
    fn for_language(language: &str) -> Self {
        if language == "zh-CN" {
            Self {
                title: "BroKnowMyQQBot 单 Bot 首次启动配置",
                environments: ["production（正式环境）", "sandbox（沙箱环境）"],
                environment_prompt: "2/9 选择 QQ Bot 环境",
                app_id_prompt: "3/9 输入 QQ AppID",
                app_secret_prompt: "4/9 输入 QQ AppSecret（输入内容不会显示）",
                public_guild_prompt: "5/9 是否启用公域频道消息 Intent？没有权限请选择否",
                direct_messages_prompt: "6/9 是否启用频道私信 Intent？仅正式环境私域机器人可用",
                direct_messages_disabled: "6/9 频道私信 Intent 不支持沙箱环境，已保持关闭",
                check_only_prompt: "7/9 首次启动是否只做凭据、Gateway 与插件预检？",
                profiles: ["基础插件（Ping/Help/Counter/Echo）", "完整功能测试插件"],
                profile_prompt: "8/9 选择插件组合",
                message_content_prompt: "9/9 是否在控制台和消息日志中完整记录消息正文？",
                completed: "配置完成",
                configuration: "普通配置",
                credentials: "登录凭据",
            }
        } else {
            Self {
                title: "BroKnowMyQQBot first-run setup",
                environments: ["production", "sandbox"],
                environment_prompt: "2/9 Select the QQ Bot environment",
                app_id_prompt: "3/9 Enter the QQ AppID",
                app_secret_prompt: "4/9 Enter the QQ AppSecret (input is hidden)",
                public_guild_prompt: "5/9 Enable the public guild message intent?",
                direct_messages_prompt: "6/9 Enable the guild direct-message intent? Production private-domain bots only.",
                direct_messages_disabled: "6/9 Guild direct-message intent is unavailable in sandbox and remains disabled.",
                check_only_prompt: "7/9 Run credential, Gateway, and plugin checks only?",
                profiles: [
                    "Basic plugins (Ping/Help/Counter/Echo)",
                    "Full smoke-test plugins",
                ],
                profile_prompt: "8/9 Select the plugin profile",
                message_content_prompt: "9/9 Record complete message content in console and message logs?",
                completed: "Setup complete",
                configuration: "Configuration",
                credentials: "Credentials",
            }
        }
    }
}

fn non_empty_environment(name: &str) -> Option<String> {
    env::var(name).ok().filter(|value| !value.trim().is_empty())
}
