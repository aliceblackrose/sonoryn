# Sonoryn Roadmap

Sonoryn is built in vertical slices, but protocol prerequisites are completed before user-facing playback is marked done.

## Phase 0 — Dependency and protocol alignment

### Repository baseline

- [x] Establish Sonoryn architecture and ownership boundaries.
- [x] Define slash-command-only scope.
- [x] Add Rust 1.98 / Edition 2024 package scaffold.
- [x] Add formatting, clippy, test, and build CI.
- [x] Add structured tracing without logging tokens, interaction tokens, source credentials, or raw media.

### Gloamwire / command framework compatibility

- [x] Advance `gloam-macro-commands` from its Phase-5 Gloamwire pin to the Gloamwire revision used by Sonoryn.
- [x] Verify `gloam-commands` tests and compile-fail fixtures against that revision.
- [x] Pin Sonoryn to exact compatible revisions of both repositories.
- [x] Add a dependency-tree CI check so two incompatible `gloamwire` versions cannot silently enter the build.

### DAVE prerequisite

- [x] Complete Gloamwire's DAVE protocol-version negotiation.
- [x] Implement MLS session lifecycle behind a dedicated DAVE provider boundary.
- [x] Handle external sender package, key packages, proposals, commit/welcome, epoch prepare/execute, and transition readiness.
- [x] Implement DAVE encoded-audio frame protection before RTP packetization/transport encryption.
- [ ] Ratchet sender keys and retain previous epoch receive keys for the protocol-defined transition window.
- [ ] Add protocol fixtures for group creation, member join/remove, epoch change, recovery, and invalid messages.
- [ ] Validate against a real Discord non-stage voice channel before considering Phase 0 complete.

## Phase 1 — Discord runtime and voice control

- [x] Create `AppState` with gateway control channel, voice-state index, and player manager.
- [x] Own a Gloamwire `GatewayConnection` directly with `GUILDS | GUILD_VOICE_STATES`.
- [x] Feed interactions into `Framework::dispatch(...)` from the same Gateway loop.
- [x] Synchronize slash commands after `READY` using the application ID from the event.
- [x] Track the current bot user ID and member voice states from typed Gateway events.
- [x] Route command-originated Gateway mutations through an `mpsc` control channel.
- [x] Implement voice join/leave rendezvous with `VoiceRendezvous`.
- [x] Start one dedicated voice worker per connected guild.
- [x] Add graceful shutdown for Gateway, players, voice sessions, and command tasks.

### Initial commands

- [x] `/join`
- [x] `/leave`
- [x] `/queue`
- [x] `/nowplaying`

## Phase 2 — Audio source and media pipeline

### Track model

- [x] Define stable `Track`, `TrackId`, `TrackSource`, and `RequestedBy` models.
- [x] Separate a submitted query from the resolved playable media URL.
- [x] Preserve title, artist/uploader, duration, artwork, webpage URL, and source metadata.

### Resolver

- [x] Define an async `TrackResolver` trait.
- [x] Implement the first resolver backend without coupling command handlers to it.
- [x] Support URLs and free-text search.
- [x] Add bounded resolution timeouts and cancellation.
- [x] Never place short-lived signed media URLs in persistent storage.

### Decoder / encoder

- [x] Produce Discord-compatible 48 kHz stereo Opus frames.
- [x] Use 20 ms frames as the baseline (`960` RTP timestamp samples).
- [x] Keep decode/encode work off the Gateway task.
- [x] Bound decoder buffers to prevent unbounded memory growth.
- [x] Cleanly cancel decoder subprocesses/tasks when tracks are skipped.

### Voice send path

- [x] Set Voice Gateway speaking state before media starts.
- [x] Apply DAVE protection to encoded Opus frames.
- [x] Packetize with Gloamwire RTP sequence/timestamp primitives.
- [x] Apply Gloamwire RTP transport encryption.
- [x] Pace frames with `VoiceFramePacer`.
- [x] Send the required Opus silence frames when transmission ends.

## Phase 3 — Core music experience

- [x] `/play <query>`
- [x] `/skip`
- [x] `/pause`
- [x] `/resume`
- [x] `/stop`
- [x] Automatic join to the invoking member's voice channel.
- [x] Per-guild FIFO queue.
- [x] Automatic transition to the next track.
- [x] Clear, structured command responses for queued/playing/error states.
- [x] Idle timeout and automatic disconnect.
- [x] Require control commands to come from an appropriate voice context.

## Phase 4 — Queue and playback controls

- [x] `/seek`
- [x] `/remove`
- [x] `/move`
- [x] `/shuffle`
- [x] Loop modes: off, track, queue.
- [x] Queue pagination.
- [ ] Playlist expansion with explicit item limits.
- [x] Previous/history behavior.
- [x] Volume control with clipping-safe gain handling.

## Phase 5 — Reliability and observability

- [ ] Per-guild player supervision and restart policy.
- [ ] Voice reconnect/resume integration.
- [ ] Re-run main-Gateway rendezvous when voice credentials become invalid.
- [ ] Source retry classification.
- [ ] Decoder crash handling.
- [ ] Bounded queues, resolution concurrency, and guild player counts.
- [ ] Metrics for command latency, resolve latency, startup latency, underruns, skips, failures, and reconnects.
- [ ] Credential/media URL redaction tests.
- [ ] Integration fixtures for player state transitions.

## Phase 6 — Sharding and scale

The initial runtime uses a directly owned `GatewayConnection` because Gloamwire's current `ShardManager` exposes a merged inbound event stream but not routed outbound Gateway events such as Update Voice State.

- [ ] Add routed outbound Gateway operations to Gloamwire's shard manager.
- [ ] Move Sonoryn to `ShardManager` without changing player/command APIs.
- [ ] Route guild voice joins to the owning shard using Discord's guild shard formula.
- [ ] Preserve shard identity in command contexts.
- [ ] Add shard-local failure isolation.

## Phase 7 — Persistence and personalization

- [ ] Durable guild settings.
- [ ] Favorites and reusable playlists.
- [ ] Playback history with retention limits.
- [ ] Default volume / autoplay preferences.
- [ ] DJ-role or guild control policy.
- [ ] Migrations and versioned configuration.

## Phase 8 — Release hardening

- [ ] Container image and reproducible deployment path.
- [ ] Health/readiness endpoints where appropriate.
- [ ] Operator documentation.
- [ ] Permission/intents setup guide.
- [ ] Load tests across many idle and active guild players.
- [ ] Release checklist and semantic versioning policy.
