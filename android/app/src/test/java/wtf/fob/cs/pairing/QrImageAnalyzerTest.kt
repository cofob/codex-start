package wtf.fob.cs.pairing

import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Test
import wtf.fob.cs.app.*
import wtf.fob.cs.conversation.*
import wtf.fob.cs.data.*
import wtf.fob.cs.navigation.*
import wtf.fob.cs.pairing.*
import wtf.fob.cs.settings.*
import wtf.fob.cs.tasks.*
import wtf.fob.cs.terminal.*
import wtf.fob.cs.ui.*
import wtf.fob.cs.workspace.*
import java.nio.ByteBuffer

class QrImageAnalyzerTest {
    @Test fun copiesLumaWithRowAndPixelPaddingWithoutMovingBuffer() {
        val source = ByteBuffer.wrap(byteArrayOf(99, 0, 9, 64, 9, 9, 9, 128.toByte(), 9, 255.toByte()))
        source.position(1)
        assertArrayEquals(
            byteArrayOf(0, 64, 128.toByte(), 255.toByte(), 128.toByte(), 128.toByte()),
            qrLumaNv21(source, 2, 2, 6, 2, false),
        )
        assertEquals(1, source.position())
    }

    @Test fun invertsOnlyLumaForWhiteOnBlackQrCodes() {
        val source = ByteBuffer.wrap(byteArrayOf(0, 64, 128.toByte(), 255.toByte()))
        assertArrayEquals(
            byteArrayOf(255.toByte(), 191.toByte(), 127, 0, 128.toByte(), 128.toByte()),
            qrLumaNv21(source, 2, 2, 2, 1, true),
        )
    }
}
