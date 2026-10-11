# Z3-code

Zeron’s native interface with T3 Code’s orchestration v2 backend. Linux x64.

Run `z3-code`. It reuses your local T3 backend or starts `~/.t3/bin/t3` headlessly with your shared T3 conversations. Install T3 Code first; the backend stays running when Z3 closes and can update independently.
For another environment, set `ZERON_T3_CONNECTION` to a private connection JSON (origin, environmentId, accessTokenFile).

Source: `native/zeron`. T3 and Zeron licenses are preserved. [Features and remaining work](FEATURES.md).
