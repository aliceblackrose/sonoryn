mod commands;
mod gateway_control;
mod history;
mod state;
mod voice_manager;

use std::{env, io, process::Output, time::Instant};

use gloam_commands::{DispatchOutcome, Framework, Registration};
use gloamwire::{
    RestClient,
    gateway::{GatewayConfig, GatewayConnection, GatewayEvent, GatewayIntents, TypedDispatchEvent},
    model::{GuildId, UserId},
};
use tokio::{process::Command, sync::mpsc, task::JoinSet};
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

use crate::{commands::command_list, state::AppState, voice_manager::VoiceManager};

type MainResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;
const GATEWAY_CONTROL_CAPACITY: usize = 64;
const COMMAND_CONCURRENCY: usize = 64;
const TOOL_VERSION_LIMIT: usize = 160;

#[tokio::main]
async fn main() -> MainResult<()> {
    init_tracing();

    let token = env::var("DISCORD_TOKEN")
        .map_err(|_| io::Error::new(io::ErrorKind::NotFound, "DISCORD_TOKEN is required"))?;
    let registration = registration_from_env()?;
    verify_runtime_dependencies().await?;

    let rest = RestClient::new(&token)?;
    let gateway_bot = rest.get_gateway_bot().await?;
    let intents = GatewayIntents::GUILDS | GatewayIntents::GUILD_VOICE_STATES;
    let config = GatewayConfig::from_gateway_bot(token, intents, &gateway_bot);
    let mut gateway = GatewayConnection::connect(config).await?;

    let (gateway_control, mut gateway_controls) = mpsc::channel(GATEWAY_CONTROL_CAPACITY);
    let state = AppState::new(gateway_control);
    let (mut voice_manager, mut voice_events) = VoiceManager::new(
        state.player_manager.clone(),
        state.history_manager.clone(),
        state.resolver.clone(),
    );
    let framework = Framework::builder(state.clone())
        .commands(command_list())
        .registration(registration)
        .max_concurrent_commands(COMMAND_CONCURRENCY)
        .build()?;

    let mut command_tasks = JoinSet::new();
    let mut synchronized = registration == Registration::None;
    let mut bot_user_id: Option<UserId> = None;

    info!("Sonoryn Gateway connected");

    loop {
        tokio::select! {
            signal = tokio::signal::ctrl_c() => {
                signal?;
                info!("shutdown signal received");
                break;
            }
            control = gateway_controls.recv() => {
                if let Some(control) = control {
                    voice_manager
                        .handle_control(control, &mut gateway, bot_user_id)
                        .await;
                }
            }
            worker_event = voice_events.recv() => {
                if let Some(worker_event) = worker_event {
                    voice_manager
                        .handle_worker_event(worker_event, &mut gateway)
                        .await;
                }
            }
            event = gateway.next_event() => {
                let event = event?;

                if matches!(event, GatewayEvent::Reconnect | GatewayEvent::InvalidSession { .. }) {
                    state.metrics.increment_reconnects();
                }

                if let GatewayEvent::Dispatch(dispatch) = &event {
                    let typed = {
                        let mut cache = state.cache.write().await;
                        cache.update_dispatch(dispatch)?
                    };

                    voice_manager.handle_dispatch(dispatch, &mut gateway).await;

                    if let TypedDispatchEvent::Ready(ready) = typed {
                        bot_user_id = Some(ready.user.id);
                        if !synchronized {
                            framework
                                .synchronize_commands(&rest, ready.application.id)
                                .await?;
                            synchronized = true;
                            info!("Discord application commands synchronized");
                        }
                    }
                }

                match framework.dispatch(&rest, &event)? {
                    DispatchOutcome::Spawned(task) => {
                        let command_name = task.command_name();
                        let metrics = state.metrics.clone();
                        command_tasks.spawn(async move {
                            let started = Instant::now();
                            if let Err(error) = task.join().await {
                                metrics.increment_failures();
                                error!(command = command_name, error = %error, "command task failed");
                            }
                            metrics.record_command_latency(started.elapsed());
                        });
                    }
                    DispatchOutcome::AtCapacity { name } => {
                        state.metrics.increment_failures();
                        warn!(command = name, "command rejected at capacity");
                    }
                    DispatchOutcome::Unregistered { name } => {
                        warn!(command = %name, "unregistered Discord command received");
                    }
                    DispatchOutcome::Ignored => {}
                }

                reap_command_tasks(&mut command_tasks);
            }
        }
    }

    gateway_controls.close();
    while let Ok(control) = gateway_controls.try_recv() {
        VoiceManager::reject_control(control);
    }

    voice_manager.shutdown(&mut gateway).await;
    gateway.shutdown().await?;
    while let Some(result) = command_tasks.join_next().await {
        if let Err(error) = result {
            error!(error = %error, "command supervisor task failed during shutdown");
        }
    }

    Ok(())
}

fn registration_from_env() -> MainResult<Registration> {
    match env::var("SONORYN_DEV_GUILD_ID") {
        Ok(value) => {
            let guild_id = value.parse::<u64>().map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("invalid SONORYN_DEV_GUILD_ID: {error}"),
                )
            })?;
            Ok(Registration::Guild(GuildId::new(guild_id)))
        }
        Err(env::VarError::NotPresent) => Ok(Registration::Global),
        Err(error) => Err(Box::new(error)),
    }
}

async fn verify_runtime_dependencies() -> MainResult<()> {
    verify_runtime_dependency("yt-dlp", &["--version"]).await?;
    verify_runtime_dependency("ffmpeg", &["-version"]).await?;
    Ok(())
}

async fn verify_runtime_dependency(binary: &str, args: &[&str]) -> MainResult<()> {
    let output = Command::new(binary)
        .args(args)
        .output()
        .await
        .map_err(|error| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("required runtime dependency `{binary}` is not available on PATH: {error}"),
            )
        })?;

    if !output.status.success() {
        return Err(io::Error::other(format!(
            "runtime dependency `{binary}` failed its startup check with status {}",
            output.status
        ))
        .into());
    }

    if let Some(version) = first_output_line(&output) {
        info!(tool = binary, version = %version, "runtime dependency available");
    } else {
        info!(tool = binary, "runtime dependency available");
    }
    Ok(())
}

fn first_output_line(output: &Output) -> Option<String> {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    stdout
        .lines()
        .chain(stderr.lines())
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(|line| line.chars().take(TOOL_VERSION_LIMIT).collect())
}

fn reap_command_tasks(tasks: &mut JoinSet<()>) {
    while let Some(result) = tasks.try_join_next() {
        if let Err(error) = result {
            error!(error = %error, "command supervisor task failed");
        }
    }
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("sonoryn=info,gloamwire=info,gloam_commands=info"));

    tracing_subscriber::fmt().with_env_filter(filter).init();
}
