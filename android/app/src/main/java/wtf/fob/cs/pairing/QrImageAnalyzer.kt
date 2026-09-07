@file:androidx.annotation.OptIn(androidx.camera.view.TransformExperimental::class)

package wtf.fob.cs.pairing

import android.graphics.RectF
import androidx.camera.core.ImageAnalysis
import androidx.camera.core.ImageProxy
import androidx.camera.view.transform.CoordinateTransform
import androidx.camera.view.transform.ImageProxyTransformFactory
import androidx.camera.view.transform.OutputTransform
import com.google.mlkit.vision.barcode.BarcodeScannerOptions
import com.google.mlkit.vision.barcode.BarcodeScanning
import com.google.mlkit.vision.barcode.common.Barcode
import com.google.mlkit.vision.common.InputImage
import wtf.fob.cs.app.*
import wtf.fob.cs.data.*
import wtf.fob.cs.ui.*
import java.nio.ByteBuffer
import java.util.concurrent.ExecutorService
import java.util.concurrent.Executors
import java.util.concurrent.atomic.AtomicBoolean

internal data class QrDetection(
    val contents: String,
    val bounds: RectF,
    val transform: OutputTransform,
)

internal class QrImageAnalyzer(
    private val onResult: (QrDetection) -> Unit,
    private val onError: () -> Unit,
) : ImageAnalysis.Analyzer,
    AutoCloseable {
    val executor: ExecutorService = Executors.newSingleThreadExecutor()
    private val scanner =
        BarcodeScanning.getClient(
            BarcodeScannerOptions.Builder().setBarcodeFormats(Barcode.FORMAT_QR_CODE).build(),
        )
    private val stopped = AtomicBoolean(false)
    private val closeRequested = AtomicBoolean(false)

    // These fields are used only on the analysis executor.
    private var closing = false
    private var processing = false
    private var inverted = false
    private var released = false

    override fun analyze(image: ImageProxy) {
        if (stopped.get()) {
            image.close()
            return
        }
        try {
            val plane = image.planes[0]
            // Alternate polarity to read both normal and inverted terminal QR codes.
            val bytes = qrLumaNv21(plane.buffer, image.width, image.height, plane.rowStride, plane.pixelStride, inverted)
            inverted = !inverted
            val input =
                InputImage.fromByteArray(
                    bytes,
                    image.width,
                    image.height,
                    image.imageInfo.rotationDegrees,
                    InputImage.IMAGE_FORMAT_NV21,
                )
            // ML Kit reports bounds in the rotated, uncropped input image.
            val transform =
                ImageProxyTransformFactory()
                    .apply {
                        isUsingRotationDegrees = true
                    }.getOutputTransform(image)
            val visibleBounds =
                RectF(image.cropRect).also {
                    CoordinateTransform(ImageProxyTransformFactory().getOutputTransform(image), transform).mapRect(it)
                }
            processing = true
            scanner.process(input).addOnCompleteListener(executor) { task ->
                processing = false
                image.close()
                if (!stopped.get()) {
                    if (task.isSuccessful) {
                        task.result
                            .firstOrNull {
                                !it.rawValue.isNullOrEmpty() &&
                                    it.boundingBox?.let { box ->
                                        visibleBounds.contains(RectF(box))
                                    } == true
                            }?.let {
                                onResult(QrDetection(it.rawValue!!, RectF(it.boundingBox!!), transform))
                            }
                    } else {
                        stopped.set(true)
                        onError()
                    }
                }
                if (closing) release()
            }
        } catch (_: Exception) {
            processing = false
            image.close()
            if (!stopped.getAndSet(true)) onError()
            if (closing) release()
        }
    }

    override fun close() {
        stopped.set(true)
        if (closeRequested.compareAndSet(false, true)) {
            executor.execute {
                closing = true
                if (!processing) release()
            }
        }
    }

    private fun release() {
        if (released) return
        released = true
        scanner.close()
        executor.shutdown()
    }
}

/** Copy the Y plane with its strides and add neutral chroma for a grayscale NV21 image. */
internal fun qrLumaNv21(
    buffer: ByteBuffer,
    width: Int,
    height: Int,
    rowStride: Int,
    pixelStride: Int,
    inverted: Boolean,
): ByteArray {
    require(width > 0 && height > 0 && width % 2 == 0 && height % 2 == 0)
    val size = width * height
    val result = ByteArray(size + size / 2) { 128.toByte() }
    val start = buffer.position()
    for (y in 0 until height) {
        for (x in 0 until width) {
            val value = buffer.get(start + y * rowStride + x * pixelStride).toInt() and 0xff
            result[y * width + x] = (if (inverted) 255 - value else value).toByte()
        }
    }
    return result
}
