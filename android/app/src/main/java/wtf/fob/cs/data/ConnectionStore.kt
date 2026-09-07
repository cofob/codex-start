package wtf.fob.cs.data

import android.annotation.SuppressLint
import android.content.Context
import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
import android.util.Base64
import org.json.JSONArray
import org.json.JSONObject
import java.security.KeyStore
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec

/** The device key and access token are encrypted with a non-exportable Android key. */
class ConnectionStore(
    context: Context,
    name: String = "connections",
) {
    private val prefs = context.getSharedPreferences(name, Context.MODE_PRIVATE)
    private val alias = "cs.fob.wtf.$name.v1"

    private fun key(): SecretKey {
        val store = KeyStore.getInstance("AndroidKeyStore").apply { load(null) }
        (store.getKey(alias, null) as? SecretKey)?.let { return it }
        return KeyGenerator
            .getInstance(KeyProperties.KEY_ALGORITHM_AES, "AndroidKeyStore")
            .apply {
                init(
                    KeyGenParameterSpec
                        .Builder(alias, KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT)
                        .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
                        .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
                        .build(),
                )
            }.generateKey()
    }

    @Synchronized fun read(): List<String> {
        val text = prefs.getString("encrypted", null) ?: return emptyList()
        val bytes = Base64.decode(text, Base64.NO_WRAP)
        val plain =
            Cipher.getInstance("AES/GCM/NoPadding").run {
                init(Cipher.DECRYPT_MODE, key(), GCMParameterSpec(128, bytes.copyOfRange(0, 12)))
                doFinal(bytes.copyOfRange(12, bytes.size)).toString(Charsets.UTF_8)
            }
        return JSONArray(plain).let { a -> List(a.length()) { a.getString(it) } }
    }

    @Synchronized fun save(connection: String) {
        val id = JSONObject(connection).getJSONObject("discovery").getString("daemonId")
        write(read().filterNot { JSONObject(it).getJSONObject("discovery").getString("daemonId") == id } + connection)
    }

    @Synchronized fun remove(id: String) = write(read().filterNot { JSONObject(it).getJSONObject("discovery").getString("daemonId") == id })

    @SuppressLint("UseKtx")
    private fun write(connections: List<String>) {
        val cipher = Cipher.getInstance("AES/GCM/NoPadding").apply { init(Cipher.ENCRYPT_MODE, key()) }
        val bytes = cipher.iv + cipher.doFinal(JSONArray(connections).toString().toByteArray())
        // The KTX edit helper discards commit's result. Keep the result check for credential storage.
        check(
            prefs.edit().putString("encrypted", Base64.encodeToString(bytes, Base64.NO_WRAP)).commit(),
        ) { "Cannot save device credentials" }
    }
}
