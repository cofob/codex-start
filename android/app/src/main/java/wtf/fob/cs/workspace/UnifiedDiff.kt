package wtf.fob.cs.workspace

import wtf.fob.cs.data.*
import wtf.fob.cs.navigation.*
import wtf.fob.cs.terminal.*
import wtf.fob.cs.ui.*

enum class DiffLineKind { Context, Added, Removed, Hunk, Header, Note }

data class DiffLine(
    val text: String,
    val kind: DiffLineKind,
    val oldNumber: Int? = null,
    val newNumber: Int? = null,
)

private val hunkHeader = Regex("^@@ -(\\d+)(?:,(\\d+))? \\+(\\d+)(?:,(\\d+))? @@.*")
private val ansiStyle = Regex("\u001b\\[[0-9;]*m")

/** Parse unified patches without counting file headers as changed lines. */
fun parseUnifiedDiff(patch: String): List<DiffLine> {
    if (patch.isEmpty()) return emptyList()
    var old = 0
    var new = 0
    var oldLeft = 0
    var newLeft = 0
    var unnumberedHunk = false
    return ansiStyle.replace(patch, "").split('\n').let { if (it.last() == "") it.dropLast(1) else it }.map { raw ->
        val line = raw.removeSuffix("\r")
        val match = hunkHeader.matchEntire(line)
        val inHunk = oldLeft > 0 || newLeft > 0 || unnumberedHunk
        when {
            match != null -> {
                old = match.groupValues[1].toIntOrNull() ?: 0
                new = match.groupValues[3].toIntOrNull() ?: 0
                oldLeft = match.groupValues[2].let { if (it.isEmpty()) 1 else it.toIntOrNull() ?: 0 }
                newLeft = match.groupValues[4].let { if (it.isEmpty()) 1 else it.toIntOrNull() ?: 0 }
                unnumberedHunk = false
                DiffLine(line, DiffLineKind.Hunk)
            }
            line.startsWith("diff ") || line.startsWith("*** ") -> {
                oldLeft = 0
                newLeft = 0
                unnumberedHunk = false
                DiffLine(line, DiffLineKind.Header)
            }
            line.startsWith("@@") -> {
                // Codex patches can omit source line numbers.
                oldLeft = 0
                newLeft = 0
                unnumberedHunk = true
                DiffLine(line, DiffLineKind.Hunk)
            }
            line.startsWith("\\ No newline") -> DiffLine(line, DiffLineKind.Note)
            !inHunk && (line.startsWith("--- ") || line.startsWith("+++ ")) -> DiffLine(line, DiffLineKind.Header)
            line.startsWith('+') -> {
                val number = if (newLeft > 0) new++ else null
                newLeft = (newLeft - 1).coerceAtLeast(0)
                DiffLine(line, DiffLineKind.Added, newNumber = number)
            }
            line.startsWith('-') -> {
                val number = if (oldLeft > 0) old++ else null
                oldLeft = (oldLeft - 1).coerceAtLeast(0)
                DiffLine(line, DiffLineKind.Removed, oldNumber = number)
            }
            inHunk && line.startsWith(' ') -> {
                val oldNumber = if (oldLeft > 0) old++ else null
                val newNumber = if (newLeft > 0) new++ else null
                oldLeft = (oldLeft - 1).coerceAtLeast(0)
                newLeft = (newLeft - 1).coerceAtLeast(0)
                DiffLine(line, DiffLineKind.Context, oldNumber, newNumber)
            }
            else -> DiffLine(line, DiffLineKind.Header)
        }
    }
}
