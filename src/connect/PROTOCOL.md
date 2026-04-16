# TIDAL Connect — module invariants

This is a short operator's reference for anyone touching `src/connect/`.
It states the invariants the module relies on and where they live.

## Layout

```
src/connect/
├── mod.rs              # ConnectManager coordination
├── consts.rs           # constants (intervals, service types, timeouts)
├── bridge.rs           # player → receiver event forwarding (statics)
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
├── ipc/                # frontend → Rust RPC, split by domain
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
│   ├── client.rs       # TLS client (controller → device)
│   ├── server.rs       # TLS server (accepts mobile clients)
│   ├── heartbeat.rs    # shared ping/pong driver
│   ├── tls.rs          # rustls config with TIDAL CA
│   └── pending.rs      # requestId → oneshot tracker
└── mdns/               # service discovery
    ├── advertiser.rs   # _tidalconnect._tcp advertisement
    ├── browser.rs      # peer discovery
    └── backend.rs      # MdnsBackend trait, bounded shutdown contract
```

## Invariants

### `QueueState` is private to the façade

`src/connect/receiver/queue/mod.rs` owns `enum QueueState`. Sub-modules
(`http.rs`, `media.rs`) take references to the data they need and never
touch state. All mutations go through `QueueManager` methods. Adding a new
state transition or side effect means adding a method on the façade, not
poking the enum from another file.

### `AuthStore` is the source of truth for tokens

`token_state.rs::AuthStore` holds `Arc<ArcSwap<TokenState>>`. The
wire-shaped `ServerInfo` copies inside `QueueManager` are a cache kept in
sync after every successful refresh. Write path:

1. `RefreshGuard::new(&store)` takes a snapshot.
2. `http::refresh_token(&client, &oauth, &snapshot.refresh_token)` does the
   POST. Returns `RefreshSuccess { access_token, refresh_token }`.
3. Build `next: TokenState` with `token_version + 1`.
4. `guard.try_apply(next)` CAS. On `VersionMismatch`, another writer won —
   do not retry here; the caller handles it.
5. `update_access_token(new_token)` syncs the cache into both
   `content_server` and `queue_server` ServerInfos.

The wire push path (`sync_auth_from_server_info`) uses `AuthStore::store`
(unconditional replace) because relogin must be able to install fresh
credentials even after `invalid_grant` marked the previous generation
`Terminated`.

### Generation lifecycle

Transitions declared by `GenerationStatus`:

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

### Exactly-once terminal notification

`QueueManager.terminal_notified: bool` gates the outbound
`notifyQueueServerError` on `invalid_grant`. It is cleared by
`sync_auth_from_server_info` whenever the wire installs a new generation,
so a relogin re-arms the notification for the new generation.

### Task ownership

Long-lived tasks live in `TaskGroup` instances so shutdown is bounded and
panics are classified. Per-connection tasks stay as raw `JoinHandle` +
explicit `abort()` because `TaskGroup` requires unique `&'static str` task
names and connection-scoped tasks need dynamic socket-id suffixes.

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

`TaskGroup::shutdown(graceful_timeout)` runs in two phases:

1. Close the spawn gate, cancel the shared `CancellationToken`, wait up to
   `graceful_timeout` for cooperating tasks to observe cancellation.
2. Abort whatever is still running, drain `JoinHandle`s, classify each via
   its shared `AtomicU8` state and the `JoinError` (`try_into_panic`).

`TaskRecord::state` transitions are enforced with
`compare_exchange`; terminal states (`PanicObserved`, `GracefulCompleted`,
`AbortObserved`) are sinks. Transitions are validated by
`TaskState::can_transition`.

`MdnsBackend::shutdown(deadline)` is idempotent and bounded: it probes
`status()` first (`AlreadyStopped` short-circuit), retries `Error::Again`
with capped backoff, treats any other non-`Again` error as
`AlreadyStopped` (the daemon's command channel is closed), and returns
`Degraded { retry_count, last_status, last_error }` on deadline miss.

### Panic reporting requires `panic = "unwind"`

`src/connect/runtime.rs` has a compile-time assertion:

```rust
const _: () = assert!(cfg!(panic = "unwind"));
```

With `panic = "abort"`, `JoinError::try_into_panic` cannot report anything
because the process is terminated before tokio classifies the join.
`scripts/check-panic-profile.sh` guards `Cargo.toml` against a profile
override at the repo level.

### TLS hostname mismatch is expected

`ws::tls::TidalCertVerifier` explicitly accepts certificates whose SAN
does not match the hostname when the CA chain is TIDAL's. Devices are
reached by IP address on the LAN; the CA chain is the real authenticator.
Each acceptance is logged via `vprintln!` so a LAN MITM would be visible
as an anomaly pattern in traces.

### IPC event forwarding goes through `bridge::forward`

Player events flow out of `ui::flush` via `crate::connect::bridge::forward`.
The bridge owns its own statics (`BRIDGE_TX`, `BRIDGE_ACTIVE`,
`ENGINE_GEN`) and its own `PlayerEvent → BridgeEvent` mapping, so
`ui::flush` never imports `BridgeEvent` or `ConnectPlayerState`.

## Testing entry points

- `cargo test --bin tidalunar connect::` — all unit tests.
- `cargo test --bin tidalunar connect::token_state::` — auth lifecycle
  and CAS adversarial tests.
- `cargo test --bin tidalunar connect::runtime::` — `TaskGroup` tests
  (graceful / aborted / panicked accounting).
- `cargo test --bin tidalunar connect::mdns::backend::` — idempotent
  shutdown behaviour.

Mock WSS helper: `src/connect/testing.rs` (`MockWsServer`).

## Scripts

- `scripts/check-panic-profile.sh` — parses `Cargo.toml` to verify
  `panic = "unwind"` on `[profile.release]` and `[profile.dev]`.
