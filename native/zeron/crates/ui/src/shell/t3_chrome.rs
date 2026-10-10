use super::*;
use crate::browser::T3SettingsRoute;

const PANEL_WIDTH: f32 = 292.0;
const PANEL_GUTTER: f32 = 24.0;
const CHAT_FLOOR: f32 = 420.0;

fn panel_rail(available: f32, open: bool) -> f32 {
    if open && available >= CHAT_FLOOR + PANEL_WIDTH + PANEL_GUTTER {
        PANEL_WIDTH + PANEL_GUTTER
    } else {
        0.0
    }
}

impl Shell {
    fn t3_panel_available(&self, cx: &App) -> f32 {
        (self.viewport_width
            - self.sidebar_now()
            - self.right_visible_width(cx)
            - self.files_reserved_width(cx))
        .max(0.0)
    }

    pub(super) fn t3_panel_visible(&self, cx: &App) -> bool {
        std::env::var_os("ZERON_T3_CONNECTION").is_some()
            && !self.active_chat.is_empty()
            && self
                .t3_panel_open
                .unwrap_or(self.t3_panel_available(cx) >= CHAT_FLOOR + PANEL_WIDTH + PANEL_GUTTER)
    }

    pub(super) fn t3_panel_rail_width(&self, cx: &App) -> f32 {
        panel_rail(self.t3_panel_available(cx), self.t3_panel_visible(cx))
    }

    pub(super) fn toggle_t3_panel(&mut self, cx: &mut Context<Self>) {
        self.t3_panel_open = Some(!self.t3_panel_visible(cx));
        cx.notify();
    }

    pub(super) fn render_t3_panel_toggle(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if std::env::var_os("ZERON_T3_CONNECTION").is_none() || self.active_chat.is_empty() {
            return None;
        }
        let theme = Theme::of(cx).clone();
        Some(
            div()
                .rounded(px(Theme::CONTROL_RADIUS))
                .when(self.t3_panel_visible(cx), |el| el.bg(theme.glass_hover()))
                .child(window_control_button(
                    "toggle-t3-project-panel",
                    icons::GIT_BRANCH,
                    "Project and agents",
                    &theme,
                    cx.listener(|this, _, _, cx| this.toggle_t3_panel(cx)),
                ))
                .into_any_element(),
        )
    }

    pub(super) fn render_t3_floating_panel(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if !self.t3_panel_visible(cx) {
            return None;
        }
        let width = PANEL_WIDTH.min((self.t3_panel_available(cx) - PANEL_GUTTER).max(220.0));
        let right = self.right_visible_width(cx) + self.files_reserved_width(cx) + 12.0;
        let max_height = (self.viewport_height - Theme::TITLEBAR_HEIGHT - 24.0).max(100.0);
        Some(
            div()
                .absolute()
                .top(px(Theme::TITLEBAR_HEIGHT + 8.0))
                .right(px(right))
                .w(px(width))
                .max_h(px(max_height))
                .id("t3-floating-project-panel")
                .role(gpui::Role::Region)
                .aria_label("Project and agents")
                .overflow_y_scroll()
                .occlude()
                .child(self.render_t3_project_panel(cx))
                .into_any_element(),
        )
    }

    pub(super) fn render_t3_footer(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let settings = window_control_button(
            "t3-footer-settings",
            icons::SETTINGS,
            "Appearance and app settings",
            theme,
            cx.listener(|this, _, _, cx| this.open_last_settings(cx)),
        );
        let prs = window_control_button(
            "t3-footer-pull-requests",
            icons::PULL_REQUEST,
            "Pull requests",
            theme,
            cx.listener(|this, _, window, cx| {
                this.open_t3_settings_route(T3SettingsRoute::PullRequests, window, cx)
            }),
        );
        let usage = window_control_button(
            "t3-footer-usage",
            icons::CHART,
            "Usage",
            theme,
            cx.listener(|this, _, window, cx| {
                this.open_t3_settings_route(T3SettingsRoute::Usage, window, cx)
            }),
        );
        let refresh = window_control_button(
            "t3-footer-refresh",
            icons::REFRESH,
            "Check T3 connection",
            theme,
            cx.listener(|this, _, _, cx| this.refresh_t3(cx)),
        );
        div()
            .w_full()
            .flex()
            .flex_col()
            .gap(px(4.0))
            .child(
                div()
                    .id("t3-connect-account")
                    .h(px(30.0))
                    .px(px(8.0))
                    .rounded(px(6.0))
                    .role(gpui::Role::Button)
                    .aria_label("T3 Connect account and devices")
                    .tab_index(0)
                    .cursor_pointer()
                    .hover(|s| s.bg(theme.glass_hover()))
                    .focus_visible(|s| s.bg(theme.glass_hover()))
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .text_size(crate::typography::ui_rems(12.0))
                    .text_color(theme.text_muted)
                    .child(icon(icons::MONITOR).size(px(15.0)))
                    .child("T3 Connect")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.open_t3_settings_route(T3SettingsRoute::Account, window, cx)
                    }))
                    .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, window, cx| {
                        if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                            this.open_t3_settings_route(T3SettingsRoute::Account, window, cx);
                            cx.stop_propagation();
                        }
                    })),
            )
            .child(
                div()
                    .w_full()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .child(settings)
                    .child(prs)
                    .child(usage)
                    .child(div().flex_1())
                    .child(refresh),
            )
            .into_any_element()
    }

    fn refresh_t3(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let result = crate::attachments::call_with_timeout(
                &engine,
                cx.background_executor(),
                "T3CheckConnection",
                serde_json::json!({}),
                Duration::from_secs(30),
            )
            .await;
            this.update(cx, |this, cx| {
                this.sidebar_notice = result
                    .err()
                    .map(|error| format!("T3 connection check failed: {error}").into());
                cx.notify();
            })
            .ok();
        })
        .detach();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn t3_floating_panel_preserves_chat_space_and_yields_on_narrow_windows() {
        assert_eq!(panel_rail(900.0, false), 0.0);
        assert_eq!(panel_rail(900.0, true), 316.0);
        assert_eq!(panel_rail(600.0, true), 0.0);
        assert!(900.0 - panel_rail(900.0, true) >= CHAT_FLOOR);
    }
}
