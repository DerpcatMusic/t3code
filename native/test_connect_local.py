import importlib.util
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from datetime import datetime, timedelta, timezone
from unittest.mock import patch
from urllib.error import HTTPError

spec = importlib.util.spec_from_file_location("connect_local", Path(__file__).with_name("connect-local.py"))
connect = importlib.util.module_from_spec(spec)
spec.loader.exec_module(connect)


class LocalConnectionTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.home = self.root / ".t3"
        self.settings = self.root / "config/native-t3"
        (self.home / "userdata").mkdir(parents=True)
        self.identity = "b458c84e-120b-4821-ac09-4618ab2c08a9"
        (self.home / "userdata/environment-id").write_text(self.identity)
        self.runtime = self.home / "userdata/server-runtime.json"
        self.set_runtime("http://127.0.0.1:3774")
        (self.home / "bin").mkdir()
        (self.home / "bin/t3").touch(mode=0o700)
        self.descriptor = {"environmentId": self.identity, "orchestrationProtocolVersion": 2}
        self.issued = {
            "token": "private-test-token", "sessionId": "test-session",
            "expiresAt": (datetime.now(timezone.utc) + timedelta(days=30)).isoformat(),
        }
        context = patch.multiple(connect, T3_HOME=self.home, SETTINGS=self.settings)
        context.start()
        self.addCleanup(context.stop)

    def set_runtime(self, origin):
        self.runtime.write_text(json.dumps({"pid": os.getpid(), "origin": origin}))

    def prepare(self):
        with patch.object(connect, "request_json", side_effect=[self.descriptor, {"ticket": "ticket"}]), \
             patch.object(connect.subprocess, "run", return_value=subprocess.CompletedProcess([], 0, json.dumps(self.issued), "")) as issue:
            path = connect.prepare_connection()
        return path, issue

    def test_first_launch_uses_installed_cli_and_private_scoped_credentials(self):
        path, issue = self.prepare()
        command = issue.call_args.args[0]
        self.assertEqual(command[:5], [str(self.home / "bin/t3"), "auth", "session", "issue", "--base-dir"])
        self.assertEqual(command.count("--scope"), 2)
        self.assertIn("orchestration:read", command)
        self.assertIn("orchestration:operate", command)
        self.assertNotIn("access:write", command)
        data = json.loads(path.read_text())
        self.assertEqual(data["environmentId"], self.identity)
        self.assertEqual(data["origin"], "http://127.0.0.1:3774")
        token = Path(data["accessTokenFile"])
        self.assertEqual(token.read_text().strip(), self.issued["token"])
        self.assertEqual(self.settings.stat().st_mode & 0o777, 0o700)
        for file in [path, token, self.settings / "local-session.json"]:
            self.assertEqual(file.stat().st_mode & 0o777, 0o600)
        self.assertNotIn(self.issued["token"], (self.settings / "local-session.json").read_text())

    def test_relaunch_reuses_login_and_follows_changed_server_port(self):
        self.prepare()
        self.set_runtime("http://127.0.0.1:39999")
        with patch.object(connect, "request_json", side_effect=[self.descriptor, {"ticket": "ticket"}]) as request, \
             patch.object(connect.subprocess, "run") as issue:
            path = connect.prepare_connection()
        issue.assert_not_called()
        self.assertEqual(json.loads(path.read_text())["origin"], "http://127.0.0.1:39999")
        self.assertEqual(request.call_args.args, ("http://127.0.0.1:39999", "/api/auth/websocket-ticket", self.issued["token"]))

    def test_expired_and_revoked_credentials_are_renewed(self):
        self.prepare()
        for revoked in [False, True]:
            if not revoked:
                path = self.settings / "local-session.json"
                data = json.loads(path.read_text())
                data["expiresAt"] = "2000-01-01T00:00:00+00:00"
                path.write_text(json.dumps(data))
            frames = [self.descriptor]
            if revoked:
                frames.append(HTTPError("http://127.0.0.1:3774", 401, "expired", {}, None))
            frames.append({"ticket": "new-ticket"})
            with patch.object(connect, "request_json", side_effect=frames), \
                 patch.object(connect.subprocess, "run", return_value=subprocess.CompletedProcess([], 0, json.dumps(self.issued), "")) as issue:
                connect.prepare_connection()
            issue.assert_called_once()

    def test_identity_mismatch_never_sends_credentials(self):
        self.prepare()
        with patch.object(connect, "request_json", return_value={**self.descriptor, "environmentId": "other"}) as request, \
             patch.object(connect.subprocess, "run") as issue:
            with self.assertRaisesRegex(ValueError, "does not match"):
                connect.prepare_connection()
        request.assert_called_once_with("http://127.0.0.1:3774", "/.well-known/t3/environment")
        issue.assert_not_called()

    def test_remote_runtime_address_is_rejected_before_any_request(self):
        self.set_runtime("http://example.com:3774")
        with patch.object(connect, "request_json") as request, patch.object(connect.subprocess, "run") as issue:
            with self.assertRaisesRegex(ValueError, "must be local"):
                connect.prepare_connection()
        request.assert_not_called()
        issue.assert_not_called()

    def test_temporary_server_failure_does_not_issue_another_login(self):
        self.prepare()
        with patch.object(connect, "request_json", side_effect=[self.descriptor, HTTPError("http://127.0.0.1:3774", 503, "unavailable", {}, None)]), \
             patch.object(connect.subprocess, "run") as issue:
            with self.assertRaises(HTTPError):
                connect.prepare_connection()
        issue.assert_not_called()


if __name__ == "__main__":
    unittest.main()
