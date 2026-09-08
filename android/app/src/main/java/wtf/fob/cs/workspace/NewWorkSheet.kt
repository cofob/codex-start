package wtf.fob.cs.workspace

import androidx.compose.foundation.layout.*
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.testTag
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.launch
import org.json.JSONObject
import wtf.fob.cs.data.*
import wtf.fob.cs.ui.*

/** The operation ID survives rotation and failed requests, so retry reuses its directory. */
@Composable
fun NewWorkSheet(
    repo: RemoteRepository,
    server: String,
    workId: String,
    close: () -> Unit,
    open: (RecentChat) -> Unit,
) {
    val (profileQuery, profiles) = rememberRemoteData(repo, server, "", "launcher/list")
    var profile by rememberSaveable(workId) { mutableStateOf("") }
    var busy by remember { mutableStateOf(false) }
    var failure by rememberSaveable(workId) { mutableStateOf("") }
    val scope = rememberCoroutineScope()
    NativeSheet("New work", { if (!busy) close() }, scrollContent = true, canDismiss = { !busy }) {
        Text("No existing project is needed. This creates a separate project in ~/Documents/Codex on this host.")
        Text(
            "Each new Work chat has its own folder and Git repository. Your files stay on the host.",
            style = MaterialTheme.typography.bodySmall,
        )
        val names = profiles.value?.optJSONArray("profiles")
        val options = listOf("") + (0 until (names?.length() ?: 0)).map { names!!.getString(it) }
        Text("Profile", style = MaterialTheme.typography.titleSmall)
        DataStatus(profiles) { repo.data.refresh(profileQuery, true) }
        options.forEach { name ->
            FilterChip(
                selected = profile == name,
                onClick = { profile = name },
                enabled = !busy,
                label = { Text(name.ifEmpty { "Use host defaults" }) },
            )
        }
        if (busy) {
            LinearProgressIndicator(Modifier.fillMaxWidth())
            Text("Starting work on the host…", style = MaterialTheme.typography.bodySmall)
        }
        if (failure.isNotBlank()) Text(failure, color = MaterialTheme.colorScheme.error)
        Button(onClick = {
            if (!busy) {
                busy = true
                failure = ""
                scope.launch {
                    try {
                        val project = repo.request(server, "project/createWork", obj("workId" to workId)) as JSONObject
                        val result =
                            repo.request(
                                server,
                                "project/open",
                                obj(
                                    "projectId" to project.getString("id"),
                                    "profile" to profile.ifEmpty { null },
                                ),
                            ) as JSONObject
                        val session = result.getJSONObject("session")
                        repo.prefetchSession(server, session)
                        // The normal chat composer creates its thread on the first send.
                        open(RecentChat(session, obj("id" to "", "name" to "New work")))
                    } catch (cancelled: CancellationException) {
                        throw cancelled
                    } catch (error: Exception) {
                        failure = error.message ?: "Cannot start work. Try again."
                    } finally {
                        busy = false
                    }
                }
            }
        }, enabled = !busy, modifier = Modifier.fillMaxWidth().testTag("work-start")) {
            Text(if (failure.isEmpty()) "Start work" else "Try again")
        }
    }
}
