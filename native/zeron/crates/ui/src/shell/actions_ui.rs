use super::*;

use zeron_proto::{
    ProjectAction, ProjectActionDraft, ProjectActionIcon, ProjectActionRun, ProjectActionsSnapshot,
};

use crate::project_actions::{
    ACTION_ICONS, ProjectActionEditor, ProjectActionsKey, ProjectActionsStatus, action_icon,
    draft_from_action, preferred_action, show_action_label,
};

const PANEL_PAGE_SIZE: usize = 5;

pub(super) struct T3PanelState {
    chat_id: String,
    workspace_path: String,
    pub(super) host_menu: bool,
    editor_menu: bool,
    scripts_open: bool,
    lineage_open: bool,
    previous_open: bool,
    scripts_page: usize,
    lineage_page: usize,
    previous_page: usize,
    stopping_child: Option<String>,
    git_busy: bool,
    branches_open: bool,
    branches_loading: bool,
    branches: Vec<String>,
    branches_error: Option<String>,
    branches_cursor: Option<u64>,
    branches_page: usize,
}

impl Default for T3PanelState {
    fn default() -> Self {
        Self {
            chat_id: String::new(),
            workspace_path: String::new(),
            host_menu: false,
            editor_menu: false,
            scripts_open: false,
            lineage_open: true,
            previous_open: false,
            scripts_page: 0,
            lineage_page: 0,
            previous_page: 0,
            stopping_child: None,
            git_busy: false,
            branches_open: false,
            branches_loading: false,
            branches: Vec::new(),
            branches_error: None,
            branches_cursor: None,
            branches_page: 0,
        }
    }
}

fn panel_page(count: usize, page: usize) -> std::ops::Range<usize> {
    let start = page.min(count.saturating_sub(1) / PANEL_PAGE_SIZE) * PANEL_PAGE_SIZE;
    start..(start + PANEL_PAGE_SIZE).min(count)
}

fn agent_provider_icon(driver: &str) -> &'static str {
    match zeron_t3::harness(driver) {
        Some("codex") => icons::OPENAI_MARK,
        Some("claude-code") => icons::CLAUDE_MARK,
        Some("cursor") => icons::CURSOR_MARK,
        Some("grok") => icons::GROK_MARK,
        Some("pi") => icons::PI_MARK,
        Some("opencode") => icons::OPENCODE_MARK,
        Some("devin") => icons::DEVIN_MARK,
        Some("hermes") => icons::HERMES_MARK,
        Some("antigravity") => icons::ANTIGRAVITY_MARK,
        _ => icons::BOT,
    }
}

fn panel_control(theme: &Theme, id: impl Into<SharedString>) -> gpui::Stateful<gpui::Div> {
    let id: SharedString = id.into();
    div()
        .id(id)
        .min_w(px(0.0))
        .h(px(30.0))
        .px(px(8.0))
        .flex()
        .items_center()
        .gap(px(8.0))
        .rounded(px(7.0))
        .text_size(crate::typography::ui_rems(12.0))
        .text_color(theme.text)
        .cursor_pointer()
        .hover(|s| s.bg(crate::theme::ink(0.05)))
}

fn panel_duration(milliseconds: u64) -> String {
    let seconds = milliseconds / 1000;
    if seconds >= 3600 {
        format!("{}h {}m", seconds / 3600, seconds % 3600 / 60)
    } else if seconds >= 60 {
        format!("{}m {}s", seconds / 60, seconds % 60)
    } else {
        format!("{seconds}s")
    }
}

#[cfg(test)]
mod t3_panel_logic_tests {
    use super::*;

    #[test]
    fn pages_are_bounded_and_clamp_after_remote_removal() {
        assert_eq!(panel_page(0, 99), 0..0);
        assert_eq!(panel_page(50, 0), 0..5);
        assert_eq!(panel_page(50, 9), 45..50);
        assert_eq!(panel_page(6, 9), 5..6);
        assert_eq!(panel_page(4, 1), 0..4);
    }

    #[test]
    fn provider_marks_follow_driver_not_account_identifier() {
        assert_eq!(agent_provider_icon("pi"), icons::PI_MARK);
        assert_eq!(agent_provider_icon("codex"), icons::OPENAI_MARK);
        assert_eq!(agent_provider_icon("claude-code"), icons::CLAUDE_MARK);
        assert_eq!(agent_provider_icon("cursor"), icons::CURSOR_MARK);
        assert_eq!(agent_provider_icon("opencode"), icons::OPENCODE_MARK);
        assert_eq!(agent_provider_icon("team-account"), icons::BOT);
    }

    #[test]
    fn duration_uses_compact_units_without_inventing_a_status() {
        assert_eq!(panel_duration(0), "0s");
        assert_eq!(panel_duration(61_000), "1m 1s");
        assert_eq!(panel_duration(3_661_000), "1h 1m");
    }
}

#[derive(Clone)]
struct ProjectActionContext {
    key: ProjectActionsKey,
    chat_id: String,
    target_device_id: Option<String>,
}

/// Reserve the titlebar, trigger gap and window margin even on short windows.
fn project_actions_menu_surface(
    theme: &Theme,
    viewport_height: Pixels,
    scroll: &gpui::ScrollHandle,
) -> gpui::Stateful<gpui::Div> {
    popover::popover_card(theme)
        .id("project-actions-scroll")
        .w(px(280.0))
        .max_h((viewport_height - px(Theme::TITLEBAR_HEIGHT + 6.0 + 16.0)).max(px(0.0)))
        .overflow_y_scroll()
        .track_scroll(scroll)
        .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
}

impl Shell {
    pub(super) fn attach_worktree_setup(
        &mut self,
        chat_id: String,
        setup_action: Option<ProjectActionRun>,
        setup_error: Option<String>,
        target_device_id: Option<String>,
        cx: &mut Context<Self>,
    ) {
        if let Some(error) = setup_error {
            self.sidebar_notice = Some(format!("Setup action failed: {error}").into());
        }
        let Some(run) = setup_action else {
            cx.notify();
            return;
        };

        let panel = self.terminal_panel(cx);
        let title = format!("{} (setup)", run.action_name);
        let tab = panel.update(cx, |panel, cx| {
            panel.reserve_tab_for_chat(chat_id.clone(), title, cx)
        });
        let attached = panel.update(cx, |panel, cx| {
            panel.attach_reserved_session(&chat_id, tab, run.terminal, target_device_id, cx)
        });
        if !attached {
            self.sidebar_notice =
                Some("Setup action started, but its terminal could not be attached".into());
        }

        let selected = self.active_chat == chat_id;
        self.panels
            .update(&chat_id, |panels| panels.terminal_open = true);
        if selected {
            self.terminal_tween = None;
            self.terminal_tween_task = None;
            panel.update(cx, |panel, cx| panel.set_open(true, cx));
            panel.update(cx, |panel, cx| panel.select_tab_by_key(tab, cx));
        }
        cx.notify();
    }

    fn project_action_context(&self, cx: &App) -> Option<ProjectActionContext> {
        let state = self.state.read(cx);
        let chat = state.selected_chat_row()?;
        let space_id = chat.space_id.clone()?;
        let space = state.space_row(&space_id)?;
        if space.device_id != chat.device_id {
            return None;
        }
        let target_device_id = (state.local_device_id.as_deref() != Some(chat.device_id.as_str()))
            .then(|| chat.device_id.clone());
        Some(ProjectActionContext {
            key: ProjectActionsKey {
                device_id: chat.device_id.clone(),
                space_id,
            },
            chat_id: chat.id.clone(),
            target_device_id,
        })
    }

    pub(super) fn ensure_project_actions(&mut self, cx: &mut Context<Self>) {
        let context = self.project_action_context(cx);
        let key = context.as_ref().map(|context| context.key.clone());
        let changed = self.project_actions.activate(key.clone());
        let needs_load = key.as_ref().is_some_and(|key| {
            !self.project_actions.cache.contains_key(key)
                || matches!(
                    self.project_actions.cache.get(key),
                    Some(ProjectActionsStatus::Idle)
                )
        });
        if (changed || needs_load)
            && let Some(context) = context
        {
            self.refresh_project_actions(context, cx);
        }
    }

    fn refresh_project_actions(&mut self, context: ProjectActionContext, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.project_actions
                .mark_unavailable(&context.key, "Engine not connected".into());
            cx.notify();
            return;
        };
        let generation = self.project_actions.begin_load(&context.key);
        let params = project_action_params(
            serde_json::json!({ "spaceId": context.key.space_id }),
            &context.target_device_id,
        );
        let key = context.key.clone();
        self.project_actions.request_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call_as::<ProjectActionsSnapshot>(methods::LIST_PROJECT_ACTIONS, params)
                .await
                .map_err(|err| err.to_string());
            this.update(cx, |shell, cx| {
                if shell.project_actions.accept_load(&key, generation, result) {
                    cx.notify();
                }
            })
            .ok();
        }));
        cx.notify();
    }

    fn close_project_actions_menu(&mut self, cx: &mut Context<Self>) {
        if self.project_actions.menu.begin_close() {
            popover::reap_popup(cx, |shell: &mut Shell| &mut shell.project_actions.menu);
        }
        cx.notify();
    }

    fn toggle_project_actions_menu(&mut self, cx: &mut Context<Self>) {
        if self.project_actions.menu.take_press_was_open() {
            self.close_project_actions_menu(cx);
            return;
        }
        self.project_actions.menu.open(());
        self.project_actions
            .menu_scroll
            .set_offset(gpui::point(px(0.0), px(0.0)));
        if let Some(context) = self.project_action_context(cx) {
            self.refresh_project_actions(context, cx);
        }
        cx.notify();
    }

    fn open_project_action_editor(
        &mut self,
        action: Option<ProjectAction>,
        import: Option<ProjectActionDraft>,
        cx: &mut Context<Self>,
    ) {
        let Some(key) = self.project_actions.active.clone() else {
            return;
        };
        self.close_project_actions_menu(cx);
        let action_id = action.as_ref().map(|action| action.id.clone());
        let draft =
            action
                .as_ref()
                .map(draft_from_action)
                .or(import)
                .unwrap_or(ProjectActionDraft {
                    name: String::new(),
                    command: String::new(),
                    icon: ProjectActionIcon::Play,
                    run_on_worktree_create: false,
                });
        let name = cx.new(|cx| ComposerInput::new("Action name", cx));
        name.update(cx, |input, cx| input.set_text(draft.name, cx));
        let command = cx.new(|cx| ComposerInput::new("Command", cx));
        command.update(cx, |input, cx| input.set_text(draft.command, cx));
        let name_events = cx.subscribe(
            &name,
            |_: &mut Shell, _, _: &crate::composer::ComposerInputEvent, cx| cx.notify(),
        );
        let command_events = cx.subscribe(
            &command,
            |_: &mut Shell, _, _: &crate::composer::ComposerInputEvent, cx| cx.notify(),
        );
        self.project_actions.editor = Some(ProjectActionEditor {
            key,
            action_id,
            name,
            command,
            icon: draft.icon,
            run_on_worktree_create: draft.run_on_worktree_create,
            error: None,
            saving: false,
            focus_pending: true,
            confirm_delete: false,
            _name_events: name_events,
            _command_events: command_events,
        });
        cx.notify();
    }

    fn save_project_action(&mut self, cx: &mut Context<Self>) {
        let Some(context) = self.project_action_context(cx) else {
            if let Some(editor) = self.project_actions.editor.as_mut() {
                editor.error = Some("Project action is no longer available".into());
                cx.notify();
            }
            return;
        };
        let Some(editor) = self.project_actions.editor.as_mut() else {
            return;
        };
        if editor.saving {
            return;
        }
        let name = editor.name.read(cx).text().trim().to_string();
        let command = editor.command.read(cx).text().trim().to_string();
        let error = if name.is_empty() {
            Some("Action name is required".to_string())
        } else if name.chars().count() > 80 {
            Some("Action name must not exceed 80 characters".to_string())
        } else if command.is_empty() {
            Some("Action command is required".to_string())
        } else if command.len() > 16 * 1024 {
            Some("Action command must not exceed 16384 bytes".to_string())
        } else {
            None
        };
        if let Some(error) = error {
            editor.error = Some(error);
            cx.notify();
            return;
        }
        if context.key != editor.key {
            editor.error = Some("The selected project changed".into());
            cx.notify();
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            editor.error = Some("Engine not connected".into());
            cx.notify();
            return;
        };
        let draft = ProjectActionDraft {
            name,
            command,
            icon: editor.icon,
            run_on_worktree_create: editor.run_on_worktree_create,
        };
        let action_id = editor.action_id.clone();
        editor.saving = true;
        editor.error = None;
        if let Some(snapshot) = self.project_actions.active_snapshot().cloned() {
            self.project_actions
                .cache
                .insert(context.key.clone(), ProjectActionsStatus::Saving(snapshot));
        }
        let params = project_action_params(
            serde_json::json!({
                "spaceId": context.key.space_id,
                "actionId": action_id,
                "action": draft,
            }),
            &context.target_device_id,
        );
        let key = context.key.clone();
        let mutation_generation = self.project_actions.begin_mutation();
        self.project_actions.mutation_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call_as::<ProjectActionsSnapshot>(methods::UPSERT_PROJECT_ACTION, params)
                .await;
            this.update(cx, |shell, cx| {
                if !shell
                    .project_actions
                    .is_current_mutation(&key, mutation_generation)
                {
                    return;
                }
                match result {
                    Ok(snapshot) => {
                        shell
                            .project_actions
                            .cache
                            .insert(key.clone(), ProjectActionsStatus::Ready(snapshot));
                        if shell
                            .project_actions
                            .editor
                            .as_ref()
                            .is_some_and(|editor| editor.key == key && editor.saving)
                        {
                            shell.project_actions.editor = None;
                        }
                    }
                    Err(err) => {
                        let message = err.to_string();
                        shell
                            .project_actions
                            .mark_unavailable(&key, message.clone());
                        if let Some(editor) = shell
                            .project_actions
                            .editor
                            .as_mut()
                            .filter(|editor| editor.key == key && editor.saving)
                        {
                            editor.saving = false;
                            editor.error = Some(message);
                        }
                    }
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn delete_project_action(&mut self, cx: &mut Context<Self>) {
        let Some(context) = self.project_action_context(cx) else {
            return;
        };
        let Some(editor) = self.project_actions.editor.as_mut() else {
            return;
        };
        let Some(action_id) = editor.action_id.clone() else {
            self.project_actions.editor = None;
            cx.notify();
            return;
        };
        if editor.saving {
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        editor.saving = true;
        let params = project_action_params(
            serde_json::json!({
                "spaceId": context.key.space_id,
                "actionId": action_id,
            }),
            &context.target_device_id,
        );
        let key = context.key.clone();
        let mutation_generation = self.project_actions.begin_mutation();
        self.project_actions.mutation_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call_as::<ProjectActionsSnapshot>(methods::DELETE_PROJECT_ACTION, params)
                .await;
            this.update(cx, |shell, cx| {
                if !shell
                    .project_actions
                    .is_current_mutation(&key, mutation_generation)
                {
                    return;
                }
                match result {
                    Ok(snapshot) => {
                        shell
                            .project_actions
                            .cache
                            .insert(key.clone(), ProjectActionsStatus::Ready(snapshot));
                        if shell
                            .settings
                            .last_project_action_by_space_id
                            .get(&key.space_id)
                            == Some(&action_id)
                        {
                            shell
                                .settings
                                .last_project_action_by_space_id
                                .remove(&key.space_id);
                            shell.schedule_save(cx);
                        }
                        if shell
                            .project_actions
                            .editor
                            .as_ref()
                            .is_some_and(|editor| editor.key == key && editor.saving)
                        {
                            shell.project_actions.editor = None;
                        }
                    }
                    Err(err) => {
                        let message = err.to_string();
                        shell
                            .project_actions
                            .mark_unavailable(&key, message.clone());
                        if let Some(editor) = shell
                            .project_actions
                            .editor
                            .as_mut()
                            .filter(|editor| editor.key == key && editor.saving)
                        {
                            editor.saving = false;
                            editor.confirm_delete = false;
                            editor.error = Some(message);
                        }
                    }
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn run_project_action(
        &mut self,
        key: &ProjectActionsKey,
        action: ProjectAction,
        cx: &mut Context<Self>,
    ) {
        let Some(context) = self.project_action_context(cx) else {
            return;
        };
        if context.key != *key
            || self.project_actions.active.as_ref() != Some(key)
            || !self
                .project_actions
                .active_status()
                .is_some_and(ProjectActionsStatus::can_run)
        {
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.close_project_actions_menu(cx);

        let panel = self.terminal_panel(cx);
        let tab = panel.update(cx, |panel, cx| {
            panel.reserve_tab_for_chat(context.chat_id.clone(), action.name.clone(), cx)
        });
        self.panels
            .update(&context.chat_id, |panels| panels.terminal_open = true);
        self.terminal_tween = None;
        self.terminal_tween_task = None;
        panel.update(cx, |panel, cx| {
            panel.set_open(true, cx);
            panel.select_tab_by_key(tab, cx);
        });

        let params = project_action_params(
            serde_json::json!({
                "spaceId": context.key.space_id,
                "chatId": context.chat_id,
                "actionId": action.id,
                "cols": 80,
                "rows": 24,
            }),
            &context.target_device_id,
        );
        let key = context.key.clone();
        let chat_id = context.chat_id.clone();
        let target = context.target_device_id.clone();
        let action_id = action.id.clone();
        cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call_as::<ProjectActionRun>(methods::RUN_PROJECT_ACTION, params)
                .await;
            match result {
                Ok(run) => {
                    let terminal_id = run.terminal.id.clone();
                    let attached = panel.update(cx, |panel, cx| {
                        panel.attach_reserved_session(
                            &chat_id,
                            tab,
                            run.terminal,
                            target.clone(),
                            cx,
                        )
                    });
                    if !attached {
                        let _ = engine
                            .client()
                            .call(
                                methods::CLOSE_TERMINAL,
                                project_action_params(
                                    serde_json::json!({ "terminalId": terminal_id }),
                                    &target,
                                ),
                            )
                            .await;
                    }
                    this.update(cx, |shell, cx| {
                        shell
                            .settings
                            .last_project_action_by_space_id
                            .insert(key.space_id, action_id);
                        shell.schedule_save(cx);
                        cx.notify();
                    })
                    .ok();
                }
                Err(err) => {
                    let message = err.to_string();
                    panel.update(cx, |panel, cx| {
                        panel.fail_reserved_tab(&chat_id, tab, &message, cx)
                    });
                    this.update(cx, |shell, cx| {
                        shell.project_actions.mark_unavailable(&key, message);
                        cx.notify();
                    })
                    .ok();
                }
            }
        })
        .detach();
    }

    fn t3_project_command(
        &mut self,
        method: &'static str,
        ref_name: Option<String>,
        cx: &mut Context<Self>,
    ) {
        if self.t3_panel.git_busy {
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        if self.active_chat.is_empty() {
            return;
        }
        let chat = self.active_chat.clone();
        self.t3_panel.git_busy = true;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(
                    method,
                    serde_json::json!({"chatId":chat,"refName":ref_name}),
                )
                .await;
            this.update(cx, |shell, cx| {
                shell.t3_panel.git_busy = false;
                shell.sidebar_notice = Some(match result {
                    Ok(value) => {
                        shell.t3_panel.branches_open = false;
                        value["toast"]["title"]
                            .as_str()
                            .unwrap_or(match method {
                                "T3InitializeGit" => "Git initialized",
                                "T3RefreshGit" => "Git status refreshed",
                                "T3CheckoutBranch" => "Branch checked out",
                                "T3PushGit" => "Push completed",
                                "T3CreatePullRequest" => "Pull request created",
                                _ => "Opened in Zed",
                            })
                            .to_owned()
                            .into()
                    }
                    Err(error) => format!("Project action failed: {error}").into(),
                });
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn load_t3_branches(&mut self, cursor: Option<u64>, cx: &mut Context<Self>) {
        if self.t3_panel.branches_loading || self.active_chat.is_empty() {
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let chat = self.active_chat.clone();
        let workspace = self.t3_panel.workspace_path.clone();
        self.t3_panel.branches_open = true;
        self.t3_panel.branches_loading = true;
        self.t3_panel.branches_error = None;
        if cursor.is_none() {
            self.t3_panel.branches.clear();
            self.t3_panel.branches_page = 0;
            self.t3_panel.branches_cursor = None;
        }
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(
                    "T3ListGitBranches",
                    serde_json::json!({"chatId":chat,"cursor":cursor}),
                )
                .await;
            this.update(cx, |shell, cx| {
                if shell.active_chat != chat || shell.t3_panel.workspace_path != workspace {
                    return;
                }
                shell.t3_panel.branches_loading = false;
                match result {
                    Ok(value) => match value["refs"].as_array() {
                        Some(refs) => {
                            for name in refs
                                .iter()
                                .filter_map(|reference| reference["name"].as_str())
                            {
                                if !shell.t3_panel.branches.iter().any(|branch| branch == name) {
                                    shell.t3_panel.branches.push(name.to_owned());
                                }
                            }
                            shell.t3_panel.branches_cursor = value["nextCursor"].as_u64();
                        }
                        None => {
                            shell.t3_panel.branches_error =
                                Some("Invalid branch list from server".into())
                        }
                    },
                    Err(error) => {
                        shell.t3_panel.branches_error =
                            Some(format!("Could not load branches: {error}"))
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn open_t3_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_t3_settings_route(crate::browser::T3SettingsRoute::General, window, cx);
    }

    pub(super) fn open_t3_settings_route(
        &mut self,
        route: crate::browser::T3SettingsRoute,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        cx.spawn_in(window, async move |this, cx| {
            let result = crate::attachments::call_with_timeout(
                &engine,
                cx.background_executor(),
                "T3BrowserBootstrap",
                serde_json::json!({}),
                Duration::from_secs(30),
            )
            .await;
            this.update_in(cx, |shell, window, cx| match result {
                Ok(session) => {
                    shell.close_settings(cx);
                    shell.route = Route::Chat;
                    shell.t3_panel_open = Some(false);
                    shell.set_surfaces_open(true, cx);
                    shell.add_browser_surface(None, window, cx);
                    if let RightSurface::Browser(id) = shell.resolved_right_active(cx) {
                        #[cfg(target_os = "linux")]
                        if let Some(browser) = shell.browsers.get(&id) {
                            browser.update(cx, |browser, cx| {
                                browser.open_t3_settings_route(session, route, window, cx)
                            });
                        }
                    }
                }
                Err(error) => {
                    shell.sidebar_notice = Some(format!("Settings could not open: {error}").into());
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    fn stop_t3_panel_agent(&mut self, child: String, cx: &mut Context<Self>) {
        if self.t3_panel.stopping_child.is_some() {
            return;
        }
        let allowed = self
            .state
            .read(cx)
            .t3_details
            .get(&self.active_chat)
            .is_some_and(|details| {
                details
                    .agents
                    .iter()
                    .any(|a| a.child_thread_id.as_deref() == Some(&child) && a.can_stop())
            });
        if !allowed {
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.t3_panel.stopping_child = Some(child.clone());
        let parent = self.active_chat.clone();
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(
                    "T3StopAgent",
                    serde_json::json!({"chatId":parent,"childThreadId":child}),
                )
                .await;
            this.update(cx, |shell, cx| {
                shell.t3_panel.stopping_child = None;
                if let Err(error) = result {
                    shell.sidebar_notice = Some(format!("Could not stop agent: {error}").into());
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn render_t3_agent_row(
        &mut self,
        agent: &zeron_t3::AgentDetails,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        use zeron_t3::AgentStatus;
        let theme = Theme::of(cx).clone();
        let failed = agent.status == AgentStatus::Failed;
        let tone = if failed {
            theme.danger
        } else if agent.status.is_active() {
            theme.accent
        } else {
            theme.text_muted
        };
        let title = zeron_t3::agent_display_title(&agent.title);
        let child = agent.child_thread_id.clone();
        let can_open = child.is_some() && !agent.missing;
        let mut content = panel_control(&theme, format!("t3-agent-open-{}", agent.id))
            .h(px(40.0))
            .flex_1()
            .child(
                icon(agent_provider_icon(&agent.driver))
                    .size(px(14.0))
                    .flex_none()
                    .text_color(theme.text_muted),
            )
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .flex()
                    .flex_col()
                    .child(div().truncate().child(SharedString::from(title.clone())))
                    .child(
                        div()
                            .truncate()
                            .text_size(crate::typography::ui_rems(10.0))
                            .text_color(theme.text_muted)
                            .child(SharedString::from(
                                agent
                                    .model
                                    .as_deref()
                                    .filter(|model| !model.trim().is_empty())
                                    .unwrap_or("Model not reported")
                                    .to_owned(),
                            )),
                    ),
            )
            .child(
                div()
                    .text_size(crate::typography::ui_rems(10.0))
                    .text_color(tone)
                    .child(agent.status.label()),
            )
            .when_some(
                agent.elapsed_ms(Utc::now().timestamp_millis()),
                |el, elapsed| {
                    el.child(
                        div()
                            .text_size(crate::typography::ui_rems(10.0))
                            .text_color(theme.text_muted)
                            .child(SharedString::from(panel_duration(elapsed))),
                    )
                },
            )
            .tooltip(crate::settings::widgets::text_tooltip(if agent.missing {
                format!("{title} — related thread unavailable")
            } else if child.is_none() {
                format!("{title} — waiting for a child thread")
            } else {
                format!(
                    "Open {title} · {} · {}{}",
                    agent.provider_instance_id,
                    agent.driver,
                    agent
                        .model
                        .as_ref()
                        .map(|model| format!(" · {model}"))
                        .unwrap_or_default()
                )
            }));
        if can_open {
            content = content.on_click(cx.listener(move |this, _, _, cx| {
                if let Some(child) = &child {
                    this.open_chat(child.clone(), cx);
                }
            }));
        } else {
            content = content.cursor_default().opacity(0.55);
        }
        let mut row = div().flex().items_center().child(content);
        if agent.can_stop()
            && let Some(child) = &agent.child_thread_id
        {
            let child = child.clone();
            let busy = self.t3_panel.stopping_child.is_some();
            row = row.child(
                panel_control(&theme, format!("t3-agent-stop-{}", agent.id))
                    .flex_none()
                    .text_color(theme.danger)
                    .tooltip(crate::settings::widgets::text_tooltip(format!(
                        "Stop {title}"
                    )))
                    .when(busy, |el| el.cursor_default().opacity(0.45))
                    .when(!busy, |el| {
                        el.on_click(cx.listener(move |this, _, _, cx| {
                            this.stop_t3_panel_agent(child.clone(), cx)
                        }))
                    })
                    .child(icon(icons::STOP).size(px(12.0)).text_color(theme.danger)),
            );
        }
        row.into_any_element()
    }

    pub(super) fn render_t3_project_panel(&mut self, cx: &mut Context<Self>) -> AnyElement {
        self.ensure_project_actions(cx);
        if self.t3_panel.chat_id != self.active_chat {
            let stopping_child = self.t3_panel.stopping_child.take();
            let git_busy = self.t3_panel.git_busy;
            self.t3_panel = T3PanelState {
                chat_id: self.active_chat.clone(),
                stopping_child,
                git_busy,
                ..Default::default()
            };
        }
        let theme = Theme::of(cx).clone();
        let state = self.state.read(cx);
        let host = state
            .selected_chat_row()
            .and_then(|chat| state.devices.iter().find(|d| d.id == chat.device_id))
            .map(|d| d.name.clone())
            .unwrap_or_else(|| "T3 environment".into());
        let path = state
            .selected_chat_row()
            .and_then(|chat| chat.cwd.clone())
            .unwrap_or_default();
        if self.t3_panel.workspace_path != path {
            self.t3_panel.workspace_path = path.clone();
            self.t3_panel.branches_open = false;
            self.t3_panel.branches_loading = false;
            self.t3_panel.branches.clear();
            self.t3_panel.branches_cursor = None;
            self.t3_panel.branches_error = None;
            self.t3_panel.branches_page = 0;
        }
        let details = state.t3_details.get(&self.active_chat).cloned();
        let actions = self
            .project_actions
            .visible_snapshot()
            .map(|s| s.actions.clone())
            .unwrap_or_default();
        let can_run = self
            .project_actions
            .active_status()
            .is_some_and(ProjectActionsStatus::can_run);
        let action_key = self.project_actions.active.clone();
        let mut card = div()
            .w_full()
            .flex()
            .flex_col()
            .p(px(8.0))
            .gap(px(2.0))
            .rounded(px(16.0))
            .border_1()
            .border_color(theme.border)
            .bg(theme.surface_card);
        card = card.child(
            div()
                .flex()
                .items_center()
                .gap(px(4.0))
                .child(
                    panel_control(&theme, "t3-panel-host")
                        .flex_1()
                        .tooltip(crate::settings::widgets::text_tooltip(
                            "Environment and devices",
                        ))
                        .child(
                            icon(icons::MONITOR)
                                .size(px(14.0))
                                .flex_none()
                                .text_color(theme.text_muted),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.0))
                                .truncate()
                                .child(SharedString::from(host)),
                        )
                        .child(
                            icon(icons::ALT_ARROW_DOWN)
                                .size(px(10.0))
                                .text_color(theme.text_muted),
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.t3_panel.host_menu = !this.t3_panel.host_menu;
                            this.t3_panel.editor_menu = false;
                            cx.notify();
                        })),
                )
                .child(
                    panel_control(&theme, "t3-panel-editor")
                        .tooltip(crate::settings::widgets::text_tooltip("Open project"))
                        .child(
                            icon(icons::TERMINAL)
                                .size(px(14.0))
                                .text_color(theme.text_muted),
                        )
                        .child("Open")
                        .child(
                            icon(icons::ALT_ARROW_DOWN)
                                .size(px(10.0))
                                .text_color(theme.text_muted),
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.t3_panel.editor_menu = !this.t3_panel.editor_menu;
                            this.t3_panel.host_menu = false;
                            cx.notify();
                        })),
                ),
        );
        if self.t3_panel.host_menu {
            card = card
                .child(
                    div()
                        .id("t3-project-path")
                        .px(px(8.0))
                        .py(px(4.0))
                        .text_size(crate::typography::ui_rems(11.0))
                        .text_color(theme.text_muted)
                        .truncate()
                        .child(SharedString::from(path.clone()))
                        .tooltip(crate::settings::widgets::text_tooltip(path.clone())),
                )
                .child(
                    panel_control(&theme, "t3-panel-connections")
                        .child(
                            icon(icons::MONITOR)
                                .size(px(14.0))
                                .text_color(theme.text_muted),
                        )
                        .child("Devices and connections")
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.open_t3_settings_route(
                                crate::browser::T3SettingsRoute::Connections,
                                window,
                                cx,
                            );
                        })),
                )
                .child(
                    panel_control(&theme, "t3-panel-providers")
                        .child(icon(icons::BOT).size(px(14.0)).text_color(theme.text_muted))
                        .child("Providers")
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.open_native_t3_providers(window, cx);
                        })),
                );
        }
        if self.t3_panel.editor_menu {
            let copy_path = path.clone();
            card = card
                .child(
                    panel_control(&theme, "t3-open-editor")
                        .child(
                            icon(icons::TERMINAL)
                                .size(px(14.0))
                                .text_color(theme.text_muted),
                        )
                        .child("Open in Zed")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.t3_panel.editor_menu = false;
                            this.t3_project_command("T3OpenInEditor", None, cx);
                        })),
                )
                .child(
                    panel_control(&theme, "t3-project-terminal")
                        .child(
                            icon(icons::TERMINAL)
                                .size(px(14.0))
                                .text_color(theme.text_muted),
                        )
                        .child("Terminal")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.t3_panel.editor_menu = false;
                            this.add_terminal_surface(cx);
                        })),
                )
                .child(
                    panel_control(&theme, "t3-copy-project-path")
                        .child(
                            icon(icons::COPY)
                                .size(px(14.0))
                                .text_color(theme.text_muted),
                        )
                        .child("Copy project path")
                        .on_click(move |_, _, cx| {
                            cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                                copy_path.clone(),
                            ));
                        }),
                );
        }
        card = card.child(
            div()
                .flex()
                .items_center()
                .child(
                    panel_control(&theme, "t3-panel-scripts")
                        .flex_1()
                        .child(
                            icon(icons::TERMINAL)
                                .size(px(14.0))
                                .text_color(theme.text_muted),
                        )
                        .child(SharedString::from(format!(
                            "Project scripts · {}",
                            actions.len()
                        )))
                        .child(
                            icon(if self.t3_panel.scripts_open {
                                icons::ALT_ARROW_UP
                            } else {
                                icons::ALT_ARROW_DOWN
                            })
                            .size(px(10.0))
                            .text_color(theme.text_muted),
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.t3_panel.scripts_open = !this.t3_panel.scripts_open;
                            cx.notify();
                        })),
                )
                .child(
                    panel_control(&theme, "t3-add-script")
                        .child(
                            icon(icons::PLUS)
                                .size(px(13.0))
                                .text_color(theme.text_muted),
                        )
                        .tooltip(crate::settings::widgets::text_tooltip("Add project script"))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.open_project_action_editor(None, None, cx)
                        })),
                ),
        );
        if self.t3_panel.scripts_open {
            let status = self.project_actions.active_status().cloned();
            let script_notice = match &status {
                Some(ProjectActionsStatus::Idle | ProjectActionsStatus::Loading) => {
                    Some("Loading project scripts…")
                }
                Some(ProjectActionsStatus::Unavailable { .. }) => {
                    Some("Project scripts could not load.")
                }
                Some(ProjectActionsStatus::Unsupported) => {
                    Some("Project scripts are unavailable on this server.")
                }
                Some(ProjectActionsStatus::Saving(_)) => Some("Saving project script…"),
                _ if actions.is_empty() => Some("Add a script to run it in this project."),
                _ => None,
            };
            if let Some(notice) = script_notice {
                card = card.child(
                    div()
                        .px(px(8.0))
                        .py(px(6.0))
                        .text_size(crate::typography::ui_rems(11.0))
                        .text_color(theme.text_muted)
                        .child(notice),
                );
            }
            if let Some(ProjectActionsStatus::Unavailable { message, .. }) = status {
                card = card.child(
                    panel_control(&theme, "t3-scripts-retry")
                        .child("Retry loading scripts")
                        .tooltip(crate::settings::widgets::text_tooltip(message))
                        .on_click(cx.listener(|this, _, _, cx| {
                            if let Some(context) = this.project_action_context(cx) {
                                this.refresh_project_actions(context, cx);
                            }
                        })),
                );
            }
            for index in panel_page(actions.len(), self.t3_panel.scripts_page) {
                let action = actions[index].clone();
                let key = action_key.clone();
                let edit_action = action.clone();
                card = card.child(
                    div()
                        .flex()
                        .items_center()
                        .child(
                            panel_control(&theme, format!("t3-script-{}", action.id))
                                .flex_1()
                                .child(
                                    icon(action_icon(action.icon))
                                        .size(px(14.0))
                                        .text_color(theme.text_muted),
                                )
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w(px(0.0))
                                        .truncate()
                                        .child(SharedString::from(action.name.clone())),
                                )
                                .tooltip(crate::settings::widgets::text_tooltip(
                                    action.command.clone(),
                                ))
                                .when(!can_run, |el| el.cursor_default().opacity(0.45))
                                .when(can_run, |el| {
                                    el.on_click(cx.listener(move |this, _, _, cx| {
                                        if let Some(key) = &key {
                                            this.run_project_action(key, action.clone(), cx);
                                        }
                                    }))
                                }),
                        )
                        .child(
                            panel_control(&theme, format!("t3-script-edit-{}", edit_action.id))
                                .child(
                                    icon(icons::SETTINGS)
                                        .size(px(12.0))
                                        .text_color(theme.text_muted),
                                )
                                .tooltip(crate::settings::widgets::text_tooltip(
                                    "Edit project script",
                                ))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.open_project_action_editor(
                                        Some(edit_action.clone()),
                                        None,
                                        cx,
                                    )
                                })),
                        ),
                );
            }
            card = card.child(self.render_t3_panel_pager(
                "scripts",
                actions.len(),
                self.t3_panel.scripts_page,
                cx,
            ));
        }
        card = card.child(div().h(px(1.0)).mx(px(8.0)).my(px(5.0)).bg(theme.border));
        if let Some(details) = &details {
            let busy = self.t3_panel.git_busy;
            if let Some(error) = &details.git_error {
                card = card.child(
                    panel_control(&theme, "t3-git-error")
                        .child(
                            icon(icons::GIT_BRANCH)
                                .size(px(14.0))
                                .text_color(theme.danger),
                        )
                        .child("Git unavailable · Retry")
                        .tooltip(crate::settings::widgets::text_tooltip(error.clone()))
                        .when(busy, |el| el.cursor_default().opacity(0.45))
                        .when(!busy, |el| {
                            el.on_click(cx.listener(|this, _, _, cx| {
                                this.t3_project_command("T3RefreshGit", None, cx)
                            }))
                        }),
                );
            } else if let Some(git) = &details.git {
                if git.is_repo {
                    card = card.child(
                        panel_control(&theme, "t3-project-branch")
                            .child(
                                icon(icons::GIT_BRANCH)
                                    .size(px(14.0))
                                    .text_color(theme.text_muted),
                            )
                            .child(div().flex_1().min_w(px(0.0)).truncate().child(
                                SharedString::from(
                                    git.branch.clone().unwrap_or_else(|| "Detached HEAD".into()),
                                ),
                            ))
                            .child(
                                icon(icons::ALT_ARROW_DOWN)
                                    .size(px(10.0))
                                    .text_color(theme.text_muted),
                            )
                            .tooltip(crate::settings::widgets::text_tooltip("Choose a branch"))
                            .when(busy, |el| el.cursor_default().opacity(0.45))
                            .when(!busy, |el| {
                                el.on_click(cx.listener(|this, _, _, cx| {
                                    if this.t3_panel.branches_open {
                                        this.t3_panel.branches_open = false;
                                        cx.notify();
                                    } else {
                                        this.load_t3_branches(None, cx);
                                    }
                                }))
                            }),
                    );
                    if self.t3_panel.branches_open {
                        if let Some(branch) = &git.branch {
                            let branch = branch.clone();
                            card = card.child(
                                panel_control(&theme, "t3-copy-branch")
                                    .child(
                                        icon(icons::COPY)
                                            .size(px(12.0))
                                            .text_color(theme.text_muted),
                                    )
                                    .child("Copy branch name")
                                    .on_click(move |_, _, cx| {
                                        cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                                            branch.clone(),
                                        ))
                                    }),
                            );
                        }
                        if let Some(error) = &self.t3_panel.branches_error {
                            card = card.child(
                                panel_control(&theme, "t3-branches-retry")
                                    .child("Could not load branches · Retry")
                                    .tooltip(crate::settings::widgets::text_tooltip(error.clone()))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.load_t3_branches(None, cx)
                                    })),
                            );
                        }
                        let can_checkout =
                            !busy && !git.has_working_tree_changes && details.active_agents == 0;
                        for index in
                            panel_page(self.t3_panel.branches.len(), self.t3_panel.branches_page)
                        {
                            let branch = self.t3_panel.branches[index].clone();
                            let selected = git.branch.as_ref() == Some(&branch);
                            card = card.child(panel_control(&theme, format!("t3-checkout-{index}"))
                                .child(icon(icons::GIT_BRANCH).size(px(12.0)).text_color(theme.text_muted))
                                .child(div().truncate().child(SharedString::from(branch.clone())))
                                .tooltip(crate::settings::widgets::text_tooltip(if !can_checkout {
                                    "Stop agents and commit or stash changes before switching branches".to_owned()
                                } else { format!("Check out {branch}") }))
                                .when(!can_checkout || selected, |el| el.cursor_default().opacity(0.45))
                                .when(can_checkout && !selected, |el| el.on_click(cx.listener(move |this, _, _, cx| {
                                    this.t3_project_command("T3CheckoutBranch", Some(branch.clone()), cx)
                                }))));
                        }
                        card = card.child(self.render_t3_panel_pager(
                            "branches",
                            self.t3_panel.branches.len(),
                            self.t3_panel.branches_page,
                            cx,
                        ));
                        if self.t3_panel.branches_loading {
                            card = card.child(
                                div()
                                    .px(px(8.0))
                                    .py(px(6.0))
                                    .text_color(theme.text_muted)
                                    .child("Loading branches…"),
                            );
                        } else if let Some(cursor) = self.t3_panel.branches_cursor {
                            card = card.child(
                                panel_control(&theme, "t3-more-branches")
                                    .child("Load more branches")
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.load_t3_branches(Some(cursor), cx)
                                    })),
                            );
                        }
                    }
                    card =
                        card.child(
                            panel_control(&theme, "t3-working-tree")
                                .child(
                                    icon(icons::GIT_BRANCH)
                                        .size(px(14.0))
                                        .text_color(theme.text_muted),
                                )
                                .child(div().flex_1().min_w(px(0.0)).truncate().child(
                                    SharedString::from(if git.has_working_tree_changes {
                                        format!("Working tree · {} files", git.working_tree_files)
                                    } else {
                                        "Working tree clean".into()
                                    }),
                                ))
                                .child(div().text_color(theme.success).child(SharedString::from(
                                    format!("+{}", git.working_tree_additions),
                                )))
                                .child(div().text_color(theme.danger).child(SharedString::from(
                                    format!("−{}", git.working_tree_deletions),
                                )))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.add_diff_surface(window, cx)
                                })),
                        );
                    card =
                        card.child(
                            panel_control(&theme, "t3-project-diff")
                                .child(
                                    icon(icons::GIT_BRANCH)
                                        .size(px(14.0))
                                        .text_color(theme.text_muted),
                                )
                                .child(div().flex_1().child("Changes"))
                                .child(
                                    div()
                                        .text_color(theme.success)
                                        .child(SharedString::from(format!("+{}", git.additions))),
                                )
                                .child(
                                    div()
                                        .text_color(theme.danger)
                                        .child(SharedString::from(format!("−{}", git.deletions))),
                                )
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.add_diff_surface(window, cx)
                                })),
                        );
                    let remote_label = match &git.remote {
                        Some(remote) => format!(
                            "{} ahead · {} behind{}",
                            remote.ahead_count,
                            remote.behind_count,
                            if remote.has_upstream {
                                ""
                            } else {
                                " · No upstream"
                            }
                        ),
                        None => "Loading remote status…".into(),
                    };
                    card = card.child(
                        panel_control(&theme, "t3-remote-status")
                            .child(
                                icon(icons::GIT_BRANCH)
                                    .size(px(14.0))
                                    .text_color(theme.text_muted),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.0))
                                    .truncate()
                                    .child(SharedString::from(remote_label)),
                            )
                            .tooltip(crate::settings::widgets::text_tooltip("Refresh Git status"))
                            .when(busy, |el| el.cursor_default().opacity(0.45))
                            .when(!busy, |el| {
                                el.on_click(cx.listener(|this, _, _, cx| {
                                    this.t3_project_command("T3RefreshGit", None, cx)
                                }))
                            }),
                    );
                    if let Some(pr) = git.remote.as_ref().and_then(|remote| remote.pr.as_ref()) {
                        let url = pr.url.clone();
                        card = card.child(
                            panel_control(&theme, "t3-pull-request")
                                .child(
                                    icon(icons::GIT_BRANCH)
                                        .size(px(14.0))
                                        .text_color(theme.text_muted),
                                )
                                .child(div().truncate().child(SharedString::from(format!(
                                    "PR #{} · {}",
                                    pr.number, pr.state
                                ))))
                                .tooltip(crate::settings::widgets::text_tooltip(pr.title.clone()))
                                .on_click(move |_, _, cx| cx.open_url(&url)),
                        );
                    }
                    for (method, label, reason) in [
                        ("T3PushGit", "Push", git.push_disabled_reason()),
                        ("T3CreatePullRequest", "Create PR", git.pr_disabled_reason()),
                    ] {
                        card = card.child(
                            panel_control(&theme, method)
                                .child(
                                    icon(icons::GIT_BRANCH)
                                        .size(px(14.0))
                                        .text_color(theme.text_muted),
                                )
                                .child(if busy {
                                    "Git action in progress…"
                                } else {
                                    label
                                })
                                .tooltip(crate::settings::widgets::text_tooltip(
                                    reason.unwrap_or(label),
                                ))
                                .when(busy || reason.is_some(), |el| {
                                    el.cursor_default().opacity(0.45)
                                })
                                .when(!busy && reason.is_none(), |el| {
                                    el.on_click(cx.listener(move |this, _, _, cx| {
                                        this.t3_project_command(method, None, cx)
                                    }))
                                }),
                        );
                    }
                } else {
                    card = card.child(
                        panel_control(&theme, "t3-initialize-git")
                            .child(
                                icon(icons::GIT_BRANCH)
                                    .size(px(14.0))
                                    .text_color(theme.text_muted),
                            )
                            .child(if busy {
                                "Initializing Git…"
                            } else {
                                "Initialize Git"
                            })
                            .when(busy, |el| el.cursor_default().opacity(0.45))
                            .when(!busy, |el| {
                                el.on_click(cx.listener(|this, _, _, cx| {
                                    this.t3_project_command("T3InitializeGit", None, cx)
                                }))
                            }),
                    );
                }
            } else {
                card = card.child(
                    div()
                        .px(px(8.0))
                        .py(px(6.0))
                        .text_size(crate::typography::ui_rems(11.0))
                        .text_color(theme.text_muted)
                        .child("Loading Git status…"),
                );
            }
            let active: Vec<_> = details
                .agents
                .iter()
                .filter(|a| a.status.is_active())
                .collect();
            let previous: Vec<_> = details
                .agents
                .iter()
                .filter(|a| !a.status.is_active())
                .collect();
            let running = active
                .iter()
                .filter(|a| a.status == zeron_t3::AgentStatus::Running)
                .count();
            card = card
                .child(div().h(px(1.0)).mx(px(8.0)).my(px(5.0)).bg(theme.border))
                .child(
                    panel_control(&theme, "t3-panel-lineage")
                        .child(div().flex_1().child(SharedString::from(if running > 0 {
                            format!("Lineage · {running} running")
                        } else {
                            "Lineage".into()
                        })))
                        .child(
                            icon(if self.t3_panel.lineage_open {
                                icons::ALT_ARROW_UP
                            } else {
                                icons::ALT_ARROW_DOWN
                            })
                            .size(px(10.0))
                            .text_color(theme.text_muted),
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.t3_panel.lineage_open = !this.t3_panel.lineage_open;
                            cx.notify();
                        })),
                );
            if self.t3_panel.lineage_open {
                let total = details.related_threads.len() + active.len();
                for index in panel_page(total, self.t3_panel.lineage_page) {
                    if let Some(related) = details.related_threads.get(index) {
                        let id = related.thread_id.clone();
                        let missing = related.missing;
                        let kind = match related.kind {
                            zeron_t3::RelationshipKind::Parent => "Parent",
                            zeron_t3::RelationshipKind::Fork => "Fork",
                            zeron_t3::RelationshipKind::Subagent => "Agent",
                            zeron_t3::RelationshipKind::Transfer => "Transfer",
                        };
                        card = card.child(
                            panel_control(&theme, format!("t3-related-{id}"))
                                .child(
                                    icon(
                                        related
                                            .driver
                                            .as_deref()
                                            .map(agent_provider_icon)
                                            .unwrap_or(icons::GIT_BRANCH),
                                    )
                                    .size(px(14.0))
                                    .text_color(theme.text_muted),
                                )
                                .child(div().flex_1().min_w(px(0.0)).truncate().child(
                                    SharedString::from(
                                        if related.kind == zeron_t3::RelationshipKind::Subagent {
                                            zeron_t3::agent_display_title(&related.title)
                                        } else {
                                            related.title.clone()
                                        },
                                    ),
                                ))
                                .child(
                                    div()
                                        .text_size(crate::typography::ui_rems(10.0))
                                        .text_color(theme.text_muted)
                                        .child(related.status.label()),
                                )
                                .tooltip(crate::settings::widgets::text_tooltip(if missing {
                                    "This related thread is unavailable".to_owned()
                                } else {
                                    format!("Open {} conversation", kind.to_lowercase())
                                }))
                                .when(missing, |el| el.cursor_default().opacity(0.45))
                                .when(!missing, |el| {
                                    el.on_click(cx.listener(move |this, _, _, cx| {
                                        this.open_chat(id.clone(), cx)
                                    }))
                                }),
                        );
                    } else {
                        card = card.child(self.render_t3_agent_row(
                            active[index - details.related_threads.len()],
                            cx,
                        ));
                    }
                }
                if total == 0 {
                    card = card.child(
                        div()
                            .px(px(8.0))
                            .py(px(6.0))
                            .text_size(crate::typography::ui_rems(11.0))
                            .text_color(theme.text_muted)
                            .child("No related conversations or running agents."),
                    );
                }
                card = card.child(self.render_t3_panel_pager(
                    "lineage",
                    total,
                    self.t3_panel.lineage_page,
                    cx,
                ));
            }
            if !previous.is_empty() {
                card = card.child(
                    panel_control(&theme, "t3-panel-previous")
                        .child(div().flex_1().child(SharedString::from(format!(
                            "Previous agents · {}",
                            previous.len()
                        ))))
                        .child(
                            icon(if self.t3_panel.previous_open {
                                icons::ALT_ARROW_UP
                            } else {
                                icons::ALT_ARROW_DOWN
                            })
                            .size(px(10.0))
                            .text_color(theme.text_muted),
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.t3_panel.previous_open = !this.t3_panel.previous_open;
                            cx.notify();
                        })),
                );
                if self.t3_panel.previous_open {
                    for index in panel_page(previous.len(), self.t3_panel.previous_page) {
                        card = card.child(self.render_t3_agent_row(previous[index], cx));
                    }
                    card = card.child(self.render_t3_panel_pager(
                        "previous",
                        previous.len(),
                        self.t3_panel.previous_page,
                        cx,
                    ));
                }
            }
        } else {
            card = card.child(
                div()
                    .px(px(8.0))
                    .py(px(6.0))
                    .text_size(crate::typography::ui_rems(11.0))
                    .text_color(theme.text_muted)
                    .child("Loading project details…"),
            );
        }
        card.into_any_element()
    }

    fn render_t3_panel_pager(
        &self,
        group: &'static str,
        total: usize,
        page: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if total <= PANEL_PAGE_SIZE {
            return div().into_any_element();
        }
        let theme = Theme::of(cx).clone();
        let range = panel_page(total, page);
        let current = range.start / PANEL_PAGE_SIZE;
        div()
            .flex()
            .items_center()
            .justify_end()
            .gap(px(4.0))
            .child(
                panel_control(&theme, format!("t3-{group}-previous"))
                    .child(
                        icon(icons::ALT_ARROW_LEFT)
                            .size(px(12.0))
                            .text_color(theme.text_muted),
                    )
                    .tooltip(crate::settings::widgets::text_tooltip("Previous page"))
                    .when(current == 0, |el| el.cursor_default().opacity(0.35))
                    .when(current > 0, |el| {
                        el.on_click(cx.listener(move |this, _, _, cx| {
                            let page = match group {
                                "scripts" => &mut this.t3_panel.scripts_page,
                                "previous" => &mut this.t3_panel.previous_page,
                                "branches" => &mut this.t3_panel.branches_page,
                                _ => &mut this.t3_panel.lineage_page,
                            };
                            *page = current - 1;
                            cx.notify();
                        }))
                    }),
            )
            .child(
                div()
                    .text_size(crate::typography::ui_rems(10.0))
                    .text_color(theme.text_muted)
                    .child(SharedString::from(format!(
                        "{}–{} of {total}",
                        range.start + 1,
                        range.end
                    ))),
            )
            .child(
                panel_control(&theme, format!("t3-{group}-next"))
                    .child(
                        icon(icons::ALT_ARROW_RIGHT)
                            .size(px(12.0))
                            .text_color(theme.text_muted),
                    )
                    .tooltip(crate::settings::widgets::text_tooltip("Next page"))
                    .when(range.end == total, |el| el.cursor_default().opacity(0.35))
                    .when(range.end < total, |el| {
                        el.on_click(cx.listener(move |this, _, _, cx| {
                            let page = match group {
                                "scripts" => &mut this.t3_panel.scripts_page,
                                "previous" => &mut this.t3_panel.previous_page,
                                "branches" => &mut this.t3_panel.branches_page,
                                _ => &mut this.t3_panel.lineage_page,
                            };
                            *page = current + 1;
                            cx.notify();
                        }))
                    }),
            )
            .into_any_element()
    }

    pub(super) fn render_project_actions_control(
        &mut self,
        available_titlebar_width: f32,
        viewport_height: Pixels,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        self.ensure_project_actions(cx);
        let status = self.project_actions.active_status()?.clone();
        let snapshot = self.project_actions.visible_snapshot()?;
        let key = self.project_actions.active.clone()?;
        let loading = matches!(
            status,
            ProjectActionsStatus::Idle | ProjectActionsStatus::Loading
        );
        let can_run = status.can_run();
        let unavailable = matches!(status, ProjectActionsStatus::Unavailable { .. });
        let theme = Theme::of(cx).clone();
        let preferred = preferred_action(
            &snapshot.actions,
            self.settings
                .last_project_action_by_space_id
                .get(&snapshot.space_id)
                .map(String::as_str),
        )
        .cloned();
        let has_imports = !snapshot.importable_actions.is_empty();
        let has_actions = !snapshot.actions.is_empty();
        let show_label = show_action_label(available_titlebar_width);
        let menu_mounted = self.project_actions.menu.get().is_some();
        let menu_closing = self.project_actions.menu.closing_since();

        // A solid frosted pill in the composer's material and edge —
        // with the frost on its own back layer so the dropdown menu (a
        // deferred child) never lands inside the blur.
        let mut control = div()
            .relative()
            .flex_none()
            .flex()
            .flex_row()
            .items_center()
            .h(px(ACTION_CONTROL_HEIGHT))
            .rounded(px(ACTION_CONTROL_RADIUS))
            .occlude()
            .child(
                div().absolute().inset_0().child(crate::frost::frosted(
                    ACTION_CONTROL_RADIUS,
                    crate::frost::MENU_BLUR,
                    div()
                        .size_full()
                        .rounded(px(ACTION_CONTROL_RADIUS))
                        .border_1()
                        .border_color(theme.composer_surface_border())
                        .bg(action_fill(&theme, false))
                        .when(!theme.is_frost(), |el| el.shadow_sm()),
                )),
            );

        if loading {
            control = control
                .child(
                    action_segment(&theme, "project-action-loading", false)
                        .rounded_l(px(ACTION_CONTROL_RADIUS))
                        .opacity(0.45)
                        .child(
                            div()
                                .size(px(13.0))
                                .flex_none()
                                .flex()
                                .items_center()
                                .justify_center()
                                .child(crate::loaders::mini_mono_spinner(
                                    "project-actions-loading",
                                    2.0,
                                    theme.text_muted,
                                    cx.entity_id(),
                                    cx,
                                )),
                        )
                        .when(show_label, |el| el.child("Loading…")),
                )
                .child(action_divider(&theme))
                .child(action_chevron(&theme, false, |_, _, _| {}));
        } else if let Some(action) = preferred.clone() {
            let run_action = action.clone();
            let run_key = key.clone();
            let main = action_segment(&theme, "project-action-main", can_run)
                .rounded_l(px(ACTION_CONTROL_RADIUS))
                .when(!can_run, |el| el.opacity(0.45))
                .when(can_run, |el| {
                    el.cursor_pointer()
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.run_project_action(&run_key, run_action.clone(), cx)
                        }))
                })
                // Without its label the segment is a bare glyph.
                .when(!show_label, |el| {
                    el.tooltip(crate::settings::widgets::text_tooltip(format!(
                        "Run {}",
                        action.name
                    )))
                })
                .child(
                    icon(action_icon(action.icon))
                        .size(px(13.0))
                        .flex_none()
                        .text_color(theme.text_muted),
                )
                .when(show_label, |el| {
                    el.child(
                        div()
                            .max_w(px(150.0))
                            .truncate()
                            .child(SharedString::from(action.name)),
                    )
                });
            control = control.child(main).child(action_divider(&theme)).child(
                action_chevron(
                    &theme,
                    true,
                    cx.listener(|this, _, _, cx| this.toggle_project_actions_menu(cx)),
                )
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, _, _| {
                        this.project_actions.menu.note_trigger_press();
                    }),
                ),
            );
        } else if unavailable {
            let retry = action_segment(&theme, "project-actions-unavailable", true)
                .rounded(px(ACTION_CONTROL_RADIUS))
                .cursor_pointer()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, _, _| {
                        this.project_actions.menu.note_trigger_press();
                    }),
                )
                .on_click(cx.listener(|this, _, _, cx| this.toggle_project_actions_menu(cx)))
                .child(
                    icon(icons::DANGER_TRIANGLE)
                        .size(px(13.0))
                        .text_color(theme.danger),
                )
                .when(show_label, |el| {
                    el.child(SharedString::from("Actions unavailable"))
                });
            control = control.child(retry);
        } else {
            let add = action_segment(&theme, "project-action-add", true)
                .rounded_l(px(ACTION_CONTROL_RADIUS))
                .when(!has_imports, |el| el.rounded_r(px(ACTION_CONTROL_RADIUS)))
                .cursor_pointer()
                .on_click(
                    cx.listener(|this, _, _, cx| this.open_project_action_editor(None, None, cx)),
                )
                .child(
                    icon(icons::PLUS)
                        .size(px(13.0))
                        .text_color(theme.text_muted),
                )
                .child("Add action");
            control = control.child(add);
            if has_imports {
                control = control.child(action_divider(&theme)).child(
                    action_chevron(
                        &theme,
                        true,
                        cx.listener(|this, _, _, cx| this.toggle_project_actions_menu(cx)),
                    )
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, _, _| {
                            this.project_actions.menu.note_trigger_press();
                        }),
                    ),
                );
            }
        }

        if !loading && menu_mounted && (has_actions || has_imports || !can_run) {
            let menu =
                self.render_project_actions_menu(&key, &status, &snapshot, viewport_height, cx);
            control = control.child(popover::anchored_menu_below(
                "project-actions-menu",
                menu,
                menu_closing,
            ));
        }
        Some(control.into_any_element())
    }

    fn render_project_actions_menu(
        &mut self,
        key: &ProjectActionsKey,
        status: &ProjectActionsStatus,
        snapshot: &ProjectActionsSnapshot,
        viewport_height: Pixels,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let mut card = project_actions_menu_surface(
            &theme,
            viewport_height,
            &self.project_actions.menu_scroll,
        )
        .on_mouse_down_out(cx.listener(|this, _, _, cx| this.close_project_actions_menu(cx)))
        .child(popover::menu_heading(&theme, "Project actions"));
        if let ProjectActionsStatus::Unavailable { message, .. } = status {
            let retry = self.project_action_context(cx);
            card = card
                .child(
                    div()
                        .px(px(8.0))
                        .py(px(6.0))
                        .text_size(px(12.0))
                        .text_color(theme.danger)
                        .child(SharedString::from(message.clone())),
                )
                .when_some(retry, |card, context| {
                    card.child(
                        popover::menu_row(&theme, false, "project-actions-retry")
                            .id("project-actions-retry")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.refresh_project_actions(context.clone(), cx)
                            }))
                            .child(icon(icons::REFRESH).size(px(15.0)))
                            .child(SharedString::from("Retry")),
                    )
                });
        }
        for action in snapshot.actions.clone() {
            let key = key.clone();
            let run = action.clone();
            let edit = action.clone();
            let row_id = SharedString::from(format!("project-action-row-{}", action.id));
            card = card.child(
                popover::menu_row(&theme, false, row_id.clone())
                    .id(row_id)
                    .when(status.can_run(), |row| {
                        row.on_click(cx.listener(move |this, _, _, cx| {
                            this.run_project_action(&key, run.clone(), cx)
                        }))
                    })
                    .child(
                        icon(action_icon(action.icon))
                            .size(px(15.0))
                            .text_color(theme.text_muted),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .child(SharedString::from(if action.run_on_worktree_create {
                                format!("{} (setup)", action.name)
                            } else {
                                action.name
                            })),
                    )
                    .child(
                        div()
                            .id(SharedString::from(format!(
                                "edit-project-action-{}",
                                edit.id
                            )))
                            .size(px(22.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(5.0))
                            .hover(|style| style.bg(crate::theme::ink(0.08)))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                this.open_project_action_editor(Some(edit.clone()), None, cx)
                            }))
                            .child(
                                icon(icons::SETTINGS_MINIMALISTIC)
                                    .size(px(14.0))
                                    .text_color(theme.text_muted),
                            ),
                    ),
            );
        }
        if !snapshot.importable_actions.is_empty() {
            card = card
                .child(popover::menu_separator())
                .child(popover::menu_heading(&theme, "Import from zeron.json"));
            for draft in snapshot.importable_actions.clone() {
                let import = draft.clone();
                let row_id = SharedString::from(format!("import-project-action-{}", draft.name));
                card = card.child(
                    popover::menu_row(&theme, false, row_id.clone())
                        .id(row_id)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.open_project_action_editor(None, Some(import.clone()), cx)
                        }))
                        .child(
                            icon(action_icon(draft.icon))
                                .size(px(15.0))
                                .text_color(theme.text_muted),
                        )
                        .child(SharedString::from(draft.name)),
                );
            }
        }
        if let Some(issue) = snapshot.project_file_issue.clone() {
            card = card.child(
                div()
                    .px(px(8.0))
                    .py(px(5.0))
                    .text_size(px(11.0))
                    .text_color(theme.text_muted)
                    .child(SharedString::from(issue)),
            );
        }
        card.child(popover::menu_separator())
            .child(
                popover::menu_row(&theme, false, "project-actions-add-row")
                    .id("project-actions-add-row")
                    .on_click(
                        cx.listener(|this, _, _, cx| {
                            this.open_project_action_editor(None, None, cx)
                        }),
                    )
                    .child(
                        icon(icons::PLUS)
                            .size(px(15.0))
                            .text_color(theme.text_muted),
                    )
                    .child(SharedString::from("Add action")),
            )
            .into_any_element()
    }

    pub(super) fn render_project_action_overlay(
        &mut self,
        viewport: gpui::Size<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let editor = self.project_actions.editor.as_mut()?;
        if std::mem::take(&mut editor.focus_pending) {
            window.focus(&editor.name.focus_handle(cx), cx);
        }
        let theme = Theme::of(cx).clone();
        if editor.confirm_delete {
            let name = editor.name.read(cx).text().trim().to_string();
            let card = popover::dialog_card(&theme)
                .child(popover::dialog_title(&theme, "Delete action?"))
                .child(div().mt(px(6.0)).child(popover::dialog_body(
                    &theme,
                    format!("“{name}” will be permanently deleted."),
                )))
                .child(
                    div()
                        .mt(px(16.0))
                        .flex()
                        .justify_end()
                        .gap(px(8.0))
                        .child(
                            popover::btn_ghost(&theme, "Cancel", "action-delete-cancel")
                                .id("action-delete-cancel")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    if let Some(editor) = this.project_actions.editor.as_mut() {
                                        editor.confirm_delete = false;
                                    }
                                    cx.notify();
                                })),
                        )
                        .child(
                            popover::btn_danger(&theme, "Delete")
                                .id("action-delete-confirm")
                                .on_click(
                                    cx.listener(|this, _, _, cx| this.delete_project_action(cx)),
                                ),
                        ),
                )
                .into_any_element();
            return Some(popover::modal("project-action-delete", viewport, card));
        }

        let name = editor.name.clone();
        let command = editor.command.clone();
        let selected_icon = editor.icon;
        let setup = editor.run_on_worktree_create;
        let editing = editor.action_id.is_some();
        let saving = editor.saving;
        let error = editor.error.clone();
        let title = if editing { "Edit action" } else { "Add action" };
        let mut card = popover::dialog_card(&theme)
            .w(px(440.0))
            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| {
                if event.keystroke.key == "escape" {
                    this.project_actions.editor = None;
                    cx.notify();
                }
            }))
            .child(popover::dialog_title(&theme, title))
            .child(action_field_label(&theme, "Name"))
            .child(popover::dialog_field(name.into_any_element()))
            .child(action_field_label(&theme, "Command"))
            .child(
                popover::dialog_field(
                    div()
                        .h(px(88.0))
                        .overflow_hidden()
                        .child(command)
                        .into_any_element(),
                )
                .font_family(theme.font_mono.clone()),
            )
            .child(action_field_label(&theme, "Icon"));
        let mut icon_row = div().flex().flex_row().gap(px(6.0));
        for (kind, label) in ACTION_ICONS {
            icon_row = icon_row.child(
                div()
                    .id(SharedString::from(format!("action-icon-{label}")))
                    .size(px(34.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(7.0))
                    .border_1()
                    .border_color(if kind == selected_icon {
                        theme.text_muted
                    } else {
                        theme.border
                    })
                    .bg(if kind == selected_icon {
                        crate::theme::ink(0.10)
                    } else {
                        crate::theme::ink(0.03)
                    })
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(editor) = this.project_actions.editor.as_mut() {
                            editor.icon = kind;
                        }
                        cx.notify();
                    }))
                    .child(
                        icon(action_icon(kind))
                            .size(px(16.0))
                            .text_color(theme.text_muted),
                    ),
            );
        }
        card = card
            .child(icon_row)
            .child(
                div()
                    .id("action-setup-toggle")
                    .mt(px(14.0))
                    .flex()
                    .items_center()
                    .gap(px(9.0))
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _, _, cx| {
                        if let Some(editor) = this.project_actions.editor.as_mut() {
                            editor.run_on_worktree_create = !editor.run_on_worktree_create;
                        }
                        cx.notify();
                    }))
                    .child(
                        div()
                            .size(px(16.0))
                            .rounded(px(4.0))
                            .border_1()
                            .border_color(if setup { theme.text } else { theme.border })
                            .bg(if setup {
                                theme.text
                            } else {
                                crate::theme::ink(0.03)
                            })
                            .when(setup, |el| {
                                el.child(
                                    icon(icons::CHECK).size(px(12.0)).text_color(theme.on_solid),
                                )
                            }),
                    )
                    .child(SharedString::from("Run automatically on worktree creation")),
            )
            .when_some(error, |card, error| {
                card.child(
                    div()
                        .mt(px(10.0))
                        .text_size(px(12.0))
                        .text_color(theme.danger)
                        .child(SharedString::from(error)),
                )
            })
            .child(
                div()
                    .mt(px(18.0))
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(div().when(editing, |el| {
                        el.child(
                            popover::btn_ghost(&theme, "Delete action", "action-delete")
                                .id("action-delete")
                                .text_color(theme.danger)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    if let Some(editor) = this.project_actions.editor.as_mut() {
                                        editor.confirm_delete = true;
                                    }
                                    cx.notify();
                                })),
                        )
                    }))
                    .child(
                        div()
                            .flex()
                            .gap(px(8.0))
                            .child(
                                popover::btn_ghost(&theme, "Cancel", "action-cancel")
                                    .id("action-cancel")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.project_actions.editor = None;
                                        cx.notify();
                                    })),
                            )
                            .child(
                                popover::btn_primary(
                                    &theme,
                                    if saving { "Saving…" } else { "Save action" },
                                )
                                .id("action-save")
                                .when(!saving, |button| {
                                    button.on_click(
                                        cx.listener(|this, _, _, cx| this.save_project_action(cx)),
                                    )
                                }),
                            ),
                    ),
            );
        Some(popover::modal(
            "project-action-editor",
            viewport,
            card.into_any_element(),
        ))
    }
}

fn project_action_params(
    mut params: serde_json::Value,
    target_device_id: &Option<String>,
) -> serde_json::Value {
    if let (Some(target), Some(object)) = (target_device_id, params.as_object_mut()) {
        object.insert(
            "targetDeviceId".into(),
            serde_json::Value::String(target.clone()),
        );
    }
    params
}

const ACTION_CONTROL_HEIGHT: f32 = 24.0;
const ACTION_CONTROL_RADIUS: f32 = 7.0;

/// The pill's fill (or its hover): the composer's own material and edge, so
/// the two read as one family. Hover is a faint wash over that dark fill —
/// the titlebar's hover role reads far too bright on it.
fn action_fill(theme: &Theme, hover: bool) -> gpui::Hsla {
    if hover {
        theme.wash(0.04)
    } else {
        theme.composer_surface_bg()
    }
}

fn action_segment(theme: &Theme, id: &'static str, enabled: bool) -> gpui::Stateful<gpui::Div> {
    // Hover tints on top of the fill, so both halves stay one piece.
    let hover = action_fill(theme, true);
    div()
        .id(id)
        .relative()
        .h_full()
        .px(px(8.0))
        .flex()
        .items_center()
        .gap(px(5.0))
        .text_size(px(11.5))
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_color(theme.text)
        .when(enabled, |el| el.hover(move |style| style.bg(hover)))
        .on_mouse_down(MouseButton::Left, |_, window, _| window.prevent_default())
}

/// The split's seam: an inset hairline, not a full-height border, so the
/// pill still reads as one solid piece.
fn action_divider(theme: &Theme) -> gpui::Div {
    div()
        .relative()
        .flex_none()
        .w(px(1.0))
        .h(px(ACTION_CONTROL_HEIGHT - 10.0))
        .bg(theme.text.opacity(0.14))
}

fn action_chevron(
    theme: &Theme,
    enabled: bool,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> gpui::Stateful<gpui::Div> {
    let hover = action_fill(theme, true);
    div()
        .id("project-actions-chevron")
        .h_full()
        .relative()
        .flex_none()
        .w(px(22.0))
        .rounded_r(px(ACTION_CONTROL_RADIUS))
        .flex()
        .items_center()
        .justify_center()
        .when(!enabled, |el| el.opacity(0.45))
        .on_mouse_down(MouseButton::Left, |_, window, _| window.prevent_default())
        .when(enabled, |el| {
            el.cursor_pointer()
                .hover(move |style| style.bg(hover))
                .on_click(move |event, window, cx| {
                    cx.stop_propagation();
                    on_click(event, window, cx)
                })
                .tooltip(crate::settings::widgets::text_tooltip("Project actions"))
        })
        .child(
            icon(icons::ALT_ARROW_DOWN)
                .size(px(11.0))
                .text_color(theme.text_muted),
        )
}

fn action_field_label(theme: &Theme, label: &str) -> gpui::Div {
    div()
        .mt(px(12.0))
        .mb(px(5.0))
        .text_size(px(12.0))
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_color(theme.text_muted)
        .child(SharedString::from(label.to_string()))
}

#[cfg(test)]
mod project_actions_scroll_tests {
    use super::*;
    use gpui::{ScrollHandle, TestAppContext, point};

    struct MenuTestView {
        scroll: ScrollHandle,
        background: ScrollHandle,
        count: usize,
        added: bool,
    }

    impl Render for MenuTestView {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let theme = Theme::of(cx);
            let mut menu =
                project_actions_menu_surface(theme, window.viewport_size().height, &self.scroll)
                    .child(popover::menu_heading(theme, "Project actions"));
            for index in 0..self.count {
                let id = SharedString::from(format!("action-{index}"));
                menu = menu.child(
                    popover::menu_row(theme, false, id.clone())
                        .id(id)
                        .child(format!("Action {index}"))
                        .child(div().size(px(22.0))),
                );
            }
            menu = menu
                .child(popover::menu_separator())
                .child(popover::menu_heading(theme, "Import from zeron.json"))
                .child(
                    popover::menu_row(theme, false, "import")
                        .id("import")
                        .child("Import action"),
                )
                .child(popover::menu_separator())
                .child(
                    popover::menu_row(theme, false, "add")
                        .id("add")
                        .debug_selector(|| "add-action".into())
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.added = true;
                            cx.notify();
                        }))
                        .child("Add action"),
                );
            div()
                .size_full()
                .child(
                    div()
                        .id("background")
                        .absolute()
                        .inset_0()
                        .overflow_y_scroll()
                        .track_scroll(&self.background)
                        .child(div().h(px(3000.0))),
                )
                .child(
                    div()
                        .absolute()
                        .top(px(Theme::TITLEBAR_HEIGHT + 6.0))
                        .child(menu),
                )
        }
    }

    #[gpui::test]
    fn fifty_actions_scroll_to_add_without_leaving_the_window(cx: &mut TestAppContext) {
        cx.update(|cx| cx.set_global(Theme::default()));
        let (view, cx) = cx.add_window_view(|_, _| MenuTestView {
            scroll: ScrollHandle::new(),
            background: ScrollHandle::new(),
            count: 50,
            added: false,
        });
        let (scroll, background) =
            view.read_with(cx, |view, _| (view.scroll.clone(), view.background.clone()));
        for height in [760.0, 300.0, 200.0] {
            cx.simulate_resize(gpui::size(px(1100.0), px(height)));
            cx.run_until_parked();
            assert!(scroll.bounds().bottom() <= px(height - 8.0));
            assert!(scroll.max_offset().y > px(0.0));
            for _ in 0..2 {
                cx.simulate_event(gpui::ScrollWheelEvent {
                    position: scroll.bounds().center(),
                    delta: gpui::ScrollDelta::Pixels(point(px(0.0), px(-10_000.0))),
                    ..Default::default()
                });
                cx.run_until_parked();
            }
            assert_eq!(scroll.offset().y, -scroll.max_offset().y);
            assert_eq!(
                background.offset().y,
                px(0.0),
                "menu wheel must not scroll the background"
            );
            let add = cx.debug_bounds("add-action").unwrap();
            assert!(add.top() >= scroll.bounds().top());
            assert!(add.bottom() <= scroll.bounds().bottom());
            view.update(cx, |view, _| view.added = false);
            cx.simulate_click(add.center(), Default::default());
            cx.run_until_parked();
            assert!(view.read_with(cx, |view, _| view.added));
        }
        view.update(cx, |view, cx| {
            view.count = 1;
            cx.notify();
        });
        cx.simulate_resize(gpui::size(px(1100.0), px(760.0)));
        cx.run_until_parked();
        assert_eq!(scroll.max_offset().y, px(0.0));
        assert_eq!(scroll.offset().y, px(0.0));
        assert!(
            scroll.bounds().size.height < px(300.0),
            "short menus should stay compact"
        );
    }
}
