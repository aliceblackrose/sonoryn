mod commands;
mod state;

use std::{env, io};

use gloam_commands::{DispatchOutcome, Framework, Registration, commands};
use gloamwire::{
    RestClient,
    gateway::{GatewayConfig, GatewayConnection, GatewayEvent, GatewayIntents, TypedDispatchEvent},
    model::GuildId,
};
use tokio::task::JoinSet;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

use crate::{
    commands::{ping, voice},
    state::AppState,
};

type MainResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

#[tokio::main]
async fn main() -> MainResult<()> {
    init_tracing();

    let token = env::var("DISCORD_TOKEN")
        .map_err(|_| io::Error::new(io::ErrorKind::NotFound, "DISCORD_TOKEN is required"))?;
    let registration = registration_from_env()?;

    let rest = RestClient::new(&token)?;
    let gateway_bot = rest.get_gateway_bot().await?;
    let intents = GatewayIntents::GUILDS | GatewayIntents::GUILD_VOICE_STATES;
    let config = GatewayConfig::from_gateway_bot(token, intents, &gateway_bot);
    let mut gateway = GatewayConnection::connect(config).await?;

    let state = AppState::new();
    let framework = Framework::builder(state.clone())
        .commands(commands![ping, voice])
        .registration(registration)
        .max_concurrent_commands(64)
        .build()?;

    let mut command_tasks = JoinSet::new();
    let mut synchronized = registration == Registration::None;

    info!("Sonoryn Gateway connected");

    loop {
        tokio::select! {
            signal = tokio::signal::ctrl_c() => {
                signal?;
                info!("shutdown signal received");
                break;
            }
            event = gateway.next_event() => {
                let event = event?;

                if let GatewayEvent::Dispatch(dispatch) = &event {
                    let typed = {
                        let mut cache = state.cache.write().await;
                        cache.update_dispatch(dispatch)?
                    };

                    if !synchronized
                        && let TypedDispatchEvent::Ready(ready) = typed
                    {
                        framework
                            .synchronize_commands(&rest, ready.application.id)
                            .await?;
                        synchronized = true;
                        info!("Discord application commands synchronized");
                    }
                }

                match framework.dispatch(&rest, &event)? {
                    DispatchOutcome::Spawned(task) => {
                        let command_name = task.command_name();
                        command_tasks.spawn(async move {
                            if let Err(error) = task.join().await {
                                error!(command = command_name, error = %error, "command task failed");
                            }
                        });
                    }
                    DispatchOutcome::AtCapacity { name } => {
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
