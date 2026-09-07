package wtf.fob.cs.ui

import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.ColumnScope
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.pulltorefresh.PullToRefreshBox
import androidx.compose.runtime.Composable
import androidx.compose.ui.Modifier
import wtf.fob.cs.data.RemoteDataState
import wtf.fob.cs.data.RemoteQuery
import wtf.fob.cs.data.RemoteRepository

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun RefreshPage(
    repo: RemoteRepository,
    query: RemoteQuery,
    state: RemoteDataState,
    content: @Composable ColumnScope.() -> Unit,
) {
    PullToRefreshBox(
        isRefreshing = state.refreshing && state.value != null,
        onRefresh = { repo.data.refresh(query, true) },
        modifier = Modifier.fillMaxSize(),
    ) {
        Column(Modifier.fillMaxSize()) {
            DataStatus(state) { repo.data.refresh(query, true) }
            content()
        }
    }
}
