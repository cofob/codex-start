package wtf.fob.cs.navigation

import android.net.Uri
import androidx.compose.animation.EnterTransition
import androidx.compose.animation.ExitTransition
import androidx.compose.runtime.Composable
import androidx.navigation.NavBackStackEntry
import androidx.navigation.NavHostController
import androidx.navigation.NavType
import androidx.navigation.compose.NavHost
import androidx.navigation.compose.composable
import androidx.navigation.navArgument
import wtf.fob.cs.data.*
import wtf.fob.cs.settings.*
import wtf.fob.cs.ui.*
import wtf.fob.cs.workspace.*

internal const val MAIN_MENU = "screen/0"
internal const val CHAT_TAB = "chatTab"
private const val SCREEN_PATTERN = "screen/{page}?context={context}&feature={feature}"

internal fun screenRoute(
    page: Int,
    context: String = "",
    feature: String = FeaturePage.Plugins.name,
) = "screen/$page?context=${Uri.encode(context)}&feature=${Uri.encode(feature)}"

/** Keep one project and one chat. Tabs are state within the chat entry. */
internal fun NavHostController.openScreen(
    page: Int,
    projectKey: String = "",
    chatId: String = "",
    feature: String = FeaturePage.Plugins.name,
) {
    fun contains(route: String) = runCatching { getBackStackEntry(route) }.isSuccess

    fun open(
        route: String,
        parent: String,
    ) {
        navigate(route) { popUpTo(parent) { inclusive = false } }
    }
    if (page == 0) {
        popBackStack(MAIN_MENU, inclusive = false)
        return
    }
    if (page in 1..4) {
        val projectRoute = screenRoute(1, projectKey)
        if (page == 1) {
            if (contains(projectRoute)) {
                popBackStack(projectRoute, inclusive = false)
            } else {
                open(projectRoute, MAIN_MENU)
            }
            return
        }
        val chatRoute = screenRoute(2, "$projectKey/$chatId")
        if (contains(chatRoute)) {
            popBackStack(chatRoute, inclusive = false)
        } else {
            if (!contains(projectRoute)) open(projectRoute, MAIN_MENU)
            open(chatRoute, projectRoute)
        }
        currentBackStackEntry?.savedStateHandle?.set(CHAT_TAB, page)
        return
    }
    val route = screenRoute(page, feature = if (page == 7) feature else FeaturePage.Plugins.name)
    if (contains(route)) {
        popBackStack(route, inclusive = false)
    } else {
        // Replace sibling detail pages; preserve their Settings or main-level parent.
        val current = currentBackStackEntry
        navigate(route) {
            if (current?.arguments?.getInt("page") in listOf(6, 7)) {
                popUpTo(
                    screenRoute(
                        current!!.arguments!!.getInt("page"),
                        current.arguments?.getString("context").orEmpty(),
                        current.arguments?.getString("feature") ?: FeaturePage.Plugins.name,
                    ),
                ) { inclusive = true }
            }
        }
    }
}

@Composable
internal fun AppScreenHost(
    navigation: NavHostController,
    content: @Composable (NavBackStackEntry) -> Unit,
) {
    NavHost(
        navController = navigation,
        startDestination = MAIN_MENU,
        enterTransition = { EnterTransition.None },
        exitTransition = { ExitTransition.None },
        popEnterTransition = { EnterTransition.None },
        popExitTransition = { ExitTransition.None },
    ) {
        composable(
            SCREEN_PATTERN,
            arguments =
                listOf(
                    navArgument("page") { type = NavType.IntType },
                    navArgument("context") { defaultValue = "" },
                    navArgument("feature") { defaultValue = FeaturePage.Plugins.name },
                ),
        ) { entry ->
            content(entry)
        }
    }
}
