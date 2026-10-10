use super::*;
use crate::{icons, popover};

const MODES: [(&str, &str, &str); 4] = [
    (
        "approval-required",
        "Supervised",
        "Ask before commands and file changes.",
    ),
    (
        "auto-accept-edits",
        "Auto-accept edits",
        "Approve edits; ask before other actions.",
    ),
    (
        "auto",
        "Auto",
        "Supported providers approve routine actions.",
    ),
    (
        "full-access",
        "Full access",
        "Allow commands and edits without prompts.",
    ),
];

pub(super) struct T3Permissions {
    menu: popover::Popup<usize>,
    new_mode: String,
    busy: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[gpui::test]
    fn permissions_follow_the_selected_t3_thread_and_close_on_navigation(
        cx: &mut gpui::TestAppContext,
    ) {
        let (_dir, handle) = super::super::tests::composer_focus_window(cx);
        handle.update(cx, |composer, _, cx| {
            assert_eq!(composer.t3_runtime_mode(cx), "approval-required");
            composer.set_t3_runtime_mode("auto-accept-edits", cx);
            assert_eq!(composer.t3_runtime_mode(cx), "auto-accept-edits");
            composer.state.update(cx, |state, _| {
                state.selected_chat = Some("existing".into());
                state.t3_details.insert("existing".into(), serde_json::from_value(serde_json::json!({
                    "model":"gpt-6.1-sol", "runtimeMode":"full-access", "activeAgents":0, "totalAgents":0
                })).unwrap());
            });
            assert_eq!(composer.t3_runtime_mode(cx), "full-access");
            composer.t3_permissions.menu.open(3);
            composer.on_state_changed(cx);
            assert!(!composer.t3_permissions.menu.is_open());
            composer.state.update(cx, |state, _| state.selected_chat = Some("loading".into()));
            assert_eq!(composer.t3_runtime_mode(cx), "approval-required");
            composer.state.update(cx, |state, _| state.selected_chat = None);
            assert_eq!(composer.t3_runtime_mode(cx), "auto-accept-edits");
        }).unwrap();
    }
}

impl Default for T3Permissions {
    fn default() -> Self {
        Self {
            menu: Default::default(),
            new_mode: "approval-required".into(),
            busy: None,
        }
    }
}

impl T3Permissions {
    pub(super) fn close_menu(&mut self) {
        self.menu.finish_close();
    }
}

impl Composer {
    pub(super) fn is_t3(&self, cx: &App) -> bool {
        self.state
            .read(cx)
            .engine()
            .is_some_and(|e| e.engine_info().supports(zeron_t3::CAPABILITY))
    }

    pub(super) fn t3_runtime_mode<'a>(&'a self, cx: &'a App) -> &'a str {
        let state = self.state.read(cx);
        match state.selected_chat.as_ref() {
            Some(id) => state
                .t3_details
                .get(id)
                .map(|details| details.runtime_mode.as_str())
                .unwrap_or("approval-required"),
            None => &self.t3_permissions.new_mode,
        }
    }

    fn set_t3_runtime_mode(&mut self, mode: &'static str, cx: &mut Context<Self>) {
        if self.t3_permissions.busy.is_some() {
            return;
        }
        self.t3_permissions.menu.finish_close();
        let Some(chat_id) = self.state.read(cx).selected_chat.clone() else {
            self.t3_permissions.new_mode = mode.into();
            cx.notify();
            return;
        };
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.t3_permissions.busy = Some(chat_id.clone());
        cx.spawn(async move |this, cx| {
            let result = attachments::call_with_timeout(
                &engine,
                cx.background_executor(),
                methods::MUTATE,
                serde_json::json!({"op":"setT3RuntimeMode", "chatId":chat_id, "runtimeMode":mode}),
                Duration::from_secs(30),
            )
            .await;
            this.update(cx, |this, cx| {
                if this.t3_permissions.busy.as_ref() == Some(&chat_id) {
                    this.t3_permissions.busy = None;
                }
                if let Err(error) = result {
                    this.failure = Some(format!("Permissions could not change: {error}"));
                    this.failure_key = Some(chat_id.clone());
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    pub(super) fn render_t3_permissions(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = Theme::of(cx).clone();
        let mode = self.t3_runtime_mode(cx).to_owned();
        let selected = MODES.iter().position(|(id, _, _)| *id == mode).unwrap_or(0);
        let busy = self.t3_permissions.busy.is_some();
        let open = self.t3_permissions.menu.is_open();
        let trigger = div()
            .id("t3-chat-permissions")
            .relative()
            .h(px(30.0))
            .px(px(10.0))
            .flex()
            .items_center()
            .gap(px(6.0))
            .rounded(px(6.0))
            .role(Role::Button)
            .aria_label(format!("Permissions: {}", MODES[selected].1))
            .aria_expanded(open)
            .tab_index(0)
            .cursor_pointer()
            .text_size(crate::typography::ui_rems(12.0))
            .text_color(theme.text_muted)
            .hover(|s| s.bg(theme.glass_hover()))
            .focus_visible(|s| s.bg(theme.glass_hover()).text_color(theme.text))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, _| this.t3_permissions.menu.note_trigger_press()),
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                if busy {
                    return;
                }
                if this.t3_permissions.menu.take_press_was_open() {
                    this.t3_permissions.menu.finish_close();
                } else {
                    this.t3_permissions.menu.open(selected);
                }
                cx.notify();
            }))
            .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
                match event.keystroke.key.as_str() {
                    "escape" => this.t3_permissions.menu.finish_close(),
                    "enter" | "space" if !busy => {
                        if let Some(index) = this.t3_permissions.menu.as_open().copied() {
                            this.set_t3_runtime_mode(MODES[index].0, cx);
                        } else {
                            this.t3_permissions.menu.open(selected);
                        }
                    }
                    "up" | "down" if !busy => {
                        let index = this
                            .t3_permissions
                            .menu
                            .as_open()
                            .copied()
                            .unwrap_or(selected);
                        this.t3_permissions
                            .menu
                            .open(if event.keystroke.key == "up" {
                                (index + 3) % 4
                            } else {
                                (index + 1) % 4
                            });
                    }
                    _ => return,
                }
                cx.stop_propagation();
                cx.notify();
            }))
            .child(icons::icon(icons::LOCK).size(px(14.0)))
            .child(if busy {
                "Updating…"
            } else {
                MODES[selected].1
            })
            .child(icons::icon(icons::ALT_ARROW_DOWN).size(px(12.0)));
        let trigger = if open {
            let active = self.t3_permissions.menu.as_open().copied();
            let rows = MODES
                .into_iter()
                .enumerate()
                .map(|(index, (mode, label, description))| {
                    div()
                        .id(SharedString::from(format!("t3-permission-{mode}")))
                        .p(px(10.0))
                        .w_full()
                        .rounded(px(8.0))
                        .cursor_pointer()
                        .role(Role::Button)
                        .aria_label(label)
                        .tab_index(0)
                        .flex()
                        .flex_col()
                        .gap(px(3.0))
                        .when(active == Some(index), |s| s.bg(theme.glass_hover()))
                        .hover(|s| s.bg(theme.glass_hover()))
                        .focus_visible(|s| s.bg(theme.glass_hover()))
                        .on_click(
                            cx.listener(move |this, _, _, cx| this.set_t3_runtime_mode(mode, cx)),
                        )
                        .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
                            if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                                this.set_t3_runtime_mode(mode, cx);
                                cx.stop_propagation();
                            }
                        }))
                        .child(
                            div()
                                .text_size(crate::typography::ui_rems(13.0))
                                .text_color(theme.text)
                                .child(label)
                                .children(
                                    (index == selected).then(|| {
                                        icons::icon(icons::CHECK).size(px(13.0)).ml(px(8.0))
                                    }),
                                ),
                        )
                        .child(
                            div()
                                .text_size(crate::typography::ui_rems(11.0))
                                .text_color(theme.text_muted)
                                .child(description),
                        )
                })
                .collect::<Vec<_>>();
            let menu = popover::popover_card(&theme.for_popup())
                .w(px(300.0))
                .children(rows)
                .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                    this.t3_permissions.menu.finish_close();
                    cx.notify();
                }));
            trigger.child(popover::anchored_menu_above(
                "t3-permission-menu",
                menu.into_any_element(),
                None,
            ))
        } else {
            trigger
        };
        trigger.into_any_element()
    }
}
