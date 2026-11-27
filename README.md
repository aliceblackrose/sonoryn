# Sonoryn

Sonoryn is a Rust Discord music bot built directly on [Gloamwire](https://github.com/aliceblackrose/gloamwire) for Discord Gateway, REST, and voice transport, with [gloam-macro-commands](https://github.com/aliceblackrose/gloam-macro-commands) providing the slash-command layer.

The project is intentionally protocol-first. Sonoryn does not introduce a second Discord transport abstraction: Discord lifecycle, REST, voice transport, and protocol behavior stay in Gloamwire; command declaration and dispatch stay in `gloam-commands`; Sonoryn owns music-specific state, playback, queueing, source resolution, and user experience.

## Status

**Working-prototype stage.**

The prototype owns the Discord Gateway loop, joins normal voice channels through Gloamwire's DAVE voice session, resolves URLs/free-text searches with `yt-dlp`, transcodes media to 48 kHz stereo 20 ms Opus frames with FFmpeg, and sends those frames through Gloamwire's DAVE → RTP → transport-encryption path. Connected guild voice workers automatically disconnect after five minutes with no active track and an empty queue; paused playback remains active and is not considered idle.

Implemented prototype commands:

- `/play <query>` — resolve and enqueue a URL or search query and automatically join the invoker's voice channel.
- `/skip` — cancel the current decoder and advance the FIFO queue.
- `/pause` and `/resume` — pause/resume frame consumption without blocking Voice Gateway/DAVE processing.
- `/stop` — cancel playback and clear the guild queue.
- `/remove <position>` — remove a queued track by its one-based queue position.
- `/move <from> <to>` — reorder queued tracks without touching the current track.
- `/queue` — inspect the current and queued tracks.
- `/nowplaying` — inspect the active track.
- `/join` and `/leave` — explicitly manage the guild voice session.
- `/health` — recheck the local `yt-dlp` and FFmpeg runtime dependencies.
- `/ping` and `/voice` — basic Gateway/voice-state diagnostics.

Queue-mutating commands use the same authoritative same-voice-channel check as playback controls. Their positions refer only to the queued entries shown under `Queue:`; the current track is not position 1.

The remaining roadmap work is reliability, richer controls, persistence, sharding, and real-world Discord validation/hardening; the current target is a development prototype, not a production music service.

## Design rules

- Rust 1.98, Edition 2024.
- Slash commands only; no prefix-command parser.
- One Gloamwire version throughout the dependency graph.
- Sonoryn owns its Gateway loop instead of using `Framework::run()`, because music requires `GUILD_VOICE_STATES` and outbound voice-state updates.
- Per-guild playback runs as an actor/task with explicit commands rather than shared mutable playback state scattered across command handlers.
- Source resolution, decoding, DAVE media protection, RTP transport, and queue policy remain separate layers.
- No bespoke cryptography in Sonoryn. The application-owned adapter delegates MLS and media cryptography to native libdave; Gloamwire owns voice framing and transport.
- Direct/signed media URLs are ephemeral and never stored in queue state.

## Prototype data flow

```text
/play <query>
    │
    ├─ TrackResolver (yt-dlp metadata)
    │      └─ durable Track metadata / public locator
    │
    ├─ per-guild PlayerManager FIFO
    │
    └─ Voice worker
           ├─ yt-dlp direct-media refresh
           ├─ bounded FFmpeg decoder task
           ├─ 48 kHz stereo / 20 ms Opus
           ├─ VoiceFramePacer
           └─ Gloamwire DaveVoiceSession
                  └─ DAVE → RTP → transport AEAD → Discord UDP
```

The decoder task communicates with the voice worker through a two-frame bounded channel. This keeps FFmpeg backpressured while the voice worker continues polling Voice Gateway heartbeats and DAVE events.

## Requirements

Install:

- Rust 1.98 (the included `rust-toolchain.toml` selects it);
- `yt-dlp` available on `PATH`;
- FFmpeg with `libopus` support available as `ffmpeg` on `PATH`;
- a Discord application/bot with access to the target guild and permission to connect/speak in the target voice channel.

Sonoryn performs a startup preflight for `yt-dlp --version` and `ffmpeg -version` before opening the Discord connection. A missing or non-runnable dependency therefore fails startup with an actionable error instead of failing only when the first track starts decoding.

Configure:

```text
DISCORD_TOKEN=...
SONORYN_DEV_GUILD_ID=...   # strongly recommended during development
RUST_LOG=sonoryn=info,gloamwire=info,gloam_commands=info
```

`SONORYN_DEV_GUILD_ID` makes slash-command registration guild-scoped so changes appear quickly. Without it, Sonoryn registers commands globally.

## Run the prototype

```bash
cargo run
```

Then join a normal voice channel and use, for example:

```text
/health
/play never gonna give you up
/play https://www.youtube.com/watch?v=dQw4w9WgXcQ
/queue
/move 2 1
/remove 2
/pause
/resume
/skip
/stop
/leave
```

Sonoryn resolves direct media immediately before decoding. A queued track therefore stores its public source locator and metadata, not a short-lived CDN URL. When the player becomes idle, Sonoryn keeps the voice session warm for five minutes before disconnecting automatically; new queued work arriving at the timeout boundary is rechecked before the disconnect is allowed to proceed.

## Architecture

See [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) for runtime ownership and data flow, and [`ROADMAP.md`](ROADMAP.md) for implementation phases.

## License

License selection is pending.

## Persistent playback and restart recovery

Sonoryn keeps a SQLite database under `$XDG_STATE_HOME/sonoryn/sonoryn.sqlite3` (default: `$HOME/.local/state/sonoryn/sonoryn.sqlite3`). Override it with `SONORYN_DATABASE_PATH` if needed. SQLite is bundled; no database server is required.

Every two seconds, queue order, current track, playback history, repeat mode, channel, volume, pause state and the latest playback position are checkpointed. SIGTERM (including systemd restart) and Ctrl-C trigger a final checkpoint. On startup, a saved nonempty session reconnects after its guild becomes available if another user is present in the saved voice channel. Otherwise the queue and current track remain available for a manual join. An explicit `/leave` removes the voice recovery state and clears playback; `/stop` clears playback without starting another song on restart.

A restart cannot preserve live Discord voice transport, encryption, or decoder state. Sonoryn establishes a fresh voice connection, resolves a fresh media URL and seeks to the saved position. Expect a short audio interruption and potentially a few seconds of replay after an abrupt crash. Persistent data includes track requests and Discord user/channel IDs, not bot tokens or temporary voice/media credentials. Keep the database private and back it up using SQLite's backup mechanism (or copy it while the bot is stopped).

### DAVE backend migration

Sonoryn tracks Gloamwire's `main` branch and uses its backend-neutral DAVE interface. The application supplies a pinned `libdave-rs` adapter to Discord's native libdave; it requires a C++ toolchain, pkg-config, and OpenSSL 3 development headers. Voice joins require DAVE protocol version 1 or higher and wait for an encryption ratchet before starting playback. The old bundled Davey feature is not used. Existing lockfiles require `cargo update -p gloamwire` to advance the upstream revision.

This migration is experimental pending native dependency review, CI, and live Discord voice interoperability testing; do not deploy the draft as a production build.
