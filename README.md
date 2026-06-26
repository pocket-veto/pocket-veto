# pocket-veto

Local-only, Bluetooth-mediated approval gate and live progress dashboard for
AI coding agents.

## Why

You are away from the PC, screen off or busy. Your AI coding agents want to
run risky tools (shell commands, file writes, MCP calls) and you want to
approve or deny them from your phone. PocketVeto does that over Bluetooth
Classic, with no internet and no LAN routing, so it keeps working under WiFi
AP isolation. The same phone also shows a live dashboard of every running
agent so you can see what they are up to without watching a terminal.

## Status

v1. Supported on **Windows**, **native Linux**, and **Linux devcontainers**
(via a host-sidecar pattern). macOS Bluetooth is deferred to post-v1; the
non-Bluetooth parts of the binary still compile and run on macOS, so a Mac
can host agents that gate against a Linux/Windows server.

## Quickstart

1. **Install the binary** on the host machine that owns the Bluetooth radio:

   ```sh
   curl -fsSL https://github.com/pocket-veto/pocket-veto/releases/latest/download/install.sh | sh
   ```

   See [docs/setup.md](docs/setup.md) for Windows, build-from-source, and
   devcontainer variants.

2. **Initialize** configuration, hook installs, and the bearer token:

   ```sh
   pocket-veto init
   ```

   This pairs Bluetooth (or lets you skip it for a devcontainer-only setup),
   writes `~/.pocket-veto/config.toml`, installs hooks into `.cursor/hooks.json`
   and `.claude/settings.json`, and registers the server as a system service.

3. **Start the server** (or reboot; the service starts automatically):

   ```sh
   pocket-veto serve
   ```

4. **Install the Android app** by sideloading the APK from the latest GitHub
   release (or build it with `./gradlew assembleDebug` from `android/`), pair
   the phone with the PC if you have not already, and grant the runtime
   permissions. The dashboard and approval notifications appear automatically.

See [docs/setup.md](docs/setup.md) for the full walkthrough.

## How it works

- **Hook.** Cursor and Claude Code invoke `pocket-veto hook` for every tool
  call and lifecycle event. The hook reads stdin JSON, auto-detects the host,
  and talks to the local server over HTTP on `127.0.0.1:38475`.
- **Server.** `pocket-veto serve` is a single Rust binary running an axum HTTP
  API, a SQLite audit log, and a Bluetooth bridge task. It persists every
  event and approval, then forwards frames to the phone.
- **Phone.** The Android app holds an RFCOMM socket open via a foreground
  service, surfaces approval requests as action-button notifications, and
  renders a live dashboard of agent cards.

See [ARCHITECTURE.md](ARCHITECTURE.md) for the full picture.

## Supported hosts

- [Cursor](https://cursor.com) (`.cursor/hooks.json`)
- [Claude Code](https://code.claude.com) (`.claude/settings.json`)

Both are wired by `pocket-veto init`. The hook subcommand auto-detects which
host invoked it from the event-name casing.

## License

MIT. See [LICENSE](LICENSE).
