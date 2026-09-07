package wtf.fob.cs.pairing

import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import com.google.android.gms.tasks.Tasks
import com.google.mlkit.vision.barcode.BarcodeScannerOptions
import com.google.mlkit.vision.barcode.BarcodeScanning
import com.google.mlkit.vision.barcode.common.Barcode
import com.google.mlkit.vision.common.InputImage
import org.junit.Assert.assertEquals
import org.junit.Test
import org.junit.runner.RunWith
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
import java.util.concurrent.TimeUnit

@RunWith(AndroidJUnit4::class)
class QrDecodingTest {
    @Test fun bundledModelReadsNormalAndInvertedInvitationsWithRotation() {
        // Fixed QR matrix generated with error correction H and a four-module border.
        val lines =
            InstrumentationRegistry
                .getInstrumentation()
                .context.assets
                .open("qr-invitation.txt")
                .bufferedReader()
                .use { it.readLines() }
        val expected = lines.first()
        val matrix = lines.drop(1)
        val scale = 8
        val size = matrix.size * scale
        BarcodeScanning
            .getClient(
                BarcodeScannerOptions.Builder().setBarcodeFormats(Barcode.FORMAT_QR_CODE).build(),
            ).use { scanner ->
                for (inverted in listOf(false, true)) {
                    val luma =
                        ByteArray(size * size) { index ->
                            val dark = matrix[index / size / scale][index % size / scale] == '1'
                            (if (dark xor inverted) 0 else 255).toByte()
                        }
                    val bytes = qrLumaNv21(ByteBuffer.wrap(luma), size, size, size, 1, inverted)
                    for (rotation in listOf(0, 90, 180, 270)) {
                        val input = InputImage.fromByteArray(bytes, size, size, rotation, InputImage.IMAGE_FORMAT_NV21)
                        val result = Tasks.await(scanner.process(input), 15, TimeUnit.SECONDS)
                        assertEquals("inverted=$inverted rotation=$rotation", listOf(expected), result.map { it.rawValue })
                    }
                }
            }
    }
}
