"""Offline regression tests for the download/verify/install sequence in install.sh.

install.sh fetches a release archive and a SHA256SUMS manifest, checks the checksum, and only then
replaces the installed binary. That ordering is the entire safety story: a failed download or a bad
checksum must leave an existing install byte-for-byte alone. There was no automated coverage for it
under scripts/tests, so a future edit that moved the install ahead of the verify, or dropped the
`|| fail`, would merge unnoticed.

The suite runs the real installer against a synthetic release archive built to the layout install.sh
expects. Only the download and platform boundaries are mocked; the checksum itself stays real. It
never reaches the network, never executes the installed fixture, and writes nothing outside a
disposable temporary directory, so it is safe on a developer machine. This mirrors the shell-stub
approach already used by test_rotate_deletion_tokens.py.
"""

import gzip
import hashlib
import os
import shutil
import subprocess
import tarfile
import tempfile
import unittest
import io
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
INSTALL_SH = ROOT / "install.sh"

VERSION = "v9.9.9-test"
TARGET = "x86_64-unknown-linux-gnu"          # pinned by the uname stub, so the host never matters
ASSET = f"sotto-{VERSION}-{TARGET}.tar.gz"
INNER_DIR = f"sotto-{VERSION}-{TARGET}"      # the directory install.sh extracts and reads from
FIXTURE_BINARY = b"sotto test fixture - not a real executable\n" * 4

# install.sh is POSIX sh. Run it with the first shell we can find; SOTTO_TEST_SHELL lets a developer
# point the suite at a specific binary. If no shell is present the tests skip rather than fail.
SHELL = (
    os.environ.get("SOTTO_TEST_SHELL")
    or shutil.which("sh")
    or shutil.which("bash")
    or ""
)

# uname is mocked so the resolved target is identical on every machine, including ones whose real
# uname (for example MSYS_NT-* ) matches none of the release targets.
UNAME_STUB = """#!/bin/sh
case "$1" in
  -s) printf 'Linux\\n' ;;
  -m) printf 'x86_64\\n' ;;
  *)  printf 'Linux\\n' ;;
esac
"""

# curl answers only from the fixture directory (referenced relative to the workspace, so no absolute
# host paths leak into the shell) or fails on command. It never touches the network. SOTTO_ARCHIVE_FAIL
# and SOTTO_MANIFEST_FAIL switch the two download-failure cases on.
CURL_STUB = """#!/bin/sh
dest=""
url=""
while [ $# -gt 0 ]; do
  case "$1" in
    -o)    dest="$2"; shift 2 ;;
    http*) url="$1";  shift ;;
    *)     shift ;;
  esac
done
name="${url##*/}"
case "$name" in
  SHA256SUMS) [ -n "${SOTTO_MANIFEST_FAIL:-}" ] && exit 22 ;;
  *)          [ -n "${SOTTO_ARCHIVE_FAIL:-}" ]  && exit 22 ;;
esac
src="${SOTTO_FIXTURE_DIR:?}/$name"
[ -f "$src" ] || exit 22          # the .sigstore.json bundle is never present: checksum-only path
cp "$src" "$dest"
"""

# cosign discovery is made deterministic: this stub shadows whatever the developer has installed, and
# fails loudly if the installer ever calls it. Because the bundle fetch above always fails, the
# signature branch is skipped and this stub is never actually invoked.
COSIGN_STUB = """#!/bin/sh
echo "cosign stub must not be invoked by the offline tests" >&2
exit 1
"""


def _archive_bytes():
    """A synthetic release archive holding the layout install.sh expects: INNER_DIR/sotto."""
    buf = io.BytesIO()
    with tarfile.open(fileobj=buf, mode="w", format=tarfile.GNU_FORMAT) as tar:
        info = tarfile.TarInfo(f"{INNER_DIR}/sotto")
        info.size = len(FIXTURE_BINARY)
        info.mode = 0o755
        tar.addfile(info, io.BytesIO(FIXTURE_BINARY))
    return gzip.compress(buf.getvalue(), mtime=0)


class InstallerPreservesExisting(unittest.TestCase):
    """Each case runs the real install.sh in a disposable directory and asserts on the outcome."""

    def setUp(self):
        if not SHELL:
            self.skipTest("no POSIX shell (sh or bash) available; cannot run install.sh")
        if not INSTALL_SH.is_file():
            self.skipTest(f"installer not found at {INSTALL_SH}")

    def run_installer(
        self,
        *,
        wrong_digest=False,
        omit_asset=False,
        fail_archive=False,
        fail_manifest=False,
        seed_existing=True,
    ):
        work = Path(self.enterContext(tempfile.TemporaryDirectory()))
        (work / "bin").mkdir()
        (work / "fixtures").mkdir()
        (work / "home").mkdir()
        install_dir = work / "install dir with spaces"      # acceptance: a destination with spaces

        archive = work / "fixtures" / ASSET
        archive_bytes = _archive_bytes()
        archive.write_bytes(archive_bytes)
        digest = hashlib.sha256(archive_bytes).hexdigest()
        if wrong_digest:
            digest = hashlib.sha256(b"a digest that is not the archive").hexdigest()
        listed = ASSET if not omit_asset else f"sotto-0.0.0.0-{TARGET}.tar.gz"
        # GNU coreutils checksum format: "<hex>  <name>"
        (work / "fixtures" / "SHA256SUMS").write_text(f"{digest}  {listed}\n", newline="\n")

        for name, body in (("uname", UNAME_STUB), ("curl", CURL_STUB), ("cosign", COSIGN_STUB)):
            stub = work / "bin" / name
            stub.write_text(body, newline="\n")
            stub.chmod(0o755)

        # Copy the real installer into the disposable directory and run it from there.
        shutil.copyfile(INSTALL_SH, work / "install.sh")

        pre_existing = None
        if seed_existing:
            install_dir.mkdir(parents=True)
            pre_existing = b"THE PREVIOUS RELEASE - A FAILED INSTALL MUST NOT TOUCH THIS\n"
            (install_dir / "sotto").write_bytes(pre_existing)

        path_parts = [str(work / "bin")]
        if parent_path := os.environ.get("PATH"):
            path_parts.append(parent_path)

        env = dict(os.environ)
        env["PATH"] = os.pathsep.join(path_parts)
        env["HOME"] = str(work / "home")
        env["SOTTO_VERSION"] = VERSION
        env["SOTTO_INSTALL_DIR"] = str(install_dir)
        env["SOTTO_FIXTURE_DIR"] = "./fixtures"
        if fail_archive:
            env["SOTTO_ARCHIVE_FAIL"] = "1"
        if fail_manifest:
            env["SOTTO_MANIFEST_FAIL"] = "1"

        result = subprocess.run(
            [SHELL, "./install.sh"], cwd=str(work), env=env, capture_output=True, text=True
        )
        return result, work, install_dir, pre_existing

    def _installed(self, install_dir):
        target = install_dir / "sotto"
        return target.read_bytes() if target.is_file() else None

    def test_valid_archive_installs_into_a_destination_with_spaces(self):
        result, work, install_dir, _ = self.run_installer()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("checksum verified", result.stderr)
        self.assertEqual(self._installed(install_dir), FIXTURE_BINARY)
        # Nothing was written into the fake HOME: no completions land outside the disposable dir.
        self.assertEqual(list((work / "home").iterdir()), [], "no writes to the user HOME")

    def test_mismatched_checksum_fails_and_leaves_the_existing_install_unchanged(self):
        result, _, install_dir, pre_existing = self.run_installer(wrong_digest=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("checksum verification FAILED", result.stderr)
        self.assertEqual(self._installed(install_dir), pre_existing, "existing bytes preserved")

    def test_manifest_that_omits_the_archive_fails_without_installing(self):
        result, _, install_dir, pre_existing = self.run_installer(omit_asset=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("is not listed in SHA256SUMS", result.stderr)
        self.assertEqual(self._installed(install_dir), pre_existing, "existing bytes preserved")

    def test_failed_archive_download_preserves_the_existing_install(self):
        result, _, install_dir, pre_existing = self.run_installer(fail_archive=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("download failed", result.stderr)
        self.assertEqual(self._installed(install_dir), pre_existing, "existing bytes preserved")

    def test_failed_manifest_download_preserves_the_existing_install(self):
        result, _, install_dir, pre_existing = self.run_installer(fail_manifest=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("download failed", result.stderr)
        self.assertIn("SHA256SUMS", result.stderr)
        self.assertEqual(self._installed(install_dir), pre_existing, "existing bytes preserved")


if __name__ == "__main__":
    unittest.main()
