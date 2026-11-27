//! Durable per-guild snapshots. Never serialize live voice or media credentials.

use std::{
    collections::{HashMap, HashSet},
    env, fs, io,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use gloamwire::model::{ChannelId, GuildId};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use sonoryn::{
    media::{Track, TrackId},
    player::{LoopMode, PlayerSnapshot},
};
use tokio::sync::RwLock;

use crate::state::AppState;

type DbResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

pub(crate) type Checkpoints = Arc<RwLock<HashMap<GuildId, CheckpointEntry>>>;

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub(crate) struct VoiceCheckpoint {
    pub(crate) channel_id: ChannelId,
    pub(crate) track_id: Option<TrackId>,
    pub(crate) position_millis: u64,
    pub(crate) paused: bool,
    pub(crate) volume_percent: u8,
}

impl VoiceCheckpoint {
    pub(crate) fn new(channel_id: ChannelId) -> Self {
        Self {
            channel_id,
            track_id: None,
            position_millis: 0,
            paused: false,
            volume_percent: 100,
        }
    }
}

pub(crate) struct CheckpointEntry {
    // Process-local ownership, not persisted.
    pub(crate) generation: u64,
    pub(crate) checkpoint: VoiceCheckpoint,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct GuildState {
    guild_id: GuildId,
    player: PlayerSnapshot,
    loop_mode: LoopMode,
    history: Vec<Track>,
    voice: Option<VoiceCheckpoint>,
}

pub(crate) struct Database {
    path: PathBuf,
}

impl Database {
    pub(crate) async fn open() -> DbResult<(Self, Vec<GuildState>)> {
        let path = database_path()?;
        let saved = tokio::task::spawn_blocking({
            let path = path.clone();
            move || -> DbResult<Vec<GuildState>> {
                prepare_path(&path)?;
                let connection = connect(&path)?;
                initialize(&connection)?;
                load(&connection)
            }
        })
        .await??;
        tracing::info!(path = %path.display(), guilds = saved.len(), "opened playback database");
        Ok((Self { path }, saved))
    }

    pub(crate) async fn save(&self, state: &AppState, checkpoints: &Checkpoints) -> DbResult<()> {
        let saved = snapshot(state, checkpoints).await;
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || -> DbResult<()> {
            let mut connection = connect(&path)?;
            write(&mut connection, &saved)
        })
        .await?
    }
}

async fn snapshot(state: &AppState, checkpoints: &Checkpoints) -> Vec<GuildState> {
    let players = state.player_manager.read().await;
    let history = state.history_manager.read().await;
    let checkpoints = checkpoints.read().await;
    let guilds: HashSet<_> = players
        .guild_ids()
        .chain(history.guild_ids())
        .chain(checkpoints.keys().copied())
        .collect();
    guilds
        .into_iter()
        .map(|guild_id| GuildState {
            guild_id,
            player: players.snapshot(guild_id),
            loop_mode: players.loop_mode(guild_id),
            history: history.snapshot(guild_id),
            voice: checkpoints.get(&guild_id).map(|entry| entry.checkpoint),
        })
        .collect()
}

pub(crate) async fn restore(state: &AppState, saved: Vec<GuildState>) -> Checkpoints {
    let mut players = state.player_manager.write().await;
    let mut history = state.history_manager.write().await;
    let mut checkpoints = HashMap::new();
    for saved in saved {
        players.restore(saved.guild_id, saved.player, saved.loop_mode);
        for track in saved.history {
            players.reserve_track_id(track.id);
            history.push(saved.guild_id, track);
        }
        if let Some(checkpoint) = saved.voice {
            checkpoints.insert(
                saved.guild_id,
                CheckpointEntry {
                    generation: 0,
                    checkpoint,
                },
            );
        }
    }
    Arc::new(RwLock::new(checkpoints))
}

fn database_path() -> DbResult<PathBuf> {
    if let Some(path) = env::var_os("SONORYN_DATABASE_PATH") {
        return Ok(PathBuf::from(path));
    }
    let root = env::var_os("XDG_STATE_HOME")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))
        .ok_or_else(|| io::Error::other("set SONORYN_DATABASE_PATH or HOME"))?;
    Ok(root.join("sonoryn").join("sonoryn.sqlite3"))
}

fn prepare_path(path: &Path) -> io::Result<()> {
    if let Some(parent) = path.parent().filter(|parent| !parent.as_os_str().is_empty()) {
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(parent)?;
    }
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)?;
    Ok(())
}

fn connect(path: &Path) -> rusqlite::Result<Connection> {
    let connection = Connection::open(path)?;
    connection.busy_timeout(Duration::from_secs(2))?;
    connection.pragma_update(None, "synchronous", "FULL")?;
    Ok(connection)
}

fn initialize(connection: &Connection) -> DbResult<()> {
    let version: u32 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if version > 1 {
        return Err(io::Error::other("unsupported Sonoryn database schema version").into());
    }
    connection.pragma_update(None, "journal_mode", "WAL")?;
    connection.execute_batch(
        "BEGIN;
         CREATE TABLE IF NOT EXISTS guild_state (
             guild_id TEXT PRIMARY KEY NOT NULL,
             snapshot TEXT NOT NULL
         );
         PRAGMA user_version = 1;
         COMMIT;",
    )?;
    Ok(())
}

fn load(connection: &Connection) -> DbResult<Vec<GuildState>> {
    let mut statement = connection.prepare("SELECT snapshot FROM guild_state")?;
    let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
    let mut saved = Vec::new();
    for row in rows {
        saved.push(serde_json::from_str(&row?)?);
    }
    Ok(saved)
}

fn write(connection: &mut Connection, saved: &[GuildState]) -> DbResult<()> {
    let transaction = connection.transaction()?;
    transaction.execute("DELETE FROM guild_state", [])?;
    for guild in saved {
        transaction.execute(
            "INSERT INTO guild_state (guild_id, snapshot) VALUES (?1, ?2)",
            params![guild.guild_id.get().to_string(), serde_json::to_string(guild)?],
        )?;
    }
    transaction.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use gloamwire::model::UserId;
    use sonoryn::media::{RequestedBy, ResolvedTrack, TrackMetadata, TrackRequest, TrackSource};

    fn track(id: u64) -> Track {
        Track::from_resolved(
            TrackId::new(id),
            TrackRequest::new("song").expect("request"),
            RequestedBy::new(UserId::new(7)),
            ResolvedTrack {
                source: TrackSource::YouTube,
                metadata: TrackMetadata {
                    title: "Song".to_owned(),
                    artist: None,
                    duration: Some(Duration::from_secs(120)),
                    artwork_url: None,
                    webpage_url: "https://example.test/song".to_owned(),
                },
                locator: "https://example.test/song".to_owned(),
            },
        )
    }

    #[test]
    fn sqlite_round_trip_preserves_playback_state() {
        let mut connection = Connection::open_in_memory().expect("sqlite");
        initialize(&connection).expect("schema");
        let guild_id = GuildId::new(42);
        let saved = GuildState {
            guild_id,
            player: PlayerSnapshot {
                now_playing: Some(track(1)),
                queue: vec![track(2), track(3)],
            },
            loop_mode: LoopMode::Queue,
            history: vec![track(4)],
            voice: Some(VoiceCheckpoint {
                channel_id: ChannelId::new(8),
                track_id: Some(TrackId::new(1)),
                position_millis: 45_000,
                paused: true,
                volume_percent: 60,
            }),
        };
        write(&mut connection, &[saved]).expect("save");
        let loaded = load(&connection).expect("load").pop().expect("guild");
        assert_eq!(loaded.guild_id, guild_id);
        assert_eq!(loaded.player.now_playing, Some(track(1)));
        assert_eq!(loaded.player.queue, vec![track(2), track(3)]);
        assert_eq!(loaded.history, vec![track(4)]);
        assert_eq!(loaded.loop_mode, LoopMode::Queue);
        let voice = loaded.voice.expect("voice");
        assert_eq!(voice.position_millis, 45_000);
        assert!(voice.paused);
        assert_eq!(voice.volume_percent, 60);
        write(&mut connection, &[]).expect("clear");
        assert!(load(&connection).expect("load").is_empty());
    }
}
