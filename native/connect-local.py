#!/usr/bin/env python3
"""Pair the native client with the existing local T3 server."""

from datetime import datetime, timedelta, timezone
from pathlib import Path
import fcntl
import json
import os
import shutil
import subprocess
import sys
import tempfile
import urllib.error
import urllib.parse
import urllib.request
from uuid import UUID

T3_HOME = Path(os.environ.get("T3CODE_HOME", str(Path.home() / ".t3"))).expanduser().resolve()
SETTINGS = Path(os.environ.get("XDG_CONFIG_HOME", str(Path.home() / ".config"))) / "native-t3"
SCOPES = ["orchestration:read", "orchestration:operate", "settings:write", "providers:manage",
          "terminal:read", "terminal:operate", "source-control:write", "filesystem:read",
          "preview:operate", "access:read", "access:write", "relay:read", "relay:write"]


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


def request_json(origin, path, token=None):
    headers = {"Authorization": "Bearer " + token} if token else {}
    request = urllib.request.Request(
        origin + path, headers=headers, method="POST" if token else "GET"
    )
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), NoRedirect())
    with opener.open(request, timeout=10) as response:
        return json.load(response)


def write_private(path, value):
    fd, temporary = tempfile.mkstemp(dir=path.parent, prefix=path.name + ".")
    try:
        with os.fdopen(fd, "w") as output:
            output.write(value)
        os.replace(temporary, path)
    finally:
        Path(temporary).unlink(missing_ok=True)


def token_valid(origin, token):
    try:
        ticket = request_json(origin, "/api/auth/websocket-ticket", token)
        return isinstance(ticket.get("ticket"), str) and bool(ticket["ticket"])
    except urllib.error.HTTPError as error:
        if error.code in (401, 403):
            error.close()
            return False
        raise


def prepare_connection():
    runtime = json.loads((T3_HOME / "userdata/server-runtime.json").read_text())
    pid = runtime.get("pid")
    if not isinstance(pid, int) or pid <= 0:
        raise ValueError("The saved T3 server is not running.")
    os.kill(pid, 0)
    address = urllib.parse.urlsplit(runtime.get("origin", ""))
    if (
        address.scheme != "http"
        or address.hostname not in ("127.0.0.1", "localhost", "::1")
        or address.username is not None
        or address.password is not None
        or address.path not in ("", "/")
        or address.query
        or address.fragment
        or address.port is None
    ):
        raise ValueError("The saved T3 server address must be local.")
    origin = urllib.parse.urlunsplit((address.scheme, address.netloc, "", "", ""))
    descriptor = request_json(origin, "/.well-known/t3/environment")
    identity = (T3_HOME / "userdata/environment-id").read_text().strip()
    UUID(identity)
    if descriptor.get("environmentId") != identity or descriptor.get("orchestrationProtocolVersion") != 2:
        raise ValueError("The running server does not match this T3 installation.")

    SETTINGS.mkdir(parents=True, mode=0o700, exist_ok=True)
    SETTINGS.chmod(0o700)
    token_file = SETTINGS / "local-token"
    session_file = SETTINGS / "local-session.json"
    with (SETTINGS / "connection.lock").open("a") as lock:
        os.chmod(lock.name, 0o600)
        fcntl.flock(lock, fcntl.LOCK_EX)
        token = None
        candidate = None
        try:
            session = json.loads(session_file.read_text())
            expires = datetime.fromisoformat(session["expiresAt"].replace("Z", "+00:00"))
            if (
                session["environmentId"] == identity
                and session["baseDir"] == str(T3_HOME)
                and set(SCOPES).issubset(session.get("scopes", []))
                and expires > datetime.now(timezone.utc) + timedelta(minutes=5)
            ):
                candidate = token_file.read_text().strip()
        except (OSError, ValueError, KeyError, TypeError):
            pass
        if candidate and token_valid(origin, candidate):
            token = candidate
            token_file.chmod(0o600)
        if token is None:
            cli = T3_HOME / "bin/t3"
            if not cli.is_file() or not os.access(cli, os.X_OK):
                raise ValueError("Open T3 Code once to install its local command.")
            # ponytail: renew at launch; reopen after a continuous 30-day session.
            issued = subprocess.run(
                [str(cli), "auth", "session", "issue", "--base-dir", str(T3_HOME),
                 *[arg for scope in SCOPES for arg in ("--scope", scope)], "--ttl", "30d",
                 "--label", "Z3-code", "--subject", "z3-code", "--json"],
                capture_output=True, text=True, timeout=30,
            )
            if issued.returncode != 0:
                raise RuntimeError("T3 could not create the native client's login.")
            session = json.loads(issued.stdout)
            token = session.get("token")
            if not isinstance(token, str) or not token or not token_valid(origin, token):
                raise RuntimeError("T3 rejected the native client's login.")
            write_private(token_file, token + "\n")
            write_private(session_file, json.dumps({
                "environmentId": identity, "baseDir": str(T3_HOME),
                "sessionId": session["sessionId"], "expiresAt": session["expiresAt"], "scopes": SCOPES,
            }) + "\n")
        connection = SETTINGS / "connection.json"
        write_private(connection, json.dumps({
            "origin": origin, "environmentId": identity, "accessTokenFile": str(token_file),
        }) + "\n")
        return connection


if __name__ == "__main__":
    try:
        print(prepare_connection())
    except (OSError, ValueError, KeyError, RuntimeError, subprocess.TimeoutExpired) as error:
        message = "Open T3 Code, then launch Z3-code again. " + str(error)
        print("Z3-code: " + message, file=sys.stderr)
        if (os.environ.get("DISPLAY") or os.environ.get("WAYLAND_DISPLAY")) and shutil.which("zenity"):
            subprocess.run(["zenity", "--error", "--title=Z3-code", "--text=" + message])
        sys.exit(1)
