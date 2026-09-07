#!/usr/bin/env python3
"""Check 16 KB ELF LOAD/RELRO boundaries and uncompressed APK library offsets."""
import argparse
import pathlib
import struct
import zipfile

PAGE = 16384
ABIS = {"arm64-v8a", "x86_64"}


def check_elf(data):
    if data[:6] != b"\x7fELF\x02\x01":
        raise ValueError("expected a little-endian 64-bit ELF library")
    offset = struct.unpack_from("<Q", data, 32)[0]
    size, count = struct.unpack_from("<HH", data, 54)
    headers = [struct.unpack_from("<IIQQQQQQ", data, offset + i * size) for i in range(count)]
    loads = [h for h in headers if h[0] == 1]
    relros = [h for h in headers if h[0] == 0x6474E552]
    if not loads or not relros:
        raise ValueError("LOAD or GNU_RELRO segment is missing")
    for _, flags, file_offset, address, _, _, memory_size, alignment in loads:
        if alignment < PAGE or (address - file_offset) % PAGE:
            raise ValueError("LOAD segment is not 16 KB aligned")
        if not flags & 2:
            continue
        for _, _, _, start, _, _, length, _ in relros:
            end = start + length
            # A writable LOAD must not share the protected final RELRO page.
            if address < end <= address + memory_size:
                writable_start = max(address, end)
                protected_end = (end + PAGE - 1) // PAGE * PAGE
                if writable_start < min(address + memory_size, protected_end):
                    raise ValueError("RELRO and writable data share a 16 KB page")


def check_apk(path):
    libraries = []
    with zipfile.ZipFile(path) as archive, path.open("rb") as apk:
        for entry in archive.infolist():
            parts = entry.filename.split("/")
            if len(parts) != 3 or parts[0] != "lib" or not parts[2].endswith(".so"):
                continue
            if parts[1] not in ABIS:
                raise ValueError(f"unexpected ABI: {parts[1]}")
            check_elf(archive.read(entry))
            if entry.compress_type == zipfile.ZIP_STORED:
                apk.seek(entry.header_offset + 26)
                name_length, extra_length = struct.unpack("<HH", apk.read(4))
                start = entry.header_offset + 30 + name_length + extra_length
                if start % PAGE:
                    raise ValueError(f"APK entry is not 16 KB aligned: {entry.filename}")
            libraries.append(entry.filename)
            print(f"PASS {entry.filename}")
    for abi in ABIS:
        if f"lib/{abi}/libcodex_start_android.so" not in libraries:
            raise ValueError(f"Rust library is missing for {abi}")
    print(f"16 KB checks passed for {len(libraries)} native libraries in {path.name}")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("apk", type=pathlib.Path)
    args = parser.parse_args()
    check_apk(args.apk)
