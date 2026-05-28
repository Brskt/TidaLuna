# TIDAL Connect - module invariants

## Layout

```
src/connect/
├── mod.rs              # ConnectManager coordination
├── consts.rs           # constants (intervals, service types, timeouts)
├── bridge.rs           # player -> receiver event forwarding (statics)
├── runtime.rs          # TaskGroup, TaskRecord, DeadlineAction
├── token_state.rs      # AuthStore, RefreshGuard, Generation
├── types/              # protocol DTOs split by domain
│   ├── device.rs       # MdnsDevice, DeviceType
│   ├── session.rs      # SessionCommand/Notification/Status
│   ├── playback.rs     # PbState, PbPlayState, PlayerState, PlaybackNotification
│   ├── media.rs        # MediaInfo, MediaMetadata
│   ├── queue.rs        # QueueInfo, QueueItem, RepeatMode, QueueNotification
│   ├── auth.rs         # ServerInfo, AuthInfo, OAuth* (wire format as received)
│   └── mod.rs          # re-exports + ReceiverConfig
├── ipc/                # frontend -> Rust RPC, split by domain
│   ├── mod.rs          # top-level dispatch
│   ├── controller.rs   # discover/connect/disconnect/set_auth
│   ├── playback.rs     # play/pause/seek/volume/mute/next/prev/repeat/shuffle
│   ├── queue.rs        # load_media/load_queue/select/update_quality
│   ├── receiver.rs     # start/stop
│   ├── state.rs        # get_state snapshot
│   └── helpers.rs      # shared: device-cmd dispatch, WS event forwarding, CEF post
├── controller/         # desktop-as-controller path
├── receiver/           # desktop-as-receiver path
│   └── queue/          # cloud-queue state machine
│       ├── mod.rs      # QueueManager façade (state machine, dispatch, notifications)
│       ├── http.rs     # stateless HTTP/OAuth (get/post/refresh)
│       ├── media.rs    # pure DASH/BTS resolution
│       └── error.rs    # QueueError
├── ws/                 # WebSocket transport
│   ├── client.rs       # TLS client (controller -> device)
│   ├── server.rs       # TLS server (accepts mobile clients)
│   ├── heartbeat.rs    # shared ping/pong driver
│   ├── tls.rs          # rustls config with TIDAL CA
│   └── pending.rs      # requestId -> oneshot tracker
└── mdns/               # service discovery
    ├── advertiser.rs   # _tidalconnect._tcp advertisement
    ├── browser.rs      # peer discovery
    └── backend.rs      # MdnsBackend trait, bounded shutdown contract
```

## Invariants

### `QueueState` is private to the façade

`src/connect/receiver/queue/mod.rs` owns `enum QueueState`. Sub-modules
(`http.rs`, `media.rs`) take references to the data they need and never
touch state directly; mutations go through `QueueManager` methods.

### `AuthStore` owns the tokens

`token_state.rs::AuthStore` holds `Arc<ArcSwap<TokenState>>`.
`QueueManager`'s `ServerInfo` copies are a cache resynced after each
refresh. Refresh path:

1. `RefreshGuard::new(&store)` snapshots the current state.
2. `http::refresh_token` POSTs with the snapshot's refresh token.
3. Build `next` with `token_version + 1`.
4. `guard.try_apply(next)` CAS. `VersionMismatch` means another writer
   won; the caller handles retry, not us.
5. `update_access_token` syncs the new token into `content_server` and
   `queue_server`.

`sync_auth_from_server_info` calls `AuthStore::store` directly (no CAS)
so relogin can install fresh credentials after `invalid_grant` has
marked the previous generation `Terminated`.

### Generation lifecycle

`GenerationStatus`:

```
         ┌────────────────────────────┐
         │          Active            │
         └──┬─────────────────────────┘
            │ refresh_token()
            ▼
    ┌───────────────────┐     success      ┌──────────────┐
    │    Refreshing     │─────────────────▶│    Active    │
    │ { prev_version }  │     (new ver)    │ (new ver)    │
    └──┬────────────────┘                  └──────────────┘
       │ network/5xx/timeout                        ▲
       ▼                                            │ manual retry /
    ┌──────────────────────┐     retry              │ backoff elapsed
    │    RefreshFailed     │────────────────────────┘
    │ { attempt, retry_after }
    └──┬──────────────────┘
       │ backoff exhausted
       ▼
    ┌───────────────────┐
    │    Suspended      │  ← not terminal, manual retry still works
    └───────────────────┘

    any Active/Refreshing ──invalid_grant──▶ Terminated(InvalidGrant)
    any Active ─────────── logout ────────▶ Terminated(Logout)
    any Active ───────── 401 on use ──────▶ Terminated(Revoked)
    any Active ──── server-confirmed ─────▶ Terminated(ServerConfirmedReplay)
                      (reserved; not wired)
```

`Terminated(_)` refuses any further `compare_and_swap`; use `store` to
install a new generation after relogin.

### Terminal notification flag

`QueueManager.terminal_notified: bool` gates the outbound
`notifyQueueServerError` on `invalid_grant`. `sync_auth_from_server_info`
clears it whenever the wire installs a new generation, so a relogin
re-arms the notification.

### Task ownership

Long-lived tasks go in a `TaskGroup` so shutdown has a deadline and
panics surface. Per-connection tasks stay as raw `JoinHandle` + `abort()`
because `TaskGroup` task names are `&'static str` and connection scopes
need socket-id suffixes.

| Task | Owner | Deadline |
|------|-------|---|
| receiver routing loop | `ConnectReceiver.tasks` | 5 s |
| mDNS browser event loop | `ConnectManager.controller_tasks` | 2 s |
| WS server listener | `WsServer.tasks` | 2 s |
| WS server per-client read/write/heartbeat | `ClientHandle` (raw) | abort on disconnect |
| WS client per-connection read/write/heartbeat | `WsClient` (raw) | abort on `shutdown()` |
| IPC fire-and-forget handlers | raw `tokio::spawn` | ephemeral |
| controller session send (fire-and-forget) | raw `tokio::spawn` | ephemeral |

### Shutdown

`TaskGroup::shutdown(graceful_timeout)` does two passes:

1. Close the spawn gate, cancel the `CancellationToken`, wait up to
   `graceful_timeout`.
2. Abort the survivors, drain handles, label each via the `AtomicU8` state
   and `JoinError::try_into_panic`.

`TaskRecord::state` uses `compare_exchange` with `TaskState::can_transition`
gating moves. `PanicObserved`, `GracefulCompleted`, and `AbortObserved`
are terminal.

`MdnsBackend::shutdown(deadline)` is safe to call twice. It probes
`status()` first, retries `Error::Again` with capped backoff, treats any
other error as already stopped (the daemon's command channel is closed),
and returns `Degraded { retry_count, last_status, last_error }` on
deadline miss.

### Panic reporting requires `panic = "unwind"`

`src/connect/runtime.rs`:

```rust
const _: () = assert!(cfg!(panic = "unwind"));
```

`panic = "abort"` kills the process before tokio can join, so
`JoinError::try_into_panic` returns nothing. `scripts/check-panic-profile.sh`
guards `Cargo.toml` against a profile override.

### TLS hostname mismatch is expected

`ws::tls::TidalCertVerifier` accepts certificates whose SAN does not
match the hostname as long as the chain is TIDAL's. LAN devices are
addressed by IP, so SAN matching is impossible; the CA chain is what we
trust. Each acceptance is logged via `vprintln!` so a LAN MITM shows up
in traces.

### IPC event forwarding goes through `bridge::forward`

`ui::flush` calls `crate::connect::bridge::forward`. The bridge owns
`BRIDGE_TX`, `BRIDGE_ACTIVE`, `ENGINE_GEN`, and the `PlayerEvent` ->
`BridgeEvent` mapping; `ui::flush` never imports `BridgeEvent` or
`ConnectPlayerState`.

## Tests

- `cargo test --bin tidalunar connect::` runs all.
- `connect::token_state::` covers auth lifecycle + CAS races.
- `connect::runtime::` covers `TaskGroup` graceful / aborted / panicked.
- `connect::mdns::backend::` covers idempotent shutdown.

Mock WSS server: `src/connect/testing.rs::MockWsServer`.

`scripts/check-panic-profile.sh` greps `Cargo.toml` for `panic = "unwind"`
on `[profile.release]` and `[profile.dev]`.
