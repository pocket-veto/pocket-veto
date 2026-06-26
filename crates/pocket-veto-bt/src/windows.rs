//! Windows Bluetooth backend built on the [`serialport`] crate.
//!
//! # Compilation gate
//!
//! This module is only compiled on `target_os = "windows"` (via `#[cfg]` in
//! `lib.rs`). It will not build on the Linux devcontainer or CI's
//! `ubuntu-latest` / `macos-latest` runners.
//!
//! # How it works
//!
//! After the phone is paired once via Windows Settings -> Bluetooth, the SPP
//! (Serial Port Profile) service shows up as a virtual COM port (e.g.
//! `COM3`) in Device Manager under "Ports (COM & LPT)". The transport opens
//! that COM port with [`serialport`] at 115200 baud (the SPP default) and
//! treats the resulting byte stream as the same length-prefixed frame
//! channel the Linux/Android sides use.
//!
//! # Blocking IO
//!
//! [`serialport::SerialPort`] is synchronous (`std::io::Read` + `Write`). To
//! expose it through the async [`BtTransport`](crate::bridge::BtTransport)
//! trait, blocking reads and writes are dispatched onto the blocking thread
//! pool via [`tokio::task::spawn_blocking`]. This keeps the async runtime's
//! worker threads free. A future revision could switch to `tokio-serial`
//! for true async IO, but the blocking-pool approach is simpler and
//! sufficient for the low-throughput approval-gate workload.

use std::sync::Arc;
use std::time::Duration;

use serialport::SerialPort;
use tokio::sync::Mutex;
use tracing::{debug, info};

use crate::bridge::BtTransport;
use crate::frame::{read_length_prefixed_sync, write_length_prefixed_sync};

/// SPP default baud rate. The actual on-wire throughput is negotiated by the
/// Bluetooth radio and is largely independent of this setting, but 115200 is
/// the conventional value for SPP virtual COM ports.
const SPP_BAUD: u32 = 115_200;

/// Per-read timeout. Short enough that the bridge's heartbeat-timeout path
/// (45 s) kicks in before stalling forever on a quiet radio; long enough
/// that a busy poll loop does not spin the CPU.
const READ_TIMEOUT: Duration = Duration::from_secs(5);

/// COM-port-backed [`BtTransport`] for Windows.
///
/// Created with [`WindowsBtTransport::new`] with the COM port name (e.g.
/// `"COM3"`). The bridge calls `connect()` to open the port and `close()`
/// to drop it on reconnect.
pub struct WindowsBtTransport {
    /// The open serial port, guarded by a mutex so the blocking-pool tasks
    /// spawned for reads/writes do not race. `Arc` so the blocking task
    /// can own a reference without borrowing the bridge.
    port: Option<Arc<Mutex<Box<dyn SerialPort>>>>,
    com_port: String,
}

impl WindowsBtTransport {
    /// Create a not-yet-opened transport for `com_port` (e.g. `"COM3"`).
    /// The port is opened on the first `connect()` call.
    #[must_use]
    pub fn new(com_port: String) -> Self {
        Self {
            port: None,
            com_port,
        }
    }
}

impl BtTransport for WindowsBtTransport {
    async fn connect(&mut self) -> anyhow::Result<()> {
        // Drop any previous handle first.
        self.port = None;

        let com_port = self.com_port.clone();
        let port = tokio::task::spawn_blocking(move || -> anyhow::Result<Box<dyn SerialPort>> {
            let p = serialport::new(&com_port, SPP_BAUD)
                .timeout(READ_TIMEOUT)
                .open()
                .map_err(|e| anyhow::anyhow!("open {com_port}: {e}"))?;
            Ok(p)
        })
        .await??;

        info!(com_port = %self.com_port, baud = SPP_BAUD, "windows bt: opened COM port");
        self.port = Some(Arc::new(Mutex::new(port)));
        Ok(())
    }

    async fn read_frame(&mut self) -> anyhow::Result<Vec<u8>> {
        let port = self
            .port
            .clone()
            .ok_or_else(|| anyhow::anyhow!("windows bt: not connected"))?;

        tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<u8>> {
            let mut guard = port.blocking_lock();
            // Read the full frame (4-byte big-endian length prefix + payload)
            // and return it verbatim so the bridge can append it to its
            // accumulation buffer and decode it with `decode_client_message`,
            // which expects the buffer to start with the prefix.
            let frame = read_length_prefixed_sync(&mut *guard)?;
            debug!(bytes = frame.len(), "windows bt: read frame");
            Ok(frame)
        })
        .await?
    }

    async fn write_frame(&mut self, payload: &[u8]) -> anyhow::Result<()> {
        let port = self
            .port
            .clone()
            .ok_or_else(|| anyhow::anyhow!("windows bt: not connected"))?;
        // Copy the payload into an owned Vec so the blocking task can be
        // 'static and not borrow from `payload`.
        let owned = payload.to_vec();

        tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            let mut guard = port.blocking_lock();
            // The bridge hands the transport a JSON payload (no prefix); the
            // transport adds the 4-byte big-endian length prefix so the wire
            // format is exactly [u32 be len][payload]. `write_length_prefixed_sync`
            // writes prefix + payload and flushes.
            write_length_prefixed_sync(&mut *guard, &owned)?;
            debug!(payload_bytes = owned.len(), "windows bt: wrote frame");
            Ok(())
        })
        .await?
    }

    async fn close(&mut self) -> anyhow::Result<()> {
        // Drop the Arc<Mutex<...>>; when the last clone is released the port
        // is closed by its `Drop` impl.
        if self.port.take().is_some() {
            debug!("windows bt: closed COM port");
        }
        Ok(())
    }

    fn is_connected(&self) -> bool {
        self.port.is_some()
    }
}
