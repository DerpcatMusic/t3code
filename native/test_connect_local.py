import importlib.util
import io
import json
import os
from pathlib import Path
import subprocess
import tempfile
import threading
import unittest
from datetime import datetime, timedelta, timezone
from unittest.mock import Mock, patch
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
        for context in [patch.object(connect, "pid_owns_listener", return_value=True),
                        patch.object(connect, "pid_generation", return_value="12345"),
                        patch.dict(os.environ, {"ZERON_T3_CONNECTION": ""})]:
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
        self.assertEqual(command.count("--scope"), len(connect.SCOPES))
        self.assertIn("orchestration:read", command)
        self.assertIn("orchestration:operate", command)
        self.assertIn("providers:manage", command)
        self.assertIn("terminal:operate", command)
        self.assertNotIn("environment:maintain", command)
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

    def test_old_login_is_renewed_when_new_controls_need_scopes(self):
        self.prepare()
        path = self.settings / "local-session.json"
        data = json.loads(path.read_text())
        data["scopes"] = ["orchestration:read", "orchestration:operate"]
        path.write_text(json.dumps(data))
        _, issue = self.prepare()
        issue.assert_called_once()

    def test_identity_mismatch_never_sends_credentials(self):
        self.prepare()
        with patch.object(connect, "request_json", return_value={**self.descriptor, "environmentId": "other"}) as request, \
             patch.object(connect.subprocess, "run") as issue:
            with self.assertRaisesRegex(ValueError, "does not match"):
                connect.prepare_connection()
        request.assert_called_once_with("http://127.0.0.1:3774", "/.well-known/t3/environment", timeout=1)
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
        unavailable = HTTPError("http://127.0.0.1:3774", 503, "unavailable", {}, None)
        with patch.object(connect, "request_json", side_effect=[self.descriptor, unavailable]), \
             patch.object(connect.subprocess, "run") as issue:
            with self.assertRaises(HTTPError):
                connect.prepare_connection()
        issue.assert_not_called()
        self.assertTrue(unavailable.closed)

    def test_absent_and_stale_pid_start_canonical_backend_headless(self):
        for stale in [False, True]:
            with self.subTest(stale=stale):
                self.runtime.unlink(missing_ok=True)
                if stale:
                    self.set_runtime("http://127.0.0.1:3774")
                child = Mock(pid=1234)
                child.poll.return_value = None

                def spawn(*args, **kwargs):
                    self.set_runtime("http://127.0.0.1:39999")
                    return child

                checks = [False, True] if stale else [True]
                with patch.object(connect, "pid_owns_listener", side_effect=checks), \
                     patch.object(connect.subprocess, "Popen", side_effect=spawn) as start, \
                     patch.dict(os.environ, {"T3CODE_HOME": "/wrong", "T3CODE_MODE": "desktop",
                                            "T3CODE_BOOTSTRAP_FD": "8", "T3CODE_TAILSCALE_SERVE": "true",
                                            "VITE_DEV_SERVER_URL": "http://wrong", "NODE_OPTIONS": "wrong"}):
                    path, _ = self.prepare()
                self.assertEqual(json.loads(path.read_text())["origin"], "http://127.0.0.1:39999")
                start.assert_called_once()
                self.assertEqual(start.call_args.args[0], [str(self.home / "bin/t3"), "serve",
                                 "--base-dir", str(self.home), "--host", "127.0.0.1", "--no-browser"])
                options = start.call_args.kwargs
                self.assertEqual(options["cwd"], self.home)
                self.assertTrue(options["start_new_session"])
                self.assertTrue(options["close_fds"])
                for channel in ["stdin", "stdout", "stderr"]:
                    self.assertEqual(options[channel], subprocess.DEVNULL)
                self.assertEqual(options["env"]["T3CODE_HOME"], str(self.home))
                for key in ["VITE_DEV_SERVER_URL", "NODE_OPTIONS", "T3CODE_MODE",
                            "T3CODE_BOOTSTRAP_FD", "T3CODE_TAILSCALE_SERVE"]:
                    self.assertNotIn(key, options["env"])
                child.terminate.assert_not_called()
                child.kill.assert_not_called()

    def test_running_backend_is_waited_for_and_never_replaced(self):
        unavailable = HTTPError("http://127.0.0.1:3774", 503, "starting", {}, None)
        with patch.object(connect, "request_json", side_effect=[unavailable, self.descriptor, {"ticket": "ticket"}]), \
             patch.object(connect.time, "sleep"), \
             patch.object(connect.subprocess, "Popen") as start, \
             patch.object(connect.subprocess, "run", return_value=subprocess.CompletedProcess([], 0, json.dumps(self.issued), "")):
            connect.prepare_connection()
        start.assert_not_called()
        self.assertTrue(unavailable.closed)

    def test_missing_installed_cli_fails_without_start_or_auth(self):
        self.runtime.unlink()
        (self.home / "bin/t3").unlink()
        with patch.object(connect.subprocess, "Popen") as start, \
             patch.object(connect.subprocess, "run") as issue, \
             patch.object(connect, "request_json") as request:
            with self.assertRaisesRegex(RuntimeError, "~/.t3/bin/t3.*missing"):
                connect.prepare_connection()
        start.assert_not_called()
        issue.assert_not_called()
        request.assert_not_called()

    def test_startup_failure_never_issues_credentials_or_kills_any_process(self):
        self.runtime.unlink()
        child = Mock(pid=1234)
        child.poll.return_value = 1
        with patch.object(connect.subprocess, "Popen", return_value=child), \
             patch.object(connect.subprocess, "run") as issue:
            with self.assertRaisesRegex(RuntimeError, "exited before"):
                connect.prepare_connection()
        issue.assert_not_called()
        child.kill.assert_not_called()
        child.terminate.assert_not_called()
        self.assertFalse((self.settings / "backend-startup.json").exists())

    def test_timeout_is_bounded_and_next_launch_does_not_duplicate_pending_start(self):
        self.runtime.unlink()
        child = Mock(pid=1234)
        child.poll.return_value = None
        with patch.object(connect.subprocess, "Popen", return_value=child) as start, \
             patch.object(connect.subprocess, "run") as issue, \
             patch.object(connect.time, "monotonic", side_effect=[0, 0, 31, 32, 33, 33, 64, 65]), \
             patch.object(connect.time, "sleep"):
            for _ in range(2):
                with self.assertRaisesRegex(RuntimeError, "did not become ready"):
                    connect.prepare_connection()
        start.assert_called_once()
        issue.assert_not_called()
        saved = json.loads((self.settings / "backend-startup.json").read_text())
        self.assertEqual(saved, {"pid": 1234, "generation": "12345"})
        child.kill.assert_not_called()
        child.terminate.assert_not_called()

    def test_recycled_startup_pid_allows_retry(self):
        self.settings.mkdir(parents=True)
        (self.settings / "backend-startup.json").write_text(json.dumps({"pid": 1234, "generation": "older"}))
        self.runtime.unlink()
        child = Mock(pid=1235)

        def spawn(*args, **kwargs):
            self.set_runtime("http://127.0.0.1:3774")
            return child

        with patch.object(connect.subprocess, "Popen", side_effect=spawn) as start:
            self.prepare()
        start.assert_called_once()
        self.assertFalse((self.settings / "backend-startup.json").exists())

    def test_two_launches_share_one_startup_and_one_scoped_login(self):
        self.runtime.unlink()
        release = threading.Event()
        second_lock = threading.Event()
        original_flock = connect.fcntl.flock
        locks = 0
        results = []
        errors = []

        def flock(fd, operation):
            nonlocal locks
            locks += 1
            if locks == 2:
                second_lock.set()
            original_flock(fd, operation)

        def spawn(*args, **kwargs):
            if not release.wait(5):
                raise AssertionError("Concurrent startup test timed out")
            self.set_runtime("http://127.0.0.1:3774")
            return Mock(pid=1234)

        def request(origin, path, token=None, **kwargs):
            return {"ticket": "ticket"} if token else self.descriptor

        def launch():
            try:
                results.append(connect.prepare_connection())
            except Exception as error:
                errors.append(error)

        with patch.object(connect.fcntl, "flock", side_effect=flock), \
             patch.object(connect.subprocess, "Popen", side_effect=spawn) as start, \
             patch.object(connect, "request_json", side_effect=request), \
             patch.object(connect.subprocess, "run", return_value=subprocess.CompletedProcess([], 0, json.dumps(self.issued), "")) as issue:
            threads = [threading.Thread(target=launch, daemon=True) for _ in range(2)]
            for thread in threads:
                thread.start()
            try:
                self.assertTrue(second_lock.wait(5))
            finally:
                release.set()
                for thread in threads:
                    thread.join(5)
            self.assertFalse(any(thread.is_alive() for thread in threads))
        self.assertEqual(errors, [])
        self.assertEqual(results, [self.settings / "connection.json"] * 2)
        start.assert_called_once()
        issue.assert_called_once()

    def test_explicit_remote_connection_bypasses_all_local_work(self):
        with patch.dict(os.environ, {"ZERON_T3_CONNECTION": "/remote/connection.json"}), \
             patch.object(connect, "ensure_server") as server, \
             patch.object(connect.subprocess, "run") as issue:
            self.assertEqual(connect.prepare_connection(), Path("/remote/connection.json"))
        server.assert_not_called()
        issue.assert_not_called()
        self.assertFalse(self.settings.exists())

    def test_symlinked_connection_lock_is_rejected(self):
        self.settings.mkdir(parents=True)
        other = self.root / "other"
        other.write_text("untouched")
        (self.settings / "connection.lock").symlink_to(other)
        with patch.object(connect, "ensure_server") as server:
            with self.assertRaises(OSError):
                connect.prepare_connection()
        server.assert_not_called()
        self.assertEqual(other.read_text(), "untouched")

    def test_error_output_does_not_include_captured_secrets(self):
        for error in [OSError("private-token"), ValueError("private-token")]:
            with self.subTest(error=type(error).__name__):
                output = io.StringIO()
                with patch.object(connect, "prepare_connection", side_effect=error), \
                     patch.object(connect.sys, "stderr", output), \
                     patch.object(connect.shutil, "which", return_value=None):
                    with self.assertRaises(SystemExit) as exit:
                        connect.main()
                self.assertEqual(exit.exception.code, 1)
                self.assertNotIn("private-token", output.getvalue())


class ProcessIdentityTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.proc = Path(temporary.name)
        self.process = self.proc / "1234"
        (self.process / "fd").mkdir(parents=True)
        (self.process / "net").mkdir()
        (self.process / "fd/3").symlink_to("socket:[99]")
        (self.process / "net/tcp").write_text("header\n0: 0100007F:0EBE 00000000:0000 0A 0:0 00:0 0 1000 0 99\n")
        (self.process / "stat").write_text("1234 (t3 worker) " + " ".join(["S"] + ["0"] * 18 + ["98765"]))
        for context in [patch.object(connect, "PROC", self.proc), patch.object(connect.os, "kill")]:
            context.start()
            self.addCleanup(context.stop)

    def test_pid_must_own_the_advertised_listening_socket(self):
        self.assertTrue(connect.pid_owns_listener(1234, "http://127.0.0.1:3774"))
        self.assertFalse(connect.pid_owns_listener(1234, "http://127.0.0.1:39999"))
        (self.process / "fd/3").unlink()
        self.assertFalse(connect.pid_owns_listener(1234, "http://127.0.0.1:3774"))

    def test_dead_invalid_and_foreign_pids_are_not_trusted(self):
        for pid in [None, 0, -1, True, "1234", 4321]:
            self.assertFalse(connect.pid_owns_listener(pid, "http://127.0.0.1:3774"))
        with patch.object(connect.os, "kill", side_effect=ProcessLookupError):
            self.assertFalse(connect.pid_owns_listener(1234, "http://127.0.0.1:3774"))
        with patch.object(connect.os, "getuid", return_value=os.getuid() + 1):
            with self.assertRaisesRegex(RuntimeError, "another user"):
                connect.pid_owns_listener(1234, "http://127.0.0.1:3774")

    def test_process_generation_survives_exec_and_changes_with_pid_reuse(self):
        self.assertEqual(connect.pid_generation(1234), "98765")
        (self.process / "stat").write_text((self.process / "stat").read_text().replace("98765", "98766"))
        self.assertEqual(connect.pid_generation(1234), "98766")
        self.assertIsNone(connect.pid_generation(4321))

if __name__ == "__main__":
    unittest.main()
