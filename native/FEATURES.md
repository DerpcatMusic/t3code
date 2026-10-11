# Native features

Z3-code keeps Zeron's GPUI shell, themes, typography, keyboard shortcuts, transcript, model picker, browser tabs, terminal UI and diff viewer. The native frontend uses the same authenticated local RPC boundary as T3's own UI, sharing its environment, providers and conversations.

Launching Z3 reuses a verified local backend or starts the installed `~/.t3/bin/t3 serve` headlessly against `~/.t3`. Startup is serialized, readiness is bounded, and the shared backend stays running when Z3 exits. T3 Code must already be installed. `T3CODE_HOME` from a development terminal does not redirect Z3's default backend; use `ZERON_T3_CONNECTION` explicitly for another environment. Native settings credentials include `providers:manage`; the server remains authoritative for provider operations.

T3 integration includes Active/Pinned chats above bottom Settled/Snoozed shelves, 25-row history paging, settling and visit tracking, a floating project/agent panel with names, provider icons, status, lineage and stopping, chat permissions, queued follow-ups, configured providers/models, approvals and question answers, image/file attachments, sandboxed interactive HTML, project scripts, terminals, Open in Zed and working-tree/branch diffs. Wallpaper remains visible during chats. T3 Connect login, device settings, Pull Requests and Usage open T3's own web controls in an isolated persistent browser session, including before the first chat.

Zeron features to preserve as integration expands: native file editing and search, Git history, session/worktree controls, dictation, appshots, theme customization and multi-device navigation. Some still depend on Zeron's standalone engine and are not connected in T3 mode yet. T3 schedules, MCP Apps, checkpoint comparisons and multi-environment switching also need native controls. Direct remote connections use `ZERON_T3_CONNECTION` with an authenticated HTTPS origin or a local SSH forward.

Providers are configured in native Zeron settings: instances, enablement, connection fields, custom models, status, refresh and supported CLI updates. The floating card subscribes to canonical Git local/remote status and agent metadata, supports safe branch switching, push and pull-request creation, and keeps loading/error states separate from an uninitialized repository.

Queue edit leases are not connected. Send-now interrupts the active turn and resumes T3's queue, including subsequent queued messages. Actual identity-provider login and device pairing require the user's account; embedded-browser restrictions may vary by provider.

The installed T3 backend can update independently of the native frontend. New T3 features still need an adapter mapping and native control when no existing control fits.
