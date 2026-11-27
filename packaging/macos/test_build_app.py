"""macOS packaging tests; set FARWING_TEST_SIGN_IDENTITY to try real signing."""

import hashlib
import os
from pathlib import Path
import plistlib
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

import build_app


HERE = Path(__file__).resolve().parent
UNKNOWN_IDENTITY = "0" * 40


class BuildAppTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        temp = tempfile.TemporaryDirectory()
        cls.addClassCleanup(temp.cleanup)
        cls.fixtures = Path(temp.name)
        source = cls.fixtures / "main.c"
        source.write_text("int main(void) { return 0; }\n", encoding="utf-8")
        for name, flags in (
            ("arm64 payload", ["-arch", "arm64"]),
            ("intel", ["-arch", "x86_64"]),
            ("universal", ["-arch", "arm64", "-arch", "x86_64"]),
            ("library", ["-arch", "arm64", "-dynamiclib"]),
        ):
            subprocess.run(
                ["/usr/bin/clang", *flags, str(source), "-o", str(cls.fixtures / name)],
                check=True,
            )
        cls.payload = cls.fixtures / "arm64 payload"
        # Patch only the load commands of the temporary C fixture. These
        # variant executables are inspected, never launched or signed.
        for name, flags, original in (
            ("ios", ["-set-build-version", "ios", "15.0", "15.5", "-replace"], cls.payload),
            ("no-version", ["-remove-build-version", "macos"], cls.payload),
            ("legacy", ["-set-version-min", "macos", "11.0", "11.3"], cls.fixtures / "no-version"),
            ("legacy-ios", ["-set-version-min", "ios", "15.0", "15.5"], cls.fixtures / "no-version"),
            ("mixed", ["-set-version-min", "macos", "11.0", "11.3"], cls.fixtures / "ios"),
        ):
            subprocess.run(
                ["/usr/bin/vtool", *flags, "-output", str(cls.fixtures / name), str(original)],
                check=True,
            )

    def setUp(self):
        temp = tempfile.TemporaryDirectory(prefix="output ")
        self.addCleanup(temp.cleanup)
        self.output = Path(temp.name)
        self.app = self.output / "Farwing.app"

    def build(self, executable=None, identity=UNKNOWN_IDENTITY):
        command = [
            sys.executable, str(HERE / "build_app.py"),
            "--executable", str(executable or self.payload),
            "--output-dir", str(self.output),
        ]
        if identity is not None:
            command += ["--sign-identity", identity]
        try:
            return subprocess.run(
                command, cwd=self.fixtures, capture_output=True, text=True, timeout=60,
            )
        except subprocess.TimeoutExpired:
            self.fail("packaging subprocess exceeded its time limit")

    def rejected(self, message, **options):
        result = self.build(**options)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn(message, result.stderr)
        self.assertEqual(list(self.output.iterdir()), [])

    def test_plist(self):
        with (HERE / "Info.plist").open("rb") as source:
            plist = plistlib.load(source)
        self.assertEqual(plist["CFBundleIdentifier"], "io.github.djkcyl.farwing")
        self.assertEqual(plist["CFBundleExecutable"], "Farwing")
        self.assertEqual(plist["CFBundlePackageType"], "APPL")
        self.assertIn("麦克风", plist["NSMicrophoneUsageDescription"])
        self.assertIn("局域网", plist["NSLocalNetworkUsageDescription"])

    def test_requires_explicit_development_identity(self):
        self.rejected("--sign-identity", identity=None)
        self.rejected("Apple Development", identity="-")
        self.rejected("Apple Development", identity="")
        self.rejected("Apple Development", identity=UNKNOWN_IDENTITY)

    def test_rejects_wrong_payloads(self):
        script = self.fixtures / "script"
        script.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
        script.chmod(0o755)
        for executable in (script, *(self.fixtures / n for n in ("intel", "universal", "library"))):
            with self.subTest(name=executable.name):
                self.rejected("arm64 Mach-O executable", executable=executable)
        self.rejected("executable file", executable=self.fixtures / "main.c")

    def test_checks_macos_platform(self):
        for name in ("arm64 payload", "legacy"):
            with self.subTest(name=name):
                self.rejected("not a valid Apple Development", executable=self.fixtures / name)
        for name in ("ios", "no-version", "legacy-ios", "mixed"):
            with self.subTest(name=name):
                self.rejected("built for macOS", executable=self.fixtures / name)

    def test_path_cannot_inject_platform_metadata(self):
        # vtool may print the supplied filename verbatim as a header.
        self.output /= "dir\nLoad command 0\n      cmd LC_BUILD_VERSION\n platform MACOS\ntrailer"
        self.output.mkdir()
        for name in ("ios", "no-version"):
            with self.subTest(name=name):
                self.rejected("built for macOS", executable=self.fixtures / name)

    def test_preserves_existing_app(self):
        self.app.mkdir()
        sentinel = self.app / "keep"
        sentinel.write_bytes(b"existing output")
        result = self.build()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("already exists", result.stderr)
        self.assertEqual(list(self.output.iterdir()), [self.app])
        self.assertEqual(list(self.app.iterdir()), [sentinel])
        self.assertEqual(sentinel.read_bytes(), b"existing output")

    def test_preserves_dangling_output_symlink(self):
        target = self.output / "absent"
        self.app.symlink_to(target)
        result = self.build()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("already exists", result.stderr)
        self.assertEqual(list(self.output.iterdir()), [self.app])
        self.assertEqual(self.app.readlink(), target)

    def test_publish_rejects_late_collision(self):
        staged = self.output / "staged" / "Farwing.app"
        staged.mkdir(parents=True)
        self.app.mkdir()  # An empty directory would be replaced by plain rename.
        with self.assertRaisesRegex(ValueError, "already exists"):
            build_app.publish_app(staged, self.app)
        self.assertTrue(staged.is_dir())
        self.assertEqual(list(self.app.iterdir()), [])

    def test_signing_or_verification_failure_does_not_publish(self):
        real_run = subprocess.run
        for fail_at, message, expected_calls in (
            ("--sign", "Apple Development signing", ["--sign"]),
            ("--verify", "strict signature verification", ["--sign", "--verify"]),
        ):
            calls = []

            def fail_codesign(args, **kwargs):
                if args[0] != "/usr/bin/codesign":
                    return real_run(args, **kwargs)
                action, = (flag for flag in ("--sign", "--verify") if flag in args)
                calls.append(action)
                self.assertTrue(Path(args[-1]).is_dir())
                self.assertFalse(self.app.exists())
                return subprocess.CompletedProcess(args, 17 if action == fail_at else 0, stdout="")

            with self.subTest(fail_at=fail_at):
                with mock.patch.object(build_app, "check_identity"), mock.patch.object(
                    subprocess, "run", side_effect=fail_codesign,
                ):
                    with self.assertRaisesRegex(ValueError, f"{message} failed"):
                        build_app.build_app(self.payload, self.output, UNKNOWN_IDENTITY)
                self.assertEqual(calls, expected_calls)
                self.assertEqual(list(self.output.iterdir()), [])

    @unittest.skipUnless(os.environ.get("FARWING_TEST_SIGN_IDENTITY"), "no signing identity supplied")
    def test_development_signing(self):
        input_bytes = self.payload.read_bytes()
        identity = os.environ["FARWING_TEST_SIGN_IDENTITY"]
        result = self.build(identity=identity)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(list(self.output.iterdir()), [self.app])
        contents = self.app / "Contents"
        self.assertEqual((contents / "Info.plist").read_bytes(), (HERE / "Info.plist").read_bytes())
        self.assertTrue(os.access(contents / "MacOS" / "Farwing", os.X_OK))
        self.assertEqual(self.payload.read_bytes(), input_bytes)
        verification = subprocess.run(
            ["/usr/bin/codesign", "--verify", "--strict", str(self.app)],
            capture_output=True, timeout=15,
        )
        self.assertEqual(verification.returncode, 0, "strict signature verification failed")

        # Independently inspect the published signature, not the plist or the
        # signing script's requirement. Never include certificate data in logs.
        signature = subprocess.run(
            ["/usr/bin/codesign", "--display", "--verbose=2", "--extract-certificates=certificate-", str(self.app)],
            cwd=self.output, capture_output=True, timeout=15,
        )
        self.assertEqual(signature.returncode, 0, "could not inspect the published signature")
        self.assertTrue(
            b"Identifier=io.github.djkcyl.farwing" in signature.stderr.splitlines(),
            "unexpected signed identifier",
        )
        leaf = self.output / "certificate-0"
        self.assertTrue(leaf.is_file(), "no embedded leaf signing certificate")
        self.assertTrue(
            hashlib.sha1(leaf.read_bytes()).hexdigest() == identity.lower(),
            "embedded leaf certificate does not match the requested signing identity",
        )


if __name__ == "__main__":
    unittest.main()
