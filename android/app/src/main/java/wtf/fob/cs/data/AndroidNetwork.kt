package wtf.fob.cs.data

import android.app.Application
import android.net.ConnectivityManager
import android.net.LinkProperties
import android.net.Network
import android.net.wifi.WifiManager
import wtf.fob.cs.nativeclient.NativeNetworkInterface
import wtf.fob.cs.nativeclient.updateNetworkInterfaces
import java.net.Inet6Address
import java.net.NetworkInterface

/** Supplies link-local IPv6 interfaces without Android VPN or restricted getifaddrs. */
class AndroidNetwork(
    private val app: Application,
    private val changed: () -> Unit,
) {
    private val connectivity = app.getSystemService(ConnectivityManager::class.java)
    private var multicast: WifiManager.MulticastLock? = null

    fun start() {
        refresh()
        connectivity.registerDefaultNetworkCallback(
            object : ConnectivityManager.NetworkCallback() {
                override fun onAvailable(network: Network) {
                    refresh()
                    changed()
                }

                override fun onLinkPropertiesChanged(
                    network: Network,
                    properties: LinkProperties,
                ) {
                    refresh()
                    changed()
                }

                override fun onLost(network: Network) {
                    refresh()
                    changed()
                }
            },
        )
    }

    // The replacement callback API does not provide a snapshot of all current networks.
    @Suppress("DEPRECATION")
    private fun refresh() {
        val interfaces =
            connectivity.allNetworks
                .take(16)
                .mapNotNull { network ->
                    val properties = connectivity.getLinkProperties(network) ?: return@mapNotNull null
                    val name = properties.interfaceName ?: return@mapNotNull null
                    val index = runCatching { NetworkInterface.getByName(name)?.index }.getOrNull() ?: return@mapNotNull null
                    val addresses =
                        properties.linkAddresses
                            .map {
                                it.address
                            }.filterIsInstance<Inet6Address>()
                            .mapNotNull { it.hostAddress?.substringBefore('%') }
                    if (index <= 0 || addresses.isEmpty()) null else NativeNetworkInterface(name, index.toUInt(), addresses)
                }.distinctBy { it.index }
        updateNetworkInterfaces(interfaces)
    }

    @Synchronized fun setEnabled(enabled: Boolean) {
        if (enabled && multicast == null) {
            multicast =
                app.getSystemService(WifiManager::class.java)?.createMulticastLock("cs.fob.wtf-discovery")?.apply {
                    setReferenceCounted(false)
                    acquire()
                }
        } else if (!enabled) {
            multicast?.let { if (it.isHeld) it.release() }
            multicast = null
        }
    }
}
