# Compact QR invitations

Invitations use `https://cs.fob.wtf/c#` followed by uppercase RFC 4648 Base32
without padding. The URL prefix uses a QR byte segment because `#` is outside
the alphanumeric alphabet. The payload uses an alphanumeric segment. The host
uses error correction level H and selects the smallest QR version that fits.
The decoder accepts case changes in the HTTPS scheme and host, but requires
exact `/c#` and uppercase Base32. It rejects `CS:` and all older formats.

The decoded binary layout is:

| Field | Bytes |
|---|---:|
| Header: version 1 in high nibble; endpoint kind and port flag in low nibble | 1 |
| Custom service port, network byte order; omitted for 47321 | 0 or 2 |
| TLS public-key SHA-256 fingerprint; direct connections only | 0 or 32 |
| Connection password | 16 |
| Direct: UTF-8 hostname length, then hostname | 1 + length |
| Direct IPv4 / IPv6 literal, packed bytes instead of text | 4 / 16 |
| Yggdrasil: public key, then group password | 32 + 16 |

Endpoint kinds are hostname = 0, Yggdrasil = 1, IPv4 = 2 and IPv6 = 3.
Header bit 2 indicates a custom port. Bit 3 is reserved and must be zero.
The three endpoint rows are alternatives. A default-port Yggdrasil invitation is
65 binary bytes, or **125 characters** including the URL prefix. It fits QR
version 9-H (53 × 53 modules, before the quiet zone). A default-port IPv4
invitation is 53 bytes, 106 characters and version 8-H (49 × 49 modules).

The QR omits the daemon UUID and display name. The app obtains both from discovery
over the authenticated connection. Direct connections verify the full TLS
fingerprint. Yggdrasil connections authenticate the host public key through
Yggdrasil itself; the packet API verifies the sender key against the packet source.
They do not pin the separate TLS certificate. TLS remains as common HTTPS/WSS
framing, and its current fingerprint still binds the protocol's device proof to
the current handshake. Saved connections still check the daemon UUID on reconnect.
No secret is sent before the transport identity check.

Passwords contain 16 random bytes each. Keys, TLS fingerprints and device tokens
retain their original sizes. Private keys and access tokens never enter the QR.
The JSON registration and saved-connection protocol still represents passwords
as Base64url strings; the QR stores their raw bytes inside its Base32 payload.

Only this invitation format is supported. Old JSON/Base64url invitations are
rejected. On upgrade, reading an old invitation or group password replaces it with
a new 16-byte password and clears pending pairing requests. Existing keys and
device tokens are retained, but clients with the old Yggdrasil group must scan a
new invitation. Installing this source change does not itself restart a daemon.

Android uses CameraX for its portrait camera preview and ML Kit Barcode Scanning
for QR decoding. The `com.google.mlkit:barcode-scanning` dependency includes the
model in the APK. Scanning does not require Google Play Services, network access,
or a model download. Do not replace it with `play-services-mlkit-barcode-scanning`
or the Google Code Scanner API.

The analyzer reads only QR codes and alternates normal and inverted luminance
so terminal themes can use black-on-white or white-on-black QR codes. It processes
one frame at a time and releases the camera and scanner when the activity closes.
The camera fills the screen, with a back button in the upper corner. A thin square
marks a decoded QR code for one second before the app returns to the connection
dialog. The back button cancels scanning, including during this delay.
Camera permission errors return to the connection dialog, where the user can
paste an invitation instead.

## Android App Links deployment

The app declares verified HTTPS links for `cs.fob.wtf`, path `/c`.
The prefix is lowercase. Incoming fragments enter the pairing screen and still
pass the Rust decoder.

Publish `https://cs.fob.wtf/.well-known/assetlinks.json` with HTTP status 200,
content type `application/json`, and no redirects. Its contents must use the
SHA-256 fingerprint of the certificate that signs the installed release APK:

```json
[{
  "relation": ["delegate_permission/common.handle_all_urls"],
  "target": {
    "namespace": "android_app",
    "package_name": "wtf.fob.cs",
    "sha256_cert_fingerprints": ["REPLACE_WITH_RELEASE_SIGNING_CERTIFICATE_SHA256"]
  }
}]
```

This certificate identifies the Android app; it is not the daemon TLS fingerprint.
Get it with Android SDK `apksigner verify --print-certs <signed-release.apk>`.
Do not publish a debug signing certificate for production links.
Serve a fallback page at `/c` with installation instructions, without analytics
or third-party scripts. The invitation secret is in the URL fragment, which is
not sent in HTTP requests; page scripts can still read it.

The repository change does not publish the domain file. Until the domain is
configured and Android verifies the installed signing certificate, camera links
can open the browser. Validate a signed install with:

```sh
adb shell pm verify-app-links --re-verify wtf.fob.cs
adb shell pm get-app-links wtf.fob.cs
```

Check the link from a camera on a physical device.
See https://developer.android.com/training/app-links/verify-android-applinks.
