#!/usr/bin/env python3
"""Assemble and development-sign Farwing.app from an explicit macOS executable."""

import argparse
import ctypes
import errno
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile


BUNDLE_ID = "io.github.djkcyl.farwing"
APP_NAME = "Farwing.app"
RENAME_EXCL = 0x00000004  # macOS <sys/stdio.h>


def run_tool(label, *args, cwd=None):
    # Keychain operations can block on authorization. Never print their raw output
    # (including certificate subjects), retry signing, or alter Keychain settings.
    try:
        result = subprocess.run(
            args, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL, text=True, errors="replace", timeout=15,
            cwd=cwd, env={**os.environ, "LC_ALL": "C"},
        )
    except subprocess.TimeoutExpired:
        raise ValueError(f"{label} timed out (15 seconds)") from None
    if result.returncode:
        raise ValueError(f"{label} failed (exit {result.returncode})")
    return result.stdout


def check_payload(path):
    kind = run_tool("Mach-O inspection", "/usr/bin/file", "-b", path).strip()
    if kind != "Mach-O 64-bit executable arm64":
        raise ValueError("input must be a thin arm64 Mach-O executable")
    # vtool echoes the input path; keep user-controlled directory names out of stdout.
    builds = run_tool(
        "Mach-O build platform inspection", "/usr/bin/vtool", "-show-build", path.name, cwd=path.parent,
    )
    platforms = set()
    for record in re.split(r"(?m)^Load command [0-9]+[ \t]*$", builds)[1:]:
        fields = dict(re.findall(r"(?m)^[ \t]*(cmd|platform)[ \t]+(\S+)[ \t]*$", record))
        command = fields.get("cmd", "")
        if command == "LC_BUILD_VERSION":
            platforms.add(fields.get("platform"))
        elif command.startswith("LC_VERSION_MIN_"):
            platforms.add("MACOS" if command == "LC_VERSION_MIN_MACOSX" else None)
    if platforms != {"MACOS"}:
        raise ValueError("input must be an arm64 Mach-O executable built for macOS")


def check_identity(identity):
    identities = run_tool(
        "Keychain identity lookup", "/usr/bin/security", "find-identity", "-v", "-p", "codesigning",
    )
    for line in identities.splitlines():
        fields = line.split(maxsplit=2)
        if len(fields) == 3 and fields[1].upper() == identity and fields[2].startswith('"Apple Development:'):
            return
    raise ValueError("specified SHA-1 is not a valid Apple Development signing identity in the Keychain search list")


def publish_app(staged, output):
    # Unlike mv -n or a check followed by os.rename, this cannot replace an
    # existing directory even if it appeared while we were signing.
    rename = ctypes.CDLL(None, use_errno=True).renamex_np
    rename.argtypes = (ctypes.c_char_p, ctypes.c_char_p, ctypes.c_uint)
    rename.restype = ctypes.c_int
    if rename(os.fsencode(staged), os.fsencode(output), RENAME_EXCL) != 0:
        error = ctypes.get_errno()
        if error == errno.EEXIST:
            raise ValueError(f"output already exists: {output}")
        raise OSError(error, os.strerror(error), output)


def build_app(executable, output_dir, identity):
    if sys.platform != "darwin":
        raise ValueError("this script requires macOS")
    if not re.fullmatch(r"[0-9a-fA-F]{40}", identity):
        raise ValueError("--sign-identity requires the 40-digit SHA-1 of an Apple Development identity; ad-hoc signing is not allowed")
    identity = identity.upper()

    output_dir = output_dir.resolve()
    if not output_dir.is_dir():
        raise ValueError("--output-dir must be an existing directory")
    output = output_dir / APP_NAME
    if os.path.lexists(output):
        raise ValueError(f"output already exists: {output}")
    if not executable.is_file() or not os.access(executable, os.X_OK):
        raise ValueError("--executable must be an executable file")

    with tempfile.TemporaryDirectory(prefix=".farwing-", dir=output_dir) as temp:
        app = Path(temp) / APP_NAME
        bin_dir = app / "Contents" / "MacOS"
        bin_dir.mkdir(parents=True)
        payload = bin_dir / "Farwing"
        shutil.copyfile(executable, payload)
        payload.chmod(0o755)
        check_payload(payload)
        shutil.copyfile(Path(__file__).resolve().with_name("Info.plist"), app / "Contents" / "Info.plist")
        check_identity(identity)
        run_tool(
            "Apple Development signing", "/usr/bin/codesign", "--force", "--sign", identity,
            "--timestamp=none", app,
        )
        run_tool(
            "strict signature verification", "/usr/bin/codesign", "--verify", "--strict",
            "-R", f'=identifier "{BUNDLE_ID}" and anchor apple generic and certificate leaf = H"{identity}"',
            app,
        )
        publish_app(app, output)
    return output


def main():
    parser = argparse.ArgumentParser(description=__doc__, allow_abbrev=False)
    parser.add_argument("--executable", type=Path, required=True, help="thin arm64 macOS Mach-O executable")
    parser.add_argument("--output-dir", type=Path, required=True, help="existing directory without Farwing.app")
    parser.add_argument("--sign-identity", required=True, help="SHA-1 of a valid Apple Development signing identity")
    args = parser.parse_args()
    try:
        output = build_app(args.executable, args.output_dir, args.sign_identity)
    except (OSError, ValueError) as error:
        parser.exit(1, f"{parser.prog}: {error}\n")
    print(output)


if __name__ == "__main__":
    main()
