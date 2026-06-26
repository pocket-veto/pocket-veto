package io.pocketveto.data

import io.pocketveto.data.FrameCodec.MAX_FRAME_SIZE
import java.io.DataInputStream
import java.io.DataOutputStream
import java.io.IOException

/**
 * Length-prefixed frame codec for the RFCOMM byte stream.
 *
 * Frame format (identical to `pocket_veto_core::protocol` on the Rust side):
 *
 * ```
 * [4 bytes: big-endian uint32 length N][N bytes: UTF-8 JSON]
 * ```
 *
 * The length prefix is written by Rust as `u32::to_be_bytes()`; on the
 * Android side we use [DataOutputStream.writeInt] / [DataInputStream.readInt],
 * which are defined by the Java DataInput/DataOutput contracts as
 * *big-endian*. So the two sides agree on byte order without any manual
 * byte swapping.
 *
 * Frames are capped at [MAX_FRAME_SIZE] (1 MiB) to match the Rust
 * `MAX_FRAME_SIZE` constant and prevent a malformed length prefix from
 * triggering an OOM allocation.
 */
object FrameCodec {

    /** Maximum frame size (length prefix + payload), matching the Rust cap. */
    const val MAX_FRAME_SIZE: Int = 1024 * 1024

    /**
     * Read one length-prefixed frame from [input].
     *
     * Blocks until a full frame is available. Throws [IOException] on EOF or
     * if the declared length exceeds [MAX_FRAME_SIZE] (mirrors the Rust
     * `frame too large` error, which is unrecoverable and forces a
     * reconnect).
     *
     * @throws IOException if the stream ends before a full frame is read or
     *   the declared length exceeds [MAX_FRAME_SIZE].
     */
    @Throws(IOException::class)
    fun readFrame(input: DataInputStream): ByteArray {
        val len = input.readInt()
        if (len !in 0..MAX_FRAME_SIZE) {
            throw IOException("frame too large: declared $len bytes > $MAX_FRAME_SIZE max")
        }
        val buf = ByteArray(len)
        // readFully blocks until len bytes arrive or throws on EOF.
        input.readFully(buf)
        return buf
    }

    /**
     * Write [payload] as one length-prefixed frame to [output].
     *
     * @throws IOException if [payload] exceeds [MAX_FRAME_SIZE] or the
     *   underlying stream fails.
     */
    @Throws(IOException::class)
    fun writeFrame(output: DataOutputStream, payload: ByteArray) {
        if (payload.size > MAX_FRAME_SIZE) {
            throw IOException("payload exceeds MAX_FRAME_SIZE: ${payload.size}")
        }
        // writeInt writes 4 big-endian bytes — exactly u32::to_be_bytes().
        output.writeInt(payload.size)
        output.write(payload)
        output.flush()
    }

    /** Decode a [ServerMessage] from a raw frame payload (UTF-8 JSON). */
    fun decodeServerMessage(payload: ByteArray): ServerMessage =
        ProtocolJson.decodeFromString(ServerMessage.serializer(), payload.toString(Charsets.UTF_8))

    /** Encode a [ClientMessage] to a raw frame payload (UTF-8 JSON bytes). */
    fun encodeClientMessage(msg: ClientMessage): ByteArray {
        val json = ProtocolJson.encodeToString(ClientMessage.serializer(), msg)
        return json.toByteArray(Charsets.UTF_8)
    }

    /**
     * Read one [ServerMessage] off [input]: reads the length-prefixed frame
     * and decodes the JSON.
     */
    @Throws(IOException::class)
    fun readServerMessage(input: DataInputStream): ServerMessage =
        decodeServerMessage(readFrame(input))

    /**
     * Write [msg] as one length-prefixed [ClientMessage] frame to [output].
     */
    @Throws(IOException::class)
    fun writeClientMessage(output: DataOutputStream, msg: ClientMessage) {
        writeFrame(output, encodeClientMessage(msg))
    }
}
