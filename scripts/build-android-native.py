#!/usr/bin/env python3
"""Build the pinned Rust core and generate its Kotlin API. No downloaded code generators."""
import os
import pathlib
import platform
import shutil
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parents[1]
OUT = pathlib.Path(sys.argv[1]).resolve()
SDK = pathlib.Path(os.environ.get("ANDROID_SDK_ROOT", os.environ.get("ANDROID_HOME", pathlib.Path.home() / "Library/Android/sdk")))
TOOLCHAIN = "1.88.0"
NDK = SDK / "ndk/28.2.13676358"
HOST = "darwin-x86_64" if platform.system() == "Darwin" else "linux-x86_64"
BIN = NDK / "toolchains/llvm/prebuilt" / HOST / "bin"
TARGET = ROOT / "target"

def run(*args, env=None):
    subprocess.run(args, cwd=ROOT, env=env, check=True)

run("cargo", f"+{TOOLCHAIN}", "build", "--locked", "-p", "codex-start-android", "--features", "bindgen")
library = TARGET / "debug" / ("libcodex_start_android.dylib" if platform.system() == "Darwin" else "libcodex_start_android.so")
(OUT / "kotlin").mkdir(parents=True, exist_ok=True)
run(str(TARGET / "debug/codex-start-bindgen"), "generate", "--library", str(library), "--language", "kotlin", "--config", str(ROOT / "crates/codex-start-android/uniffi.toml"), "--out-dir", str(OUT / "kotlin"))
for abi, target, clang in [("arm64-v8a", "aarch64-linux-android", "aarch64-linux-android28-clang"), ("x86_64", "x86_64-linux-android", "x86_64-linux-android28-clang")]:
    env = dict(os.environ)
    env["CARGO_TARGET_" + target.upper().replace("-", "_") + "_LINKER"] = str(BIN / clang)
    env["CC_" + target.replace("-", "_")] = str(BIN / clang)
    env["AR_" + target.replace("-", "_")] = str(BIN / "llvm-ar")
    env["CFLAGS_" + target.replace("-", "_")] = "-D__ANDROID_API__=28"
    # Set both flags: LOAD alignment alone does not isolate RELRO on 16 KB devices.
    run("cargo", f"+{TOOLCHAIN}", "rustc", "--locked", "-p", "codex-start-android", "--lib", "--release", "--target", target,
        "--", "-C", "link-arg=-Wl,-z,max-page-size=16384", "-C", "link-arg=-Wl,-z,common-page-size=16384", env=env)
    destination = OUT / "jniLibs" / abi
    destination.mkdir(parents=True, exist_ok=True)
    shutil.copy2(TARGET / target / "release/libcodex_start_android.so", destination)
