# PocketVeto Android app — contributor notes

This directory holds the Android side of PocketVeto: a Kotlin + Jetpack
Compose app that holds a Bluetooth Classic RFCOMM socket open to the PC server
and renders a live dashboard of running coding agents, with action-button
notifications for approval requests.

The Rust workspace under `../crates/` is the source of truth for the wire
protocol. **Anything you change on the wire here must be mirrored there and
vice versa.** See `Protocol-mirror verification` below.

## Foreground service

`BluetoothService` runs as a foreground service with
`android:foregroundServiceType="connectedDevice"` (declared in
`AndroidManifest.xml`). `minSdk = 31` and `targetSdk = 37`
(`app/build.gradle.kts`).

Permissions relevant to the service, all declared in `AndroidManifest.xml`:

- `FOREGROUND_SERVICE` — required since API 31 to start any foreground service.
- `FOREGROUND_SERVICE_CONNECTED_DEVICE` — required since API 34 for a
  `connectedDevice`-typed foreground service; still required at our target 37.
- `POST_NOTIFICATIONS` — runtime permission since API 33. The persistent
  notification still posts, but the user can disable the channel; we check
  `NotificationManagerCompat.areNotificationsEnabled()` in `BluetoothService`
  before posting any notification and silently skip if disabled.
- `BLUETOOTH_CONNECT`, `BLUETOOTH_SCAN`, `VIBRATE` — also declared.

The service must call `startForeground` within ~5s of `startService`. We do
this synchronously in `onStartCommand` (in the initial-start branch, before
launching the socket loop).

The service is **started, not bound**. `onBind` returns `null`; there are no
`bindService`/`ServiceConnection` calls anywhere in the app. `onStartCommand`
is the entry point for three intent actions: the initial start
(`ACTION_START`), forwarded decision intents
(`DecisionReceiver.ACTION_DECISION`, which do not re-promote to foreground),
and an explicit shutdown (`ACTION_STOP`). We deliberately do not bind because
the dashboard collects `AgentRegistry` flows directly (no binder needed).

`onStartCommand` returns `START_STICKY` for the start and decision paths so
the system recreates the service after a low-memory kill. The `ACTION_STOP`
branch returns `START_NOT_STICKY` instead — the user killed the service on
purpose and it must not come back until the app is reopened.

## Shutdown

The service has two user-facing shutdown paths, both converging on a single
private `shutdown()` helper (which delegates resource release to
`releaseResources()` and then calls
`ServiceCompat.stopForeground(this, STOP_FOREGROUND_REMOVE)` + `stopSelf()`):

1. **Stop button.** The dashboard top bar has a `PowerSettingsNew` icon that
   sends `ACTION_STOP` to the service via `startService`. The dashboard then
   calls `finish()` so the user is returned to the home screen. Reopening the
   app restarts the service through `DashboardActivity.maybeStartService()`.
2. **Swipe from recents.** `BluetoothService.onTaskRemoved` calls
   `shutdown()`, so swiping the app out of the recents list tears down the
   socket and stops the service. This is the documented Android hook for the
   swipe gesture on a started, non-bound service. Some OEM task managers may
   skip `onTaskRemoved`; the Stop button is the deterministic shutdown path,
   and `onTaskRemoved` is the best-effort hook for the swipe.

`onDestroy` calls only `releaseResources()` (not `stopForeground`/`stopSelf`,
which are redundant when the system is already tearing the service down).
`releaseResources()` is idempotent: it cancels the coroutine scope, closes +
clears the current socket, and pushes `Disconnected` to `AgentRegistry`.

## No auto-start on boot

The app deliberately does **not** auto-start on device reboot or after a
force-stop. Concretely:

- There is **no** `BOOT_COMPLETED` or `LOCKED_BOOT_COMPLETED` receiver in
  `AndroidManifest.xml`, and **no** `RECEIVE_BOOT_COMPLETED` permission.
- There is **no** `MY_PACKAGE_REPLACED` receiver, so an app update does not
  relaunch the service.
- There is **no** `WorkManager` / `JobScheduler` / `AlarmManager` that could
  reschedule a launch.

`START_STICKY` does **not** conflict with this: it only causes the system to
recreate the service after a low-memory kill while the device is already
running. It does **not** resurrect the service across a reboot (the manifest
has no boot receiver to receive `BOOT_COMPLETED`) or after a force-stop
(Android keeps a force-stopped app in a stopped state until the user
explicitly launches it). So the only ways the service starts are: the user
opens the app (`PairingActivity` → `DashboardActivity.maybeStartService()`),
or the system recreates it after a low-memory kill while the device stays on.

## Doze bypass strategy

Approval notifications are time-sensitive: the server-side approval times out
after 5 minutes (`DEFAULT_APPROVAL_TIMEOUT_SECONDS = 300` in
`crates/pocket-veto-core/src/config.rs`) and is mapped to `Decision::Deny` (fail-closed,
see `crates/pocket-veto/src/hook.rs`). To make sure the notification surfaces
promptly even when the phone is in Doze:

1. **Channel importance HIGH.** `CHANNEL_APPROVAL` is registered with
   `IMPORTANCE_HIGH` in `PocketVetoApplication.onCreate` (via
   `createNotificationChannels()`). This makes notifications on the channel
   pop heads-up and vibrate.
2. **`NotificationCompat.Category.CATEGORY_CALL`** on the approval
   notification (`NotifFactory.buildApprovalNotification`). Doze exempts
   `CATEGORY_CALL` notifications from most deferral, treating them like an
   incoming phone call. This is the documented Android 12+ pattern for
   "must surface now" notifications that are not actual phone calls.
3. **Vibrate pattern** `longArrayOf(0, 200, 100, 200)` set on the channel
   (so it persists even if the user later lowers the channel's importance in
   system settings) and on the notification itself.
4. **`lockscreenVisibility = VISIBILITY_PUBLIC`** on the channel so the
   action buttons are visible on the lock screen without unlocking.

The persistent service notification uses `IMPORTANCE_LOW` (channel) /
`PRIORITY_LOW` (notification) and `CATEGORY_SERVICE` — it must not interrupt
the user, only signal that the service is alive.

Channel IDs live in `NotifFactory` (`CHANNEL_PERSISTENT`,
`CHANNEL_APPROVAL`); the channels themselves are created in
`PocketVetoApplication.onCreate`.

## The SPP UUID choice

Both sides use the standard Serial Port Profile UUID
`00001101-0000-1000-8000-00805F9B347F` (defined in
`BluetoothService.UUID_SPP`). Rationale:

- It is the Bluetooth SIG-assigned UUID for SPP, so any RFCOMM service
  advertising under it is universally recognized as a serial stream.
- The Android `BluetoothDevice.createRfcommSocketToServiceRecord(uuid)` API
  performs an SDP lookup on the remote device and connects to whichever
  channel is registered under that UUID. On the Linux side, `bluer`'s
  `Listener::bind` does **not** register an SDP record (see the Rust
  `pocket-veto-bt/src/linux.rs` notes); the `pocket-veto init` subcommand records the
  channel number explicitly. The SPP UUID is still the right identifier on
  the Android side because it is what `createRfcommSocketToServiceRecord`
  expects, and the matching channel on the host is discovered by `init`.
- On Windows, the OS pairs the phone's SPP service to a virtual COM port
  (e.g. `COM3`); the Rust side opens that COM port directly via the
  `serialport` crate, so the UUID matters only on the Android side.

If you change the UUID here you must also change it in the Rust `init`
subcommand's pairing logic — they must match for the SDP lookup to succeed.

## The frame codec contract with the Rust side

Every frame on the RFCOMM byte stream is:

```
[4 bytes: big-endian uint32 length N][N bytes: UTF-8 JSON]
```

- The Rust side encodes with `u32::to_be_bytes()` (big-endian) and enforces a
  1 MiB cap (`pocket_veto_core::protocol::MAX_FRAME_SIZE`).
- The Android side reads with `DataInputStream.readInt()` (Java DataInput is
  defined as big-endian — no manual byte swap) and `readFully(N)`, and writes
  with `DataOutputStream.writeInt()` (big-endian) — see `FrameCodec.kt`.
- The cap on the Android side (`FrameCodec.MAX_FRAME_SIZE`) is the same 1 MiB.
  A declared length larger than the cap is treated as an unrecoverable
  protocol error (throws `IOException`, mirroring the Rust `frame too large`
  error) and forces a reconnect.

Do not change the byte order or the cap on either side without coordinating
across the workspace — the codec is symmetric by construction.

## Protocol-mirror verification

`data/Protocol.kt` mirrors `crates/pocket-veto-core/src/protocol.rs` field-for-field.
The two arrays of `@SerialName` annotations and Rust `#[serde(rename_all =
...)]` attributes must stay in sync. Concretely:

| Rust enum / struct   | Kotlin counterpart   | Rename rule                                     |
|----------------------|----------------------|-------------------------------------------------|
| `Host`               | `Host`               | `lowercase` / `@SerialName("cursor"\|"claude")` |
| `EventKind`          | `EventKind`          | `snake_case` / per-variant `@SerialName`        |
| `AgentStatus`        | `AgentStatus`        | `snake_case` / per-variant `@SerialName`        |
| `Decision`           | `Decision`           | `snake_case` / per-variant `@SerialName`        |
| `SubscriptionFilter` | `SubscriptionFilter` | field names `agent_ids`, `kinds`                |
| `ServerMessage`      | `ServerMessage`      | `tag = "type"`, `snake_case` variants           |
| `ClientMessage`      | `ClientMessage`      | `tag = "type"`, `snake_case` variants           |

On the Kotlin side the `tag = "type"` discriminator is set globally on the
shared `Json` instance (`ProtocolJson`: `classDiscriminator = "type"`,
`ignoreUnknownKeys = true`, `encodeDefaults = false`, `explicitNulls = false`)
rather than per-class, since kotlinx.serialization has no per-class tag
attribute. The wire effect matches Rust's `#[serde(tag = "type")]`.

The same `ProtocolJson` config (`ignoreUnknownKeys`, `encodeDefaults = false`,
`explicitNulls = false`) makes the wire bytes a Kotlin client produces match
what Rust's serde produces with
`#[serde(skip_serializing_if = "Option::is_none")]`.

## Build

The devcontainer does not have the Android SDK installed. Local builds use
the committed Gradle wrapper: `./gradlew assembleDebug` (Linux/mac) or
`gradlew.bat assembleDebug` (Windows). The wrapper scripts and
`gradle/wrapper/gradle-wrapper.jar` are tracked in git. Running Gradle
(including `./gradlew`) requires JDK 21+: `settings.gradle.kts` enforces
`JavaVersion.VERSION_21` at config-eval time, and the app targets Java 24
(`JvmTarget.JVM_24` / `JavaVersion.VERSION_24` in `app/build.gradle.kts`).
CI sets up JDK 24 to satisfy that floor.

CI and the release workflow share their setup via two composite actions —
`.github/actions/setup-rust/action.yml` and
`.github/actions/setup-android/action.yml` — called inline from the jobs in
`.github/workflows/ci.yml` and `.github/workflows/release.yml`. The
toolchain versions (Gradle, JDK, Android SDK levels, Rust) live in the
source-of-truth files those actions read — `gradle/wrapper/gradle-wrapper.properties`,
`settings.gradle.kts`, `app/build.gradle.kts`, and `rust-toolchain.toml` at
the repo root — not in this paragraph. Grep those files for the canonical
versions.

The app module uses the Compose BOM
(`androidx.compose:compose-bom:2024.10.01` in `app/build.gradle.kts`) so
there are no per-file Compose version drifts to chase.

## What is intentionally NOT here (v1 scope)

- No symmetric heartbeat timeout on the phone side. The Rust bridge forces
  a reconnect after 45s without an ack (`HEARTBEAT_TIMEOUT` in
  `pocket-veto-bt/src/bridge.rs`); on the phone side the read loop throwing on a dead
  socket is the reconnect trigger. A symmetric timeout is post-v1.
- No replay-on-reconnect request from the phone. The Rust bridge replays
  missed events proactively on every fresh connection
  (`pocket-veto-bt/src/bridge.rs::replay_missed_events`); the phone just dispatches
  what it receives.
- No tablet / large-screen layout. The dashboard is a single-column phone
  layout (`LazyColumn` in `DashboardActivity`); a two-pane layout for tablets
  is post-v1.
