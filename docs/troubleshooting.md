# Troubleshooting

A practical FAQ for the most common PocketVeto failure modes. If something
here does not cover your issue, check the per-crate `AGENTS.md` files for
technical notes and mandatory rules, or [architecture.md](architecture.md) for the data flow.

## `config not found` / hook exits with a config error

The hook subcommand could not find `~/.pocket-veto/config.toml`.

**Fix:** run `pocket-veto init` on the host that owns the Bluetooth radio.
That writes the config file (mode 0600), generates the bearer token, and
installs the hook configs. If you are inside a devcontainer, the container
needs its own config pointing at `host.docker.internal` — see
[devcontainer.md](devcontainer.md).

## Hook exits 2 on every tool call

Exit code 2 is the fail-closed contract: the agent is denied because
PocketVeto could not reach a decision. Two common causes:

1. **The server is not running.** Check `pocket-veto serve` is up, or that
   the systemd / launchd / Windows-autostart service is enabled:

   ```sh
   systemctl --user status pocket-veto   # Linux
   ```
2. **Bluetooth is disconnected and the approval timed out.** The hook
   long-polls `GET /approvals/:id/wait` for 300 seconds. If the phone never
   answers (because the Bluetooth link is down or the app is not running),
   the wait times out, the decision defaults to deny, and the hook exits 2.
   Check the phone app is open and the foreground service notification is
   present. Check `pocket-veto serve` logs for bridge reconnects.

A deny here is correct behavior: a dead approval gate must not silently allow
risky tools. See `crates/pocket-veto/src/hook.rs` for the exact fail-closed
matrix.

## Phone not receiving events

The server is running and the hook fires, but the phone dashboard and
notifications stay empty.

1. **Check Bluetooth pairing.** The phone and PC must be paired at the system
   level. On the phone, open PocketVeto's `PairingActivity` and pick the PC
   from the paired-devices list. On Linux, `bluetoothctl devices` should list
   the phone; on Windows, Settings -> Bluetooth should show the phone as
   connected.
2. **Check the RFCOMM channel / SPP COM port.** On Linux, the channel in
   `~/.pocket-veto/config.toml` (`bt_channel`) must match what `init`
   recorded. On Windows, `bt_com_port` must be the SPP COM port (for example
   `COM3`), not some other serial device. Re-run `pocket-veto init` to
   re-detect.
3. **Check `pocket-veto serve` logs.** Look for `bt bridge: connected` (the
   bridge accepted the phone) and `bt bridge: wrote outbox frame` (frames are
   flowing). If you see `bt bridge: connect failed` repeating, the listener
   is not accepting connections — usually a pairing or channel mismatch.

## Linux build fails on `bluer` / `dbus`

The `bluer` crate needs `libdbus-1-dev` at build time. Without it you will
see errors about `dbus-1` not being found, or a missing `pkg-config` entry.

**Fix:**

```sh
sudo apt install libdbus-1-dev   # Debian/Ubuntu
sudo dnf install dbus-devel      # Fedora
sudo pacman -S dbus              # Arch
```

Then build with the feature enabled:

```sh
cargo build --release --features pocket-veto-bt/linux-bt
```

Without `--features pocket-veto-bt/linux-bt`, the Linux backend is not compiled and
the build succeeds without `libdbus-1-dev` — but the phone never receives
frames (the bridge outbox is drained to a no-op logger). This is the
configuration CI and the devcontainer use.

## Devcontainer can't reach the server

The container's `pocket-veto hook` cannot reach the server on the host.

1. **Use the host alias, not `127.0.0.1`.** Inside the container,
   `127.0.0.1` is the container itself. Set the server URL to
   `http://host.docker.internal:38475` (Docker Desktop) or
   `http://host-gateway:38475` (Linux Docker with
   `--add-host=host-gateway:host-gateway`).
2. **The server must bind `0.0.0.0`.** Run `pocket-veto init --devcontainer`
   on the host, or edit `~/.pocket-veto/config.toml` to set
   `bind_addr = "0.0.0.0:38475"` and `devcontainer = true`, then restart the
   server. The bearer token gates the now-network-reachable endpoint.
3. **Set the env var.** The container's config can be overridden by
   `POCKET_VETO_SERVER_URL` so you do not need a per-container config file.
   See [devcontainer.md](devcontainer.md).

## Cursor agent not triggering hooks

You installed PocketVeto but Cursor is not calling the hook.

1. **Check `.cursor/hooks.json` was written.** `pocket-veto init` writes it
   to the project directory (or `~/.cursor/hooks.json` for user-level). If
   it is missing, re-run `pocket-veto init --install-hooks cursor`.
2. **Check `failClosed` is `true`** on the blocking hooks
   (`beforeShellExecution`, `preToolUse`, `beforeMCPExecution`). Without it,
   a dead server would silently let the tool through — the wrong default for
   an approval gate. The init subcommand sets this for you; do not remove it.
3. **Check the binary is on PATH** in the environment Cursor runs in. If
   Cursor was started before you installed PocketVeto, it may have a stale
   `PATH`. Restart Cursor.
4. **Check the matcher.** The Cursor `preToolUse` hook uses
   `"matcher": "Shell|Write|Task|MCP:.*"`. Tools outside that matcher are
   not gated. Adjust the matcher if you want a wider net.

## Claude Code agent not triggering hooks

Same idea, different file: check `.claude/settings.json` has a `hooks`
section with `PreToolUse`, `PostToolUse`, `Stop`, and `SessionStart` entries
pointing at `pocket-veto hook`. Re-run `pocket-veto init --install-hooks
claude` if missing. Claude Code uses exit code 2 from the binary for
fail-closed (no `failClosed` field in the config).

## `pocket-veto init` fails to register the service

- **Linux systemd:** the user-level systemd instance must be running. On a
  headless server you may need `loginctl enable-linger $USER` so the user
  service survives logout. The unit file lands at
  `~/.config/systemd/user/pocket-veto.service`.
- **Windows:** `init` writes a Registry Run key
  (`HKCU\Software\Microsoft\Windows\CurrentVersion\Run`). If you want a
  scheduled task instead, that is a future enhancement — for v1 the Run key
  is the supported path.
- **macOS:** the launchd plist lands at
  `~/Library/LaunchAgents/io.pocketveto.plist`. The server runs without
  Bluetooth, which is useful as a devcontainer host.

## Bluetooth keeps dropping

The bridge sends a heartbeat every 15 seconds and forces a reconnect if no
frame arrives within 45 seconds (three missed acks). Frequent drops usually
mean the phone is going to sleep and killing the foreground service.

- Confirm the PocketVeto foreground service notification is present on the
  phone. If Android killed the service for memory pressure, it should
  restart automatically (`START_STICKY`).
- Disable battery optimization for PocketVeto in the phone's system settings
  (Apps -> PocketVeto -> Battery -> Unrestricted).
- Keep the phone within Bluetooth range of the PC. Classic Bluetooth range
  is roughly 10 meters, less through walls.
