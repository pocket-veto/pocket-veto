package io.pocketveto.data

import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertThrows
import org.junit.Assert.assertTrue
import org.junit.Test
import java.io.ByteArrayInputStream
import java.io.ByteArrayOutputStream
import java.io.DataInputStream
import java.io.DataOutputStream
import java.io.IOException

/**
 * JVM unit tests for [FrameCodec].
 *
 * These mirror the Rust frame-codec tests in
 * `crates/pocket-veto-core/src/protocol.rs` (round-trip, big-endian length
 * prefix, oversized-frame rejection) so a regression on either side of the
 * wire is caught in CI without an emulator. The frame format is
 * `[4-byte big-endian u32 length N][N bytes UTF-8 JSON]`, matching Rust's
 * `u32::to_be_bytes()`.
 */
class FrameCodecTest {

    @Test
    fun writeFrameThenReadFrameRoundTripsPayload() {
        val payload = byteArrayOf(0x48, 0x65, 0x6c, 0x6c, 0x6f) // "Hello"
        val baos = ByteArrayOutputStream()
        FrameCodec.writeFrame(DataOutputStream(baos), payload)

        val read = FrameCodec.readFrame(DataInputStream(ByteArrayInputStream(baos.toByteArray())))
        assertArrayEquals(payload, read)
    }

    @Test
    fun lengthPrefixIsBigEndian() {
        // 0x00010203 (66051) exercises three non-zero prefix bytes, so a
        // little-endian writer would produce a visibly different prefix.
        val payload = ByteArray(0x00010203)
        val baos = ByteArrayOutputStream()
        FrameCodec.writeFrame(DataOutputStream(baos), payload)

        val raw = baos.toByteArray()
        val prefix = raw.copyOf(4)
        // Big-endian encoding of 66051 == 0x00010203.
        assertArrayEquals(byteArrayOf(0x00, 0x01, 0x02, 0x03), prefix)

        // Independent big-endian decode — not via DataInputStream.readInt,
        // which shares the same contract under test here.
        val decodedLen = (prefix[0].toInt() and 0xff shl 24) or
            (prefix[1].toInt() and 0xff shl 16) or
            (prefix[2].toInt() and 0xff shl 8) or
            (prefix[3].toInt() and 0xff)
        assertEquals(payload.size, decodedLen)
    }

    @Test
    fun readFrameRejectsOversizedDeclaredLength() {
        // 4-byte big-endian prefix = 2 MiB (0x00200000), then a tiny body.
        // Mirrors the Rust `decode_oversized_declared_length_errors_not_eof`.
        val buf = byteArrayOf(0x00, 0x20, 0x00, 0x00, '{'.code.toByte(), '}'.code.toByte())
        assertThrows(IOException::class.java) {
            FrameCodec.readFrame(DataInputStream(ByteArrayInputStream(buf)))
        }
    }

    @Test
    fun writeFrameRejectsOversizedPayload() {
        val out = DataOutputStream(ByteArrayOutputStream())
        assertThrows(IOException::class.java) {
            FrameCodec.writeFrame(out, ByteArray(FrameCodec.MAX_FRAME_SIZE + 1))
        }
    }

    @Test
    fun decodeServerMessageParsesHeartbeat() {
        val msg = FrameCodec.decodeServerMessage(
            """{"type":"heartbeat","ts":42}""".toByteArray(Charsets.UTF_8),
        )
        assertTrue(msg is ServerMessage.Heartbeat)
        val hb = msg as ServerMessage.Heartbeat
        assertEquals(42L, hb.ts)
    }

    @Test
    fun encodeClientMessageProducesHeartbeatAckJson() {
        val bytes = FrameCodec.encodeClientMessage(ClientMessage.HeartbeatAck(ts = 7L))
        assertEquals("""{"type":"heartbeat_ack","ts":7}""", bytes.toString(Charsets.UTF_8))
    }

    @Test
    fun readServerMessageFramesAndDecodesHeartbeat() {
        // Build a framed heartbeat by hand: 4-byte BE length + JSON payload,
        // then read it back through the full frame+decode path.
        val json = """{"type":"heartbeat","ts":99}""".toByteArray(Charsets.UTF_8)
        val frame = ByteArray(4 + json.size)
        frame[0] = (json.size shr 24).toByte()
        frame[1] = (json.size shr 16).toByte()
        frame[2] = (json.size shr 8).toByte()
        frame[3] = json.size.toByte()
        System.arraycopy(json, 0, frame, 4, json.size)

        val msg = FrameCodec.readServerMessage(DataInputStream(ByteArrayInputStream(frame)))
        assertTrue(msg is ServerMessage.Heartbeat)
        val hb = msg as ServerMessage.Heartbeat
        assertEquals(99L, hb.ts)
    }
}
