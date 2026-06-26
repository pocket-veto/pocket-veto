//! Shared length-prefix framing for the Bluetooth transports.
//!
//! The wire format is `[4-byte big-endian u32 length][JSON payload]`, matching
//! [`pocket_veto_core::protocol`]. Centralizing the framing here keeps the
//! platform backends (`linux`, `windows`) thin and prevents the framing logic
//! from drifting across backends. [`read_length_prefixed`] and
//! [`read_length_prefixed_sync`] return the **full frame** (prefix + payload)
//! so the bridge can feed the bytes straight into
//! [`pocket_veto_core::protocol::decode_client_message`], which expects the
//! buffer to start with the length prefix. [`write_length_prefixed`] and
//! [`write_length_prefixed_sync`] take a JSON **payload** and add the prefix,
//! so the transport owns the wire framing end to end.

use std::io::{Read, Write};

use anyhow::anyhow;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Length of the big-endian `u32` length prefix, in bytes. Matches
/// `pocket_veto_core::protocol::LENGTH_PREFIX_BYTES`.
const LENGTH_PREFIX_BYTES: usize = 4;

/// Decode the 4-byte big-endian prefix into a payload length, enforcing
/// `MAX_FRAME_SIZE` on the **total** frame size (prefix + payload) — matching
/// `pocket_veto_core::protocol::decode_frame`, so a transport never accepts a
/// frame the bridge decoder would reject.
// justification: u32→usize is lossless on the 64-bit targets this crate ships
// on; `len` is immediately bounds-checked against MAX_FRAME_SIZE below.
#[allow(clippy::as_conversions)]
fn decode_len(prefix: [u8; LENGTH_PREFIX_BYTES]) -> anyhow::Result<usize> {
    let len = u32::from_be_bytes(prefix) as usize;
    let total = LENGTH_PREFIX_BYTES
        .checked_add(len)
        .ok_or_else(|| anyhow!("bt frame: declared length overflows usize"))?;
    if total > pocket_veto_core::protocol::MAX_FRAME_SIZE {
        return Err(anyhow!(
            "bt frame: frame size {total} exceeds MAX_FRAME_SIZE {}",
            pocket_veto_core::protocol::MAX_FRAME_SIZE
        ));
    }
    Ok(len)
}

/// Validate the payload length: it must fit in a `u32` and the total frame
/// size (prefix + payload) must not exceed `MAX_FRAME_SIZE`. Returns the
/// payload length as a `u32` on success.
fn checked_u32_len(payload: &[u8]) -> anyhow::Result<u32> {
    let total = LENGTH_PREFIX_BYTES
        .checked_add(payload.len())
        .ok_or_else(|| anyhow!("bt frame: payload length overflows usize"))?;
    if total > pocket_veto_core::protocol::MAX_FRAME_SIZE {
        return Err(anyhow!(
            "bt frame: frame size {total} exceeds MAX_FRAME_SIZE {}",
            pocket_veto_core::protocol::MAX_FRAME_SIZE
        ));
    }
    u32::try_from(payload.len()).map_err(|_e| anyhow!("bt frame: payload length exceeds u32"))
}

/// Build a full length-prefixed frame (prefix + payload) from a JSON payload.
/// Enforces `MAX_FRAME_SIZE` on the total frame size. Used by transports that
/// exchange whole frames over an in-order channel (the mock backend).
///
/// # Errors
///
/// Returns an error if `prefix + payload` overflows `usize` or `u32`, or if the
/// total frame size exceeds `MAX_FRAME_SIZE`.
// justification: u32→usize is lossless on the 64-bit targets this crate ships
// on; `len` was just validated by `checked_u32_len` against MAX_FRAME_SIZE.
#[allow(clippy::as_conversions)]
pub fn build_frame(payload: &[u8]) -> anyhow::Result<Vec<u8>> {
    let len = checked_u32_len(payload)?;
    let mut frame = Vec::with_capacity(LENGTH_PREFIX_BYTES + len as usize);
    frame.extend_from_slice(&len.to_be_bytes());
    frame.extend_from_slice(payload);
    Ok(frame)
}

/// Read one length-prefixed frame from `r` and return the **full frame**
/// (prefix + payload). Enforces `MAX_FRAME_SIZE` on the total frame size.
///
/// # Errors
///
/// Returns an error if the prefix cannot be read, the declared length
/// overflows or exceeds `MAX_FRAME_SIZE`, or the payload read is short.
pub async fn read_length_prefixed<R: AsyncRead + Unpin>(r: &mut R) -> anyhow::Result<Vec<u8>> {
    let mut prefix = [0u8; LENGTH_PREFIX_BYTES];
    r.read_exact(&mut prefix).await?;
    let len = decode_len(prefix)?;
    let mut frame = vec![0u8; LENGTH_PREFIX_BYTES + len];
    frame[..LENGTH_PREFIX_BYTES].copy_from_slice(&prefix);
    r.read_exact(&mut frame[LENGTH_PREFIX_BYTES..]).await?;
    Ok(frame)
}

/// Synchronous counterpart of [`read_length_prefixed`] for blocking transports
/// (the Windows `serialport` backend reads via `std::io::Read` on the blocking
/// thread pool). Returns the full frame (prefix + payload).
///
/// # Errors
///
/// Returns an error if the prefix cannot be read, the declared length
/// overflows or exceeds `MAX_FRAME_SIZE`, or the payload read is short.
pub fn read_length_prefixed_sync<R: Read + Unpin>(r: &mut R) -> anyhow::Result<Vec<u8>> {
    let mut prefix = [0u8; LENGTH_PREFIX_BYTES];
    r.read_exact(&mut prefix)?;
    let len = decode_len(prefix)?;
    let mut frame = vec![0u8; LENGTH_PREFIX_BYTES + len];
    frame[..LENGTH_PREFIX_BYTES].copy_from_slice(&prefix);
    r.read_exact(&mut frame[LENGTH_PREFIX_BYTES..])?;
    Ok(frame)
}

/// Write one length-prefixed frame to `w`: 4-byte big-endian length prefix
/// followed by `payload`. Enforces `MAX_FRAME_SIZE` on the total frame size.
///
/// # Errors
///
/// Returns an error if `payload` cannot be framed (overflow / too large) or
/// the write/flush fails.
pub async fn write_length_prefixed<W: AsyncWrite + Unpin>(
    w: &mut W,
    payload: &[u8],
) -> anyhow::Result<()> {
    let frame = build_frame(payload)?;
    w.write_all(&frame).await?;
    w.flush().await?;
    Ok(())
}

/// Synchronous counterpart of [`write_length_prefixed`] for blocking transports
/// (the Windows `serialport` backend writes via `std::io::Write` on the
/// blocking thread pool).
///
/// # Errors
///
/// Returns an error if `payload` cannot be framed (overflow / too large) or
/// the write/flush fails.
pub fn write_length_prefixed_sync<W: Write + Unpin>(
    w: &mut W,
    payload: &[u8],
) -> anyhow::Result<()> {
    let frame = build_frame(payload)?;
    w.write_all(&frame)?;
    w.flush()?;
    Ok(())
}
