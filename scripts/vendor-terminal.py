#!/usr/bin/env python3
"""Update the pinned, offline xterm.js assets. Verify each npm archive first."""
import base64
import hashlib
import io
from pathlib import Path
import tarfile
import urllib.request

ROOT = Path(__file__).resolve().parents[1] / "android/app/src/main/assets/terminal"
PACKAGES = [
    ("xterm", "6.0.0", "TQwDdQGtwwDt+2cgKDLn0IRaSxYu1tSUjgKarSDkUM0ZNiSRXFpjxEsvc/Zgc5kq5omJ+V0a8/kIM2WD3sMOYg==",
     {"lib/xterm.js": "xterm.js", "css/xterm.css": "xterm.css", "LICENSE": "LICENSE.xterm"}),
    ("addon-fit", "0.11.0", "jYcgT6xtVYhnhgxh3QgYDnnNMYTcf8ElbxxFzX0IZo+vabQqSPAjC3c1wJrKB5E19VwQei89QCiZZP86DCPF7g==",
     {"lib/addon-fit.js": "addon-fit.js", "LICENSE": "LICENSE.addon-fit"}),
]

if __name__ == "__main__":
    ROOT.mkdir(parents=True, exist_ok=True)
    for name, version, digest, files in PACKAGES:
        url = f"https://registry.npmjs.org/@xterm/{name}/-/{name}-{version}.tgz"
        with urllib.request.urlopen(url, timeout=60) as response:
            data = response.read()
        if base64.b64encode(hashlib.sha512(data).digest()).decode() != digest:
            raise RuntimeError(f"Archive checksum mismatch: {name}")
        with tarfile.open(fileobj=io.BytesIO(data), mode="r:gz") as archive:
            for source, destination in files.items():
                (ROOT / destination).write_bytes(archive.extractfile(f"package/{source}").read())
        print(f"Verified and copied @xterm/{name} {version}")
