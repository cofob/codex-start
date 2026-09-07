package wtf.fob.cs.pairing

import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
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

class InvitationLinkTest {
    @Test fun acceptsCameraHostNormalization() {
        assertTrue(isInvitationLink("https://CS.FOB.WTF/c#ABCD2345"))
        assertTrue(isInvitationLink("https://cs.fob.wtf/c#ABCD2345"))
    }

    @Test fun rejectsLegacyAndOtherOrigins() {
        for (link in listOf(
            "CS:ABCD",
            "https://cs.fob.wtf/c/#ABCD",
            "http://CS.FOB.WTF/c#ABCD",
            "https://evil.example/c#ABCD",
            "https://cs.fob.wtf/C/#ABCD",
            "https://cs.fob.wtf/c#",
            "https://CS.FOB.WTF/c#abcd",
        )) {
            assertFalse(link, isInvitationLink(link))
        }
    }
}
