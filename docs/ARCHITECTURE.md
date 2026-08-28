# Sonoryn Architecture

## Ownership boundaries

Sonoryn deliberately builds on the two lower-level projects instead of wrapping them with competing abstractions.

### Gloamwire owns

- Discord REST and rate limiting.
- Main Gateway lifecycle and event models.
- Voice Gateway lifecycle.
- UDP discovery and protocol selection.
- RTP sequencing and transport encryption.
- Opus frame boundary/pacing primitives.
- DAVE protocol and media-encryption machinery once completed.

### `gloam-commands` owns

- Slash-command declaration macros.
- Typed command option extraction.
- Command registration metadata.
- Interaction acknowledgement/reply lifecycle.
- Command policy, hooks, autocomplete, and dispatch.

### Sonoryn owns

- Music commands and UX.
- Voice-member lookup policy.
- Per-guild player lifecycle.
- Track queues and playback state.
- Source resolution.
- Decoder/encoder orchestration.
- Player supervision, idle behavior, and music-specific observability.

## Dependency invariant

Sonoryn must have exactly one effective `gloamwire` crate instance.

`gloam-commands::Context`, `Framework`, `RestClient`, Discord IDs, Gateway events, and Sonoryn's direct voice APIs all exchange Gloamwire-owned types. If `gloam-commands` is compiled against one Gloamwire git revision while Sonoryn directly uses another, Rust treats those types as belonging to separate crates even when the source names match.

The command framework's Gloamwire pin therefore has to be advanced before Sonoryn adds its final dependency manifest. Sonoryn should then pin both repositories to exact compatible commits rather than float on `main` in production.

## Top-level runtime

Sonoryn owns the Discord Gateway loop rather than calling `Framework::run()`.

The managed command-framework runtime intentionally requests no Gateway intents because slash-command interactions do not require any. Music playback does require `GUILD_VOICE_STATES`, and joining/leaving voice also requires access to the live Gateway connection for opcode 4 Update Voice State.

Initial runtime shape:

```text
                    +------------------+
                    | Discord Gateway  |
                    +--------+---------+
                             |
                             v
                  +----------+-----------+
                  | Sonoryn Gateway loop |
                  +----+-------------+---+
                       |             |
          interactions |             | typed voice/lifecycle events
                       v             v
             +---------+----+   +----+----------------+
             | Framework    |   | Gateway event logic |
             | dispatch(...)|   | cache/rendezvous    |
             +---------+----+   +----+----------------+
                       |             |
                       v             v
                 command tasks   player/voice actors
```

The Gateway task must remain responsive. It does not perform media decoding, source resolution, or long voice setup work inline.

## Application state

The command framework receives an `AppState` containing cloneable handles rather than owning network transports directly.

Proposed shape:

```rust,ignore
pub struct AppState {
    pub gateway: GatewayControl,
    pub voice_index: VoiceIndex,
    pub players: PlayerManager,
    pub resolver: Arc<dyn TrackResolver>,
}
```

### `GatewayControl`

A bounded `tokio::mpsc` sender used by command/player tasks to request mutations that must run on the task owning the mutable `GatewayConnection`.

Initial messages:

```rust,ignore
pub enum GatewayCommand {
    JoinVoice {
        guild_id: GuildId,
        channel_id: ChannelId,
    },
    LeaveVoice {
        guild_id: GuildId,
    },
}
```

The command task never locks the live Gateway socket.

### `VoiceIndex`

Tracks the latest `(GuildId, UserId) -> Option<ChannelId>` mapping from `VOICE_STATE_UPDATE` events. `/play` uses the invoking member's ID and guild ID to determine which channel Sonoryn should join.

The index is not the player state. It is only Discord-derived membership state.

### `PlayerManager`

Maps `GuildId` to one per-guild player actor handle. It is responsible for creating one player at a time and removing stopped actors.

## Per-guild player actor

Every actively controlled guild gets a single task that serializes playback mutations.

```text
commands/resolver
      |
      v
+------------------+
| Player mailbox   |
+--------+---------+
         |
         v
+--------+---------+
| GuildPlayer      |
|------------------|
| queue            |
| current track    |
| pause state      |
| loop mode        |
| voice handle     |
| decoder handle   |
+--------+---------+
         |
         v
+--------+---------+
| Voice worker     |
+------------------+
```

Representative player messages:

```rust,ignore
pub enum PlayerCommand {
    Enqueue(Track),
    Skip,
    Pause,
    Resume,
    Stop,
    SetLoop(LoopMode),
    Shutdown,
}
```

A command handler sends a message and waits only for a bounded response/acknowledgement. It does not perform the playback transition itself.

## Voice join flow

1. `/play` resolves the invoking guild and user.
2. `VoiceIndex` returns the user's current voice channel.
3. Sonoryn ensures a guild player exists.
4. The player sends `GatewayCommand::JoinVoice`.
5. The Gateway task sends `UpdateVoiceState` through Gloamwire.
6. Discord emits the bot's `VOICE_STATE_UPDATE` and guild `VOICE_SERVER_UPDATE` in either order.
7. A `VoiceRendezvous` collects both events.
8. Once ready, a dedicated voice worker establishes the Gloamwire voice session.
9. The voice worker continuously polls the Voice Gateway while also accepting player commands.
10. Playback begins only after DAVE is negotiated and a usable media session exists.

## Why the first version is unsharded

Gloamwire's current `ShardManager` owns each `GatewayConnection` in an internal task and exposes only a merged inbound event receiver plus shutdown. It does not currently expose a routed way to send Update Voice State to the shard responsible for a guild.

The first Sonoryn runtime therefore owns a `GatewayConnection` directly. This is an implementation constraint, not a player API assumption.

Later, Gloamwire should expose routed shard operations such as:

```rust,ignore
shards.update_voice_state(guild_id, update).await?;
```

or a general routed outbound command channel. Sonoryn can then replace the Gateway owner without changing command or player actor APIs.

## DAVE boundary

Normal Discord voice channels require DAVE/E2EE. DAVE therefore belongs before RTP packetization/transport encryption in the encoded-media path.

Target send pipeline:

```text
source
  -> decode/resample
  -> Opus encode (48 kHz stereo)
  -> DAVE encoded-frame protection
  -> RTP header/sequence/timestamp
  -> RTP transport AEAD
  -> UDP send
```

Sonoryn should never implement MLS or DAVE cryptographic primitives itself. Gloamwire exposes the protocol lifecycle and a media-transform boundary; the concrete DAVE provider belongs there.

The implementation should prefer a maintained, interoperable implementation over custom cryptography. Any provider must be hidden behind Gloamwire's feature/API boundary so Sonoryn's player code only asks to protect an encoded frame.

## Audio pipeline

The player consumes a stream of already encoded Discord-compatible Opus frames. Source resolution and decoding are separate concerns.

Baseline media format:

- 48,000 Hz.
- Stereo.
- Opus.
- 20 ms frames.
- RTP timestamp step: 960 samples.

Decoder execution must not happen on the Tokio core worker that is polling Discord Gateway/Voice Gateway sockets. External decoders or CPU-heavy Rust decode/encode loops should be isolated with subprocess I/O or blocking/worker execution as appropriate.

The decoder-to-player channel is bounded so a fast producer cannot buffer an entire track in memory.

## Resolver boundary

Commands submit a query, not a playable URL.

```rust,ignore
#[async_trait]
pub trait TrackResolver: Send + Sync {
    async fn resolve(&self, query: &str) -> Result<ResolvedTrack>;
}
```

`ResolvedTrack` contains stable display metadata and enough information to obtain media for immediate playback. Short-lived signed CDN/media URLs are treated as ephemeral and are never the durable track identity.

## Command behavior

### `/play`

- Guild-only.
- Requires the invoker to be in voice unless Sonoryn is already in an allowed control context.
- Defers promptly when resolution may exceed Discord's initial response window.
- Resolves off the Gateway task.
- Creates/uses the guild player.
- Enqueues the result.
- Responds with whether the track started immediately or which queue position it received.

### Control commands

`/skip`, `/pause`, `/resume`, `/stop`, and `/leave` validate guild/player/voice context before mutating state. The policy should eventually be centralized rather than duplicated in every command.

## Shutdown

Shutdown order:

1. Stop accepting new player mutations.
2. Ask guild players to terminate decoders and stop playback.
3. Flush required final silence frames when appropriate.
4. Close voice sessions.
5. Send main-Gateway voice disconnects where possible.
6. Drain/finish command tasks.
7. Shut down the main Gateway.

No detached player or decoder task should survive process shutdown.

## Observability

Useful structured fields:

- guild ID;
- command path;
- player state transition;
- queue length;
- source kind;
- track duration;
- resolve/decode/startup latency;
- voice reconnect outcome;
- underrun count.

Never log:

- Discord bot tokens;
- interaction tokens;
- Voice Gateway tokens;
- DAVE keys/MLS secrets;
- RTP transport keys;
- signed source media URLs when they contain credentials/tokens;
- raw encrypted or decrypted media frames.
