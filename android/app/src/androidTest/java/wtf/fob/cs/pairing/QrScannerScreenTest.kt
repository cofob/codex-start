package wtf.fob.cs.pairing

import android.graphics.Bitmap
import android.graphics.RectF
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.runtime.mutableStateOf
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.asAndroidBitmap
import androidx.compose.ui.test.*
import androidx.compose.ui.test.junit4.createComposeRule
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import org.junit.Assert.assertEquals
import org.junit.Rule
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
import java.io.File

@RunWith(AndroidJUnit4::class)
class QrScannerScreenTest {
    @get:Rule val compose = createComposeRule()

    @Test fun showsDetectionForOneSecondBeforeReturningOnce() {
        val bounds = mutableStateOf<RectF?>(null)
        var returns = 0
        compose.mainClock.autoAdvance = false
        compose.setContent {
            QrScannerScreen(bounds.value, {}, { returns++ }) {
                Box(Modifier.fillMaxSize().background(Color.DarkGray))
            }
        }
        compose.onNodeWithContentDescription("Back").assertIsDisplayed()
        compose.onNodeWithContentDescription("QR code detected").assertDoesNotExist()
        compose.runOnIdle { bounds.value = RectF(100f, 300f, 300f, 500f) }
        compose.mainClock.advanceTimeByFrame()
        compose.mainClock.advanceTimeByFrame()
        compose.onNodeWithContentDescription("QR code detected").assertIsDisplayed()
        val screenshot = compose.onRoot().captureToImage().asAndroidBitmap()
        val cache = InstrumentationRegistry.getInstrumentation().targetContext.cacheDir
        File(cache, "qr-scanner-detected.png").outputStream().use {
            screenshot.compress(Bitmap.CompressFormat.PNG, 100, it)
        }
        compose.mainClock.advanceTimeBy(900)
        compose.runOnIdle { assertEquals(0, returns) }
        compose.mainClock.advanceTimeBy(200)
        compose.runOnIdle { assertEquals(1, returns) }
        compose.mainClock.advanceTimeBy(2_000)
        compose.runOnIdle { assertEquals(1, returns) }
    }

    @Test fun backWorksDuringDetectionAndDisposalCancelsReturn() {
        val open = mutableStateOf(true)
        var backs = 0
        var returns = 0
        compose.mainClock.autoAdvance = false
        compose.setContent {
            if (open.value) {
                QrScannerScreen(RectF(100f, 300f, 300f, 500f), {
                    backs++
                    open.value = false
                }, { returns++ }) {}
            }
        }
        compose.onNodeWithContentDescription("Back").performClick()
        compose.mainClock.advanceTimeBy(2_000)
        compose.runOnIdle {
            assertEquals(1, backs)
            assertEquals(0, returns)
        }
    }

    @Test fun movingBoundsDoNotRestartTheConfirmationTimer() {
        val bounds = mutableStateOf<RectF?>(null)
        var returns = 0
        compose.mainClock.autoAdvance = false
        compose.setContent {
            QrScannerScreen(bounds.value, {}, { returns++ }) {}
        }

        compose.runOnIdle { bounds.value = RectF(100f, 100f, 300f, 300f) }
        compose.mainClock.advanceTimeBy(200)
        compose.runOnIdle { bounds.value = RectF(120f, 120f, 320f, 320f) }
        compose.mainClock.advanceTimeBy(200)
        compose.runOnIdle { bounds.value = RectF(140f, 140f, 340f, 340f) }
        compose.mainClock.advanceTimeBy(700)

        compose.runOnIdle { assertEquals(1, returns) }
    }
}
