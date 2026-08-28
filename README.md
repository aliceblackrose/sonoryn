# Sonoryn

Sonoryn is a Rust Discord music bot built directly on [Gloamwire](https://github.com/aliceblackrose/gloamwire) for Discord Gateway, REST, and voice transport, with [gloam-macro-commands](https://github.com/aliceblackrose/gloam-macro-commands) providing the slash-command layer.

The project is intentionally protocol-first. Sonoryn does not introduce a second Discord transport abstraction: Discord lifecycle, REST, voice transport, and protocol behavior stay in Gloamwire; command declaration and dispatch stay in `gloam-commands`; Sonoryn owns music-specific state, playback, queueing, source resolution, and user experience.

## Status

**Phase 0 — architecture and dependency alignment.**

Two upstream requirements currently gate normal voice-channel playback:

1. `gloam-macro-commands` is pinned to an older Gloamwire revision and must be synchronized with the Gloamwire revision Sonoryn uses so command/runtime types come from one crate instance.
2. Discord requires DAVE/E2EE for normal voice channels. Gloamwire already has the Voice Gateway/UDP/RTP/Opus transport boundary, but its DAVE/MLS media layer is still unfinished.

Until both are resolved, Sonoryn will not claim normal voice playback as functional.

## Design rules

- Rust 1.98, Edition 2024.
- Slash commands only; no prefix-command parser.
- One Gloamwire version throughout the dependency graph.
- Sonoryn owns its Gateway loop instead of using `Framework::run()`, because music requires `GUILD_VOICE_STATES` and outbound voice-state updates.
- Per-guild playback runs as an actor/task with explicit commands rather than shared mutable playback state scattered across command handlers.
- Source resolution, decoding, DAVE media protection, RTP transport, and queue policy remain separate layers.
- No bespoke cryptography in Sonoryn. DAVE belongs in Gloamwire behind a dedicated implementation boundary.

## Planned commands

The initial user-facing surface is deliberately small:

- `/play <query>` — resolve and enqueue a track, joining the invoker when necessary.
- `/skip` — skip the current track.
- `/pause` and `/resume` — control playback.
- `/stop` — stop playback and clear the queue.
- `/queue` — inspect the guild queue.
- `/nowplaying` — inspect the active track.
- `/leave` — disconnect and tear down the guild player.

Later phases add seek, loop modes, shuffle, remove/move, filters, playlists, history, favorites, and persistence.

## Architecture

See [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) for runtime ownership and data flow, and [`ROADMAP.md`](ROADMAP.md) for implementation phases.

## Configuration

The intended baseline environment is:

```text
DISCORD_TOKEN=...
SONORYN_DEV_GUILD_ID=...   # optional; fast guild command registration while developing
RUST_LOG=sonoryn=info,gloamwire=info,gloam_commands=info
```

Additional source/decoder configuration will be introduced with the audio pipeline rather than pre-committing Sonoryn to a resolver backend before playback transport is ready.

## License

License selection is pending.
