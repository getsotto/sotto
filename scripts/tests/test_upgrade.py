"""Behaviour of deploy/upgrade.sh, against a stubbed host.

The script exists because two upgrade steps were skipped in practice without anything noticing:
the `git pull`, so the host kept a two-month-old backup.sh, and editing the pin without sudo, so
.env ended up owned by root and the nightly backup could not read it. These tests pin down that
the pull happens before anything is recreated, that a failed pull stops the upgrade, and that the
pin is rewritten without disturbing the rest of the file or who may read it.
"""

import os
import stat
import subprocess
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "deploy/upgrade.sh"

# Each stub appends its arguments to one shared file, so the tests can check the order of steps
# across both tools and not only that each was called.
GIT_STUB = """#!/bin/sh
echo "git $*" >> "$STUB_CALLS"
case " $* " in
  *" pull "*) if [ -n "${STUB_PULL_FAILS:-}" ]; then echo "fatal: Not possible to fast-forward, aborting." >&2; exit 128; fi ;;
  *" log "*) echo "abc1234 a commit" ;;
  *" rev-parse "*) echo "abc1234" ;;
esac
"""

DOCKER_STUB = """#!/bin/sh
echo "docker $*" >> "$STUB_CALLS"
"""

ENV = (
    "SOTTO_DOMAIN=example.test\n"
    "SOTTO_IMAGE_TAG=v0.10.2\n"
    "STRIPE_API_KEY=untouched\n"
)
TEMPLATE = (
    "SOTTO_DOMAIN=app.example.com\n"
    "# SOTTO_IMAGE_TAG=v0.10.0\n"
    "STRIPE_API_KEY=\n"
)


class Upgrade(unittest.TestCase):
    def run_script(self, *args, env_text=ENV, env_mode=0o600, **environ):
        work = Path(self.enterContext(tempfile.TemporaryDirectory()))
        bin_dir = work / "bin"
        bin_dir.mkdir()
        for name, body in (("git", GIT_STUB), ("docker", DOCKER_STUB)):
            (bin_dir / name).write_text(body)
            (bin_dir / name).chmod(0o755)
        deploy = work / "sotto/deploy"
        deploy.mkdir(parents=True)
        script = deploy / "upgrade.sh"
        script.write_bytes(SCRIPT.read_bytes())
        script.chmod(0o755)
        (deploy / ".env.example").write_text(TEMPLATE)
        env_file = deploy / ".env"
        if env_text is not None:
            env_file.write_text(env_text)
            env_file.chmod(env_mode)
        calls = work / "calls"
        calls.touch()
        env = {
            # The stubs come first, so the real git and docker are never reached.
            "PATH": f"{bin_dir}:/usr/bin:/bin",
            "HOME": str(work),
            "STUB_CALLS": str(calls),
            **environ,
        }
        result = subprocess.run(
            ["bash", str(script), *args], env=env, capture_output=True, text=True, timeout=60
        )
        return result, calls.read_text().splitlines(), env_file, deploy

    def test_pull_comes_before_anything_is_recreated(self):
        result, calls, _, _ = self.run_script("v0.10.3")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(calls[0], "git -C .. pull --ff-only")
        docker = [c for c in calls if c.startswith("docker")]
        self.assertEqual(docker, [
            "docker compose -f docker-compose.prod.yml pull",
            "docker compose -f docker-compose.prod.yml up -d",
            "docker compose -f docker-compose.prod.yml restart caddy",
        ])
        self.assertIn("upgrade ok: checkout abc1234, image pin v0.10.3", result.stdout)

    def test_failed_pull_stops_the_upgrade(self):
        # The skipped pull is the failure this script exists to prevent, so a pull that fails must
        # not quietly carry on to new images over an old checkout.
        result, calls, env_file, _ = self.run_script("v0.10.3", STUB_PULL_FAILS="1")
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse([c for c in calls if c.startswith("docker")])
        self.assertIn("SOTTO_IMAGE_TAG=v0.10.2", env_file.read_text())

    def test_pin_is_rewritten_and_nothing_else_changes(self):
        result, _, env_file, deploy = self.run_script("v0.10.3")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(env_file.read_text(), ENV.replace("v0.10.2", "v0.10.3"))
        # The replacement must stay private to its owner, and nothing may be left beside it.
        self.assertEqual(stat.S_IMODE(env_file.stat().st_mode), 0o600)
        self.assertEqual(env_file.stat().st_uid, os.getuid())
        self.assertEqual(list(deploy.glob(".env.upgrading.*")), [])

    def test_missing_pin_is_appended_on_its_own_line(self):
        result, _, env_file, _ = self.run_script("v0.10.3", env_text="SOTTO_DOMAIN=example.test")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(env_file.read_text(), "SOTTO_DOMAIN=example.test\nSOTTO_IMAGE_TAG=v0.10.3\n")

    def test_no_argument_keeps_the_pin_and_still_pulls(self):
        result, calls, env_file, _ = self.run_script()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(env_file.read_text(), ENV)
        self.assertEqual(calls[0], "git -C .. pull --ff-only")
        self.assertIn("image pin v0.10.2", result.stdout)

    def test_bad_tag_is_refused_before_anything_runs(self):
        result, calls, env_file, _ = self.run_script("v0.10.3; rm -rf /")
        self.assertEqual(result.returncode, 1)
        self.assertIn("usage", result.stderr)
        self.assertEqual(calls, [])
        self.assertEqual(env_file.read_text(), ENV)

    @unittest.skipIf(os.geteuid() == 0, "root reads a mode-000 file, so nothing is unreadable")
    def test_env_the_user_cannot_use_is_refused_before_anything_runs(self):
        result, calls, _, _ = self.run_script("v0.10.3", env_mode=0o000)
        self.assertEqual(result.returncode, 1)
        self.assertIn("not owned, readable and writable by", result.stderr)
        self.assertIn("sudo chown", result.stderr)
        self.assertEqual(calls, [])

    def test_missing_env_is_refused(self):
        result, calls, _, _ = self.run_script(env_text=None)
        self.assertEqual(result.returncode, 1)
        self.assertIn("no deploy/.env", result.stderr)
        self.assertEqual(calls, [])

    def test_setting_absent_from_the_template_is_named_by_name_only(self):
        result, _, _, _ = self.run_script(env_text=ENV + "STRIPE_SECRET_KEY=sk_live_secret\n")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("STRIPE_SECRET_KEY", result.stderr)
        self.assertNotIn("sk_live_secret", result.stdout + result.stderr)
        # A name the template lists only as a comment is still a documented setting.
        self.assertNotIn("  SOTTO_IMAGE_TAG", result.stderr)


if __name__ == "__main__":
    unittest.main()
