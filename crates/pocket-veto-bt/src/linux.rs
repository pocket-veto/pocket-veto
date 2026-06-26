//! Linux Bluetooth backend built on the [`bluer`] crate (`BlueZ` bindings).
//!
//! # Feature gate
//!
//! This module is only compiled when **both** of the following hold:
//!
//! - `target_os = "linux"` (via `#[cfg]` in `lib.rs`), and
//! - the `pocket-veto-bt/linux-bt` cargo feature is enabled.
//!
//! The feature pulls in `bluer` with its `rfcomm` and `bluetoothd` features.
//! Building `bluer` requires the **`libdbus-1-dev`** system package (the
//! devcontainer and CI intentionally do not install it, so the default
//! `cargo build --workspace` does not enable `linux-bt`).
//!
//! # System requirements at runtime
//!
//! `bluer` talks to the `BlueZ` daemon over D-Bus, so `bluetoothd` must be
//! running on the host. This is a host-only requirement — devcontainers do
//! not run the server and therefore do not need a Bluetooth stack.
//!
//! # Channel discovery
//!
//! [`Listener::bind`](bluer::rfcomm::Listener::bind) does **not** register an
//! SDP record, so the RFCOMM channel number must be known to both sides in
//! advance. The `pocket-veto init` subcommand handles pairing + channel
//! discovery once at setup time and stores the channel in
//! `~/.pocket-veto/config.toml`. The Android side connects via the standard
//! SPP UUID `00001101-0000-1000-8000-00805F9B347F`.

use bluer::Address;
use bluer::rfcomm::{Listener, SocketAddr, Stream};
use tokio::io::AsyncWriteExt;
use tracing::{debug, info, warn};

use crate::bridge::BtTransport;
use crate::frame::{read_length_prefixed, write_length_prefixed};

/// RFCOMM-server-backed [`BtTransport`] for Linux.
///
/// Created with [`LinuxBtTransport::new`], which binds a
/// [`bluer::rfcomm::Listener`] to `(adapter_addr, channel)`. The bridge then
/// calls `connect()` to `accept()` an inbound phone connection.
pub struct LinuxBtTransport {
    listener: Listener,
    stream: Option<Stream>,
}

impl LinuxBtTransport {
    /// Bind a RFCOMM listener on `(adapter_addr, channel)`.
    ///
    /// Does **not** register an SDP record; the channel must be pre-agreed
    /// with the phone (see the module docs).
    ///
    /// # Errors
    ///
    /// Returns an error if `Listener::bind` fails (adapter missing, channel
    /// in use, `bluetoothd` not running, etc.).
    pub async fn new(adapter_addr: Address, channel: u8) -> anyhow::Result<Self> {
        let sa = SocketAddr::new(adapter_addr, channel);
        let listener = Listener::bind(sa).await?;
        info!(%sa, "linux bt: bound RFCOMM listener");
        Ok(Self {
            listener,
            stream: None,
        })
    }
}

impl BtTransport for LinuxBtTransport {
    async fn connect(&mut self) -> anyhow::Result<()> {
        // If a live stream is already present, drop it first so a reconnect
        // starts clean.
        if let Some(mut s) = self.stream.take() {
            let _s = s.shutdown().await;
        }
        let (stream, peer) = self.listener.accept().await?;
        info!(%peer, "linux bt: phone connected");
        self.stream = Some(stream);
        Ok(())
    }

    async fn read_frame(&mut self) -> anyhow::Result<Vec<u8>> {
        let stream = self
            .stream
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("linux bt: not connected"))?;
        // Read the full frame (4-byte big-endian length prefix + payload) and
        // return it verbatim so the bridge can append it to its accumulation
        // buffer and decode it with `decode_client_message`, which expects the
        // buffer to start with the prefix. `read_length_prefixed` reads the
        // prefix big-endian (matching the codec/Android/windows) and enforces
        // `MAX_FRAME_SIZE`.
        let frame = read_length_prefixed(stream).await?;
        debug!(bytes = frame.len(), "linux bt: read frame");
        Ok(frame)
    }

    async fn write_frame(&mut self, payload: &[u8]) -> anyhow::Result<()> {
        let stream = self
            .stream
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("linux bt: not connected"))?;
        // The bridge hands the transport a JSON payload (no prefix); the
        // transport adds the 4-byte big-endian length prefix so the wire
        // format is exactly [u32 be len][payload].
        write_length_prefixed(stream, payload).await?;
        debug!(payload_bytes = payload.len(), "linux bt: wrote frame");
        Ok(())
    }

    async fn close(&mut self) -> anyhow::Result<()> {
        if let Some(mut s) = self.stream.take() {
            // Best-effort shutdown; a peer that has already disappeared will
            // surface an error that can be safely ignored.
            if let Err(e) = s.shutdown().await {
                warn!(error = %e, "linux bt: stream shutdown error (ignored)");
            }
        }
        Ok(())
    }

    fn is_connected(&self) -> bool {
        self.stream.is_some()
    }
}
