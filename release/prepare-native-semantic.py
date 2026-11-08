#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Provision pinned local inference assets for the opt-in native CI workflow."""

import hashlib
import json
import os
import platform as host_platform
import re
import shutil
import socket
import subprocess
import sys
import urllib.request
from pathlib import Path, PurePosixPath


ROOT = Path(__file__).resolve().parents[1]
MODEL_MANIFEST = ROOT / "crates/cli/src/model_manifest.json"
INFERENCE_SOURCE = ROOT / "crates/cli/src/inference.rs"
REPOSITORY_URL = "https://github.com/microsoft/onnxruntime"
MODEL_NAME = "intfloat/multilingual-e5-small"
MODEL_REVISION = "ccc66d3bcd826f577e26b9a4072cc5fe3a7ad6a3"


def fail(message):
    raise SystemExit(f"native semantic setup: {message}")


def sha256(path):
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def verify_direct_egress():
    for family, kind, protocol, _, address in socket.getaddrinfo(
        "github.com", 443, socket.AF_INET, socket.SOCK_STREAM
    ):
        try:
            with socket.socket(family, kind, protocol) as connection:
                connection.settimeout(10)
                connection.connect(address)
            return address[0]
        except OSError:
            continue
    fail("direct GitHub TCP egress positive control failed before query isolation")


def rust_constant(name, pattern):
    match = re.search(pattern, INFERENCE_SOURCE.read_text(encoding="utf-8"))
    if not match:
        fail(f"cannot read the pinned {name} from inference.rs")
    return match.group(1)


def provision_model(model_dir, manifest):
    if manifest.get("model") != MODEL_NAME or manifest.get("revision") != MODEL_REVISION:
        fail("model manifest differs from the approved multilingual-e5-small pin")
    for asset in manifest["files"]:
        relative = PurePosixPath(asset["path"])
        if relative.is_absolute() or any(part in ("", ".", "..") for part in relative.parts):
            fail(f"unsafe model asset path: {asset['path']}")
        destination = model_dir.joinpath(*relative.parts)
        if not destination.resolve().is_relative_to(model_dir):
            fail(f"model asset escaped its temporary directory: {asset['path']}")
        destination.parent.mkdir(parents=True, exist_ok=True)
        partial = destination.with_suffix(destination.suffix + ".part")
        url = (
            f"https://huggingface.co/{manifest['model']}/resolve/"
            f"{manifest['revision']}/{asset['path']}"
        )
        request = urllib.request.Request(url, headers={"User-Agent": "riltra-native-ci"})
        with urllib.request.urlopen(request, timeout=120) as response, partial.open("wb") as output:
            shutil.copyfileobj(response, output, 1024 * 1024)
        actual = sha256(partial)
        if actual != asset["sha256"]:
            partial.unlink(missing_ok=True)
            fail(f"model asset SHA-256 mismatch: {asset['path']}")
        partial.replace(destination)


def runtime_files(ort_source):
    if sys.platform == "win32":
        build_dir = ort_source / "build/Windows/Release"
        main_pattern = "onnxruntime.dll"
        companion_pattern = "onnxruntime_providers_shared.dll"
        platform = "windows-x86_64"
    elif sys.platform == "darwin":
        build_dir = ort_source / "build/MacOS/Release"
        main_pattern = "libonnxruntime*.dylib"
        companion_pattern = "libonnxruntime_providers_shared.dylib"
        platform = "macos-x86_64"
    elif sys.platform == "linux":
        build_dir = ort_source / "build/Linux/Release"
        main_pattern = "libonnxruntime.so*"
        companion_pattern = "libonnxruntime_providers_shared.so"
        platform = "linux-x86_64"
    else:
        fail(f"unsupported runner platform: {sys.platform}")

    main_files = sorted(
        path
        for path in build_dir.glob(main_pattern)
        if path.name != companion_pattern and path.is_file() and not path.is_symlink()
    )
    companion_files = sorted(
        path
        for path in build_dir.glob(companion_pattern)
        if path.is_file() and not path.is_symlink()
    )
    if len(main_files) != 1:
        fail(f"expected one native runtime library matching {main_pattern}; found {len(main_files)}")
    if len(companion_files) != 1:
        fail(
            f"expected one companion runtime library matching {companion_pattern}; "
            f"found {len(companion_files)}"
        )
    main = main_files[0]
    companion = companion_files[0]

    cache = build_dir / "CMakeCache.txt"
    cache_text = cache.read_text(encoding="utf-8", errors="replace")
    for option in (
        "onnxruntime_USE_TELEMETRY:BOOL=OFF",
        "onnxruntime_BUILD_SHARED_LIB:BOOL=ON",
    ):
        if option not in cache_text:
            fail(f"ONNX Runtime build cache does not prove {option}")
    for option in ("onnxruntime_USE_CUDA:BOOL=ON", "onnxruntime_USE_DML:BOOL=ON"):
        if option in cache_text:
            fail(f"ONNX Runtime build unexpectedly enabled {option}")
    for provider in (
        "ACL",
        "ARMNN",
        "AZURE",
        "CANN",
        "CUDA",
        "DML",
        "TENSORRT",
        "MIGRAPHX",
        "DNNL",
        "OPENVINO",
        "COREML",
        "JSEP",
        "NNAPI_BUILTIN",
        "QNN",
        "RKNPU",
        "VITISAI",
        "VSINPU",
        "WEBGPU",
        "WEBNN",
        "XNNPACK",
    ):
        if f"onnxruntime_USE_{provider}:BOOL=ON" in cache_text:
            fail(f"ONNX Runtime build unexpectedly enabled the {provider} provider")
    return platform, main, companion


def main():
    if os.environ.get("GITHUB_ACTIONS", "").lower() != "true":
        fail("this provisioning helper is CI-only and will not download assets locally")
    if host_platform.machine().lower() not in ("amd64", "x86_64"):
        fail(f"native runner must be x64; detected {host_platform.machine()}")
    runner_temp = Path(os.environ["RUNNER_TEMP"]).resolve()
    model_dir = runner_temp / "riltra-native-e5-model"
    ort_source = runner_temp / "riltra-onnxruntime"
    if model_dir.is_relative_to(ROOT) or ort_source.is_relative_to(ROOT):
        fail("model and ONNX Runtime source must stay outside the checkout")
    if not ort_source.is_relative_to(runner_temp):
        fail("ONNX Runtime source must stay under RUNNER_TEMP")

    model_manifest = json.loads(MODEL_MANIFEST.read_text(encoding="utf-8"))
    source_commit = rust_constant(
        "source commit", r'RUNTIME_SOURCE_COMMIT:\s*&str\s*=\s*"([0-9a-f]{40})"'
    )
    version = rust_constant(
        "runtime version", r'RUNTIME_VERSION:\s*&str\s*=\s*"([0-9.]+)"'
    )
    api = int(
        rust_constant("runtime API", r"RUNTIME_API:\s*u32\s*=\s*(\d+)")
    )
    if ort_source.exists():
        fail("temporary ONNX Runtime source path already exists")
    subprocess.check_call(
        ["git", "clone", "--filter=blob:none", "--no-checkout", REPOSITORY_URL, str(ort_source)]
    )
    subprocess.check_call(["git", "-C", str(ort_source), "checkout", "--detach", source_commit])
    subprocess.check_call(
        ["git", "-C", str(ort_source), "submodule", "update", "--init", "--recursive"]
    )
    build_args = [
        sys.executable,
        "tools/ci_build/build.py",
        "--build_dir",
        "build",
        "--config",
        "Release",
        "--update",
        "--build",
        "--build_shared_lib",
        "--skip_tests",
        "--skip_pip_install",
        "--parallel",
        "2",
        "--cmake_extra_defines",
        "onnxruntime_USE_TELEMETRY=OFF",
    ]
    if sys.platform == "darwin":
        build_args.extend(["--osx_arch", "x86_64"])
    subprocess.check_call(build_args, cwd=ort_source)
    checked_out = subprocess.check_output(
        ["git", "-C", str(ort_source), "rev-parse", "HEAD"], text=True
    ).strip()
    if checked_out != source_commit:
        fail("ONNX Runtime source commit does not match the application pin")

    model_dir.mkdir(parents=True, exist_ok=True)
    provision_model(model_dir, model_manifest)
    platform, built_main, built_companion = runtime_files(ort_source)
    runtime_dir = model_dir / "runtime"
    runtime_dir.mkdir()
    main = runtime_dir / built_main.name
    companion = runtime_dir / built_companion.name
    shutil.copyfile(built_main, main)
    shutil.copyfile(built_companion, companion)
    runtime_manifest = {
        "version": version,
        "platform": platform,
        "build_options": [
            "onnxruntime_USE_TELEMETRY=OFF",
            "onnxruntime_BUILD_SHARED_LIB=ON",
        ],
        "source_repository": REPOSITORY_URL,
        "source_commit": source_commit,
        "cpu_only": True,
        "api": api,
        "main_library": {"path": main.name, "sha256": sha256(main)},
        "companion_libraries": [
            {"path": companion.name, "sha256": sha256(companion)}
        ],
        "telemetry_disabled": True,
    }
    (runtime_dir / "runtime-manifest.json").write_text(
        json.dumps(runtime_manifest, separators=(",", ":")), encoding="utf-8"
    )
    env_path = Path(os.environ["GITHUB_ENV"])
    probe_ip = verify_direct_egress()
    with env_path.open("a", encoding="utf-8") as env_file:
        env_file.write(f"RILTRA_TEST_MODEL_DIR={model_dir}\n")
        env_file.write(f"RILTRA_NATIVE_EGRESS_PROBE_IP={probe_ip}\n")
        loader_path = "PATH" if sys.platform == "win32" else (
            "DYLD_LIBRARY_PATH" if sys.platform == "darwin" else "LD_LIBRARY_PATH"
        )
        current_loader_path = os.environ.get(loader_path, "")
        env_file.write(
            f"{loader_path}={runtime_dir}{os.pathsep}{current_loader_path}\n"
        )
    print(
        "native model assets matched their pinned hashes; "
        f"ONNX Runtime {version} source and telemetry-off build were verified"
    )


if __name__ == "__main__":
    main()
