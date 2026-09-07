package wtf.fob.cs.navigation

import org.junit.Assert.*
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

class AdaptiveLayoutTest {
    @Test fun compactWindowUsesOnePane() {
        assertEquals(PaneLayout(Pane(0, 0, 400, 800)), paneLayout(400, 800, 1f, null))
        assertNull(paneLayout(839, 900, 1f, null).navigation)
        assertNull(paneLayout(1000, 400, 1f, null).navigation)
    }

    @Test fun tabletUsesWindowSizeAndDensity() {
        val tablet = paneLayout(1680, 1600, 2f, null)
        assertEquals(Pane(0, 0, 600, 1600), tablet.navigation)
        assertEquals(Pane(600, 0, 1080, 1600), tablet.content)
        assertNull(paneLayout(1200, 1600, 2f, null).navigation)
    }

    @Test fun bookPostureExcludesHingeEvenBelowTabletBreakpoint() {
        val book = paneLayout(720, 800, 1f, FoldBounds(350, 370, true))
        assertEquals(Pane(0, 0, 350, 800), book.navigation)
        assertEquals(Pane(370, 0, 350, 800), book.content)
        val crease = paneLayout(720, 800, 1f, FoldBounds(360, 360, true))
        assertEquals(360, crease.content.x)
        assertEquals(360, crease.navigation!!.width)
    }

    @Test fun tabletopKeepsContentAboveHinge() {
        val tabletop = paneLayout(800, 1000, 1f, FoldBounds(480, 520, false))
        assertEquals(Pane(0, 0, 800, 480), tabletop.content)
        assertEquals(Pane(0, 520, 800, 480), tabletop.navigation)
    }

    @Test fun smallPaneUsesLargerSafeRegion() {
        assertEquals(PaneLayout(Pane(220, 0, 500, 800)), paneLayout(720, 800, 1f, FoldBounds(200, 220, true)))
        assertEquals(PaneLayout(Pane(0, 0, 800, 500)), paneLayout(800, 720, 1f, FoldBounds(500, 520, false)))
    }

    @Test fun rightToLeftPlacesNavigationOnRightWithoutMovingHinge() {
        val book = paneLayout(720, 800, 1f, FoldBounds(340, 360, true), rtl = true)
        assertEquals(Pane(0, 0, 340, 800), book.content)
        assertEquals(Pane(360, 0, 360, 800), book.navigation)
        assertEquals(700, paneLayout(1000, 800, 1f, null, rtl = true).navigation!!.x)
    }

    @Test fun staleFoldOutsideResizedWindowIsIgnored() {
        for (fold in listOf(FoldBounds(-20, 0, true), FoldBounds(500, 520, true), FoldBounds(0, 0, false))) {
            assertEquals(PaneLayout(Pane(0, 0, 400, 400)), paneLayout(400, 400, 1f, fold))
        }
    }
}
