# Native features

Z3-code keeps Zeron's GPUI shell, themes, typography, keyboard shortcuts, transcript, model picker, browser tabs, terminal UI and diff viewer. It connects these controls to the running T3 backend; it does not copy your conversations into a second engine.

T3 integration includes Active/Pinned chats above bottom Settled/Snoozed shelves, 25-row history paging, settling and visit tracking, a floating project/agent panel with names, provider icons, status, lineage and stopping, chat permissions, queued follow-ups, configured providers/models, approvals and question answers, image/file attachments, sandboxed interactive HTML, project scripts, terminals, Open in Zed and working-tree/branch diffs. Wallpaper remains visible during chats. T3 Connect login, provider/device settings, Pull Requests and Usage open T3's own web controls in an isolated persistent browser session, including before the first chat.

Zeron features to preserve as integration expands: native file editing and search, Git history, session/worktree controls, dictation, appshots, theme customization and multi-device navigation. Some still depend on Zeron's standalone engine and are not connected in T3 mode yet. T3 schedules, MCP Apps, checkpoint comparisons and multi-environment switching also need native controls. Direct remote connections use `ZERON_T3_CONNECTION` with an authenticated HTTPS origin or a local SSH forward.

Branch checkout and queue edit leases are not connected. Send-now interrupts the active turn and resumes T3's queue, including subsequent queued messages. Actual identity-provider login and device pairing require the user's account; embedded-browser restrictions may vary by provider.

The T3 backend can update independently. Daily CI merges current upstream into a temporary checkout and runs adapter, native UI and browser tests. It detects compatibility problems without rewriting native UI or changing the release branch. New T3 features still need an adapter mapping and native control when no existing control fits.
