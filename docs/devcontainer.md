# Devcontainer support

PocketVeto works inside a Docker devcontainer with no Bluetooth hardware in
the container. The key insight: **the devcontainer does not need Bluetooth.
Only the host does.**

## The host-sidecar pattern

```
+---------------------------+          +-------------------------+
| Host (Windows / Linux)    |          | Devcontainer            |
|                           |          |                         |
|  pocket-veto serve        |  HTTP    |  pocket-veto hook       |
|  bound to 0.0.0.0:38475   | <------> |  -> host.docker.internal|
|  owns the Bluetooth radio |          |                         |
|        |                  |          |  agent runs here        |
|        v RFCOMM           |          |                         |
|  Android phone            |          |                         |
+---------------------------+          +-------------------------+
```

1. `pocket-veto serve` runs on the host (Windows, Linux, or macOS). It owns
   the Bluetooth radio and, in devcontainer mode, listens on
   `0.0.0.0:38475` instead of `127.0.0.1:38475`.
2. The devcontainer's `pocket-veto hook` subcommand talks to the host server
   via `host.docker.internal:38475` (Docker Desktop) or
   `host-gateway:38475` (Linux Docker with
   `--add-host=host-gateway:host-gateway`).
3. The devcontainer's hook config (`.cursor/hooks.json` inside the container)
   points at the same `pocket-veto hook` binary, which reads its server URL
   from `~/.pocket-veto/config.toml` inside the container — or from the
   `POCKET_VETO_SERVER_URL` environment variable.
4. The bearer token gates the now-network-reachable endpoint. Generate the
   token on the host with `pocket-veto init`, then copy it into the
   container's config or env so the container's hook can authenticate.

This is why the devcontainer question is easy in the Rust design: the binary
is a single static file that runs identically inside and outside the
container, and the only thing that changes is the server URL in config.

## Setup

### On the host

Run init with devcontainer mode enabled. This binds `0.0.0.0` and prints the
URL to put in the container:

```sh
pocket-veto init --devcontainer
```

If you already have a config and just want to switch the bind address, edit
`~/.pocket-veto/config.toml`:

```toml
bind_addr = "0.0.0.0:38475"
devcontainer = true
```

Then restart the server. Confirm it is reachable from the container network:

```sh
curl -H "Authorization: Bearer <token>" http://host.docker.internal:38475/health
```

### In the devcontainer

Install the binary inside the container (it does not need Bluetooth — the
Linux build without `--features pocket-veto-bt/linux-bt` is fine). Then either set the
env var, or write a config file.

The simplest path is the env var. Add it to your `devcontainer.json`:

```json
{
  "postCreateCommand": "curl -fsSL https://github.com/pocket-veto/pocket-veto/releases/latest/download/install.sh | sh",
  "remoteEnv": { "POCKET_VETO_SERVER_URL": "http://host.docker.internal:38475" }
}
```

Note: the `install.sh` script also runs `pocket-veto init`, which inside a
container will not find a Bluetooth radio. That is fine — it writes a config
with the bearer token and hook configs, and you can override the server URL
via the env var. If you want a fully headless setup, use
`pocket-veto init --skip-bt --devcontainer` in the `postCreateCommand` and
pass the token in via another env var.

If you prefer a config file in the container, write
`~/.pocket-veto/config.toml` with:

```toml
server_url = "http://host.docker.internal:38475"
bind_addr = "127.0.0.1:38475"
token = "<same token as the host>"
db_path = "~/.pocket-veto/pocket-veto.sqlite"
approval_timeout_seconds = 300
bt_backend = "bluer"
devcontainer = true
```

The `bt_*` fields are ignored inside the container (the container never runs
the server), but the config loader expects them to be present or skipped via
`Option`.

## Linux Docker without Docker Desktop

On a native Linux Docker host there is no `host.docker.internal` by default.
Either:

- Add `--add-host=host-gateway:host-gateway` to your `docker run` (or the
  `runArgs` in `devcontainer.json`) and use `http://host-gateway:38475`, or
- Use the host's LAN IP directly (for example
  `http://192.168.1.10:38475`), or
- Run the container with `--network=host` (then `127.0.0.1:38475` works, but
  you lose network namespace isolation).

The `host-gateway` approach is the recommended one because it does not
require knowing the host's IP at build time.

## Security note

Binding the server to `0.0.0.0` makes it reachable from the host's network,
not just the container. The bearer token gates every route except `/health`,
so an attacker on the same network cannot Approve a tool call without the
token. That said, this mode only makes sense on a trusted home network —
which is the deployment target (the user is behind WiFi AP isolation and
wants the phone and PC on the same radio). Do not enable devcontainer mode on
an untrusted network. See [../ARCHITECTURE.md#security-model](../ARCHITECTURE.md#security-model).

## Expected build targets

The devcontainer installs `libdbus-1-dev` (see `.devcontainer/Dockerfile`), so the
Linux BlueZ backend **compiles** in the container. `bluetoothd` is NOT started
(no radio), so only the BUILD is supported in the container — runtime Bluetooth
testing requires a real Linux host with BlueZ. The `linux-bt-build` CI job
(`.github/workflows/ci.yml`) gives the same compile guarantee on every PR.

Cargo aliases (`.cargo/config.toml`) wrap the common invocations:

| Target | Command | Needs `libdbus-1-dev`? | Runs in devcontainer? | Notes |
|---|---|---|---|---|
| Default workspace build | `cargo build-dev` | No | Yes | pocket-veto-core + pocket-veto-bt (no linux backend) + pocket-veto |
| Linux BlueZ build | `cargo build-bt` | Yes (installed in devcontainer) | Build yes / Run no (no radio) | Verifies `bluer` backend compiles; cannot run without bluetoothd + hardware |
| Mock-enabled build | `cargo build-mock` | No | Yes | Enables the mock feature; for dev runs with the fake transport |
| Default test suite | `cargo test-dev` | No | Yes | All unit + integration tests on default features |
| Mock-feature test suite | `cargo test-mock` | No | Yes | Adds the framing round-trip + mock-backed integration tests |
| Release binary | `cargo build --release -p pocket-veto` | No | Yes | The shipped binary |
| Release binary + Linux BT | `cargo build --release -p pocket-veto --features pocket-veto-bt/linux-bt` | Yes | Build yes / Run no | The shipped binary for Linux hosts with BlueZ |
| Clippy gate | `cargo lint` | No | Yes | The CI gate |
| Fmt gate | `cargo fmt-check` | No | Yes | The CI gate |
| Doc gate | `cargo doc-check` | No | Yes | The CI gate |
