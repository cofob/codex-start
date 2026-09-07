@file:androidx.annotation.OptIn(androidx.camera.view.TransformExperimental::class)

package wtf.fob.cs.pairing

import android.Manifest
import android.content.Intent
import android.content.pm.PackageManager
import android.graphics.RectF
import android.os.Bundle
import android.util.Size
import android.view.WindowManager
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.activity.result.contract.ActivityResultContracts
import androidx.camera.core.CameraSelector
import androidx.camera.core.ImageAnalysis
import androidx.camera.core.Preview
import androidx.camera.core.UseCaseGroup
import androidx.camera.core.resolutionselector.ResolutionSelector
import androidx.camera.core.resolutionselector.ResolutionStrategy
import androidx.camera.lifecycle.ProcessCameraProvider
import androidx.camera.view.PreviewView
import androidx.camera.view.transform.CoordinateTransform
import androidx.compose.foundation.layout.*
import androidx.compose.runtime.*
import androidx.compose.ui.Modifier
import androidx.compose.ui.viewinterop.AndroidView
import androidx.core.content.ContextCompat
import androidx.core.view.doOnLayout
import androidx.lifecycle.lifecycleScope
import kotlinx.coroutines.android.awaitFrame
import kotlinx.coroutines.launch
import wtf.fob.cs.app.*
import wtf.fob.cs.data.*
import wtf.fob.cs.ui.*

/** CameraX preview and a bundled ML Kit model; no service or model download is needed. */
class QrCaptureActivity : ComponentActivity() {
    private lateinit var previewView: PreviewView
    private lateinit var analyzer: QrImageAnalyzer
    private var provider: ProcessCameraProvider? = null
    private var analysis: ImageAnalysis? = null
    private var completed = false
    private var detectedBounds by mutableStateOf<RectF?>(null)
    private var detectedContents: String? = null
    private val permission =
        registerForActivityResult(ActivityResultContracts.RequestPermission()) { granted ->
            if (granted) {
                previewView.doOnLayout {
                    startCamera()
                }
            } else {
                fail("Camera permission was denied. Allow camera access in app settings, or paste the invitation.")
            }
        }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        enableEdgeToEdge()
        window.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
        previewView =
            PreviewView(this).apply {
                scaleType = PreviewView.ScaleType.FILL_CENTER
                implementationMode = PreviewView.ImplementationMode.COMPATIBLE
            }
        analyzer =
            QrImageAnalyzer(
                onResult = { detection ->
                    runOnUiThread {
                        if (!completed &&
                            !isFinishing &&
                            !isDestroyed &&
                            (detectedContents == null || detectedContents == detection.contents)
                        ) {
                            lifecycleScope.launch {
                                // Wait for the preview transform before showing the detected bounds.
                                var target = previewView.outputTransform
                                while (target == null) {
                                    awaitFrame()
                                    target = previewView.outputTransform
                                }
                                val bounds = RectF(detection.bounds)
                                CoordinateTransform(detection.transform, target).mapRect(bounds)
                                if (detectedContents == null) detectedContents = detection.contents
                                detectedBounds = bounds
                            }
                        }
                    }
                },
                onError = { runOnUiThread { fail("Could not read camera images. Close the scanner and try again.") } },
            )
        setContent {
            CodexTheme {
                QrScannerScreen(
                    detectedBounds = detectedBounds,
                    onBack = { finish() },
                    onConfirmed = {
                        if (!isFinishing && !isDestroyed) {
                            completed = true
                            setResult(RESULT_OK, Intent().putExtra(EXTRA_CONTENTS, detectedContents))
                            finish()
                        }
                    },
                    preview = { AndroidView(factory = { previewView }, modifier = Modifier.fillMaxSize()) },
                )
            }
        }
        if (ContextCompat.checkSelfPermission(this, Manifest.permission.CAMERA) == PackageManager.PERMISSION_GRANTED) {
            previewView.doOnLayout { startCamera() }
        } else {
            permission.launch(Manifest.permission.CAMERA)
        }
    }

    private fun startCamera() {
        val future = ProcessCameraProvider.getInstance(this)
        future.addListener({
            if (isFinishing || isDestroyed) return@addListener
            try {
                val cameraProvider = future.get()
                provider = cameraProvider
                val selector =
                    when {
                        cameraProvider.hasCamera(CameraSelector.DEFAULT_BACK_CAMERA) -> CameraSelector.DEFAULT_BACK_CAMERA
                        cameraProvider.hasCamera(CameraSelector.DEFAULT_FRONT_CAMERA) -> CameraSelector.DEFAULT_FRONT_CAMERA
                        else -> {
                            fail("No camera is available. Paste the invitation instead.")
                            return@addListener
                        }
                    }
                val preview = Preview.Builder().build().also { it.surfaceProvider = previewView.surfaceProvider }
                val imageAnalysis =
                    ImageAnalysis
                        .Builder()
                        .setBackpressureStrategy(ImageAnalysis.STRATEGY_KEEP_ONLY_LATEST)
                        .setResolutionSelector(
                            ResolutionSelector
                                .Builder()
                                .setResolutionStrategy(
                                    ResolutionStrategy(Size(1280, 720), ResolutionStrategy.FALLBACK_RULE_CLOSEST_HIGHER_THEN_LOWER),
                                ).build(),
                        ).build()
                analysis = imageAnalysis
                imageAnalysis.setAnalyzer(analyzer.executor, analyzer)
                val group =
                    UseCaseGroup
                        .Builder()
                        .setViewPort(requireNotNull(previewView.viewPort))
                        .addUseCase(preview)
                        .addUseCase(imageAnalysis)
                        .build()
                val camera = cameraProvider.bindToLifecycle(this, selector, group)
                camera.cameraInfo.cameraState.observe(this) { state ->
                    if (state.error != null) fail("The camera is unavailable. Close other camera apps and try again.")
                }
            } catch (_: Exception) {
                fail("Could not start the camera. Close other camera apps and try again.")
            }
        }, ContextCompat.getMainExecutor(this))
    }

    private fun fail(message: String) {
        if (completed || isFinishing || isDestroyed) return
        completed = true
        setResult(RESULT_CANCELED, Intent().putExtra(EXTRA_ERROR, message))
        finish()
    }

    override fun onDestroy() {
        analysis?.clearAnalyzer()
        provider?.unbindAll()
        analyzer.close()
        super.onDestroy()
    }

    companion object {
        const val EXTRA_CONTENTS = "wtf.fob.cs.pairing.QR_CONTENTS"
        const val EXTRA_ERROR = "wtf.fob.cs.pairing.QR_ERROR"
    }
}
