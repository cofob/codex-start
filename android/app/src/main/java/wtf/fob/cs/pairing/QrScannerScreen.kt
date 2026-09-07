package wtf.fob.cs.pairing

import android.graphics.RectF
import androidx.compose.animation.core.Animatable
import androidx.compose.animation.core.AnimationVector4D
import androidx.compose.animation.core.Spring
import androidx.compose.animation.core.TwoWayConverter
import androidx.compose.animation.core.spring
import androidx.compose.foundation.Canvas
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.ArrowBack
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.geometry.CornerRadius
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.geometry.Size
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.drawscope.Stroke
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.unit.dp
import kotlinx.coroutines.delay
import wtf.fob.cs.app.*
import wtf.fob.cs.data.*
import wtf.fob.cs.ui.*
import kotlin.math.max
import kotlin.time.Duration.Companion.milliseconds

private val RectFVectorConverter =
    TwoWayConverter<RectF, AnimationVector4D>(
        convertToVector = { AnimationVector4D(it.left, it.top, it.right, it.bottom) },
        convertFromVector = { RectF(it.v1, it.v2, it.v3, it.v4) },
    )

@Composable
internal fun QrScannerScreen(
    detectedBounds: RectF?,
    onBack: () -> Unit,
    onConfirmed: () -> Unit,
    preview: @Composable () -> Unit,
) {
    val confirm by rememberUpdatedState(onConfirmed)
    val animatedBounds = remember { Animatable(RectF(), RectFVectorConverter) }
    var showBounds by remember { mutableStateOf(false) }
    LaunchedEffect(detectedBounds != null) {
        if (detectedBounds != null) {
            delay(1_000.milliseconds)
            confirm()
        }
    }
    LaunchedEffect(detectedBounds) {
        val target =
            detectedBounds ?: run {
                showBounds = false
                return@LaunchedEffect
            }
        if (!showBounds) {
            animatedBounds.snapTo(RectF(target))
            showBounds = true
        } else {
            animatedBounds.animateTo(
                RectF(target),
                animationSpec =
                    spring(
                        dampingRatio = Spring.DampingRatioNoBouncy,
                        stiffness = Spring.StiffnessHigh,
                    ),
            )
        }
    }
    Box(Modifier.fillMaxSize().background(Color.Black)) {
        preview()
        if (showBounds) {
            Canvas(Modifier.matchParentSize().semantics { contentDescription = "QR code detected" }) {
                val bounds = animatedBounds.value
                val side = max(bounds.width(), bounds.height()) + 16.dp.toPx()
                val origin = Offset(bounds.centerX() - side / 2, bounds.centerY() - side / 2)
                drawRoundRect(
                    color = Color(0xFF86EFAC),
                    topLeft = origin,
                    size = Size(side, side),
                    cornerRadius = CornerRadius(6.dp.toPx()),
                    style = Stroke(2.dp.toPx()),
                )
            }
        }
        IconButton(
            onClick = onBack,
            modifier =
                Modifier
                    .align(Alignment.TopStart)
                    .safeDrawingPadding()
                    .padding(16.dp)
                    .size(48.dp)
                    .background(Color.Black.copy(alpha = 0.45f), CircleShape),
        ) {
            Icon(Icons.AutoMirrored.Filled.ArrowBack, contentDescription = "Back", tint = Color.White)
        }
    }
}
