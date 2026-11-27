# Runtime audit

Baseline: `dffcb19c020733545a78ca5bc17599e783502bdc` (premature-track-repeat patch merged).

## Findings corrected in this change

| Severity | Subsystem | Finding / correction |
|---|---|---|
| High | Decoder | An Ogg stream truncated at a packet boundary could be treated as valid EOF. Require an EOS page and test the truncated case. |
| High | Playback | A decoder could wait indefinitely for a frame; bound reads to 30 seconds and route stalls through bounded playback recovery. |
| High | Playback | A resumed decoder that emits no packets could appear complete because its stored seek offset is nonzero. Require actual transmitted audio for completion. |
| High | Voice | Voice rendezvous had a timeout but the subsequent DAVE session connection lacked an application-level deadline. Bound that phase too. |
| Medium | Decoder | Automatic reconnect of a non-seekable HTTP stream can restart content at an unknown offset. Leave recovery of interrupted known-duration media to the worker. |
| Medium | Decoder | Remote manifests could reference additional FFmpeg protocol handlers. Restrict remote input protocols to HTTP(S), TCP/TLS, crypto and HTTP proxy; not a network sandbox. |
| Medium | Media resolver | Out-of-range finite duration metadata could panic. Use checked conversion and reject blank titles. |
| Medium | Resolver | Some yt-dlp invocations lacked an explicit end-of-options delimiter. Add it before targets and validate HTTP(S) URL shape. |
| Medium | Commands | Worker-dependent queue and playback commands could exceed the initial interaction deadline. Defer replies before waiting. |
| Medium | Operations | Hung executable version checks could stall startup and /health. Bound subprocess waits and kill on drop. |
| Medium | CI | Formatting failure hid downstream compiler and test errors. Run validation steps even after preceding failures; keep every failure fatal. Add shell syntax checks. |

## Other findings and remaining risks

These are not represented as fixed or live-verified by the patch.

| Priority | Area | Risk and recommended work |
|---|---|---|
| High | Network trust | Arbitrary HTTP(S) sources, yt-dlp extraction, DNS and redirects can reach unintended internal endpoints. Run media subprocesses with restricted outbound networking and no access to secrets or internal services; URL validation alone is not an SSRF defense. |
| High | Queue authorization | Authorization is checked separately from command-side queue mutations, leaving a race around leave, move and concurrent commands. Move mutations into the guild actor with authorization and generation checks. |
| High | Voice lifecycle | Unexpected external moves/kicks and voice server changes need reconciliation with live gateway voice state. Do not silently rejoin after a moderator disconnect. Add integration tests. |
| High | Durability | Periodic snapshots are best effort; an abrupt crash can lose acknowledged mutations. Playback position and player/history snapshots are not a fully atomic actor transaction. Use ordered durable command commits for stronger guarantees. |
| Medium | Capacity | No aggregate queue limit, per-user autocomplete rate limit or global subprocess concurrency limit. Introduce quotas and backpressure before public multi-guild deployment. |
| Medium | Recovery | Unknown-duration/non-seekable streams cannot reliably resume by offset. Inaccurate durations require source-specific policies and live testing. |
| Medium | Persistence | No cross-process ownership lock prevents two processes writing one database. Do not run two instances against the same state file. |
| Medium | Data lifecycle | Guild history and requester identifiers remain in SQLite; establish guild-removal and retention policies and backup/restore tests. |
| Medium | Supply chain | Cargo.lock is not tracked; dependency resolution is not fully reproducible. Generate a lockfile with the pinned toolchain, review dependencies with cargo-audit/deny and pin CI actions to reviewed SHAs. |
| Low | UI/search | Undocumented YouTube search HTML can change; no search cache/coalescing; embeds lack actual elapsed position. Add search load testing and live metadata fixtures. |

## Validation

Static source review covered command handlers, embed helpers, player/history/state models, media lookup and decoder, voice worker and gateway lifecycle, SQLite persistence, installer/updater and CI. This is not a penetration test or dependency vulnerability scan. The prior repeat fix passed CI. New changes require CI and live Discord tests; no Rust toolchain was available in the editing environment.

Acceptance tests: play a known four-minute song with loop off, track and queue; interrupt its HTTP source midway; confirm the last sent offset, no false completion and bounded retry. Exercise pause, seek and volume changes, moderator move/disconnect, concurrent /play and /leave, long queues, systemd restart, SIGKILL recovery and two-guild playback. Verify logs do not disclose tokens or signed media URLs. Do not deploy publicly without media egress isolation.
