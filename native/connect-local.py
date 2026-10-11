#!/usr/bin/env python3
"""Start or reuse the installed local T3 backend and pair the native client."""

from datetime import datetime, timedelta, timezone
from pathlib import Path
import fcntl
import json
import os
import shutil
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request
from uuid import UUID

T3_HOME = (Path.home() / ".t3").resolve()
SETTINGS = Path(os.environ.get("XDG_CONFIG_HOME", str(Path.home() / ".config"))) / "native-t3"
PROC = Path("/proc")
STARTUP_TIMEOUT = 30
SCOPES = ["orchestration:read", "orchestration:operate", "settings:write", "providers:manage",
          "terminal:read", "terminal:operate", "source-control:write", "filesystem:read",
          "preview:operate", "access:read", "access:write", "relay:read", "relay:write"]


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


def request_json(origin, path, token=None, timeout=10):
    headers = {"Authorization": "Bearer " + token} if token else {}
    request = urllib.request.Request(
        origin + path, headers=headers, method="POST" if token else "GET"
    )
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), NoRedirect())
    with opener.open(request, timeout=timeout) as response:
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
        return isinstance(ticket, dict) and isinstance(ticket.get("ticket"), str) and bool(ticket["ticket"])
    except urllib.error.HTTPError as error:
        error.close()
        if error.code in (401, 403):
            return False
        raise


def installed_cli():
    cli = T3_HOME / "bin/t3"
    if not cli.is_file() or not os.access(cli, os.X_OK):
        raise RuntimeError("The installed T3 command ~/.t3/bin/t3 is missing or not executable. Install T3 Code first.")
    return cli


def local_origin(origin):
    address = urllib.parse.urlsplit(origin)
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
    return urllib.parse.urlunsplit((address.scheme, address.netloc, "", "", ""))


def pid_owns_listener(pid, origin):
    """A recycled PID must not make an unrelated process count as our server."""
    if type(pid) is not int or pid <= 0:
        return False
    try:
        os.kill(pid, 0)
        process = PROC / str(pid)
        if process.stat().st_uid != os.getuid():
            raise RuntimeError("The saved T3 process belongs to another user.")
        sockets = set()
        for fd in (process / "fd").iterdir():
            try:
                target = os.readlink(fd)
            except FileNotFoundError:
                continue
            if target.startswith("socket:[") and target.endswith("]"):
                sockets.add(target[8:-1])
        port = urllib.parse.urlsplit(origin).port
        for table in ("tcp", "tcp6"):
            try:
                lines = (process / "net" / table).read_text().splitlines()[1:]
            except FileNotFoundError:
                continue
            for line in lines:
                fields = line.split()
                if (len(fields) > 9 and fields[3] == "0A"
                        and int(fields[1].rsplit(":", 1)[1], 16) == port
                        and fields[9] in sockets):
                    return True
        return False
    except (ProcessLookupError, FileNotFoundError):
        return False


def pid_generation(pid):
    if type(pid) is not int or pid <= 0:
        return None
    try:
        os.kill(pid, 0)
        process = PROC / str(pid)
        if process.stat().st_uid != os.getuid():
            raise RuntimeError("The saved T3 process belongs to another user.")
        # Field 22 is Linux's process start tick, which survives exec but not PID reuse.
        return (process / "stat").read_text().rsplit(")", 1)[1].split()[19]
    except (ProcessLookupError, FileNotFoundError):
        return None


def runtime_endpoint():
    try:
        runtime = json.loads((T3_HOME / "userdata/server-runtime.json").read_text())
    except FileNotFoundError:
        return None
    if not isinstance(runtime, dict):
        raise ValueError("The saved T3 runtime is invalid.")
    origin = local_origin(runtime.get("origin", ""))
    return origin if pid_owns_listener(runtime.get("pid"), origin) else None


def cli_environment():
    # A desktop launched from a dev terminal must still use the installed backend.
    env = {key: value for key, value in os.environ.items()
           if not key.startswith(("T3CODE_", "VITE_"))
           and key not in ("NODE_OPTIONS", "ELECTRON_RUN_AS_NODE")}
    env["T3CODE_HOME"] = str(T3_HOME)
    return env


def ensure_server():
    deadline = time.monotonic() + STARTUP_TIMEOUT
    origin = runtime_endpoint()
    child = None
    startup = SETTINGS / "backend-startup.json"
    pending = False
    if origin is None:
        try:
            saved = json.loads(startup.read_text())
            generation = pid_generation(saved["pid"])
            pending = generation is not None and generation == saved["generation"]
        except FileNotFoundError:
            pass
    if origin is None and not pending:
        cli = installed_cli()
        # Headless startup prints pairing secrets; never capture or persist its output.
        child = subprocess.Popen(
            [str(cli), "serve", "--base-dir", str(T3_HOME), "--host", "127.0.0.1", "--no-browser"],
            cwd=T3_HOME, env=cli_environment(), stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
            start_new_session=True, close_fds=True,
        )
        write_private(startup, json.dumps({"pid": child.pid, "generation": pid_generation(child.pid)}) + "\n")
    while time.monotonic() < deadline:
        origin = runtime_endpoint()
        if origin is not None:
            try:
                descriptor = request_json(origin, "/.well-known/t3/environment", timeout=1)
                identity = (T3_HOME / "userdata/environment-id").read_text().strip()
                UUID(identity)
                if (not isinstance(descriptor, dict) or descriptor.get("environmentId") != identity
                        or descriptor.get("orchestrationProtocolVersion") != 2):
                    raise ValueError("The running server does not match this T3 installation.")
                startup.unlink(missing_ok=True)
                return origin, identity
            except (urllib.error.URLError, TimeoutError, ConnectionError) as error:
                if isinstance(error, urllib.error.HTTPError):
                    error.close()
        if child is not None and child.poll() is not None:
            startup.unlink(missing_ok=True)
            raise RuntimeError("The installed T3 backend exited before it became ready. Run ~/.t3/bin/t3 serve to diagnose startup.")
        time.sleep(min(0.1, max(0, deadline - time.monotonic())))
    # The detached process belongs to the shared installation, even after a timeout.
    raise RuntimeError("The local T3 backend did not become ready within 30 seconds. Check the installed T3 backend and retry.")


def prepare_connection():
    explicit = os.environ.get("ZERON_T3_CONNECTION")
    if explicit:
        return Path(explicit)

    SETTINGS.mkdir(parents=True, mode=0o700, exist_ok=True)
    if SETTINGS.is_symlink() or SETTINGS.stat().st_uid != os.getuid():
        raise RuntimeError("The native connection directory must belong to this user and not be a symlink.")
    SETTINGS.chmod(0o700)
    token_file = SETTINGS / "local-token"
    session_file = SETTINGS / "local-session.json"
    fd = os.open(SETTINGS / "connection.lock", os.O_CREAT | os.O_RDWR | os.O_NOFOLLOW, 0o600)
    with os.fdopen(fd, "w") as lock:
        if os.fstat(lock.fileno()).st_uid != os.getuid():
            raise RuntimeError("The native connection lock belongs to another user.")
        os.fchmod(lock.fileno(), 0o600)
        fcntl.flock(lock, fcntl.LOCK_EX)
        origin, identity = ensure_server()
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
            cli = installed_cli()
            # shortcut: renew at launch; reopen after a continuous 30-day session.
            issued = subprocess.run(
                [str(cli), "auth", "session", "issue", "--base-dir", str(T3_HOME),
                 *[arg for scope in SCOPES for arg in ("--scope", scope)], "--ttl", "30d",
                 "--label", "Z3-code", "--subject", "z3-code", "--json"],
                capture_output=True, text=True, timeout=30, env=cli_environment(),
            )
            if issued.returncode != 0:
                raise RuntimeError("T3 could not create the native client's login.")
            session = json.loads(issued.stdout)
            if not isinstance(session, dict):
                raise RuntimeError("T3 returned an invalid native client login.")
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


def main():
    try:
        print(prepare_connection())
    except (OSError, ValueError, KeyError, TypeError, RuntimeError, subprocess.TimeoutExpired) as error:
        message = (str(error) if isinstance(error, RuntimeError)
                   else "Could not connect to the installed local T3 backend. Check the T3 installation and retry.")
        print("Z3-code: " + message, file=sys.stderr)
        if (os.environ.get("DISPLAY") or os.environ.get("WAYLAND_DISPLAY")) and shutil.which("zenity"):
            subprocess.run(["zenity", "--error", "--title=Z3-code", "--text=" + message])
        sys.exit(1)


if __name__ == "__main__":
    main()
