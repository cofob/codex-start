package wtf.fob.cs.workspace

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

class UnifiedDiffTest {
    @Test fun unifiedPatchHasSourceNumbersAndNeutralFileHeaders() {
        val lines = parseUnifiedDiff("diff --git a/a b/a\n--- a/a\n+++ b/a\n@@ -10,3 +20,3 @@\n same\n-old\n+new\n tail\n")
        assertEquals(
            listOf(
                DiffLineKind.Header,
                DiffLineKind.Header,
                DiffLineKind.Header,
                DiffLineKind.Hunk,
                DiffLineKind.Context,
                DiffLineKind.Removed,
                DiffLineKind.Added,
                DiffLineKind.Context,
            ),
            lines.map { it.kind },
        )
        assertEquals(11, lines[5].oldNumber)
        assertNull(lines[5].newNumber)
        assertEquals(21, lines[6].newNumber)
        assertEquals(12, lines[7].oldNumber)
        assertEquals(22, lines[7].newNumber)
    }

    @Test fun addedDeletedFilesAndNoNewlineMarkers() {
        val lines =
            parseUnifiedDiff(
                "--- /dev/null\n+++ b/a\n@@ -0,0 +1 @@\n+hello\n\\ No newline at end of file\n--- a/b\n+++ /dev/null\n@@ -1 +0,0 @@\n-goodbye\n",
            )
        assertEquals(1, lines.count { it.kind == DiffLineKind.Added })
        assertEquals(1, lines.count { it.kind == DiffLineKind.Removed })
        assertEquals(1, lines[3].newNumber)
        assertEquals(DiffLineKind.Note, lines[4].kind)
        assertEquals(DiffLineKind.Header, lines[5].kind)
        assertEquals(1, lines.last().oldNumber)
    }

    @Test fun contentThatLooksLikeFileHeadersIsStillAChange() {
        val lines = parseUnifiedDiff("@@ -1 +1 @@\n--- old content\n+++ new content\n")
        assertEquals(DiffLineKind.Removed, lines[1].kind)
        assertEquals(DiffLineKind.Added, lines[2].kind)
    }

    @Test fun handlesCodexPatchesAnsiStylesCrLfAndEmptyOutput() {
        assertTrue(parseUnifiedDiff("").isEmpty())
        val lines = parseUnifiedDiff("*** Update File: a\r\n@@\r\n\u001b[31m-old\u001b[0m\r\n+new\r\n")
        assertEquals("-old", lines[2].text)
        assertEquals(DiffLineKind.Removed, lines[2].kind)
        assertEquals(DiffLineKind.Added, lines[3].kind)
        assertNull(lines[3].newNumber)
    }

    @Test fun binaryAndRenameMetadataAreNotChangedLines() {
        val lines = parseUnifiedDiff("diff --git a/a b/b\nrename from a\nrename to b\nBinary files a/a and b/b differ\n")
        assertTrue(lines.all { it.kind == DiffLineKind.Header })
    }
}
