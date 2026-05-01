#![allow(dead_code)]

mod cli;
mod config;
mod context;
mod oauth;
mod process;
mod proxy;
mod router;
mod sets;
mod terminal;
mod tui;
mod update;
mod util;

use std::ffi::OsString;
use std::path::Path;

use anyhow::Result;
use clap::Parser;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

use cli::{AuthAction, Cli, Commands, ProfileAction, ProxyAction, SetsAction};
use config::{ClaudexConfig, ProfileConfig, ProfileModels, ProviderType};
use oauth::{AuthType, OAuthProvider};

#[tokio::main]
async fn main() -> Result<()> {
    let cli = parse_cli();

    let mut config = ClaudexConfig::load(cli.config.as_deref())?;
    if invoked_as_claudex5() {
        ensure_claudex5_profile(&mut config);
    }

    // `claudex run` 时 proxy 日志只写文件，不污染 Claude Code 终端输出
    let is_run_command = matches!(&cli.command, Some(Commands::Run { .. }));

    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&config.log_level));

    // 日志文件（所有模式都写）
    let file_layer = proxy::proxy_log_path().and_then(|log_path| {
        if let Some(parent) = log_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .ok()
            .map(|file| {
                tracing_subscriber::fmt::layer()
                    .with_ansi(false)
                    .with_writer(std::sync::Mutex::new(file))
            })
    });

    // stderr（run 模式不输出）
    let stderr_layer = if is_run_command {
        None
    } else {
        Some(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
    };

    tracing_subscriber::registry()
        .with(env_filter)
        .with(stderr_layer)
        .with(file_layer)
        .init();

    match cli.command {
        Some(Commands::Run {
            profile: profile_name,
            model,
            hyperlinks,
            args,
        }) => {
            // Ensure proxy is running
            if !process::daemon::is_proxy_running()? {
                tracing::info!("proxy not running, starting in background...");
                start_proxy_background(&config).await?;
                // Brief wait for proxy to be ready
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }

            let profile = config
                .find_profile(&profile_name)
                .ok_or_else(|| anyhow::anyhow!("profile '{}' not found", profile_name))?
                .clone();

            process::launch::launch_claude(&config, &profile, model.as_deref(), &args, hyperlinks)?;

            // Claude 退出后，输出日志文件路径
            if let Some(log_path) = proxy::proxy_log_path() {
                if log_path.exists() {
                    eprintln!("\nClaudex proxy log: {}", log_path.display());
                }
            }
        }

        Some(Commands::Profile { action }) => match action {
            ProfileAction::List => {
                config::profile::list_profiles(&config).await;
            }
            ProfileAction::Show { name } => {
                config::profile::show_profile(&config, &name).await?;
            }
            ProfileAction::Test { name } => {
                config::profile::test_profile(&config, &name).await?;
            }
            ProfileAction::Add => {
                config::profile::interactive_add(&mut config).await?;
            }
            ProfileAction::Remove { name } => {
                config::profile::remove_profile(&mut config, &name)?;
            }
        },

        Some(Commands::Proxy { action }) => match action {
            ProxyAction::Start {
                port,
                daemon: as_daemon,
            } => {
                if as_daemon {
                    start_proxy_background(&config).await?;
                } else {
                    proxy::start_proxy(config, port).await?;
                }
            }
            ProxyAction::Stop => {
                process::daemon::stop_proxy()?;
            }
            ProxyAction::Status => {
                process::daemon::proxy_status()?;
            }
        },

        Some(Commands::Dashboard) => {
            let config_arc = std::sync::Arc::new(tokio::sync::RwLock::new(config));
            let metrics_store = proxy::metrics::MetricsStore::new();
            let health =
                std::sync::Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new()));
            tui::run_tui(config_arc, metrics_store, health).await?;
        }

        Some(Commands::Config { action }) => {
            config::cmd::dispatch(action, &mut config).await?;
        }

        Some(Commands::Update { check }) => {
            if check {
                match update::check_update().await? {
                    Some(version) => println!("New version available: {version}"),
                    None => println!("Already up to date (v{})", env!("CARGO_PKG_VERSION")),
                }
            } else {
                update::self_update().await?;
            }
        }

        Some(Commands::Sets { action }) => match action {
            SetsAction::Add {
                source,
                global,
                r#ref,
            } => {
                sets::add(&source, global, r#ref.as_deref()).await?;
            }
            SetsAction::Remove { name, global } => {
                sets::remove(&name, global).await?;
            }
            SetsAction::List { global } => {
                sets::list(global)?;
            }
            SetsAction::Update { name, global } => {
                sets::update(name.as_deref(), global).await?;
            }
            SetsAction::Show { name, global } => {
                sets::show(&name, global)?;
            }
        },

        Some(Commands::Auth { action }) => match action {
            AuthAction::Login {
                provider,
                profile,
                force,
                headless,
                enterprise_url,
            } => {
                let profile_name = profile.unwrap_or_else(|| provider.clone());
                oauth::providers::login(
                    &mut config,
                    &provider,
                    &profile_name,
                    force,
                    headless,
                    enterprise_url.as_deref(),
                )
                .await?;
            }
            AuthAction::Status { profile } => {
                oauth::providers::status(&config, profile.as_deref()).await?;
            }
            AuthAction::Logout { profile } => {
                oauth::providers::logout(&config, &profile).await?;
            }
            AuthAction::Refresh { profile } => {
                oauth::providers::refresh(&config, &profile).await?;
            }
        },

        None => {
            // Default: launch TUI if profiles exist, else show help
            if config.profiles.is_empty() {
                println!("Welcome to Claudex!");
                println!();
                println!("Get started:");
                println!("  1. Create config: claudex config");
                println!(
                    "  2. Add a profile: edit {:?}",
                    ClaudexConfig::config_path()?
                );
                println!("  3. Run claude:    claudex run <profile>");
                println!();
                println!("Use --help for more options.");
            } else {
                let config_arc = std::sync::Arc::new(tokio::sync::RwLock::new(config));
                let metrics_store = proxy::metrics::MetricsStore::new();
                let health =
                    std::sync::Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new()));
                tui::run_tui(config_arc, metrics_store, health).await?;
            }
        }
    }

    Ok(())
}

async fn start_proxy_background(config: &ClaudexConfig) -> Result<()> {
    let port = config.proxy_port;
    let host = config.proxy_host.clone();

    // Spawn proxy in a background task
    let config_clone = config.clone();
    tokio::spawn(async move {
        if let Err(e) = proxy::start_proxy(config_clone, None).await {
            tracing::error!("proxy failed: {e}");
        }
    });

    // Wait for it to be ready
    let client = reqwest::Client::new();
    let health_url = format!("http://{host}:{port}/health");
    for _ in 0..20 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        if client.get(&health_url).send().await.is_ok() {
            tracing::info!("proxy is ready");
            return Ok(());
        }
    }

    anyhow::bail!("proxy failed to start within 2 seconds")
}

fn parse_cli() -> Cli {
    let args: Vec<OsString> = std::env::args_os().collect();
    Cli::parse_from(default_claudex5_args(args))
}

fn invoked_as_claudex5() -> bool {
    std::env::args_os()
        .next()
        .is_some_and(|program| is_claudex5_program(&program))
}

fn default_claudex5_args(args: Vec<OsString>) -> Vec<OsString> {
    if !should_default_claudex5_to_codex_sub(&args) {
        return args;
    }

    let mut rewritten = Vec::with_capacity(args.len() + 4);
    if let Some(program) = args.first() {
        rewritten.push(program.clone());
    }
    rewritten.push("run".into());
    rewritten.push("codex-sub".into());
    rewritten.push("--setting-sources".into());
    rewritten.push("project,local".into());
    rewritten.extend(args.into_iter().skip(1));
    rewritten
}

fn should_default_claudex5_to_codex_sub(args: &[OsString]) -> bool {
    let Some(program) = args.first() else {
        return false;
    };
    if !is_claudex5_program(program) {
        return false;
    }

    match args.get(1).and_then(|arg| arg.to_str()) {
        None => true,
        Some("-h" | "--help" | "-V" | "--version" | "help") => false,
        Some("run" | "profile" | "proxy" | "dashboard" | "config" | "update" | "auth" | "sets") => {
            false
        }
        Some(_) => true,
    }
}

fn is_claudex5_program(program: &OsString) -> bool {
    let stem = Path::new(program)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default();
    stem == "claudex5"
}

fn ensure_claudex5_profile(config: &mut ClaudexConfig) {
    if config.find_profile("codex-sub").is_some() {
        return;
    }

    let model = "gpt-5.5".to_string();
    config.profiles.push(ProfileConfig {
        name: "codex-sub".to_string(),
        provider_type: ProviderType::OpenAIResponses,
        base_url: "https://chatgpt.com/backend-api/codex".to_string(),
        default_model: model.clone(),
        auth_type: AuthType::OAuth,
        oauth_provider: Some(OAuthProvider::Openai),
        models: ProfileModels {
            haiku: Some(model.clone()),
            sonnet: Some(model.clone()),
            opus: Some(model),
        },
        ..ProfileConfig::default()
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os_args(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    #[test]
    fn claudex5_without_command_defaults_to_codex_sub() {
        let args = default_claudex5_args(os_args(&["claudex5"]));
        assert_eq!(
            args,
            os_args(&[
                "claudex5",
                "run",
                "codex-sub",
                "--setting-sources",
                "project,local"
            ])
        );
    }

    #[test]
    fn claudex5_prompt_args_default_to_codex_sub() {
        let args = default_claudex5_args(os_args(&["claudex5", "-p", "hi"]));
        assert_eq!(
            args,
            os_args(&[
                "claudex5",
                "run",
                "codex-sub",
                "--setting-sources",
                "project,local",
                "-p",
                "hi"
            ])
        );
    }

    #[test]
    fn claudex5_explicit_subcommand_is_preserved() {
        let args = default_claudex5_args(os_args(&["claudex5", "profile", "list"]));
        assert_eq!(args, os_args(&["claudex5", "profile", "list"]));
    }

    #[test]
    fn claudex_binary_is_preserved() {
        let args = default_claudex5_args(os_args(&["claudex", "-p", "hi"]));
        assert_eq!(args, os_args(&["claudex", "-p", "hi"]));
    }

    #[test]
    fn claudex5_profile_is_injected_when_missing() {
        let mut config = ClaudexConfig::default();

        ensure_claudex5_profile(&mut config);

        let profile = config.find_profile("codex-sub").unwrap();
        assert_eq!(profile.default_model, "gpt-5.5");
        assert_eq!(profile.provider_type, ProviderType::OpenAIResponses);
        assert_eq!(profile.auth_type, AuthType::OAuth);
        assert_eq!(profile.oauth_provider, Some(OAuthProvider::Openai));
    }

    #[test]
    fn claudex5_profile_injection_keeps_existing_profile() {
        let mut config = ClaudexConfig::default();
        config.profiles.push(ProfileConfig {
            name: "codex-sub".to_string(),
            default_model: "custom".to_string(),
            ..ProfileConfig::default()
        });

        ensure_claudex5_profile(&mut config);

        assert_eq!(config.profiles.len(), 1);
        assert_eq!(
            config.find_profile("codex-sub").unwrap().default_model,
            "custom"
        );
    }
}
