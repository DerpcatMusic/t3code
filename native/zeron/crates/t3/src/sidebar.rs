use anyhow::{Context, Result, ensure};
use chrono::{DateTime, Datelike, Duration, Local, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    cmp::Ordering,
    collections::{HashMap, HashSet},
};
use zeron_proto::{ChatIndicator, SidebarPinChange};

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SidebarCapabilities {
    pub thread_settlement: bool,
    pub thread_snooze: bool,
    pub thread_pinning: bool,
    pub thread_pin_reorder: bool,
    pub thread_auto_settle_opt_out: bool,
    pub thread_visited_tracking: bool,
    pub thread_title_regeneration: bool,
}

impl SidebarCapabilities {
    pub fn from_config(config: &Value) -> Result<Self> {
        let value = &config["environment"]["capabilities"];
        if value.is_null() {
            return Ok(Self::default());
        }
        Ok(serde_json::from_value(value.clone())?)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SidebarSection {
    Active,
    Working,
    Snoozed,
    Settled,
}

impl SidebarSection {
    pub fn includes(self, section: Self) -> bool {
        self == section || (self == Self::Active && section == Self::Working)
    }

    pub fn group(self) -> (String, String) {
        let (key, label) = match self {
            Self::Active => ("status:0", "Active"),
            Self::Working => ("status:1", "Working"),
            Self::Snoozed => ("status:2", "Snoozed"),
            Self::Settled => ("status:3", "Settled"),
        };
        (key.into(), label.into())
    }
}

// T3 lifecycle metadata travels with each native chat, without changing Zeron's protocol.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SidebarThread {
    pub capabilities: SidebarCapabilities,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(default)]
    archived_at: Option<DateTime<Utc>>,
    status: String,
    #[serde(default)]
    activity_run_status: Option<String>,
    #[serde(default)]
    active_run_id: Option<String>,
    #[serde(default)]
    active_provider_thread_id: Option<String>,
    #[serde(default)]
    latest_run_id: Option<String>,
    #[serde(default)]
    latest_run_requested_at: Option<DateTime<Utc>>,
    #[serde(default)]
    latest_run_started_at: Option<DateTime<Utc>>,
    #[serde(default)]
    latest_run_completed_at: Option<DateTime<Utc>>,
    #[serde(default)]
    latest_user_message_at: Option<DateTime<Utc>>,
    #[serde(default)]
    latest_user_authored_message_at: Option<DateTime<Utc>>,
    #[serde(default)]
    authored_timestamp_supported: bool,
    #[serde(default)]
    pending_runtime_request: Option<Value>,
    #[serde(default)]
    pending_background_tasks: Vec<Value>,
    #[serde(default)]
    has_actionable_proposed_plan: bool,
    interaction_mode: String,
    #[serde(default)]
    settled_override: Option<String>,
    #[serde(default)]
    settled_at: Option<DateTime<Utc>>,
    #[serde(default)]
    unsettled_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub snoozed_until: Option<DateTime<Utc>>,
    #[serde(default)]
    snoozed_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub pinned_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub pin_order_key: Option<String>,
    #[serde(default)]
    auto_settle_disabled_at: Option<DateTime<Utc>>,
    #[serde(default)]
    last_visited_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub title_regeneration: Option<Value>,
}

impl SidebarThread {
    pub fn from_shell(thread: &Value, capabilities: &SidebarCapabilities) -> Result<Self> {
        let mut value = thread.clone();
        value["capabilities"] = serde_json::to_value(capabilities)?;
        value["authoredTimestampSupported"] =
            json!(thread.get("latestUserAuthoredMessageAt").is_some());
        Ok(serde_json::from_value(value)?)
    }

    pub fn is_settled(&self) -> bool {
        self.capabilities.thread_settlement && self.settled_override.as_deref() == Some("settled")
    }

    pub fn auto_settle_enabled(&self) -> bool {
        self.auto_settle_disabled_at.is_none()
    }

    fn needs_input(&self) -> bool {
        self.pending_runtime_request
            .as_ref()
            .is_some_and(|request| request["kind"].as_str() != Some("auth_refresh"))
    }

    fn raised_hand(&self) -> bool {
        self.needs_input()
            || (self.status == "failed" && self.snoozed_at.is_none_or(|at| self.updated_at > at))
            || (matches!(self.status.as_str(), "idle" | "completed")
                && self.snoozed_at.is_some_and(|at| {
                    self.latest_run_completed_at
                        .is_some_and(|completed| completed > at)
                }))
    }

    pub fn is_snoozed(&self, now: DateTime<Utc>) -> bool {
        self.capabilities.thread_snooze
            && self.snoozed_until.is_some_and(|until| until > now)
            && !self.raised_hand()
    }

    pub fn snooze_label(&self, now: DateTime<Utc>) -> Option<String> {
        if !self.is_snoozed(now) {
            return None;
        }
        let label = zeron_proto::view::format_time_ago(now, self.snoozed_until?);
        Some(if label == "now" {
            "soon".into()
        } else {
            format!("in {label}")
        })
    }

    fn runtime_status(&self) -> Option<&str> {
        let background = self.status != "failed"
            && self.pending_background_tasks.iter().any(|task| {
                matches!(
                    task["kind"].as_str(),
                    Some("subagent" | "monitor" | "background_task")
                )
            });
        if self.latest_run_id.is_none() && self.active_provider_thread_id.is_none() && !background {
            return None;
        }
        Some(if background {
            "idle"
        } else {
            self.activity_run_status.as_deref().unwrap_or(&self.status)
        })
    }

    fn working(&self) -> bool {
        !self.needs_input()
            && matches!(
                self.runtime_status(),
                Some("preparing" | "queued" | "starting" | "running" | "waiting" | "idle")
            )
            && !(self.interaction_mode == "plan"
                && self.has_actionable_proposed_plan
                && self.latest_run_id.is_some()
                && !matches!(
                    self.status.as_str(),
                    "preparing" | "queued" | "starting" | "running" | "waiting"
                )
                && self.active_run_id != self.latest_run_id)
    }

    pub fn section(&self, now: DateTime<Utc>) -> SidebarSection {
        if self.is_snoozed(now) {
            SidebarSection::Snoozed
        } else if self.is_settled() {
            SidebarSection::Settled
        } else if self.working() {
            SidebarSection::Working
        } else {
            SidebarSection::Active
        }
    }

    pub fn visible_pin(&self, now: DateTime<Utc>) -> bool {
        self.capabilities.thread_pinning
            && self.archived_at.is_none()
            && self.pinned_at.is_some()
            && !self.is_snoozed(now)
            && !self.is_settled()
    }

    pub fn can_snooze(&self, now: DateTime<Utc>) -> bool {
        if !self.capabilities.thread_snooze
            || self.needs_input()
            || matches!(
                self.runtime_status(),
                Some("preparing" | "queued" | "starting")
            )
        {
            return false;
        }
        let queued = self.runtime_status() != Some("failed")
            && self.latest_user_message_at.is_some_and(|message| {
                (now - message).num_milliseconds().abs() <= 120_000
                    && [
                        self.latest_run_requested_at,
                        self.latest_run_started_at,
                        self.latest_run_completed_at,
                    ]
                    .iter()
                    .all(|at| at.is_none_or(|at| at < message))
            });
        !queued
    }

    pub fn indicator(&self) -> ChatIndicator {
        if self.needs_input() {
            ChatIndicator::AwaitingInput
        } else if self.status == "failed" {
            ChatIndicator::Errored
        } else if self.working() {
            ChatIndicator::Working
        } else if self
            .latest_run_completed_at
            .zip(self.last_visited_at)
            .is_some_and(|(completed, visited)| completed > visited)
        {
            ChatIndicator::Completed
        } else {
            ChatIndicator::Idle
        }
    }

    pub fn compare(&self, other: &Self, now: DateTime<Utc>) -> Ordering {
        let section = self.section(now);
        section
            .cmp(&other.section(now))
            .then_with(|| match section {
                SidebarSection::Snoozed => self.snoozed_until.cmp(&other.snoozed_until),
                SidebarSection::Settled => other.settled_timestamp().cmp(&self.settled_timestamp()),
                SidebarSection::Working => other.send_timestamp().cmp(&self.send_timestamp()),
                SidebarSection::Active => other.return_timestamp().cmp(&self.return_timestamp()),
            })
    }

    fn settled_timestamp(&self) -> DateTime<Utc> {
        self.settled_at.unwrap_or_else(|| {
            [
                self.latest_user_message_at,
                self.latest_run_requested_at,
                self.latest_run_started_at,
                self.latest_run_completed_at,
            ]
            .into_iter()
            .flatten()
            .max()
            .unwrap_or(self.updated_at)
        })
    }

    fn send_timestamp(&self) -> DateTime<Utc> {
        let sent = if self.authored_timestamp_supported {
            self.latest_user_authored_message_at
        } else {
            self.latest_run_requested_at
        };
        sent.unwrap_or(self.created_at).max(self.created_at)
    }

    fn return_timestamp(&self) -> DateTime<Utc> {
        [
            Some(self.created_at),
            self.unsettled_at,
            self.latest_run_requested_at,
            self.latest_run_completed_at,
        ]
        .into_iter()
        .flatten()
        .max()
        .unwrap()
    }

    pub(crate) fn section_commands(
        &self,
        params: &Value,
        now: DateTime<Utc>,
    ) -> Result<Vec<Value>> {
        let section = params["section"]
            .as_str()
            .context("missing sidebar section")?;
        ensure!(
            matches!(section, "active" | "settled"),
            "Unsupported sidebar drop target"
        );
        let mut operations = Vec::new();
        if self.snoozed_until.is_some() {
            operations.push("wakeChat");
        }
        if section == "settled" {
            operations.push("settleChat");
        } else {
            if self.is_settled() {
                operations.push("unsettleChat");
            }
            if self.pinned_at.is_some() {
                operations.push("unpinChat");
            }
        }
        operations
            .into_iter()
            .map(|op| self.command(&json!({"op":op,"chatId":params["chatId"]}), now))
            .collect()
    }

    pub fn command(&self, params: &Value, now: DateTime<Utc>) -> Result<Value> {
        let op = params["op"].as_str().context("missing sidebar operation")?;
        let id = params["chatId"].as_str().context("missing thread id")?;
        let (capability, mut command) = match op {
            "settleChat" => (
                self.capabilities.thread_settlement,
                json!({"type":"thread.settle"}),
            ),
            "unsettleChat" => (
                self.capabilities.thread_settlement,
                json!({"type":"thread.unsettle","reason":"user"}),
            ),
            "snoozeChat" => {
                ensure!(
                    self.can_snooze(now),
                    "This thread needs attention or has queued work; it cannot be snoozed yet"
                );
                let until: DateTime<Utc> = serde_json::from_value(params["until"].clone())?;
                ensure!(until > now, "Choose a snooze time in the future");
                (
                    self.capabilities.thread_snooze,
                    json!({"type":"thread.snooze","snoozedUntil":until}),
                )
            }
            "wakeChat" => (
                self.capabilities.thread_snooze,
                json!({"type":"thread.unsnooze","reason":"user"}),
            ),
            "pinChat" => (
                self.capabilities.thread_pinning,
                json!({"type":"thread.pin"}),
            ),
            "unpinChat" => (
                self.capabilities.thread_pinning,
                json!({"type":"thread.unpin"}),
            ),
            "setChatAutoSettle" => (
                self.capabilities.thread_auto_settle_opt_out,
                json!({"type":"thread.auto-settle.set","enabled":params["enabled"].as_bool().context("enabled must be boolean")?}),
            ),
            "markChatUnread" => (
                self.capabilities.thread_visited_tracking,
                json!({"type":"thread.mark-unread"}),
            ),
            "regenerateChatTitle" => {
                ensure!(
                    self.title_regeneration.is_none(),
                    "A title is already being generated"
                );
                (
                    self.capabilities.thread_title_regeneration,
                    json!({"type":"thread.metadata.update","regenerateTitle":true}),
                )
            }
            _ => anyhow::bail!("Unsupported sidebar operation"),
        };
        ensure!(
            capability,
            "This T3 server does not support this sidebar action"
        );
        command["threadId"] = json!(id);
        Ok(command)
    }
}

pub fn sidebar_pins(threads: &HashMap<String, SidebarThread>, now: DateTime<Utc>) -> Vec<String> {
    let mut pins: Vec<_> = threads
        .iter()
        .filter(|(_, thread)| thread.visible_pin(now))
        .collect();
    pins.sort_by(
        |(a_id, a), (b_id, b)| match (&a.pin_order_key, &b.pin_order_key) {
            (Some(a), Some(b)) => a.cmp(b).then_with(|| a_id.cmp(b_id)),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => b.created_at.cmp(&a.created_at).then_with(|| a_id.cmp(b_id)),
        },
    );
    pins.into_iter().map(|(id, _)| id.clone()).collect()
}

// Same alphabet and midpoint as client-runtime/state/threadSort.ts; keys converge across clients.
fn pin_key_between(a: Option<&str>, b: Option<&str>) -> Option<String> {
    fn valid(key: &str) -> bool {
        !key.is_empty()
            && key.len() <= 1024
            && key.bytes().all(|c| c.is_ascii_lowercase())
            && !key.ends_with('a')
    }
    fn midpoint(a: &[u8], b: &[u8]) -> Vec<u8> {
        if !b.is_empty() {
            let n = b
                .iter()
                .enumerate()
                .take_while(|(i, c)| a.get(*i).copied().unwrap_or(b'a') == **c)
                .count();
            if n > 0 {
                let mut key = b[..n].to_vec();
                key.extend(midpoint(a.get(n..).unwrap_or_default(), &b[n..]));
                return key;
            }
        }
        let digit_a = a.first().map_or(0, |c| c - b'a');
        let digit_b = b.first().map_or(26, |c| c - b'a');
        if digit_b - digit_a > 1 {
            return vec![b'a' + (digit_a + digit_b).div_ceil(2)];
        }
        if b.len() > 1 {
            return vec![b[0]];
        }
        let mut key = vec![b'a' + digit_a];
        key.extend(midpoint(a.get(1..).unwrap_or_default(), &[]));
        key
    }
    if a.is_some_and(|a| !valid(a)) || b.is_some_and(|b| !valid(b) || a.unwrap_or_default() >= b) {
        return None;
    }
    String::from_utf8(midpoint(
        a.unwrap_or_default().as_bytes(),
        b.unwrap_or_default().as_bytes(),
    ))
    .ok()
}

pub(crate) fn pin_change_commands(
    threads: &HashMap<String, SidebarThread>,
    change: &SidebarPinChange,
    now: DateTime<Utc>,
) -> Result<Vec<Value>> {
    let id = change.session_id();
    let thread = threads.get(id).context("T3 thread no longer exists")?;
    ensure!(
        thread.capabilities.thread_pinning,
        "This server does not support pins"
    );
    if matches!(change, SidebarPinChange::Unpin { .. }) {
        return Ok(vec![json!({"type":"thread.unpin","threadId":id})]);
    }
    let (after, before) = match change {
        SidebarPinChange::Pin { after, before, .. }
        | SidebarPinChange::Move { after, before, .. } => (after, before),
        _ => anyhow::bail!("Custom sections are local preferences in Native T3"),
    };
    ensure!(
        thread.capabilities.thread_pin_reorder,
        "This server does not support pin ordering"
    );
    let pin_moved = matches!(change, SidebarPinChange::Pin { .. })
        || thread.pinned_at.is_none()
        || thread.is_settled()
        || thread.is_snoozed(now);
    let mut order = sidebar_pins(threads, now);
    for anchor in [after, before].into_iter().flatten() {
        ensure!(
            anchor != id && order.contains(anchor),
            "Pin order changed; try the drop again"
        );
    }
    change.project(&mut order);
    let index = order
        .iter()
        .position(|item| item == id)
        .context("Invalid pin drop")?;
    let visible: HashSet<&str> = order.iter().map(String::as_str).collect();
    let reserved: HashSet<&str> = threads
        .iter()
        .filter(|(id, thread)| thread.pinned_at.is_some() && !visible.contains(id.as_str()))
        .filter_map(|(_, thread)| thread.pin_order_key.as_deref())
        .collect();
    let left = index.checked_sub(1).and_then(|i| order.get(i));
    let right = order.get(index + 1);
    let key_for = |id: &String| {
        threads
            .get(id)
            .and_then(|thread| thread.pin_order_key.as_deref())
    };
    let mut assignments = Vec::new();
    if left.is_none_or(|id| key_for(id).is_some()) && right.is_none_or(|id| key_for(id).is_some()) {
        let mut key = pin_key_between(left.and_then(key_for), right.and_then(key_for));
        while key
            .as_ref()
            .is_some_and(|key| reserved.contains(key.as_str()))
        {
            key = pin_key_between(key.as_deref(), right.and_then(key_for));
        }
        if let Some(key) = key {
            assignments.push((id.to_owned(), key));
        }
    }
    if assignments.is_empty() {
        // Legacy keyless pins get the same one-time materialization as T3's web client.
        let count = order.len() + reserved.len();
        let mut width = 2u32;
        let mut space = 26u64.pow(width);
        while space <= (count as u64 + 1) * 2 {
            width += 1;
            space *= 26;
        }
        let mut keys = (1..=count).filter_map(|i| {
            let mut value = ((space as f64 / (count + 1) as f64) * i as f64).round() as u64;
            if value % 26 == 0 {
                value += 1;
            }
            let mut key = vec![b'a'; width as usize];
            for digit in key.iter_mut().rev() {
                *digit += (value % 26) as u8;
                value /= 26;
            }
            let key = String::from_utf8(key).unwrap();
            (!reserved.contains(key.as_str())).then_some(key)
        });
        assignments.extend(
            order
                .iter()
                .zip(&mut keys)
                .filter(|(item, key)| {
                    (item.as_str() == id && pin_moved) || key_for(item) != Some(key.as_str())
                })
                .map(|(id, key)| (id.clone(), key)),
        );
    }
    let mut commands = Vec::new();
    for (item, key) in assignments {
        let promote = item == id && pin_moved;
        if promote {
            commands.push(json!({"type":"thread.pin","threadId":item,"orderKey":key}));
        }
        // T3 preserves an existing pin's key when promoting it out of snooze.
        if !promote || (thread.pinned_at.is_some() && thread.pin_order_key.as_deref() != Some(&key))
        {
            commands.push(json!({"type":"thread.pin.reorder","threadId":item,"orderKey":key}));
        }
    }
    Ok(commands)
}

pub fn snooze_presets(now: DateTime<Local>) -> Vec<(&'static str, DateTime<Utc>)> {
    let mut presets = vec![
        ("In 1 hour", (now + Duration::hours(1)).to_utc()),
        ("In 3 hours", (now + Duration::hours(3)).to_utc()),
    ];
    let at_hour = |days: i64, hour| {
        now.date_naive()
            .checked_add_signed(Duration::days(days))
            .and_then(|date| date.and_hms_opt(hour, 0, 0))
            .and_then(|date| Local.from_local_datetime(&date).earliest())
            .map(|date| date.to_utc())
    };
    if let Some(evening) = at_hour(0, 18).filter(|at| *at - now.to_utc() > Duration::hours(1)) {
        presets.push(("This evening", evening));
    }
    let tomorrow = at_hour(1, 9);
    if let Some(tomorrow) = tomorrow {
        presets.push(("Tomorrow", tomorrow));
    }
    let weekday = i64::from(now.weekday().num_days_from_monday());
    let days = if weekday == 0 { 7 } else { 7 - weekday };
    if let Some(monday) = at_hour(days, 9).filter(|at| Some(*at) != tomorrow) {
        presets.push(("Next week", monday));
    }
    presets
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inbox_views_keep_running_work_active_and_separate_finished_and_snoozed_work() {
        assert!(SidebarSection::Active.includes(SidebarSection::Active));
        assert!(SidebarSection::Active.includes(SidebarSection::Working));
        assert!(!SidebarSection::Active.includes(SidebarSection::Settled));
        assert!(!SidebarSection::Active.includes(SidebarSection::Snoozed));
        assert!(SidebarSection::Settled.includes(SidebarSection::Settled));
        assert!(!SidebarSection::Settled.includes(SidebarSection::Working));
        assert!(SidebarSection::Snoozed.includes(SidebarSection::Snoozed));
        assert!(!SidebarSection::Snoozed.includes(SidebarSection::Active));
    }
    fn thread() -> SidebarThread {
        let now = Utc::now();
        SidebarThread::from_shell(
            &json!({"status":"completed","createdAt":now,"updatedAt":now,
            "interactionMode":"default","latestRunId":"run","latestRunCompletedAt":now}),
            &SidebarCapabilities {
                thread_settlement: true,
                thread_snooze: true,
                thread_pinning: true,
                thread_pin_reorder: true,
                thread_auto_settle_opt_out: true,
                thread_visited_tracking: true,
                ..Default::default()
            },
        )
        .unwrap()
    }
    #[test]
    fn snooze_wakes_on_time_and_attention_preserves_pin() {
        let now = Utc::now();
        let mut row = thread();
        row.pinned_at = Some(now);
        row.snoozed_at = Some(now);
        row.latest_run_completed_at = Some(now - Duration::seconds(1));
        row.snoozed_until = Some(now + Duration::hours(1));
        assert_eq!(row.section(now), SidebarSection::Snoozed);
        assert_eq!(row.snooze_label(now).as_deref(), Some("in 1h"));
        assert_eq!(
            row.snooze_label(now + Duration::seconds(3590)).as_deref(),
            Some("soon")
        );
        assert_eq!(row.snooze_label(now + Duration::hours(1)), None);
        assert!(!row.visible_pin(now));
        assert!(row.visible_pin(now + Duration::hours(1)));
        row.pending_runtime_request = Some(json!({"kind":"approval"}));
        assert_eq!(row.section(now), SidebarSection::Active);
        assert!(row.visible_pin(now));
        assert!(!row.can_snooze(now));
        row.pending_runtime_request = None;
        row.status = "failed".into();
        row.updated_at = now - Duration::seconds(1);
        assert_eq!(row.section(now), SidebarSection::Snoozed);
        row.updated_at = now + Duration::seconds(1);
        assert_eq!(row.section(now), SidebarSection::Active);
    }
    #[test]
    fn lifecycle_actions_are_guarded_and_reversible() {
        let now = Utc::now();
        let mut row = thread();
        for (op, tag) in [
            ("settleChat", "thread.settle"),
            ("unsettleChat", "thread.unsettle"),
            ("wakeChat", "thread.unsnooze"),
            ("pinChat", "thread.pin"),
            ("unpinChat", "thread.unpin"),
            ("markChatUnread", "thread.mark-unread"),
        ] {
            let command = row.command(&json!({"op":op,"chatId":"test"}), now).unwrap();
            assert_eq!(command["type"], tag);
            assert_eq!(command["threadId"], "test");
        }
        assert!(
            row.command(&json!({"op":"snoozeChat","chatId":"test","until":now}), now)
                .is_err()
        );
        assert!(
            row.command(
                &json!({"op":"setChatAutoSettle","chatId":"test","enabled":"yes"}),
                now
            )
            .is_err()
        );
        row.capabilities = SidebarCapabilities::default();
        row.settled_override = Some("settled".into());
        assert_eq!(row.section(now), SidebarSection::Active);
        assert!(
            row.command(&json!({"op":"settleChat","chatId":"test"}), now)
                .is_err()
        );
    }

    #[test]
    fn section_drops_clear_snooze_and_validate_before_writes() {
        let now = Utc::now();
        let mut row = thread();
        row.snoozed_until = Some(now + Duration::hours(1));
        row.pinned_at = Some(now);
        let params = json!({"chatId":"test","section":"settled"});
        let commands = row.section_commands(&params, now).unwrap();
        assert_eq!(
            commands
                .iter()
                .map(|command| command["type"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec!["thread.unsnooze", "thread.settle"]
        );
        row.capabilities.thread_settlement = false;
        assert!(row.section_commands(&params, now).is_err());
        assert!(
            row.section_commands(&json!({"chatId":"test","section":"unknown"}), now)
                .is_err()
        );
    }
    #[test]
    fn working_and_unread_follow_the_t3_inbox() {
        let now = Utc::now();
        let mut row = thread();
        row.status = "running".into();
        assert_eq!(row.section(now), SidebarSection::Working);
        row.pending_runtime_request = Some(json!({"kind":"user_input"}));
        assert_eq!(row.section(now), SidebarSection::Active);
        row.pending_runtime_request = None;
        row.status = "completed".into();
        row.last_visited_at = Some(now - Duration::seconds(1));
        row.latest_run_completed_at = Some(now);
        assert_eq!(row.indicator(), ChatIndicator::Completed);
        row.last_visited_at = Some(now);
        assert_eq!(row.indicator(), ChatIndicator::Idle);
        row.settled_override = Some("settled".into());
        row.settled_at = Some(now);
        assert_eq!(row.section(now), SidebarSection::Settled);
        assert!(row.compare(&thread(), now).is_gt());
    }

    #[test]
    fn background_work_and_queued_turns_keep_t3_priority() {
        let now = Utc::now();
        let mut row = thread();
        row.pending_background_tasks = vec![json!({"kind":"command"})];
        assert_eq!(row.section(now), SidebarSection::Active);
        row.pending_background_tasks = vec![json!({"kind":"subagent"})];
        assert_eq!(row.section(now), SidebarSection::Working);
        row.interaction_mode = "plan".into();
        row.has_actionable_proposed_plan = true;
        assert_eq!(row.section(now), SidebarSection::Active);
        row.latest_user_message_at = Some(now + Duration::seconds(1));
        row.latest_run_completed_at = Some(now - Duration::seconds(1));
        assert!(!row.can_snooze(now));
        assert!(row.can_snooze(now + Duration::minutes(3)));
        row.pending_runtime_request = Some(json!({"kind":"user_input"}));
        assert!(!row.can_snooze(now + Duration::minutes(3)));
        row.pending_runtime_request = Some(json!({"kind":"auth_refresh"}));
        assert!(row.can_snooze(now + Duration::minutes(3)));
        row.pending_background_tasks.clear();
        row.activity_run_status = Some("preparing".into());
        assert!(!row.can_snooze(now + Duration::minutes(3)));
        row.interaction_mode = "default".into();
        row.status = "failed".into();
        row.activity_run_status = Some("running".into());
        assert_eq!(row.section(now), SidebarSection::Working);
        assert!(!row.can_snooze(now));
        row.activity_run_status = None;
        assert_eq!(row.section(now), SidebarSection::Active);
        assert!(row.can_snooze(now));
    }

    #[test]
    fn pin_midpoints_match_t3_and_stay_strictly_between_neighbors() {
        assert_eq!(pin_key_between(None, None).as_deref(), Some("n"));
        assert_eq!(pin_key_between(Some("n"), Some("t")).as_deref(), Some("q"));
        assert_eq!(pin_key_between(Some("n"), Some("o")).as_deref(), Some("nn"));
        assert_eq!(pin_key_between(None, Some("ab")).as_deref(), Some("aan"));
        assert!(pin_key_between(Some("a"), None).is_none());
        assert!(pin_key_between(Some(&"z".repeat(1025)), None).is_none());
        assert!(pin_key_between(Some("z"), Some("n")).is_none());
        let mut left = "n".to_owned();
        for _ in 0..100 {
            let key = pin_key_between(Some(&left), Some("o")).unwrap();
            assert!(left < key && key.as_str() < "o" && !key.ends_with('a'));
            left = key;
        }
    }

    #[test]
    fn pin_reorder_materializes_legacy_keys_and_avoids_hidden_pins() {
        let now = Utc::now();
        let mut threads = HashMap::new();
        for id in ["left", "moved", "right", "hidden"] {
            let mut row = thread();
            row.pinned_at = Some(now);
            threads.insert(id.to_owned(), row);
        }
        threads.get_mut("hidden").unwrap().snoozed_at = Some(now);
        threads.get_mut("hidden").unwrap().latest_run_completed_at =
            Some(now - Duration::seconds(1));
        threads.get_mut("hidden").unwrap().snoozed_until = Some(now + Duration::hours(1));
        let change = SidebarPinChange::Move {
            session_id: "moved".into(),
            after: Some("left".into()),
            before: Some("right".into()),
        };
        let commands = pin_change_commands(&threads, &change, now).unwrap();
        assert_eq!(commands.len(), 3);
        let mut keys: Vec<_> = commands
            .iter()
            .map(|command| command["orderKey"].as_str().unwrap())
            .collect();
        keys.sort();
        keys.dedup();
        assert_eq!(keys.len(), 3);
        assert!(
            commands
                .iter()
                .all(|command| command["threadId"] != "hidden")
        );
        for (id, key) in [("left", "n"), ("right", "t"), ("hidden", "q")] {
            threads.get_mut(id).unwrap().pin_order_key = Some(key.into());
        }
        let commands = pin_change_commands(&threads, &change, now).unwrap();
        assert_eq!(
            commands,
            vec![json!({"type":"thread.pin.reorder","threadId":"moved","orderKey":"s"})]
        );
        let stale = SidebarPinChange::Pin {
            session_id: "moved".into(),
            after: Some("missing".into()),
            before: None,
        };
        assert!(pin_change_commands(&threads, &stale, now).is_err());
        threads
            .get_mut("moved")
            .unwrap()
            .capabilities
            .thread_pin_reorder = false;
        assert!(pin_change_commands(&threads, &change, now).is_err());
    }

    #[test]
    fn pin_drops_wake_and_unsettle_through_the_atomic_t3_pin() {
        let now = Utc::now();
        for section in [SidebarSection::Snoozed, SidebarSection::Settled] {
            let mut row = thread();
            if section == SidebarSection::Snoozed {
                row.pinned_at = Some(now);
                row.snoozed_at = Some(now);
                row.latest_run_completed_at = Some(now - Duration::seconds(1));
                row.snoozed_until = Some(now + Duration::hours(1));
            } else {
                row.settled_override = Some("settled".into());
            }
            let threads = HashMap::from([("moved".into(), row)]);
            let change = SidebarPinChange::Pin {
                session_id: "moved".into(),
                after: None,
                before: None,
            };
            let mut expected = vec![json!({"type":"thread.pin","threadId":"moved","orderKey":"n"})];
            if section == SidebarSection::Snoozed {
                expected
                    .push(json!({"type":"thread.pin.reorder","threadId":"moved","orderKey":"n"}));
            }
            assert_eq!(
                pin_change_commands(&threads, &change, now).unwrap(),
                expected
            );
        }
    }
}
