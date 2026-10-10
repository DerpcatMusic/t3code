//! T3's lifecycle shelves, in Zeron's native sidebar cloth. History is paged
//! before row elements are created; a routed history row survives both paging
//! and collapse. The footer remains owned by the shell.

use super::*;
use spaces::ActiveChatRow;
use std::collections::HashMap;
use zeron_t3::SidebarSection as LifecycleSection;

pub(super) const SETTLED_PAGE_SIZE: usize = 25;
pub(super) const SETTLED_COLLAPSE_KEY: &str = "t3:settled";
pub(super) const SNOOZED_COLLAPSE_KEY: &str = "t3:snoozed";

/// Keep the newest page in order, plus only the selected deep-history row.
/// Never expand through the selected index: a deep link must not mount history.
fn shelf_indices<T>(
    rows: &[T],
    shown: usize,
    expanded: bool,
    selected: Option<&str>,
    id: impl Fn(&T) -> &str,
) -> Vec<usize> {
    let end = if expanded { shown.min(rows.len()) } else { 0 };
    let mut visible: Vec<_> = (0..end).collect();
    if let Some(selected) = selected
        && let Some(index) = rows.iter().position(|row| id(row) == selected)
        && index >= end
    {
        visible.push(index);
    }
    visible
}

fn settled_budget(
    shown: usize,
    previous_filter: &Option<String>,
    filter: &Option<String>,
) -> usize {
    if previous_filter != filter {
        SETTLED_PAGE_SIZE
    } else {
        shown.max(SETTLED_PAGE_SIZE)
    }
}

struct T3ActiveGroup {
    key: String,
    label: String,
    custom: Option<SidebarSection>,
    header_chat: Option<String>,
    rows: Vec<ActiveChatRow>,
    count: usize,
    open: bool,
}

struct T3SidebarRows {
    pinned: Vec<ActiveChatRow>,
    pinned_ids: std::sync::Arc<Vec<String>>,
    pinned_count: usize,
    groups: Vec<T3ActiveGroup>,
    snoozed: Vec<ActiveChatRow>,
    snoozed_count: usize,
    settled: Vec<ActiveChatRow>,
    settled_count: usize,
    hidden_settled: usize,
}

impl T3SidebarRows {
    fn visible_order(&self) -> Vec<String> {
        self.pinned
            .iter()
            .chain(self.groups.iter().flat_map(|group| group.rows.iter()))
            .chain(self.snoozed.iter())
            .chain(self.settled.iter())
            .map(|row| row.chat.id.clone())
            .collect()
    }
}

impl Shell {
    /// Keyboard cycling and jump hints must use the same paged/collapsed roster.
    pub(super) fn t3_sidebar_visible_order(&self, cx: &App) -> Vec<String> {
        self.t3_sidebar_rows(cx).visible_order()
    }

    fn t3_sidebar_rows(&self, cx: &App) -> T3SidebarRows {
        let now = Utc::now();
        let state = self.state.read(cx);
        let mut chats = state.sidebar_chats(now, self.settings.space_filter.as_deref());
        // Shelf order is independent of active-list preferences. Working
        // threads stay in Active; runtime progress never promotes a row.
        chats.sort_by(|(_, left), (_, right)| {
            let left_thread = state.t3_sidebar.get(&left.id);
            let right_thread = state.t3_sidebar.get(&right.id);
            let shelf = |thread: Option<&zeron_t3::SidebarThread>| match thread
                .map(|thread| thread.section(now))
            {
                Some(LifecycleSection::Settled) => 2,
                Some(LifecycleSection::Snoozed) => 1,
                _ => 0,
            };
            shelf(left_thread)
                .cmp(&shelf(right_thread))
                .then_with(|| match (left_thread, right_thread) {
                    (Some(left_thread), Some(right_thread)) if shelf(Some(left_thread)) > 0 => {
                        left_thread.compare(right_thread, now)
                    }
                    (Some(left_thread), Some(right_thread))
                        if self.settings.sidebar_sort == SidebarSort::LastUpdated =>
                    {
                        left_thread.compare_active(right_thread)
                    }
                    _ => spaces::compare_sidebar_chats(self.settings.sidebar_sort, left, right),
                })
                .then_with(|| left.id.cmp(&right.id))
        });
        let saved_pins = self.active_sidebar_pins(cx);
        let filtered_ids: std::collections::HashSet<_> =
            chats.iter().map(|(_, chat)| chat.id.as_str()).collect();
        let pinned_ids = std::sync::Arc::new(
            saved_pins
                .iter()
                .filter(|id| filtered_ids.contains(id.as_str()))
                .cloned()
                .collect::<Vec<_>>(),
        );
        let pin_rank: HashMap<_, _> = pinned_ids
            .iter()
            .enumerate()
            .map(|(index, id)| (id.as_str(), index))
            .collect();
        let (mut pinned, mut active, mut snoozed, mut settled) =
            (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        for (status, chat) in chats {
            let section = state
                .t3_sidebar
                .get(&chat.id)
                .map_or(LifecycleSection::Active, |thread| thread.section(now));
            match section {
                LifecycleSection::Settled => settled.push((status, chat)),
                LifecycleSection::Snoozed => snoozed.push((status, chat)),
                _ if pin_rank.contains_key(chat.id.as_str()) => pinned.push((status, chat)),
                _ => active.push((status, chat)),
            }
        }
        pinned.sort_by_key(|(_, chat)| {
            pin_rank
                .get(chat.id.as_str())
                .copied()
                .unwrap_or(usize::MAX)
        });
        let pinned_count = pinned.len();
        let pinned = if self.pinned_open {
            pinned
                .into_iter()
                .map(|(status, chat)| self.sidebar_chat_data(status, chat.clone(), state))
                .collect()
        } else {
            Vec::new()
        };
        let custom_sections = self.active_sidebar_sections(cx);
        let mut groups: Vec<T3ActiveGroup> = custom_sections
            .into_iter()
            .map(|section| T3ActiveGroup {
                key: format!("section:{}", section.id),
                label: section.name.clone(),
                open: !section.collapsed,
                custom: Some(section),
                header_chat: None,
                rows: Vec::new(),
                count: 0,
            })
            .collect();
        for (status, chat) in active {
            let row = self.sidebar_chat_data(status, chat.clone(), state);
            let custom_index = groups.iter().position(|group| {
                group
                    .custom
                    .as_ref()
                    .is_some_and(|section| section.session_ids.contains(&row.chat.id))
            });
            let index = if let Some(index) = custom_index {
                index
            } else {
                // Active/Working remain above history, even in status-group mode.
                let (key, label) =
                    if self.settings.sidebar_organization == SidebarOrganization::ByStatus {
                        ("status:0".to_owned(), "Active".to_owned())
                    } else {
                        row.group
                            .clone()
                            .unwrap_or_else(|| ("regular".into(), "Active".into()))
                    };
                if let Some(index) = groups.iter().position(|group| group.key == key) {
                    index
                } else {
                    let open =
                        if self.settings.sidebar_organization == SidebarOrganization::InOneList {
                            self.sessions_open
                        } else {
                            !self
                                .sidebar_collapsed_groups
                                .contains(&self.sidebar_group_collapse_key(&key))
                        };
                    groups.push(T3ActiveGroup {
                        key,
                        label,
                        custom: None,
                        header_chat: None,
                        rows: Vec::new(),
                        count: 0,
                        open,
                    });
                    groups.len() - 1
                }
            };
            let group = &mut groups[index];
            if group.header_chat.is_none() {
                group.header_chat = Some(row.chat.id.clone());
            }
            group.count += 1;
            if group.open {
                group.rows.push(row);
            }
        }
        // Preserve the user's device-group convention: this device first.
        if self.settings.sidebar_organization == SidebarOrganization::ByDevice
            && let Some(local) = state.local_device_id.as_deref()
            && let Some(index) = groups
                .iter()
                .position(|group| group.custom.is_none() && group.key == local)
        {
            let group = groups.remove(index);
            let insertion = groups
                .iter()
                .take_while(|group| group.custom.is_some())
                .count();
            groups.insert(insertion, group);
        }
        let selected = state.selected_chat.as_deref();
        let shown = settled_budget(
            self.t3_settled_shown,
            &self.t3_settled_filter,
            &self.settings.space_filter,
        );
        let settled_count = settled.len();
        let settled_indices = shelf_indices(
            &settled,
            shown,
            !self.sidebar_collapsed_groups.contains(SETTLED_COLLAPSE_KEY),
            selected,
            |(_, chat)| chat.id.as_str(),
        );
        let hidden_settled = settled_count.saturating_sub(settled_indices.len());
        let settled = settled_indices
            .into_iter()
            .map(|index| {
                let (status, chat) = settled[index];
                self.sidebar_chat_data(status, chat.clone(), state)
            })
            .collect();
        let snoozed_count = snoozed.len();
        let snoozed = shelf_indices(
            &snoozed,
            snoozed_count,
            !self.sidebar_collapsed_groups.contains(SNOOZED_COLLAPSE_KEY),
            selected,
            |(_, chat)| chat.id.as_str(),
        )
        .into_iter()
        .map(|index| {
            let (status, chat) = snoozed[index];
            self.sidebar_chat_data(status, chat.clone(), state)
        })
        .collect();
        T3SidebarRows {
            pinned,
            pinned_ids,
            pinned_count,
            groups,
            snoozed,
            snoozed_count,
            settled,
            settled_count,
            hidden_settled,
        }
    }

    fn render_t3_sidebar_row(
        &self,
        row: ActiveChatRow,
        pinned_ids: &std::sync::Arc<Vec<String>>,
        project_icon: bool,
        jump_slot: Option<usize>,
        moving: &mut Option<(AnyElement, f32)>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> SidebarKeyedRow {
        let now = Utc::now();
        let state = self.state.read(cx);
        let thread = state.t3_sidebar.get(&row.chat.id);
        let time: SharedString = thread
            .and_then(|thread| thread.snooze_label(now))
            .unwrap_or_else(|| {
                let timestamp = thread.filter(|thread| thread.is_settled()).map_or_else(
                    || row.chat.last_message_at.unwrap_or(row.chat.created_at),
                    zeron_t3::SidebarThread::settled_timestamp,
                );
                format_time_ago(timestamp, now)
            })
            .into();
        let selected = state.selected_chat.as_deref() == Some(row.chat.id.as_str());
        let agents = state
            .t3_details
            .get(&row.chat.id)
            .filter(|details| details.total_agents > 0)
            .map(|details| (details.active_agents, details.total_agents));
        let id = row.chat.id.clone();
        let height = sidebar_row_height(
            self.settings.sidebar_compact,
            self.settings.sidebar_show_project_label,
            row.branch.is_some(),
            row.change_request.is_some(),
        ) + if agents.is_some() { 20.0 } else { 0.0 };
        let jump_label = if self.jump_hints && !self.overlay_owns_keyboard(cx) {
            jump_slot
                .filter(|slot| *slot < JUMP_SLOTS)
                .and_then(|slot| {
                    let combo = self.settings.keymap.get(ShortcutId::JumpSession(slot));
                    (!combo.is_empty()).then(|| badge_combo(combo).into())
                })
        } else {
            None
        };
        let status =
            if thread.is_some_and(|thread| thread.section(now) == LifecycleSection::Settled) {
                // History's corner says when work ended, not whether a completed
                // run was visited. The same renderer still owns hover/actions.
                zeron_proto::ChatIndicator::Idle
            } else {
                row.status
            };
        let draggable = thread.is_some_and(|thread| {
            thread.capabilities.thread_pinning && thread.capabilities.thread_pin_reorder
        });
        let drag = draggable
            .then(|| self.active_sidebar_pin_profile_key(cx))
            .flatten()
            .map(|profile_key| SidebarSessionDrag {
                chat_id: id.clone(),
                visible_ids: pinned_ids.clone(),
                filter: self.settings.space_filter.clone(),
                profile_key,
            });
        let harness = self
            .settings
            .sidebar_show_harness
            .then(|| row.chat.config.as_ref().map(|config| config.harness))
            .flatten();
        let element = self.render_chat_row(
            id.clone(),
            transcript::single_line(&row.chat.title.unwrap_or_else(|| "New session".into())).into(),
            time,
            row.folder.into(),
            row.branch.map(Into::into),
            row.change_request,
            harness,
            status,
            selected,
            false,
            false,
            drag,
            jump_label,
            project_icon && self.settings.sidebar_show_project_icon,
            None,
            theme,
            cx,
        );
        // Details are already cached by the normal thread watch. No sidebar
        // render fetches details, launches agents, or manufactures a count.
        let element = div()
            .w_full()
            .flex_none()
            .flex()
            .flex_col()
            .child(element)
            .when_some(agents, |el, (active, total)| {
                el.child(
                    div()
                        .h(px(20.0))
                        .px(px(Theme::SPACE_SM))
                        .flex()
                        .items_center()
                        .child(
                            div()
                                .h(px(16.0))
                                .px(px(4.0))
                                .rounded(px(4.0))
                                .bg(theme.text_muted.opacity(0.08))
                                .text_size(crate::typography::ui_rems(10.0))
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .text_color(theme.text_muted)
                                .aria_label(format!("{total} agents, {active} active"))
                                .tooltip(crate::settings::widgets::text_tooltip_above(format!(
                                    "{total} agents, {active} active"
                                )))
                                .child(SharedString::from(if active > 0 {
                                    format!("Agents {active}/{total}")
                                } else {
                                    format!("Agents {total}")
                                })),
                        ),
                )
            })
            .into_any_element();
        let transfer = self.sidebar_session_transfer.as_ref().or_else(|| {
            self.sidebar_session_return
                .as_ref()
                .map(|returning| &returning.transfer)
        });
        if transfer.is_some_and(|transfer| transfer.payload.chat_id == id) {
            *moving = Some((element, height));
            (
                id,
                height,
                div().h(px(height)).flex_none().into_any_element(),
            )
        } else {
            (id, height, element)
        }
    }

    fn render_t3_group_header(
        &mut self,
        key: String,
        label: String,
        count: usize,
        open: bool,
        project: Option<String>,
        target: Option<&'static str>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let visible_label = if open {
            label.clone()
        } else {
            format!("{label} ({count})")
        };
        let chevron = self.sidebar_disclosure_chevron(&key, open, theme);
        let project = project
            .map(|id| self.render_project_group_icon(&id, SIDEBAR_ACTIVE_HARNESS_ICON_SIZE, cx));
        let toggle_key = key.clone();
        let keyboard_key = key.clone();
        let dragging = self.sidebar_session_transfer.is_some();
        spaces::sidebar_disclosure_header(theme, project, visible_label.into(), None, chevron)
            .id(SharedString::from(format!("t3-header-{key}")))
            .debug_selector(move || format!("t3-header-{key}"))
            .rounded(px(Theme::CONTROL_RADIUS))
            .hover(|el| el.bg(theme.glass_hover()))
            .focus_visible(|el| el.bg(crate::theme::card_selected_bg()))
            .tab_index(0)
            .role(gpui::Role::Button)
            .aria_label(format!(
                "{label}, {count} threads, {}",
                if open { "expanded" } else { "collapsed" }
            ))
            .when(dragging && target.is_some(), |el| {
                el.bg(theme.text_muted.opacity(0.08))
            })
            .on_click(cx.listener(move |this, _, _, cx| {
                this.toggle_t3_sidebar_group(&toggle_key, cx);
            }))
            .on_key_down(cx.listener(move |this, event: &gpui::KeyDownEvent, _, cx| {
                if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                    cx.stop_propagation();
                    this.toggle_t3_sidebar_group(&keyboard_key, cx);
                }
            }))
            .when_some(target, |el, target| {
                el.on_drop(
                    cx.listener(move |this, payload: &SidebarSessionDrag, _, cx| {
                        cx.stop_propagation();
                        this.finish_t3_status_transfer(payload, target, cx);
                    }),
                )
            })
            .into_any_element()
    }

    fn toggle_t3_sidebar_group(&mut self, key: &str, cx: &mut Context<Self>) {
        self.cancel_sidebar_session_transfer(cx);
        if key == "sessions" {
            self.sessions_open = !self.sessions_open;
        } else if key == "pinned" {
            self.pinned_open = !self.pinned_open;
        } else if !self.sidebar_collapsed_groups.remove(key) {
            self.sidebar_collapsed_groups.insert(key.to_owned());
        }
        // Selected history remains present outside a collapsed disclosure;
        // no full-history body is mounted just to run its closing animation.
        self.sidebar_disclosure_motion.remove(key);
        self.sidebar_prev_order.clear();
        self.sidebar_resort.clear();
        cx.notify();
    }

    pub(super) fn render_t3_sidebar(
        &mut self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        self.t3_settled_shown = settled_budget(
            self.t3_settled_shown,
            &self.t3_settled_filter,
            &self.settings.space_filter,
        );
        self.t3_settled_filter = self.settings.space_filter.clone();
        if self.sidebar_session_transfer.as_ref().is_some_and(|drag| {
            !cx.has_active_drag() || !self.sidebar_session_transfer_is_valid(&drag.payload, cx)
        }) {
            self.cancel_sidebar_session_transfer(cx);
        }
        if self.pinned_session_drag.is_some()
            && (!cx.has_active_drag() || !self.pinned_session_drag_is_valid(cx))
        {
            self.cancel_pinned_session_drag(cx);
        }
        let roster = self.t3_sidebar_rows(cx);
        let slots: HashMap<_, _> = roster
            .visible_order()
            .into_iter()
            .enumerate()
            .map(|(index, id)| (id, index))
            .collect();
        let T3SidebarRows {
            pinned,
            pinned_ids,
            pinned_count,
            groups,
            snoozed,
            snoozed_count,
            settled,
            settled_count,
            hidden_settled,
        } = roster;
        let mut moving = None;
        let mut top = div()
            .w_full()
            .flex_none()
            .flex()
            .flex_col()
            .gap(px(SIDEBAR_LIST_GAP));
        if pinned_count > 0 || self.sidebar_session_transfer.is_some() {
            let rows: Vec<_> = pinned
                .into_iter()
                .map(|row| {
                    let slot = slots.get(&row.chat.id).copied();
                    self.render_t3_sidebar_row(row, &pinned_ids, true, slot, &mut moving, theme, cx)
                })
                .collect();
            self.sidebar_pinned_heights = rows.iter().map(|(_, height, _)| *height).collect();
            let height = spaces::SIDEBAR_DISCLOSURE_BODY_INSET
                + rows.iter().map(|(_, height, _)| height).sum::<f32>()
                + SIDEBAR_LIST_GAP * rows.len().saturating_sub(1) as f32;
            if self.pinned_open {
                top = top.child(self.render_pinned_section(
                    rows.into_iter().map(|(_, _, element)| element).collect(),
                    height,
                    theme,
                    cx,
                ));
            } else {
                let header = self.render_t3_group_header(
                    "pinned".into(),
                    "Pinned".into(),
                    pinned_count,
                    false,
                    None,
                    None,
                    theme,
                    cx,
                );
                top = top.child(
                    div()
                        .id("sidebar-pinned-section")
                        .child(header)
                        .on_drop::<SidebarSessionDrag>(cx.listener(|this, payload, _, cx| {
                            cx.stop_propagation();
                            this.finish_sidebar_session_transfer(
                                payload,
                                SidebarSessionDrop::Pinned(0),
                                cx,
                            );
                        })),
                );
            }
        }
        let active_count = groups.iter().map(|group| group.count).sum::<usize>();
        for group in groups {
            let project_group = group.custom.is_none()
                && self.settings.sidebar_organization == SidebarOrganization::ByProject;
            let project = (project_group && self.settings.sidebar_show_project_icon)
                .then(|| group.header_chat.clone())
                .flatten();
            let rows: Vec<_> = group
                .rows
                .into_iter()
                .map(|row| {
                    let slot = slots.get(&row.chat.id).copied();
                    self.render_t3_sidebar_row(
                        row,
                        &pinned_ids,
                        !project_group,
                        slot,
                        &mut moving,
                        theme,
                        cx,
                    )
                })
                .collect();
            if let Some(section) = group.custom {
                let (_, _, element) = self.render_custom_sidebar_section(
                    section,
                    rows,
                    format!("regular:{}", group.key),
                    theme,
                    cx,
                );
                top = top.child(element);
            } else {
                let key = if self.settings.sidebar_organization == SidebarOrganization::InOneList {
                    "sessions".to_owned()
                } else {
                    self.sidebar_group_collapse_key(&group.key)
                };
                let header = self.render_t3_group_header(
                    key,
                    group.label,
                    group.count,
                    group.open,
                    project,
                    Some("active"),
                    theme,
                    cx,
                );
                top = top.child(
                    div()
                        .w_full()
                        .flex_none()
                        .flex()
                        .flex_col()
                        .pt(px(SIDEBAR_SECTION_GAP))
                        .child(header)
                        .child(
                            div()
                                .w_full()
                                .flex()
                                .flex_col()
                                .gap(px(SIDEBAR_LIST_GAP))
                                .children(rows.into_iter().map(|(_, _, element)| element))
                                .on_drop(cx.listener(
                                    |this, payload: &SidebarSessionDrag, _, cx| {
                                        cx.stop_propagation();
                                        this.finish_t3_status_transfer(payload, "active", cx);
                                    },
                                )),
                        ),
                );
            }
        }
        // An empty Active region remains an un-settle target, including when
        // history is the only content or the active groups are collapsed.
        top = top.on_drop(cx.listener(|this, payload: &SidebarSessionDrag, _, cx| {
            cx.stop_propagation();
            // Existing pin handlers already accepted or rejected their drop.
            // Do not turn their bubbling release into a second active move.
            if this.sidebar_session_transfer.is_some() {
                this.finish_t3_status_transfer(payload, "active", cx);
            }
        }));
        if active_count + pinned_count == 0 {
            top = top.child(
                div()
                    .id("t3-active-empty")
                    .min_h(px(48.0))
                    .px(px(Theme::SPACE_SM))
                    .py(px(8.0))
                    .text_size(crate::typography::ui_rems(12.0))
                    .text_color(theme.text_muted)
                    .child("No active threads. Start a new thread with +."),
            );
        }
        let mut tail = div()
            .id("t3-sidebar-shelves")
            .mt_auto()
            .w_full()
            .flex_none()
            .flex()
            .flex_col()
            .pt(px(SIDEBAR_SECTION_GAP));
        if snoozed_count > 0 {
            let open = !self.sidebar_collapsed_groups.contains(SNOOZED_COLLAPSE_KEY);
            let header = self.render_t3_group_header(
                SNOOZED_COLLAPSE_KEY.into(),
                "Snoozed".into(),
                snoozed_count,
                open,
                None,
                None,
                theme,
                cx,
            );
            let rows = snoozed
                .into_iter()
                .map(|row| {
                    let slot = slots.get(&row.chat.id).copied();
                    self.render_t3_sidebar_row(row, &pinned_ids, true, slot, &mut moving, theme, cx)
                        .2
                })
                .collect::<Vec<_>>();
            tail = tail.child(header).child(
                div()
                    .flex_none()
                    .flex()
                    .flex_col()
                    .gap(px(SIDEBAR_LIST_GAP))
                    .children(rows),
            );
        }
        let settled_open = !self.sidebar_collapsed_groups.contains(SETTLED_COLLAPSE_KEY);
        let header = self.render_t3_group_header(
            SETTLED_COLLAPSE_KEY.into(),
            "Settled".into(),
            settled_count,
            settled_open,
            None,
            Some("settled"),
            theme,
            cx,
        );
        let rows = settled
            .into_iter()
            .map(|row| {
                let slot = slots.get(&row.chat.id).copied();
                self.render_t3_sidebar_row(row, &pinned_ids, true, slot, &mut moving, theme, cx)
                    .2
            })
            .collect::<Vec<_>>();
        let shelf = div()
            .id("t3-settled-shelf")
            .flex_none()
            .flex()
            .flex_col()
            .pt(px(if snoozed_count > 0 {
                SIDEBAR_SECTION_GAP
            } else {
                0.0
            }))
            .child(header)
            .child(
                div()
                    .flex_none()
                    .flex()
                    .flex_col()
                    .gap(px(SIDEBAR_LIST_GAP))
                    .children(rows),
            )
            .when(settled_open && settled_count == 0, |el| {
                el.child(
                    div()
                        .px(px(Theme::SPACE_SM))
                        .py(px(8.0))
                        .text_size(crate::typography::ui_rems(12.0))
                        .text_color(theme.text_muted)
                        .child("Use Settle to finish a thread."),
                )
            })
            .when(settled_open && hidden_settled > 0, |el| {
                el.child(
                    div()
                        .id("t3-settled-more")
                        .h(px(36.0))
                        .flex()
                        .items_center()
                        .gap(px(8.0))
                        .px(px(Theme::SPACE_SM))
                        .rounded(px(Theme::CONTROL_RADIUS))
                        .text_size(crate::typography::ui_rems(12.0))
                        .text_color(theme.text_muted)
                        .cursor_pointer()
                        .role(gpui::Role::Button)
                        .tab_index(0)
                        .aria_label(format!(
                            "Show {} more settled threads",
                            hidden_settled.min(SETTLED_PAGE_SIZE)
                        ))
                        .hover(|el| el.bg(theme.glass_hover()))
                        .focus_visible(|el| el.bg(crate::theme::card_selected_bg()))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.show_more_t3_settled(cx);
                        }))
                        .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| {
                            if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                                cx.stop_propagation();
                                this.show_more_t3_settled(cx);
                            }
                        }))
                        .child(
                            icon(icons::PLUS)
                                .size(px(12.0))
                                .text_color(theme.text_muted),
                        )
                        .child(SharedString::from(format!(
                            "Show {} more",
                            hidden_settled.min(SETTLED_PAGE_SIZE)
                        ))),
                )
            })
            .on_drop(cx.listener(|this, payload: &SidebarSessionDrag, _, cx| {
                cx.stop_propagation();
                this.finish_t3_status_transfer(payload, "settled", cx);
            }));
        tail = tail.child(shelf);
        let moving =
            moving.map(|(row, height)| self.render_moving_sidebar_session(row, height, theme));
        let lists = crate::edge_fade::edge_faded(
            SIDEBAR_GLASS_FADE_BAND,
            true,
            true,
            div().relative().flex_1().min_h_0().child(
                div()
                    .id("sidebar-lists")
                    .relative()
                    .size_full()
                    .overflow_y_scroll()
                    .track_scroll(&self.sidebar_scroll)
                    .on_drag_move::<SidebarSessionDrag>(cx.listener(
                        |this, event: &gpui::DragMoveEvent<SidebarSessionDrag>, _, cx| {
                            if let Some(transfer) = this.sidebar_session_transfer.as_mut() {
                                transfer.viewport = Some(event.bounds);
                                transfer.pointer = event.event.position;
                            }
                            if event.bounds.contains(&event.event.position) {
                                let payload = event.drag(cx).clone();
                                this.track_pinned_session_drag_pointer(
                                    payload,
                                    f32::from(event.event.position.y),
                                    f32::from(event.bounds.top()),
                                    f32::from(event.bounds.bottom()),
                                    cx,
                                );
                            }
                        },
                    ))
                    .on_drop::<SidebarSessionDrag>(cx.listener(|this, _, _, cx| {
                        this.cancel_sidebar_session_transfer(cx);
                    }))
                    .child(
                        div()
                            .w_full()
                            .min_h(gpui::relative(1.0))
                            .flex()
                            .flex_col()
                            .px(px(Theme::SPACE_SM))
                            .pt(px(SIDEBAR_LIST_PAD_TOP))
                            .pb(px(Theme::SPACE_SM))
                            .child(top)
                            .child(div().flex_1().min_h(px(SIDEBAR_SECTION_GAP)).on_drop(
                                cx.listener(|this, payload: &SidebarSessionDrag, _, cx| {
                                    cx.stop_propagation();
                                    this.finish_t3_status_transfer(payload, "active", cx);
                                }),
                            ))
                            .child(tail),
                    )
                    .children(moving),
            ),
        )
        .fade_overflow_y(&self.sidebar_scroll);
        let filter = self.render_spaces_filter(theme, cx);
        let footer = self.render_sidebar_footer(theme, cx);
        div()
            .w(px(self.settings.sidebar_width))
            .h_full()
            .flex()
            .flex_col()
            .child(filter)
            .child(lists)
            .when_some(self.render_connection_pill(theme, cx), |el, pill| {
                el.child(pill)
            })
            .when_some(self.render_update_strip(theme, cx), |el, strip| {
                el.child(strip)
            })
            .when_some(self.sidebar_notice.clone(), |el, notice| {
                el.child(
                    div()
                        .id("sidebar-notice")
                        .mx(px(Theme::SPACE_SM))
                        .mb(px(Theme::SPACE_SM))
                        .px(px(Theme::SPACE_SM))
                        .py(px(4.0))
                        .rounded(px(Theme::CONTROL_RADIUS))
                        .border_1()
                        .border_color(theme.danger)
                        .text_size(crate::typography::ui_rems(11.0))
                        .text_color(theme.danger)
                        .cursor_pointer()
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.sidebar_notice = None;
                            cx.notify();
                        }))
                        .child(notice),
                )
            })
            .child(div().p(px(Theme::SPACE_SM)).flex_none().child(footer))
            .into_any_element()
    }

    fn show_more_t3_settled(&mut self, cx: &mut Context<Self>) {
        self.t3_settled_shown = self.t3_settled_shown.saturating_add(SETTLED_PAGE_SIZE);
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deep_selected_history_adds_one_row_without_mounting_intermediate_pages() {
        let history: Vec<_> = (0..10_000).map(|index| format!("thread-{index}")).collect();
        let rows = shelf_indices(
            &history,
            SETTLED_PAGE_SIZE,
            true,
            Some("thread-9999"),
            String::as_str,
        );
        assert_eq!(rows, (0..25).chain([9999]).collect::<Vec<_>>());
    }

    #[test]
    fn selected_history_is_not_duplicated_when_its_page_is_visible() {
        let history: Vec<_> = (0..100).map(|index| format!("thread-{index}")).collect();
        assert_eq!(
            shelf_indices(&history, 50, true, Some("thread-30"), String::as_str),
            (0..50).collect::<Vec<_>>()
        );
    }

    #[test]
    fn collapsed_history_only_mounts_the_selected_row() {
        let history = ["new", "older", "deep"];
        assert_eq!(
            shelf_indices(&history, 25, false, Some("deep"), |id| *id),
            vec![2]
        );
    }

    #[test]
    fn collapsed_history_without_selection_mounts_no_rows() {
        assert!(shelf_indices(&["new", "old"], 25, false, None, |id| *id).is_empty());
    }

    #[test]
    fn paging_adds_twenty_five_without_reordering_history() {
        let history: Vec<_> = (0..100).map(|index| format!("thread-{index}")).collect();
        assert_eq!(
            shelf_indices(&history, SETTLED_PAGE_SIZE * 2, true, None, String::as_str),
            (0..50).collect::<Vec<_>>()
        );
    }

    #[test]
    fn switching_project_resets_the_history_budget() {
        assert_eq!(
            settled_budget(250, &Some("old".into()), &Some("new".into())),
            25
        );
    }

    #[test]
    fn same_project_keeps_the_history_budget() {
        assert_eq!(
            settled_budget(250, &Some("project".into()), &Some("project".into())),
            250
        );
    }

    #[test]
    fn settlement_order_uses_end_time_not_last_message_and_has_stable_ties() {
        let now = Utc::now();
        let thread = |settled: chrono::DateTime<Utc>, message: chrono::DateTime<Utc>| {
            zeron_t3::SidebarThread::from_shell(
                &serde_json::json!({
                    "createdAt": now - chrono::Duration::days(30), "updatedAt": now,
                    "status":"completed", "interactionMode":"default",
                    "settledOverride":"settled", "settledAt":settled, "latestUserMessageAt":message,
                }),
                &zeron_t3::SidebarCapabilities {
                    thread_settlement: true,
                    ..Default::default()
                },
            )
            .unwrap()
        };
        let mut rows = [
            ("z", thread(now, now - chrono::Duration::days(10))),
            ("old", thread(now - chrono::Duration::days(1), now)),
            ("a", thread(now, now - chrono::Duration::days(20))),
        ];
        rows.sort_by(|(left_id, left), (right_id, right)| {
            left.compare(right, now).then_with(|| left_id.cmp(right_id))
        });
        assert_eq!(rows.map(|(id, _)| id), ["a", "z", "old"]);
    }

    #[test]
    fn active_order_uses_unsettle_anchor_then_manual_keys_not_runtime_progress() {
        let now = Utc::now();
        let thread = |created: chrono::DateTime<Utc>,
                      unsettled: Option<chrono::DateTime<Utc>>,
                      key: Option<&str>,
                      status: &str| {
            zeron_t3::SidebarThread::from_shell(&serde_json::json!({
                "createdAt":created, "updatedAt":now, "status":status, "interactionMode":"default",
                "unsettledAt":unsettled, "activeOrderKey":key,
            }), &Default::default()).unwrap()
        };
        let mut rows = [
            (
                "working-old",
                thread(now - chrono::Duration::days(3), None, None, "running"),
            ),
            ("keyed-z", thread(now, None, Some("z"), "idle")),
            ("keyed-b", thread(now, None, Some("b"), "idle")),
            (
                "returned",
                thread(now - chrono::Duration::days(10), Some(now), None, "idle"),
            ),
        ];
        rows.sort_by(|(left_id, left), (right_id, right)| {
            left.compare_active(right)
                .then_with(|| left_id.cmp(right_id))
        });
        assert_eq!(
            rows.map(|(id, _)| id),
            ["returned", "working-old", "keyed-b", "keyed-z"]
        );
    }

    #[gpui::test]
    fn native_roster_bounds_history_and_keyboard_order_across_collapse_and_paging(
        cx: &mut gpui::TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        cx.update(|cx| {
            gpui_base::init(cx);
            cx.set_global(Theme::default());
            crate::app_menus::init(cx);
            crate::history::init(
                Default::default(),
                Default::default(),
                Default::default(),
                Default::default(),
                cx,
            );
        });
        let window = cx.add_window(|_, cx| {
            let state = cx.new(|_| AppState::new());
            Shell::new(
                state,
                EngineBootConfig {
                    data_dir: dir.path().into(),
                    ipc_port: 0,
                    edge_url: "http://127.0.0.1:1".into(),
                    edge_token: None,
                    org_id: None,
                    workos_client_id: None,
                    default_harness: zeron_proto::HarnessId::Mock,
                },
                cx,
            )
        });
        window.update(cx, |shell, _, cx| {
            let now = Utc::now();
            let capabilities = zeron_t3::SidebarCapabilities { thread_settlement: true, ..Default::default() };
            shell.state.update(cx, |state, _| {
                for index in 0..10_000 {
                    let id = format!("history-{index:05}");
                    let chat: zeron_proto::Chat = serde_json::from_value(serde_json::json!({
                        "id":id, "title":id, "deviceId":"local", "archived":false, "createdAt":now,
                    })).unwrap();
                    let thread = zeron_t3::SidebarThread::from_shell(&serde_json::json!({
                        "createdAt":now, "updatedAt":now, "status":"completed", "interactionMode":"default",
                        "settledOverride":"settled", "settledAt":now - chrono::Duration::seconds(index),
                    }), &capabilities).unwrap();
                    state.t3_sidebar.insert(id, thread);
                    state.chats.push(chat);
                }
                state.selected_chat = Some("history-09999".into());
            });
            shell.t3_settled_shown = 25;
            shell.t3_settled_filter = None;
            shell.settings.space_filter = None;
            shell.sidebar_collapsed_groups.remove(SETTLED_COLLAPSE_KEY);
            let rows = shell.t3_sidebar_rows(cx);
            assert_eq!((rows.settled.len(), rows.hidden_settled, rows.visible_order().last().map(String::as_str)), (26, 9974, Some("history-09999")));
            shell.show_more_t3_settled(cx);
            assert_eq!(shell.t3_sidebar_rows(cx).settled.len(), 51);
            shell.sidebar_collapsed_groups.insert(SETTLED_COLLAPSE_KEY.into());
            assert_eq!(shell.t3_sidebar_visible_order(cx), vec!["history-09999".to_owned()]);
            shell.state.update(cx, |state, _| { state.selected_chat = None; });
            assert!(shell.t3_sidebar_visible_order(cx).is_empty());
        }).unwrap();
    }

    #[test]
    fn settled_label_timestamp_and_order_share_the_fallback() {
        let now = Utc::now();
        let completed = now - chrono::Duration::minutes(5);
        let thread = zeron_t3::SidebarThread::from_shell(&serde_json::json!({
            "createdAt": now - chrono::Duration::days(30), "updatedAt": now,
            "status":"completed", "interactionMode":"default", "settledOverride":"settled",
            "latestUserMessageAt":now - chrono::Duration::days(1), "latestRunCompletedAt":completed,
        }), &zeron_t3::SidebarCapabilities { thread_settlement: true, ..Default::default() }).unwrap();
        assert_eq!(thread.settled_timestamp(), completed);
    }
}
