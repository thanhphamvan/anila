"""Regression tests for public deployment configuration and installer guards."""
import importlib.util
import os
from pathlib import Path
import subprocess
import tempfile
import tomllib
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parent.parent
spec = importlib.util.spec_from_file_location("deploy_config", ROOT / "scripts/ci_wrangler_config.py")
deploy = importlib.util.module_from_spec(spec)
spec.loader.exec_module(deploy)


class DeployConfigTests(unittest.TestCase):
    def generate(self, **changes):
        values = dict(ANILA_ORIGIN="https://terminal.example.net", ANILA_RP_ID="terminal.example.net",
                      ANILA_DEFAULT_HOST="first-host", ANILA_HOST_KEYS='{"first-host":"public"}')
        values.update(changes)
        with tempfile.TemporaryDirectory() as tmp, patch.dict(os.environ, values, clear=True):
            output = Path(tmp) / "out.toml"
            deploy.main(str(ROOT / "worker/wrangler.toml"), str(output))
            return tomllib.loads(output.read_text())

    def test_custom_domain_routes_and_no_identity_provider_configuration(self):
        config = self.generate(ANILA_IDP_ISSUER="https://obsolete.example.net")
        self.assertFalse(config["workers_dev"])
        self.assertFalse(config["preview_urls"])
        self.assertEqual(config["routes"], [{"pattern": "terminal.example.net", "custom_domain": True}])
        self.assertFalse(any(key.startswith("IDP_") for key in config["vars"]))
        self.assertEqual(len(config["durable_objects"]["bindings"]), 2)
        self.assertEqual(config["migrations"][0]["new_sqlite_classes"], ["HostRoom", "AuthStore"])

    def test_workers_dev_keeps_default_routing(self):
        config = self.generate(ANILA_ORIGIN="https://anila.owner.workers.dev", ANILA_RP_ID="anila.owner.workers.dev")
        self.assertNotIn("routes", config)
        self.assertNotIn("workers_dev", config)

    def test_invalid_origin_rp_or_bootstrap_keys_are_rejected(self):
        for changes in [dict(ANILA_ORIGIN="http://terminal.example.net"),
                        dict(ANILA_ORIGIN="https://terminal.example.net/path"),
                        dict(ANILA_RP_ID="other.example.net"),
                        dict(ANILA_HOST_KEYS="{}"), dict(ANILA_HOST_KEYS="not-json"),
                        dict(ANILA_ORIGIN="https://term.example.com", ANILA_RP_ID="term.example.com")]:
            with self.subTest(changes=changes), self.assertRaises(SystemExit):
                self.generate(**changes)

    def test_local_config_uses_same_custom_domain_rules(self):
        with tempfile.TemporaryDirectory() as tmp, patch.dict(os.environ, {}, clear=True):
            source = Path(tmp) / "wrangler.toml"
            source.write_text((ROOT / "worker/wrangler.toml").read_text().replace("term.example.com", "terminal.example.net").replace('HOST_KEYS = "{}"', "HOST_KEYS = '{\"my-mac\":\"public\"}'"))
            output = Path(tmp) / "deploy.toml"
            deploy.main(str(source), str(output), from_toml=True)
            self.assertEqual(tomllib.loads(output.read_text())["routes"][0]["pattern"], "terminal.example.net")

    def test_installers_fail_before_mutating_machine_without_explicit_settings(self):
        cases = [("init-server.sh", ["--code", "not-a-real-code"], "--worker URL is required"),
                 ("init-server.sh", ["--version", "v0.0.3", "--worker", "https://terminal.example.net"], "ANILA_REPO=owner/repo"),
                 ("verify-release.sh", [], "ANILA_REPO=owner/repo")]
        for script, args, expected in cases:
            with self.subTest(script=script, args=args):
                env = os.environ.copy()
                env.pop("ANILA_REPO", None)
                result = subprocess.run(["sh", str(ROOT / "scripts" / script), *args], env=env, capture_output=True, text=True)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(expected, result.stderr)


if __name__ == "__main__":
    unittest.main()
