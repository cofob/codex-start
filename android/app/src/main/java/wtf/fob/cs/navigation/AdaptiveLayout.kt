package wtf.fob.cs.navigation

import android.annotation.SuppressLint
import androidx.activity.compose.LocalActivity
import androidx.compose.foundation.layout.*
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Modifier
import androidx.compose.ui.layout.Layout
import androidx.compose.ui.layout.onGloballyPositioned
import androidx.compose.ui.layout.positionInWindow
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.platform.LocalLayoutDirection
import androidx.compose.ui.unit.*
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import androidx.window.layout.FoldingFeature
import androidx.window.layout.WindowInfoTracker
import androidx.window.layout.WindowLayoutInfo
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.flowOf
import wtf.fob.cs.data.*
import wtf.fob.cs.settings.*
import wtf.fob.cs.ui.*
import wtf.fob.cs.workspace.*

internal data class Pane(
    val x: Int,
    val y: Int,
    val width: Int,
    val height: Int,
)

internal data class PaneLayout(
    val content: Pane,
    val navigation: Pane? = null,
)

internal data class FoldBounds(
    val start: Int,
    val end: Int,
    val vertical: Boolean,
)

/** All bounds use local physical pixels. A fold can have zero width. */
internal fun paneLayout(
    width: Int,
    height: Int,
    density: Float,
    fold: FoldBounds?,
    rtl: Boolean = false,
): PaneLayout {
    val full = Pane(0, 0, width, height)
    if (fold != null) {
        val extent = if (fold.vertical) width else height
        if (fold.start in 1 until extent && fold.end in fold.start until extent) {
            val first = if (fold.vertical) Pane(0, 0, fold.start, height) else Pane(0, 0, width, fold.start)
            val second = if (fold.vertical) Pane(fold.end, 0, width - fold.end, height) else Pane(0, fold.end, width, height - fold.end)
            if (fold.vertical && minOf(first.width, second.width) >= 280 * density && height >= 320 * density) {
                return if (rtl) PaneLayout(first, second) else PaneLayout(second, first)
            }
            // Tabletop: keep the task above the fold and navigation below it.
            if (!fold.vertical && minOf(first.height, second.height) >= 240 * density) return PaneLayout(first, second)
            // Small panes cannot hold useful controls. Use the larger safe region.
            return PaneLayout(if (first.width.toLong() * first.height >= second.width.toLong() * second.height) first else second)
        }
    }
    if (width >= 840 * density && height >= 480 * density) {
        val sidebar = (300 * density).toInt()
        return if (rtl) {
            PaneLayout(Pane(0, 0, width - sidebar, height), Pane(width - sidebar, 0, sidebar, height))
        } else {
            PaneLayout(Pane(sidebar, 0, width - sidebar, height), Pane(0, 0, sidebar, height))
        }
    }
    return PaneLayout(full)
}

/** The content keeps the same composition location during resize and posture changes. */
@SuppressLint("UnusedBoxWithConstraintsScope")
@Composable
internal fun AdaptiveNavigation(
    drawerState: DrawerState,
    navigation: @Composable (visible: Boolean) -> Unit,
    content: @Composable (persistentNavigation: Boolean) -> Unit,
) {
    val activity = LocalActivity.current
    val updates: Flow<WindowLayoutInfo?> =
        remember(activity) {
            activity?.let { WindowInfoTracker.getOrCreate(it).windowLayoutInfo(it) } ?: flowOf(null)
        }
    val info by updates.collectAsStateWithLifecycle(null)
    var origin by remember { mutableStateOf(IntOffset.Zero) }
    val density = LocalDensity.current
    val rtl = LocalLayoutDirection.current == LayoutDirection.Rtl
    BoxWithConstraints(
        Modifier
            .fillMaxSize()
            .windowInsetsPadding(WindowInsets.safeDrawing)
            .imePadding()
            .onGloballyPositioned { origin = it.positionInWindow().let { p -> IntOffset(p.x.toInt(), p.y.toInt()) } },
    ) {
        val feature =
            info?.displayFeatures.orEmpty().filterIsInstance<FoldingFeature>().firstOrNull {
                it.isSeparating || it.occlusionType == FoldingFeature.OcclusionType.FULL
            }
        val fold =
            feature?.let {
                val vertical = it.orientation == FoldingFeature.Orientation.VERTICAL
                FoldBounds(
                    if (vertical) it.bounds.left - origin.x else it.bounds.top - origin.y,
                    if (vertical) it.bounds.right - origin.x else it.bounds.bottom - origin.y,
                    vertical,
                )
            }
        val panes = paneLayout(constraints.maxWidth, constraints.maxHeight, density.density, fold, rtl)
        val persistent = panes.navigation != null
        LaunchedEffect(persistent) { if (persistent) drawerState.close() }
        Layout(content = {
            Box { if (persistent) navigation(true) }
            ModalNavigationDrawer(
                drawerState = drawerState,
                gesturesEnabled = !persistent,
                drawerContent = { if (!persistent) navigation(drawerState.isOpen) },
            ) {
                // Insets were consumed by the outer layout, including the IME.
                content(persistent)
            }
        }) { measurables, _ ->
            val nav = panes.navigation ?: Pane(0, 0, 0, 0)
            val sidebar = measurables[0].measure(Constraints.fixed(nav.width, nav.height))
            val main = measurables[1].measure(Constraints.fixed(panes.content.width, panes.content.height))
            layout(constraints.maxWidth, constraints.maxHeight) {
                if (persistent) sidebar.place(nav.x, nav.y)
                main.place(panes.content.x, panes.content.y)
            }
        }
    }
}
