# Setup guide

This walks through installing PocketVeto on the host machine that owns the
Bluetooth radio, installing the Android app on the phone, and verifying the
end-to-end flow. For the architecture and design rationale, see
[../ARCHITECTURE.md](../ARCHITECTURE.md). For common problems, see
[troubleshooting.md](troubleshooting.md).

## Prerequisites

### Linux (native host)

- `bluetoothd` running (standard on most desktop distros; on a server install
  you may need `sudo systemctl enable --now bluetooth`).
- `libdbus-1-dev` installed if you want the real Bluetooth backend. The
  `bluer` crate talks to BlueZ over D-Bus and needs the headers at build time:

  ```sh
  sudo apt install libdbus-1-dev   # Debian/Ubuntu
  sudo dnf install dbus-devel      # Fedora
  ```

- A Bluetooth adapter that supports Classic (BR/EDR). PocketVeto uses RFCOMM,
  not BLE.

### Windows

- The phone paired with the PC via Windows Settings -> Bluetooth. After
  pairing, the SPP (Serial Port Profile) service shows up as a virtual COM
  port (for example `COM3`) in Device Manager under "Ports (COM & LPT)".
- The `pocket-veto init` subcommand enumerates paired COM ports and helps you
  pick the right one.

### macOS

Bluetooth is deferred to post-v1 (see
[../ARCHITECTURE.md#macos-roadmap](../ARCHITECTURE.md#macos-roadmap)). The
non-Bluetooth parts of the binary compile and run on macOS, so a Mac can host
agents that gate against a Linux/Windows server, or act as a devcontainer host
that delegates the radio to a peer. To build on macOS today, omit the
`linux-bt` feature (it is Linux-only anyway).

### Devcontainer

No Bluetooth needed inside the container. The container only runs
`pocket-veto hook`, which talks to a server on the host. See
[devcontainer.md](devcontainer.md).

## Install

Pick one of three options.

### Option A: download the release binary

The easiest path. The install scripts detect the OS and architecture, download
the right binary from GitHub Releases, put it on PATH, and run `pocket-veto
init`:

```sh
# Linux / macOS
curl -fsSL https://github.com/pocket-veto/pocket-veto/releases/latest/download/install.sh | sh
```

```powershell
# Windows (PowerShell)
irm https://github.com/pocket-veto/pocket-veto/releases/latest/download/install.ps1 | iex
```

### Option B: `cargo install`

```sh
cargo install pocket-veto
```

For Linux with the real Bluetooth backend:

```sh
cargo install pocket-veto --features pocket-veto-bt/linux-bt
```

This requires `libdbus-1-dev` (see Prerequisites).

### Option C: build from source

```sh
git clone https://github.com/pocket-veto/pocket-veto.git
cd pocket-veto
cargo build --release
```

For Linux with the real Bluetooth backend:

```sh
sudo apt install libdbus-1-dev   # or your distro equivalent
cargo build --release --features pocket-veto-bt/linux-bt
```

The resulting binary is at `target/release/pocket-veto` (or
`target/release/pocket-veto.exe` on Windows). Copy it somewhere on PATH.

Without `--features pocket-veto-bt/linux-bt`, the Linux build still works: the server,
hook, and init subcommands all run, and the bridge outbox is drained to a
no-op logger. The phone just never receives frames. This is the configuration
CI and the devcontainer use.

## `pocket-veto init`

Run once after the binary is on PATH. It performs six steps:

1. **Bluetooth pairing** (platform-specific, skippable with `--skip-bt`):
   - Linux: makes the adapter discoverable via `bluer`, prints the adapter
     address, prompts you to pair from the phone, then accepts the inbound
     RFCOMM connection and records the channel.
   - Windows: prompts you to pair via Windows Settings, then enumerates paired
     SPP COM ports via `serialport::available_ports` and lets you pick.
2. **Channel / COM port detection**: stored in `~/.pocket-veto/config.toml`.
3. **Hook config installation**: detects installed agent hosts (looks for
   `.cursor/` and `.claude/` in the home dir and cwd) and writes the hook
   configs. Use `--install-hooks cursor|claude|both|none` to scope this.
4. **Bearer token generation**: 32 random bytes, hex-encoded (64 chars),
   written to config.
5. **Service registration**:
   - Linux: writes `~/.config/systemd/user/pocket-veto.service` and runs
     `systemctl --user enable --now pocket-veto`.
   - Windows: writes a Registry Run key (or a scheduled task).
   - macOS: writes `~/Library/LaunchAgents/io.pocketveto.plist`. The server
     runs without Bluetooth, useful as a devcontainer host.
6. **Devcontainer support**: asks whether to enable it. If yes, the server
   binds `0.0.0.0:38475` instead of `127.0.0.1:38475`, and `init` prints the
   `host.docker.internal` URL to put in the container's config. Pass
   `--devcontainer` to skip the prompt.

Useful flags:

```sh
pocket-veto init --skip-bt --devcontainer   # headless / container-host setup
pocket-veto init --bt-channel 1             # Linux: skip the channel prompt
pocket-veto init --bt-com-port COM3         # Windows: skip the COM prompt
pocket-veto init --keep-token               # re-init without rotating the token
```

The config file lands at `~/.pocket-veto/config.toml` (mode 0600 on Unix):

```toml
server_url = "http://127.0.0.1:38475"
bind_addr = "127.0.0.1:38475"     # or 0.0.0.0:38475 with devcontainer = true
token = "<64 hex chars>"
db_path = "~/.pocket-veto/pocket-veto.sqlite"
approval_timeout_seconds = 300
bt_backend = "bluer"              # linux | serialport (windows)
bt_com_port = "COM3"             # windows only
bt_adapter_addr = "AA:BB:CC:DD:EE:FF"  # linux only
bt_channel = 1                   # linux only
devcontainer = false
```

## Install the Android app

1. **Get the APK.** Either:
   - Download `pocket-veto.apk` from the latest GitHub release, or
   - Build it from source:

     ```sh
     cd android
     ./gradlew assembleDebug
     # output: android/app/build/outputs/apk/debug/app-debug.apk
     ```
2. **Sideload.** Copy the APK to the phone and install it. You may need to
   enable "Install unknown apps" for your file manager / browser.
3. **Pair.** If you have not already, pair the phone with the PC via the
   system Bluetooth settings. On the phone, open PocketVeto and pick the PC
   from the paired-devices list in `PairingActivity`.
4. **Grant permissions.** On first launch the app requests
   `BLUETOOTH_CONNECT`, `BLUETOOTH_SCAN`, and `POST_NOTIFICATIONS` at runtime.
   Approve all three — the foreground service and notifications do not work
   without them.

The phone needs Android 12 (API 31) or newer.

## Verify it works

1. **Confirm the server is up.** On the host:

   ```sh
   curl -H "Authorization: Bearer <token>" http://127.0.0.1:38475/health
   ```

   `token` is in `~/.pocket-veto/config.toml`. You should get a JSON status
   with the Bluetooth bridge state.

2. **Trigger a test hook.** Run any agent in a Cursor or Claude Code session
   in a workspace whose `.cursor/hooks.json` or `.claude/settings.json` points
   at `pocket-veto hook`. A `PreToolUse` event should make the phone vibrate
   and surface an approval notification.

3. **Check the phone dashboard.** Open PocketVeto on the phone. You should
   see a card for the running agent with its name, elapsed time, and last
   tool. Approval requests show inline Allow / Deny / Reply buttons as well
   as in the notification shade.

If something is off, see [troubleshooting.md](troubleshooting.md).
