# Native T3 client

This experimental desktop client uses Zeron's Rust/GPUI interface with T3's
standalone server and orchestration v2. Zeron's upstream source and MIT notice
are preserved in `zeron/`. The existing T3 clients continue to use the same
server.

## Run

Build from `native/zeron`:

```sh
cargo build --locked -p zeron
```

The desktop dictation dependency requires ONNX Runtime API 28 or newer. If its
automatic download is unavailable, extract an official Microsoft ONNX Runtime
Linux archive and build with:

```sh
ORT_LIB_LOCATION=/absolute/path/to/onnxruntime-linux-x64-1.30.0/lib \
ORT_PREFER_DYNAMIC_LINK=1 cargo build --locked -p zeron
```

Start a T3 server with an explicit data directory. For an isolated source-build
environment, run the built server entry point from the T3 repository root:

```sh
node apps/server/dist/bin.mjs --base-dir /absolute/path/to/test-t3-home \
  --host 127.0.0.1 --port 39741 --mode web --no-browser
```

Issue a bearer session for that environment and keep its token in a private file:

```sh
umask 077
node apps/server/dist/bin.mjs auth session issue \
  --base-dir /absolute/path/to/test-t3-home \
  --scope orchestration:read --scope orchestration:operate \
  --ttl 1h --label native-client --token-only > /absolute/path/to/native-token
```

Read `/.well-known/t3/environment` from the server origin to find its
`environmentId`. Create a connection file:

```json
{
  "origin": "http://127.0.0.1:39741",
  "environmentId": "the-environment-id-from-the-descriptor",
  "accessTokenFile": "/absolute/path/to/native-token"
}
```

Launch the built native binary with `ZERON_T3_CONNECTION` pointing to that file,
or from `native/zeron`:

```sh
ZERON_T3_CONNECTION=/absolute/path/to/connection.json cargo run --locked -p zeron
```

If you built with an external ONNX Runtime, use the same `ORT_LIB_LOCATION`
and `ORT_PREFER_DYNAMIC_LINK` when running through Cargo. When launching the
binary directly on Linux, add that archive's `lib` directory to
`LD_LIBRARY_PATH` so the dynamic loader can find `libonnxruntime.so`.

The `Native T3 Linux` workflow builds and tests this client on a Linux runner.
Its experimental artifact includes ONNX Runtime and a `native-t3` launcher:

```sh
ZERON_T3_CONNECTION=/absolute/path/to/connection.json /path/to/bundle/native-t3
```

The launcher requires a T3 connection and sets the bundled library path.

This mode keeps native preferences in `~/.zeron-t3` (`Zeron T3` under Local App
Data on Windows). It attaches to T3 without starting Zeron's execution engine,
WorkOS sync, or automatic application updater. Closing the window leaves T3
and its background agents running.

The adapter verifies server identity and protocol before sending credentials.
Direct remote connections use HTTPS; an SSH forward can use loopback HTTP.
Renew an expired bearer session in its existing token file and restart the
client. T3 Connect pairing is not yet supported by the native client.

## Current coverage

The bridge lists projects and threads, projects live transcripts into native
views, reconnects without replaying mutations, and supports creating a thread
in an existing project, rename, archive/unarchive, seen state, plain-text send,
steer, stop, questions, and approvals. The header shows model, branch, live
Git additions/deletions, agent counts, and context usage when reported. Git
counts use T3's branch comparison when available and the working tree otherwise;
an unavailable status clears the counts.

This is not yet a replacement for the complete T3 desktop client. Attachments,
worktree preparation, native Git/file/terminal RPCs, schedules, interactive HTML
and MCP apps, T3 Connect, and Codex live voice still require integration. Their
native controls fail explicitly rather than running a second execution engine.
Zeron's local visual components and offline dictation source remain intact.

## Focused verification

```sh
cargo test --locked -p zeron-t3 --lib
cargo run --locked -p zeron-t3 --example smoke -- /absolute/path/to/connection.json
```

The smoke example only reads the paired environment and decodes native view
types. Append a thread ID to read a particular conversation. Use an isolated
environment when testing mutations or provider runs.

For a disposable thread, the exercise example sends a short instruction twice
with the same message identity, checks that only one response is produced,
and tests visit, rename, and archive/unarchive:

```sh
cargo run --locked -p zeron-t3 --example exercise -- \
  /absolute/path/to/connection.json disposable-thread-id
```

This invokes the thread's configured provider and can consume provider usage.
