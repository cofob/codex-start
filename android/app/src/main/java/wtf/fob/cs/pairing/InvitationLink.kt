package wtf.fob.cs.pairing

import wtf.fob.cs.app.*
import wtf.fob.cs.data.*
import wtf.fob.cs.ui.*

internal fun isInvitationLink(value: String): Boolean {
    val parts = value.split("/c#", limit = 2)
    return parts.size == 2 &&
        parts[0].equals("https://cs.fob.wtf", ignoreCase = true) &&
        parts[1].isNotEmpty() &&
        parts[1].all { it in 'A'..'Z' || it in '2'..'7' }
}
