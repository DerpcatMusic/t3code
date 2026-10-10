use anyhow::{Context, Result, bail, ensure};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use zeron_doc::{MessagePart, SessionMessageEntry, TranscriptFrame};
use zeron_proto::{Chat, Session, Space};

pub fn text<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value[key]
        .as_str()
        .filter(|s| !s.is_empty())
        .with_context(|| format!("missing T3 {key}"))
}

pub fn rows<'a>(value: &'a Value, key: &str) -> Result<&'a [Value]> {
    value[key]
        .as_array()
        .map(Vec::as_slice)
        .with_context(|| format!("invalid T3 {key}"))
}

pub fn option_map(options: &Value) -> Result<serde_json::Map<String, Value>> {
    if options.is_null() {
        return Ok(Default::default());
    }
    if let Some(options) = options.as_object() {
        return Ok(options.clone());
    }
    options
        .as_array()
        .context("invalid T3 model options")?
        .iter()
        .map(|option| {
            let id = text(option, "id")?;
            let value = &option["value"];
            ensure!(
                value.is_string() || value.is_boolean(),
                "invalid T3 option value"
            );
            Ok((id.to_owned(), value.clone()))
        })
        .collect()
}

pub fn approval_choices(item: &Value) -> Result<Vec<(String, String)>> {
    if item["options"].is_null() {
        return Ok(vec![
            ("Allow once".into(), "accept".into()),
            ("Deny".into(), "decline".into()),
        ]);
    }
    rows(item, "options")?
        .iter()
        .map(|option| {
            let decision = text(option, "decision")?;
            ensure!(
                matches!(
                    decision,
                    "accept" | "acceptForSession" | "acceptAlways" | "decline" | "cancel"
                ),
                "invalid T3 approval decision"
            );
            let label = text(option, "label")?;
            let label = if let Some(warning) = option["warning"].as_str() {
                format!("{label} — {warning}")
            } else {
                label.into()
            };
            Ok((label, decision.into()))
        })
        .collect()
}

pub fn upsert(values: &mut Vec<Value>, item: Value) -> Result<()> {
    let id = text(&item, "id")?;
    if let Some(index) = values.iter().position(|v| v["id"] == id) {
        values[index] = item;
    } else {
        values.push(item);
    }
    Ok(())
}

#[derive(Clone, Default)]
pub struct Shell {
    pub capabilities: crate::SidebarCapabilities,
    pub projects: Vec<Value>,
    pub threads: Vec<Value>,
    pub archived_threads: Vec<Value>,
    sequence: u64,
    archive_sequence: u64,
}

impl Shell {
    pub fn apply(&mut self, frame: &Value) -> Result<bool> {
        match text(frame, "kind")? {
            "snapshot" => {
                if let Some(roots) = frame["resolvedRepositoryIdentityRoots"].as_array() {
                    // Enrichment repairs repository identity only; it is not a new shell.
                    for candidate in rows(&frame["snapshot"], "projects")? {
                        if let Some(project) = self.projects.iter_mut().find(|project| {
                            project["id"] == candidate["id"]
                                && project["workspaceRoot"] == candidate["workspaceRoot"]
                        }) {
                            if roots.contains(&project["workspaceRoot"])
                                || (project["repositoryIdentity"].is_null()
                                    && !candidate["repositoryIdentity"].is_null())
                            {
                                project["repositoryIdentity"] =
                                    candidate["repositoryIdentity"].clone();
                            }
                        }
                    }
                    return Ok(true);
                }
                self.sequence = frame["snapshot"]["snapshotSequence"]
                    .as_u64()
                    .context("missing shell sequence")?;
                self.projects = rows(&frame["snapshot"], "projects")?.to_vec();
                self.threads = rows(&frame["snapshot"], "threads")?.to_vec();
            }
            "synchronized" => return Ok(false),
            kind => {
                let sequence = frame["sequence"]
                    .as_u64()
                    .context("missing shell sequence")?;
                if sequence <= self.sequence {
                    return Ok(false);
                }
                self.sequence = sequence;
                match kind {
                    "project.updated" => upsert(&mut self.projects, frame["project"].clone())?,
                    "project.removed" => self.projects.retain(|p| p["id"] != frame["projectId"]),
                    "thread.updated" => upsert(&mut self.threads, frame["thread"].clone())?,
                    "thread.removed" => self.threads.retain(|p| p["id"] != frame["threadId"]),
                    _ => bail!("unsupported T3 shell event"),
                }
            }
        }
        Ok(true)
    }

    pub fn apply_archive(&mut self, frame: &Value) -> Result<bool> {
        if frame["kind"] == "snapshot" {
            self.archive_sequence = frame["snapshot"]["snapshotSequence"]
                .as_u64()
                .context("missing archive sequence")?;
            self.archived_threads = rows(&frame["snapshot"], "threads")?.to_vec();
            return Ok(true);
        }
        let sequence = frame["sequence"]
            .as_u64()
            .context("missing archive sequence")?;
        if sequence <= self.archive_sequence {
            return Ok(false);
        }
        self.archive_sequence = sequence;
        match text(frame, "kind")? {
            "thread.updated" => upsert(&mut self.archived_threads, frame["thread"].clone())?,
            "thread.removed" => self
                .archived_threads
                .retain(|thread| thread["id"] != frame["threadId"]),
            _ => bail!("unsupported T3 archive event"),
        }
        Ok(true)
    }

    pub fn all_threads(&self) -> impl Iterator<Item = &Value> {
        let active: std::collections::HashSet<_> = self
            .threads
            .iter()
            .filter_map(|thread| thread["id"].as_str())
            .collect();
        self.threads.iter().chain(
            self.archived_threads
                .iter()
                .filter(move |archived| !active.contains(archived["id"].as_str().unwrap_or(""))),
        )
    }

    pub fn spaces(&self, environment: &str) -> Result<Vec<Space>> {
        self.projects
            .iter()
            .map(|p| {
                serde_json::from_value(json!({
                    "id":text(p,"id")?, "deviceId":environment, "path":text(p,"workspaceRoot")?,
                    "name":p["title"], "gitDetected":!p["repositoryIdentity"].is_null(),
                    "createdAt":p["createdAt"], "checkoutId":null,
                }))
                .map_err(Into::into)
            })
            .collect()
    }

    pub fn chats(&self, environment: &str, providers: &[Value]) -> Result<Vec<Chat>> {
        self.all_threads()
            .filter(|t| t["deletedAt"].is_null())
            .map(|t| {
                let project = self.projects.iter().find(|p| p["id"] == t["projectId"]);
                let selection = &t["modelSelection"];
                let provider = providers
                    .iter()
                    .find(|p| p["instanceId"] == selection["instanceId"]);
                let options: serde_json::Map<String,Value> = option_map(&selection["options"])?.into_iter()
                    .map(|(id,value)| (id, if let Some(value) = value.as_bool() { json!(value.to_string()) } else { value })).collect();
                let config = provider
                    .and_then(|p| harness(p["driver"].as_str()?))
                    .map(|h| {
                        json!({
                            "harness":h, "model":super::native_model_id(selection["instanceId"].as_str().unwrap_or(""), selection["model"].as_str().unwrap_or("")), "reasoning":null,
                            "modelOptions":options,
                            "sandbox":if t["runtimeMode"]=="full-access" {"danger-full-access"} else {"workspace-write"},
                        })
                    });
                let cwd = t["worktreePath"]
                    .as_str()
                    .or_else(|| project?.get("workspaceRoot")?.as_str());
                Ok(serde_json::from_value(json!({
                    "id":text(t,"id")?, "deviceId":environment, "spaceId":t["projectId"],
                    "title":t["title"], "archived":!t["archivedAt"].is_null(), "cwd":cwd,
                    "branch":t["branch"], "checkoutId":null, "config":config,
                    "lastMessagePreview":t["latestVisibleMessage"]["text"],
                "lastMessageAt":t["latestVisibleMessage"]["updatedAt"],
                    "lastSeenAt":t["lastVisitedAt"], "createdAt":t["createdAt"],
                    "parentChatId":if t["lineage"]["relationshipToParent"] == "subagent" { t["lineage"]["parentThreadId"].clone() } else { Value::Null },
                }))?)
            })
            .collect()
    }

    pub fn sessions(&self, environment: &str) -> Result<Vec<Session>> {
        self.all_threads().map(|t| {
            let status = match t["status"].as_str() {
                Some("preparing" | "starting" | "running") => "working",
                Some("waiting") if t["pendingRuntimeRequest"].is_null() => "working",
                Some("waiting" | "awaiting_input") => "awaitingInput",
                Some("failed" | "errored") => "errored",
                _ => "idle",
            };
            Ok(serde_json::from_value(json!({"chatId":text(t,"id")?, "deviceId":environment,
                "status":status, "startedAt":t["latestRunStartedAt"],
                // Native working indicators have a lease; snapshot receipt renews it.
                "updatedAt":Utc::now(), "lastCompletedTurn":if t["latestRunCompletedAt"].is_null() { Value::Null } else { t["latestRunId"].clone() },
            }))?)
        }).collect()
    }
}

pub fn harness(driver: &str) -> Option<&'static str> {
    Some(match driver {
        "codex" => "codex",
        "claude" | "claude-code" => "claude-code",
        "cursor" => "cursor",
        "opencode" => "opencode",
        "grok" => "grok",
        "pi" => "pi",
        "devin" => "devin",
        "hermes" => "hermes",
        "antigravity" => "antigravity",
        _ => return None,
    })
}

pub struct Projection {
    pub value: Value,
    sequence: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadDetails {
    pub model: String,
    pub branch: Option<String>,
    pub active_agents: usize,
    pub total_agents: usize,
    pub context_tokens: Option<u64>,
    pub git: Option<GitStats>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GitStats {
    pub branch: Option<String>,
    pub additions: u64,
    pub deletions: u64,
}

impl GitStats {
    pub fn apply(current: &mut Option<Self>, frame: Option<&Value>) -> Result<bool> {
        let next = if let Some(frame) = frame {
            match text(frame, "_tag")? {
                "remoteUpdated" => return Ok(false),
                "snapshot" | "localUpdated" => {}
                _ => bail!("unknown T3 Git status event"),
            }
            let local = &frame["local"];
            let is_repo = local["isRepo"]
                .as_bool()
                .context("invalid T3 repository status")?;
            if is_repo {
                let totals = if local["branchChanges"].is_object() {
                    &local["branchChanges"]
                } else {
                    &local["workingTree"]
                };
                Some(Self {
                    branch: if local["refName"].is_null() {
                        None
                    } else {
                        Some(text(local, "refName")?.into())
                    },
                    additions: totals["insertions"]
                        .as_u64()
                        .context("invalid T3 Git additions")?,
                    deletions: totals["deletions"]
                        .as_u64()
                        .context("invalid T3 Git deletions")?,
                })
            } else {
                None
            }
        } else {
            None
        };
        let changed = *current != next;
        *current = next;
        Ok(changed)
    }
}

impl Projection {
    pub fn context_usage(&self) -> Option<zeron_proto::ContextUsage> {
        let active = &self.value["thread"]["activeProviderThreadId"];
        if active.is_null() {
            return None;
        }
        let thread = self.value["providerThreads"]
            .as_array()?
            .iter()
            .find(|thread| thread["id"] == *active);
        let usage = thread
            .filter(|thread| thread["contextUsage"].is_object())
            .map(|thread| &thread["contextUsage"])
            .or_else(|| {
                self.value["providerTurns"]
                    .as_array()?
                    .iter()
                    .filter(|turn| {
                        turn["providerThreadId"] == *active && turn["tokenUsage"].is_object()
                    })
                    .max_by_key(|turn| turn["tokenUsage"]["updatedAt"].as_str().unwrap_or(""))
                    .map(|turn| &turn["tokenUsage"])
            })?;
        Some(zeron_proto::ContextUsage {
            tokens: usage["usedTokens"].as_u64(),
            window: usage["maxTokens"].as_u64().filter(|window| *window > 0),
        })
    }

    pub fn details(&self) -> Result<ThreadDetails> {
        let agents = rows(&self.value, "subagents")?;
        Ok(ThreadDetails {
            model: text(&self.value["thread"]["modelSelection"], "model")?.into(),
            branch: self.value["thread"]["branch"].as_str().map(Into::into),
            active_agents: agents
                .iter()
                .filter(|agent| {
                    matches!(
                        agent["status"].as_str(),
                        Some("pending" | "running" | "waiting")
                    )
                })
                .count(),
            total_agents: agents.len(),
            context_tokens: self.context_usage().and_then(|usage| usage.tokens),
            git: None,
        })
    }
    pub fn snapshot(frame: &Value) -> Result<Self> {
        // The adapter marks opening tails historyPending until the full replay arrives.
        let projection = Self {
            value: frame["projection"].clone(),
            sequence: frame["snapshotSequence"]
                .as_u64()
                .context("missing thread sequence")?,
        };
        text(&projection.value["thread"], "id")?;
        for key in [
            "visibleTurnItems",
            "turnItems",
            "runs",
            "attempts",
            "runtimeRequests",
        ] {
            rows(&projection.value, key)?;
        }
        Ok(projection)
    }

    // Mirrors client-runtime/state/orchestrationV2Projection and shared/orchestrationV2Timeline.
    pub fn apply(&mut self, frame: &Value) -> Result<bool> {
        if frame["kind"] == "synchronized" {
            return Ok(false);
        }
        let sequence = frame["sequence"]
            .as_u64()
            .context("missing thread event sequence")?;
        if sequence <= self.sequence {
            return Ok(false);
        }
        self.sequence = sequence;
        let event = &frame["event"];
        ensure!(
            event["threadId"] == self.value["thread"]["id"],
            "T3 thread event identity mismatch"
        );
        let kind = text(event, "type")?;
        let payload = &event["payload"];
        if kind.starts_with("thread.") && payload["id"] == self.value["thread"]["id"] {
            self.value["thread"] = payload.clone();
        } else {
            let key = match kind {
                "run.created" | "run.updated" => "runs",
                "run-attempt.created" | "run-attempt.updated" => "attempts",
                "node.updated" => "nodes",
                "subagent.updated" => "subagents",
                "provider-session.attached" | "provider-session.updated" => "providerSessions",
                "provider-thread.updated" => "providerThreads",
                "provider-turn.updated" => "providerTurns",
                "runtime-request.updated" => "runtimeRequests",
                "message.updated" => "messages",
                "plan.updated" => "plans",
                "turn-item.updated" => "turnItems",
                "checkpoint-scope.created" => "checkpointScopes",
                "checkpoint.captured" => "checkpoints",
                "context-handoff.updated" => "contextHandoffs",
                "context-transfer.created" | "context-transfer.updated" => "contextTransfers",
                _ => return Ok(false),
            };
            let values = self.value[key]
                .as_array_mut()
                .context("invalid T3 projection array")?;
            upsert(values, payload.clone())?;
            if key == "turnItems" {
                let visible = self.visible(payload);
                let visible_rows = self.value["visibleTurnItems"].as_array_mut().unwrap();
                if visible
                    && let Some(row) = visible_rows.iter_mut().find(|row| {
                        row["sourceItemId"] == payload["id"]
                            && row["item"]["ordinal"] == payload["ordinal"]
                    })
                {
                    // Token growth changes content, not timeline order.
                    row["item"] = payload.clone();
                    return Ok(true);
                }
                visible_rows.retain(|r| r["sourceItemId"] != payload["id"]);
                if visible {
                    let insertion = visible_rows
                        .iter()
                        .position(|r| {
                            r["visibility"] == "local"
                                && (r["item"]["ordinal"].as_u64(), r["item"]["id"].as_str())
                                    > (payload["ordinal"].as_u64(), payload["id"].as_str())
                        })
                        .unwrap_or(visible_rows.len());
                    visible_rows.insert(insertion,json!({"position":insertion,"visibility":"local",
                        "sourceThreadId":payload["threadId"],"sourceItemId":payload["id"],"item":payload}));
                }
            }
            if matches!(key, "runs" | "attempts") || payload["type"] == "run_interrupt_request" {
                let retained: Vec<Value> = rows(&self.value, "visibleTurnItems")?
                    .iter()
                    .filter(|r| r["visibility"] != "local" || self.visible(&r["item"]))
                    .cloned()
                    .collect();
                self.value["visibleTurnItems"] = json!(retained);
            } else if key != "turnItems" {
                return Ok(true);
            }
        }
        if let Some(values) = self.value["visibleTurnItems"].as_array_mut() {
            for (position, row) in values.iter_mut().enumerate() {
                row["position"] = json!(position);
            }
        }
        Ok(true)
    }

    fn visible(&self, item: &Value) -> bool {
        if !item["runId"].is_null()
            && rows(&self.value, "runs")
                .unwrap_or_default()
                .iter()
                .any(|r| {
                    r["id"] == item["runId"]
                        && (r["status"] == "rolled_back"
                            || (r["status"] == "cancelled"
                                && item["type"] == "user_message"
                                && item["inputIntent"] == "queued_turn"))
                })
        {
            return false;
        }
        !(item["type"] == "run_interrupt_result"
            && !item["runId"].is_null()
            && !item["nodeId"].is_null()
            && rows(&self.value, "attempts")
                .unwrap_or_default()
                .iter()
                .any(|a| {
                    a["runId"] == item["runId"]
                        && a["rootNodeId"] == item["nodeId"]
                        && a["status"] == "superseded"
                })
            && !rows(&self.value, "turnItems")
                .unwrap_or_default()
                .iter()
                .any(|i| i["type"] == "run_interrupt_request" && i["runId"] == item["runId"]))
    }

    pub fn entries(&self, environment: &str) -> Result<Vec<SessionMessageEntry>> {
        self.entry_groups()?
            .into_iter()
            .map(|group| self.group_entry(&group, environment))
            .collect()
    }

    fn entry_groups(&self) -> Result<Vec<Vec<&Value>>> {
        let mut groups: Vec<Vec<&Value>> = Vec::new();
        for row in rows(&self.value, "visibleTurnItems")? {
            let item = &row["item"];
            // Checkpoints are workspace metadata, not provider tool calls.
            if item["type"] == "checkpoint" {
                continue;
            }
            let joins = groups.last().is_some_and(|group| {
                let previous = group.last().unwrap();
                !item["runId"].is_null()
                    && item["runId"] == previous["runId"]
                    && item["threadId"] == previous["threadId"]
                    && !matches!(
                        item["type"].as_str(),
                        Some("user_message" | "system_notice" | "notification")
                    )
                    && !matches!(
                        previous["type"].as_str(),
                        Some("user_message" | "system_notice" | "notification")
                    )
            });
            if joins {
                groups.last_mut().unwrap().push(item);
            } else {
                groups.push(vec![item]);
            }
        }
        Ok(groups)
    }

    fn group_entry(&self, group: &[&Value], environment: &str) -> Result<SessionMessageEntry> {
        let mut entry = self.entry(group[0], environment)?;
        for item in &group[1..] {
            let next = self.entry(item, environment)?;
            entry.parts.extend(next.parts);
            entry.status = next.status;
        }
        entry.status = Some(self.group_status(group)?);
        Ok(entry)
    }

    fn group_status(&self, group: &[&Value]) -> Result<zeron_doc::MessageStatus> {
        if rows(&self.value, "runs")?.iter().any(|run| {
            run["id"] == group[0]["runId"]
                && matches!(
                    run["status"].as_str(),
                    Some("completed" | "cancelled" | "failed")
                )
        }) {
            return Ok(zeron_doc::MessageStatus::Complete);
        }
        let last = group.last().unwrap();
        Ok(
            if last["streaming"] == true
                || matches!(
                    last["status"].as_str(),
                    Some("running" | "pending" | "in_progress")
                )
            {
                zeron_doc::MessageStatus::Streaming
            } else {
                zeron_doc::MessageStatus::Complete
            },
        )
    }

    pub fn transcript_delta(
        &self,
        frame: &Value,
        environment: &str,
        previous: &mut Vec<SessionMessageEntry>,
    ) -> Result<TranscriptFrame> {
        if frame["event"]["type"] != "turn-item.updated" {
            let next = self.entries(environment)?;
            let delta = zeron_doc::diff_transcript(previous, &next);
            *previous = next;
            return Ok(delta);
        }
        // Streaming updates decode only the changed item, retaining native text-tail deltas.
        let item = &frame["event"]["payload"];
        let groups = self.entry_groups()?;
        let position = groups
            .iter()
            .position(|group| group.iter().any(|row| row["id"] == item["id"]));
        // Insertion/removal can merge or split adjacent work groups.
        if groups.len() != previous.len()
            || groups.iter().zip(previous.iter()).any(|(group, entry)| {
                group[0]["messageId"].as_str().or(group[0]["id"].as_str())
                    != Some(entry.id.as_str())
                    || group.len() != entry.parts.len()
                    || group
                        .iter()
                        .zip(&entry.parts)
                        .any(|(item, part)| item["id"].as_str() != Some(part.id()))
            })
        {
            let next = self.entries(environment)?;
            let delta = zeron_doc::diff_transcript(previous, &next);
            *previous = next;
            return Ok(delta);
        }
        let Some(position) = position else {
            return Ok(TranscriptFrame::Delta {
                upsert: vec![],
                append: vec![],
                remove: vec![],
                count: previous.len(),
            });
        };
        let changed = self.entry(item, environment)?;
        let mut next = previous[position].clone();
        let part = next
            .parts
            .iter_mut()
            .find(|part| part.id() == item["id"].as_str().unwrap_or(""))
            .context("changed T3 part missing from work group")?;
        *part = changed.parts.into_iter().next().unwrap();
        next.status = Some(self.group_status(&groups[position])?);
        let mut delta = zeron_doc::diff_transcript(
            std::slice::from_ref(&previous[position]),
            std::slice::from_ref(&next),
        );
        if let TranscriptFrame::Delta { upsert, count, .. } = &mut delta {
            if let Some(upsert) = upsert.first_mut() {
                upsert.after = position
                    .checked_sub(1)
                    .map(|index| previous[index].id.clone());
            }
            *count = previous.len();
        }
        previous[position] = next;
        Ok(delta)
    }

    fn entry(&self, item: &Value, environment: &str) -> Result<SessionMessageEntry> {
        let id = text(item, "id")?;
        let kind = text(item, "type")?;
        let complete = !matches!(
            item["status"].as_str(),
            Some("running" | "pending" | "in_progress")
        );
        let part = match kind {
            "user_message" | "assistant_message" => {
                let mut body = item["text"].as_str().unwrap_or_default().to_owned();
                if let Some(attachments) = item["attachments"].as_array().filter(|a| !a.is_empty())
                {
                    let paths = attachments
                        .iter()
                        .map(crate::attachment_path)
                        .collect::<Result<Vec<_>>>()?;
                    body.push_str("\n\nAttached images (local files — open them to view):\n");
                    body.push_str(
                        &paths
                            .iter()
                            .map(|path| format!("- {path}"))
                            .collect::<Vec<_>>()
                            .join("\n"),
                    );
                }
                json!({"kind":"text","id":id,"text":body})
            }
            "reasoning" => json!({"kind":"reasoning","id":id,"text":item["text"]}),
            "proposed_plan" => json!({"kind":"text","id":id,"text":item["markdown"]}),
            "system_notice" => json!({"kind":"text","id":id,"text":item["message"]}),
            "run_interrupt_request" => json!({"kind":"text","id":id,"text":"Stop requested."}),
            "run_interrupt_result" => json!({"kind":"text","id":id,"text":"Agent stopped."}),
            "error" => {
                json!({"kind":"error","id":id,"message":item["failure"]["message"].as_str().unwrap_or("Provider failed")})
            }
            "user_input_request" | "approval_request" => {
                let request_id = text(item, "requestId")?;
                let request = rows(&self.value, "runtimeRequests")?
                    .iter()
                    .find(|r| r["id"] == request_id);
                let questions: Vec<Value> = if kind == "user_input_request" {
                    rows(item,"questions")?.iter().map(|q| json!({"id":q["id"],"header":q["header"],
                        "question":q["question"],"options":q["options"].as_array().into_iter().flatten().map(|o|o["label"].clone()).collect::<Vec<_>>(),
                        "multiSelect":q["multiSelect"].as_bool().unwrap_or(false)})).collect()
                } else {
                    let options: Vec<_> = approval_choices(item)?
                        .into_iter()
                        .map(|(label, _)| label)
                        .collect();
                    vec![
                        json!({"id":request_id,"header":item["appName"].as_str().unwrap_or("Approval"),"question":item["prompt"].as_str().unwrap_or("Allow this action?"),
                        "options":options,"multiSelect":false}),
                    ]
                };
                json!({"kind":"input","id":id,"requestId":request_id,"questions":questions,
                    "resolved":request.is_some_and(|r| r["status"] != "pending")})
            }
            _ => {
                if kind == "dynamic_tool"
                    && let Some(reference) = crate::visual_reference(&item["output"])
                {
                    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
                    let resource = json!({"_tag":"attachment","attachmentId":reference["attachmentId"],"fileName":"visual.html","mimeType":"text/html","disposition":"inline"});
                    let title = reference["title"]
                        .as_str()
                        .unwrap_or("Interactive visual")
                        .replace(['[', ']', '\n', '\r'], " ");
                    let link = format!(
                        "[Open {title}](t3-visual:{})",
                        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&resource)?)
                    );
                    return self.entry(&json!({"id":id,"type":"assistant_message","text":link,"startedAt":item["startedAt"],"updatedAt":item["updatedAt"],"status":item["status"]}), environment);
                }
                let call = match kind {
                    "command_execution" => json!({"kind":"exec","command":item["input"]}),
                    "file_change" => json!({"kind":"applyPatch","path":text(item,"fileName")?}),
                    "todo_list" => json!({"kind":"todo","items":rows(item,"steps")?.iter().map(|s|
                        json!({"text":s["text"].as_str().or_else(||s["step"].as_str()).unwrap_or("Task"),
                            "done":s["status"]=="completed", "status":if s["status"]=="running" {json!("inProgress")} else {Value::Null}})).collect::<Vec<_>>()}),
                    "subagent" => {
                        json!({"kind":"unknown","name":"Agent","input":{"prompt":item["prompt"],"model":item["model"]}})
                    }
                    _ => {
                        json!({"kind":"unknown","name":item["toolName"].as_str().or_else(||item["title"].as_str()).unwrap_or(kind),"input":item.get("input")})
                    }
                };
                let mut part = json!({"kind":"tool","id":id,"call":call,"resolved":complete,
                    "isError":item["status"]=="failed" || item["outputIndicatesFailure"]==true,"output":item["output"].as_str().map(str::to_string).or_else(||item["result"].as_str().map(str::to_string)).or_else(|| (!item["output"].is_null()).then(||item["output"].to_string())),
                    "subagentRef":item["childThreadId"], "subagentTail":item["progress"]});
                if kind == "subagent" {
                    part["subagentStatus"] = json!(if !complete {
                        "running"
                    } else if item["status"] == "failed" {
                        "failed"
                    } else {
                        "done"
                    });
                }
                part
            }
        };
        // Typed native parts keep malformed remote data out of renderers.
        let part: MessagePart = serde_json::from_value(part)?;
        let time = item["startedAt"]
            .as_str()
            .or_else(|| item["updatedAt"].as_str())
            .context("missing item timestamp")?;
        let role = if kind == "user_message" {
            "user"
        } else if matches!(kind, "system_notice" | "notification") {
            "system"
        } else {
            "assistant"
        };
        Ok(serde_json::from_value(
            json!({"id":item["messageId"].as_str().unwrap_or(id),"role":role,
                "parts":[part],"createdAt":DateTime::parse_from_rfc3339(time)?.timestamp_millis(),"deviceId":environment,
                "status":if item["streaming"]==true || !complete {"streaming"} else {"complete"},
            }),
        )?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_turn_groups_reasoning_and_tools_without_fake_checkpoint_calls() {
        let items = vec![
            json!({"id":"user","type":"user_message","text":"Fix the bug","runId":"run"}),
            json!({"id":"thought","type":"reasoning","text":"Checking the code","runId":"run"}),
            json!({"id":"read","type":"command_execution","input":"cat src/lib.rs","output":"code","runId":"run"}),
            json!({"id":"test","type":"command_execution","input":"cargo test","output":"passed","runId":"run"}),
            json!({"id":"reply","type":"assistant_message","text":"Fixed","runId":"run","streaming":true,"status":"running"}),
            json!({"id":"checkpoint","type":"checkpoint","runId":"run"}),
        ].into_iter().enumerate().map(|(ordinal, mut item)| {
            item["threadId"] = json!("thread");
            item["ordinal"] = json!(ordinal);
            item["startedAt"] = json!("2026-10-09T00:00:00Z");
            item["updatedAt"] = json!("2026-10-09T00:00:00Z");
            item
        }).collect::<Vec<_>>();
        let visible = items
            .iter()
            .map(|item| json!({"visibility":"local","sourceItemId":item["id"],"item":item}))
            .collect::<Vec<_>>();
        let mut projection = Projection::snapshot(&json!({"kind":"snapshot","snapshotSequence":1,"hasMoreHistory":true,"projection":{
            "thread":{"id":"thread"},"runs":[{"id":"run","status":"running"}],"attempts":[],"runtimeRequests":[],"turnItems":items,"visibleTurnItems":visible
        }})).unwrap();
        let mut previous = projection.entries("env").unwrap();
        assert_eq!(previous.len(), 2);
        assert_eq!(
            previous[1]
                .parts
                .iter()
                .map(MessagePart::id)
                .collect::<Vec<_>>(),
            ["thought", "read", "test", "reply"]
        );
        let mut applied = previous.clone();
        let mut reply = items[4].clone();
        reply["text"] = json!("Fixed the bug.");
        let frame = json!({"kind":"event","sequence":2,"event":{"type":"turn-item.updated","threadId":"thread","payload":reply}});
        projection.apply(&frame).unwrap();
        let delta = projection
            .transcript_delta(&frame, "env", &mut previous)
            .unwrap();
        assert!(
            matches!(&delta, TranscriptFrame::Delta { append, upsert, count: 2, .. } if append.len() == 1 && upsert.is_empty())
        );
        zeron_doc::apply_transcript_frame(&mut applied, delta).unwrap();
        assert_eq!(applied, projection.entries("env").unwrap());
        let mut extra = items[3].clone();
        extra["id"] = json!("extra-tool");
        extra["ordinal"] = json!(3);
        let frame = json!({"kind":"event","sequence":3,"event":{"type":"turn-item.updated","threadId":"thread","payload":extra}});
        projection.apply(&frame).unwrap();
        let delta = projection
            .transcript_delta(&frame, "env", &mut previous)
            .unwrap();
        zeron_doc::apply_transcript_frame(&mut applied, delta).unwrap();
        assert_eq!(applied, projection.entries("env").unwrap());
        assert_eq!(applied[1].parts.len(), 5);
        projection.apply(&json!({"kind":"event","sequence":4,"event":{"type":"run.updated","threadId":"thread","payload":{"id":"run","status":"completed"}}})).unwrap();
        let next = projection.entries("env").unwrap();
        assert_eq!(next[1].status, Some(zeron_doc::MessageStatus::Complete));
    }

    #[test]
    fn forks_stay_in_the_sidebar_but_delegated_children_do_not() {
        let shell = Shell {
            threads: ["fork", "subagent"].into_iter().map(|kind| json!({
                "id":kind,"projectId":"project","title":"Child","createdAt":"2026-10-09T00:00:00Z",
                "lineage":{"parentThreadId":"parent","relationshipToParent":kind}
            })).collect(),
            ..Default::default()
        };
        let chats = shell.chats("environment", &[]).unwrap();
        assert_eq!(chats[0].parent_chat_id, None);
        assert_eq!(chats[1].parent_chat_id.as_deref(), Some("parent"));
    }

    #[test]
    fn git_counts_use_branch_changes_and_clear_when_unavailable() {
        let mut current = None;
        let mut frame = json!({"_tag":"snapshot","local":{"isRepo":true,"refName":"feature/native",
            "workingTree":{"insertions":1,"deletions":2},"branchChanges":{"insertions":100,"deletions":50}}});
        assert!(GitStats::apply(&mut current, Some(&frame)).unwrap());
        assert_eq!(current.as_ref().unwrap().additions, 100);
        assert!(
            !GitStats::apply(
                &mut current,
                Some(&json!({"_tag":"remoteUpdated","remote":null}))
            )
            .unwrap()
        );
        assert_eq!(
            current.as_ref().unwrap().branch.as_deref(),
            Some("feature/native")
        );
        frame["_tag"] = json!("localUpdated");
        frame["local"]["branchChanges"] = Value::Null;
        GitStats::apply(&mut current, Some(&frame)).unwrap();
        assert_eq!(current.as_ref().unwrap().deletions, 2);
        frame["local"]["workingTree"]["insertions"] = json!(-1);
        assert!(GitStats::apply(&mut current, Some(&frame)).is_err());
        assert!(GitStats::apply(&mut current, None).unwrap());
        assert!(current.is_none());
        frame["local"]["isRepo"] = json!(false);
        assert!(!GitStats::apply(&mut current, Some(&frame)).unwrap());
    }

    #[test]
    fn context_meter_uses_only_the_active_provider_thread() {
        let mut projection = Projection {
            sequence: 1,
            value: json!({"thread":{"activeProviderThreadId":"current"},
            "providerThreads":[{"id":"other","contextUsage":{"usedTokens":9000,"maxTokens":10000}},{"id":"current","contextUsage":{"usedTokens":100,"maxTokens":1000}}],
            "providerTurns":[{"providerThreadId":"current","tokenUsage":{"usedTokens":50,"maxTokens":1000,"updatedAt":"2026-10-09T00:00:00Z"}}]}),
        };
        assert_eq!(projection.context_usage().unwrap().tokens, Some(100));
        projection.value["providerThreads"][1]["contextUsage"] = Value::Null;
        assert_eq!(projection.context_usage().unwrap().tokens, Some(50));
        projection.value["thread"]["activeProviderThreadId"] = Value::Null;
        assert!(projection.context_usage().is_none());
    }

    #[test]
    fn repository_enrichment_preserves_threads_and_event_cursor() {
        let mut shell = Shell::default();
        shell.apply(&json!({"kind":"snapshot","snapshot":{"snapshotSequence":1,"projects":[{"id":"project","workspaceRoot":"/repo","title":"Current title"}],"threads":[{"id":"active"}]}})).unwrap();
        shell.apply_archive(&json!({"kind":"snapshot","snapshot":{"snapshotSequence":1,"threads":[{"id":"archive"}]}})).unwrap();
        shell.apply(&json!({"kind":"snapshot","resolvedRepositoryIdentityRoots":["/repo"],"snapshot":{"snapshotSequence":10,"projects":[{"id":"project","workspaceRoot":"/repo","title":"Stale title","repositoryIdentity":{"id":"repo"}}],"threads":[],"archivedThreads":[]}})).unwrap();
        assert_eq!(shell.all_threads().count(), 2);
        assert_eq!(shell.projects[0]["title"], "Current title");
        assert_eq!(shell.projects[0]["repositoryIdentity"]["id"], "repo");
        assert!(
            shell
                .apply(&json!({"kind":"thread.updated","sequence":2,"thread":{"id":"new"}}))
                .unwrap()
        );
        assert_eq!(shell.all_threads().count(), 3);
        shell
            .apply_archive(&json!({"kind":"thread.updated","sequence":3,"thread":{"id":"active"}}))
            .unwrap();
        shell
            .apply(&json!({"kind":"thread.removed","sequence":3,"threadId":"active"}))
            .unwrap();
        assert_eq!(shell.all_threads().count(), 3);
        shell
            .apply(&json!({"kind":"thread.updated","sequence":4,"thread":{"id":"archive"}}))
            .unwrap();
        shell
            .apply_archive(&json!({"kind":"thread.removed","sequence":4,"threadId":"archive"}))
            .unwrap();
        assert_eq!(shell.all_threads().count(), 3);
    }

    #[test]
    fn canonical_model_options_are_native_picker_values() {
        let shell = Shell {
            threads: vec![
                json!({"id":"thread","projectId":"project","title":"Test","createdAt":"2026-10-09T00:00:00Z",
            "runtimeMode":"approval-required","modelSelection":{"instanceId":"provider","model":"model","options":[{"id":"effort","value":"high"},{"id":"fast","value":true}]}}),
            ],
            ..Default::default()
        };
        let chats = shell
            .chats("env", &[json!({"instanceId":"provider","driver":"codex"})])
            .unwrap();
        let value = serde_json::to_value(&chats[0]).unwrap();
        assert_eq!(value["config"]["modelOptions"]["effort"], "high");
        assert_eq!(value["config"]["modelOptions"]["fast"], "true");
    }

    #[test]
    fn live_text_and_rollback_keep_native_history_consistent() {
        let item = json!({"id":"item","threadId":"thread","runId":"run","nodeId":"node","type":"assistant_message",
            "ordinal":0,"text":"hello","streaming":true,"status":"running","updatedAt":"2026-10-09T00:00:00Z"});
        let mut projection = Projection::snapshot(&json!({"kind":"snapshot","snapshotSequence":1,"projection":{
            "thread":{"id":"thread"},"runs":[{"id":"run","status":"running"}],"attempts":[],"runtimeRequests":[],
            "turnItems":[item],"visibleTurnItems":[{"visibility":"local","sourceItemId":"item","item":item}]}})).unwrap();
        let mut previous = projection.entries("env").unwrap();
        let mut applied = previous.clone();
        let mut updated = item.clone();
        updated["text"] = json!("hello world");
        let frame = json!({"kind":"event","sequence":2,"event":{"type":"turn-item.updated","threadId":"thread","payload":updated}});
        projection.apply(&frame).unwrap();
        let next = projection.entries("env").unwrap();
        let delta = projection
            .transcript_delta(&frame, "env", &mut previous)
            .unwrap();
        assert!(
            matches!(&delta, TranscriptFrame::Delta { append, upsert, .. } if append.len() == 1 && upsert.is_empty())
        );
        zeron_doc::apply_transcript_frame(&mut applied, delta).unwrap();
        assert_eq!(applied, next);
        projection.apply(&json!({"kind":"event","sequence":3,"event":{"type":"run.updated","threadId":"thread","payload":{"id":"run","status":"rolled_back"}}})).unwrap();
        assert!(projection.entries("env").unwrap().is_empty());
        assert!(
            !projection
                .apply(&json!({"kind":"event","sequence":2}))
                .unwrap()
        );
    }

    #[test]
    fn streaming_changes_keep_predecessor_and_completion_state() {
        let items: Vec<Value> = (0..10).map(|n| json!({"id":format!("item-{n}"),"threadId":"thread",
            "type":"assistant_message","ordinal":n,"text":"hello","streaming":true,"status":"running",
            "startedAt":"2026-10-09T00:00:00Z","updatedAt":"2026-10-09T00:00:00Z"})).collect();
        let visible: Vec<Value> = items
            .iter()
            .map(|item| json!({"visibility":"local","sourceItemId":item["id"],"item":item}))
            .collect();
        let mut projection = Projection::snapshot(&json!({"kind":"snapshot","snapshotSequence":1,"projection":{
            "thread":{"id":"thread"},"runs":[],"attempts":[],"runtimeRequests":[],"turnItems":items,"visibleTurnItems":visible}})).unwrap();
        let mut previous = projection.entries("env").unwrap();
        let mut applied = previous.clone();
        for sequence in 2..5 {
            let mut item = items[4].clone();
            item["text"] = json!(format!("hello {}", "world ".repeat(sequence)));
            if sequence == 4 {
                item["status"] = json!("completed");
                item["streaming"] = json!(false);
            }
            let frame = json!({"kind":"event","sequence":sequence,"event":{"type":"turn-item.updated","threadId":"thread","payload":item}});
            projection.apply(&frame).unwrap();
            let delta = projection
                .transcript_delta(&frame, "env", &mut previous)
                .unwrap();
            assert!(matches!(&delta, TranscriptFrame::Delta { count: 10, .. }));
            zeron_doc::apply_transcript_frame(&mut applied, delta).unwrap();
            assert_eq!(applied, projection.entries("env").unwrap());
        }
    }
}
