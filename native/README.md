# Z3-code

Zeron’s native interface with T3 Code’s orchestration v2 backend. Linux x64.

Run `z3-code`. It connects to the T3 server already running on your PC.
For another environment, set `ZERON_T3_CONNECTION` to a private connection JSON (origin, environmentId, accessTokenFile).

Source: `native/zeron`. Build: `cargo build --release --locked -p zeron` there.
T3 and Zeron licenses are preserved. [Features and remaining work](FEATURES.md).
