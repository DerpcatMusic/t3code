//! T3 owns providers, workspaces and durable history. This crate only adapts views.

mod attachments;
mod projection;
mod sidebar;
mod transport;
mod workspace;

pub use attachments::{attachment_path, parse_attachment_path, visual_reference};
pub use projection::{GitStats, ThreadDetails};
pub use sidebar::{
    SidebarCapabilities, SidebarSection, SidebarThread, sidebar_pins, snooze_presets,
};

use anyhow::{Context, Result, ensure};
use async_trait::async_trait;
use futures::{StreamExt, stream};
use projection::{Projection, Shell, approval_choices, harness, option_map, rows, text};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::sync::{RwLock, watch};
use transport::ConnectionConfig;
use zeron_doc::{SessionCommandPayload, TranscriptBaseline, TranscriptFrame, TranscriptUpdate};
use zeron_proto::{EngineInfo, UserInputAnswer, WorkspaceScope};
use zeron_rpc::{RpcClient, RpcError, RpcReply, RpcService, methods};

pub const CAPABILITY: &str = "t3.orchestration.v2";

/// Private bootstrap data travels over stdin, never through a URL or command line.
pub async fn browser_bootstrap(path: &Path) -> Result<Value> {
    ConnectionConfig::load(path)?.browser_bootstrap().await
}

pub struct T3Service {
    client: Arc<RwLock<Option<Arc<RpcClient>>>>,
    shell: watch::Receiver<Arc<Shell>>,
    connectivity: watch::Receiver<Value>,
    pub engine_info: EngineInfo,
    pub origin: String,
    attachments: attachments::Attachments,
    connection_path: PathBuf,
    project_write: tokio::sync::Mutex<()>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for T3Service {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl T3Service {
    pub async fn connect(path: &Path) -> Result<Arc<Self>> {
        let config = ConnectionConfig::load(path)?;
        let session = config.connect().await?;
        let client = session.client.clone();
        // Read the server catalog before exposing Ready to the native shell.
        let server = tokio::time::timeout(
            Duration::from_secs(15),
            client.call("server.getConfig", json!({})),
        )
        .await??;
        ensure!(
            server["environment"]["environmentId"] == config.environment_id,
            "T3 config identity mismatch"
        );
        let mut subscription = tokio::time::timeout(
            Duration::from_secs(15),
            client.subscribe_checked("orchestration.subscribeShell", json!({})),
        )
        .await??;
        let first = tokio::time::timeout(Duration::from_secs(15), subscription.recv())
            .await?
            .context("T3 shell stream ended before its snapshot")?;
        let mut shell = Shell::default();
        shell.capabilities = SidebarCapabilities::from_config(&server)?;
        shell.apply(&first)?;
        let mut archives = tokio::time::timeout(
            Duration::from_secs(15),
            client.subscribe_checked("orchestration.subscribeArchivedShell", json!({})),
        )
        .await??;
        let first = tokio::time::timeout(Duration::from_secs(15), archives.recv())
            .await?
            .context("T3 archive stream ended before its snapshot")?;
        shell.apply_archive(&first)?;
        let (shell_tx, shell_rx) = watch::channel(Arc::new(shell));
        let (connectivity_tx, connectivity) = watch::channel(json!({"state":"connected"}));
        let client_slot = Arc::new(RwLock::new(Some(client)));
        let task_slot = client_slot.clone();
        let task_config = config.clone();
        let task = tokio::spawn(async move {
            let mut session = session;
            let mut heartbeat = tokio::time::interval(Duration::from_secs(20));
            loop {
                let disconnected = loop {
                    tokio::select! {
                        _ = session.closed.changed() => break true,
                        next = subscription.recv() => {
                            let Some(next) = next else { break true; };
                            let mut shell = shell_tx.borrow().as_ref().clone();
                            match shell.apply(&next) {
                                Ok(true) => { let _ = shell_tx.send(Arc::new(shell)); }
                                Ok(false) => {},
                                Err(_) => { tracing::warn!("invalid T3 shell projection; reconnecting"); break true; }
                            }
                        }
                        next = archives.recv() => {
                            let Some(next) = next else { break true; };
                            let mut shell = shell_tx.borrow().as_ref().clone();
                            match shell.apply_archive(&next) {
                                Ok(true) => { let _ = shell_tx.send(Arc::new(shell)); }
                                Ok(false) => {},
                                Err(_) => { tracing::warn!("invalid T3 archive projection; reconnecting"); break true; }
                            }
                        }
                        _ = heartbeat.tick() => {
                            // Renew the native session liveness lease without polling T3.
                            let current = shell_tx.borrow().clone();
                            let _ = shell_tx.send(current);
                        }
                    }
                };
                if !disconnected {
                    break;
                }
                *task_slot.write().await = None;
                let _ = connectivity_tx.send(
                    json!({"state":"reconnecting","lastFailure":"T3 connection interrupted"}),
                );
                let mut delay = 1;
                loop {
                    tokio::time::sleep(Duration::from_secs(delay)).await;
                    let connect = tokio::time::timeout(Duration::from_secs(45), async {
                        let next = task_config.connect().await?;
                        let server = next.client.call("server.getConfig", json!({})).await?;
                        ensure!(
                            server["environment"]["environmentId"] == task_config.environment_id,
                            "T3 config identity mismatch"
                        );
                        let mut rx = next
                            .client
                            .subscribe_checked("orchestration.subscribeShell", json!({}))
                            .await?;
                        let first = rx.recv().await.context("T3 shell stream ended")?;
                        let mut shell = Shell::default();
                        shell.capabilities = SidebarCapabilities::from_config(&server)?;
                        shell.apply(&first)?;
                        let mut archives = next
                            .client
                            .subscribe_checked("orchestration.subscribeArchivedShell", json!({}))
                            .await?;
                        let first = archives.recv().await.context("T3 archive stream ended")?;
                        shell.apply_archive(&first)?;
                        Ok::<_, anyhow::Error>((next, rx, archives, shell))
                    })
                    .await;
                    match connect {
                        Ok(Ok((next, rx, archive_rx, shell))) => {
                            *task_slot.write().await = Some(next.client.clone());
                            let _ = shell_tx.send(Arc::new(shell));
                            let _ = connectivity_tx.send(json!({"state":"connected"}));
                            session = next;
                            subscription = rx;
                            archives = archive_rx;
                            break;
                        }
                        _ => delay = (delay * 2).min(60),
                    }
                }
            }
        });
        Ok(Arc::new(Self {
            client: client_slot,
            shell: shell_rx,
            connectivity,
            engine_info: EngineInfo {
                device_id: config.environment_id,
                workspace_scope: WorkspaceScope::Local,
                cursor_sdk_version: None,
                capabilities: vec![CAPABILITY.into()],
            },
            attachments: attachments::Attachments::new(&config.origin)?,
            origin: config.origin,
            connection_path: path.to_owned(),
            project_write: tokio::sync::Mutex::new(()),
            task,
        }))
    }

    async fn client(&self) -> Result<Arc<RpcClient>, RpcError> {
        self.client
            .read()
            .await
            .clone()
            .ok_or_else(|| RpcError::Failed("T3 is reconnecting; try again when connected".into()))
    }

    async fn providers(&self) -> Result<Vec<Value>, RpcError> {
        let config = self
            .client()
            .await?
            .call("server.getConfig", json!({}))
            .await?;
        rows(&config, "providers")
            .map(<[Value]>::to_vec)
            .map_err(failed)
    }

    async fn dispatch(&self, mut command: Value) -> Result<Value, RpcError> {
        command["commandId"] = command
            .get("commandId")
            .cloned()
            .unwrap_or_else(|| json!(uuid::Uuid::new_v4().to_string()));
        // A lost acknowledgement is never automatically replayed.
        self.client()
            .await?
            .call("orchestration.dispatchCommand", command)
            .await
    }

    async fn thread(&self, id: &str) -> Result<Value, RpcError> {
        if let Some(thread) = self.shell.borrow().all_threads().find(|t| t["id"] == id) {
            return Ok(thread.clone());
        }
        let projection = self
            .client()
            .await?
            .call("orchestration.getThreadProjection", json!({"threadId":id}))
            .await?;
        Ok(projection["thread"].clone())
    }

    async fn visit(&self, id: &str) -> Result<Value, RpcError> {
        visit_command(id, &self.thread(id).await?)
    }

    async fn mutate(&self, params: Value) -> Result<Value, RpcError> {
        let op = text(&params, "op").map_err(failed)?;
        let id = text(&params, "chatId").map_err(failed)?;
        if op == "moveChatToSection" {
            let commands = {
                let shell = self.shell.borrow();
                let thread = shell
                    .all_threads()
                    .find(|thread| thread["id"] == id)
                    .ok_or_else(|| RpcError::BadParams("T3 thread no longer exists".into()))?;
                SidebarThread::from_shell(thread, &shell.capabilities)
                    .map_err(failed)?
                    .section_commands(&params, chrono::Utc::now())
                    .map_err(failed)?
            };
            for command in commands {
                self.dispatch(command).await?;
            }
            return Ok(json!({"ok":true}));
        }
        if op == "changeChatPin" {
            let change: zeron_proto::SidebarPinChange =
                serde_json::from_value(params["change"].clone()).map_err(failed)?;
            if change.session_id() != id {
                return Err(RpcError::BadParams("pin change thread mismatch".into()));
            }
            let commands = {
                let shell = self.shell.borrow();
                let threads = shell
                    .all_threads()
                    .filter(|thread| thread["deletedAt"].is_null())
                    .map(|thread| {
                        Ok((
                            text(thread, "id")?.to_owned(),
                            SidebarThread::from_shell(thread, &shell.capabilities)?,
                        ))
                    })
                    .collect::<Result<std::collections::HashMap<_, _>>>()
                    .map_err(failed)?;
                sidebar::pin_change_commands(&threads, &change, chrono::Utc::now())
                    .map_err(failed)?
            };
            for command in commands {
                self.dispatch(command).await?;
            }
            return Ok(json!({"ok":true}));
        }
        if op == "setChatConfig" {
            let thread = self.thread(id).await?;
            let providers = self.providers().await?;
            let config = &params["config"];
            let selection = model_selection(
                &providers,
                &thread["modelSelection"],
                config["harness"].as_str(),
                config["model"].as_str(),
            )?;
            let mut selection = selection;
            selection["options"] = json!(
                option_map(&config["modelOptions"])
                    .map_err(failed)?
                    .into_iter()
                    .map(|(id, value)| json!({"id":id,"value":value}))
                    .collect::<Vec<_>>()
            );
            self.dispatch(json!({"type":if selection["instanceId"] == thread["modelSelection"]["instanceId"] {"thread.model-selection.set"} else {"provider.switch"}, "threadId":id,"modelSelection":selection})).await?;
            return Ok(json!({"ok":true}));
        }
        let command = match op {
            "renameChat" => {
                json!({"type":"thread.metadata.update","threadId":id,"title":text(&params,"title").map_err(failed)?})
            }
            "setChatArchived" => {
                let archived = params["archived"]
                    .as_bool()
                    .ok_or_else(|| RpcError::BadParams("archived must be boolean".into()))?;
                json!({"type":if archived {"thread.archive"} else {"thread.unarchive"},"threadId":id})
            }
            "markChatSeen" => self.visit(id).await?,
            "deleteChat" => json!({"type":"thread.delete","threadId":id}),
            "settleChat"
            | "unsettleChat"
            | "snoozeChat"
            | "wakeChat"
            | "pinChat"
            | "unpinChat"
            | "setChatAutoSettle"
            | "markChatUnread"
            | "regenerateChatTitle" => {
                let shell = self.shell.borrow();
                let thread = shell
                    .all_threads()
                    .find(|thread| thread["id"] == id)
                    .ok_or_else(|| RpcError::BadParams("T3 thread no longer exists".into()))?;
                SidebarThread::from_shell(thread, &shell.capabilities)
                    .map_err(failed)?
                    .command(&params, chrono::Utc::now())
                    .map_err(failed)?
            }
            "createChat" => {
                let project_id = text(&params, "spaceId").map_err(failed)?;
                let shell = self.shell.borrow().clone();
                let project = shell
                    .projects
                    .iter()
                    .find(|p| p["id"] == project_id)
                    .ok_or_else(|| RpcError::BadParams("select an existing T3 project".into()))?;
                let providers = self.providers().await?;
                let selection = model_selection(
                    &providers,
                    &project["defaultModelSelection"],
                    params["config"]["harness"].as_str(),
                    params["config"]["model"].as_str(),
                )?;
                if selection.is_null() {
                    return Err(RpcError::BadParams(
                        "set this project's default model in T3 first".into(),
                    ));
                }
                // Native worktree creation needs T3 launch preparation, not a path-only mutation.
                if params["cwd"]
                    .as_str()
                    .is_some_and(|cwd| Some(cwd) != project["workspaceRoot"].as_str())
                    || !params["parentChatId"].is_null()
                {
                    return Err(RpcError::BadParams(
                        "worktree and fork creation are not migrated yet; use T3".into(),
                    ));
                }
                json!({"type":"thread.create","threadId":id,"projectId":project_id,
                    "title":params["title"].as_str().filter(|s|!s.trim().is_empty()).unwrap_or("New session"),
                    "modelSelection":selection,"runtimeMode":"approval-required","interactionMode":"default",
                    "branch":params["branch"],"worktreePath":null,"createdBy":"user","creationSource":"web"})
            }
            // Zeron account and sync mutations are not T3 operations.
            _ => return Err(RpcError::UnknownMethod(format!("T3 native mutation {op}"))),
        };
        self.dispatch(command).await?;
        Ok(json!({"ok":true}))
    }

    async fn command(&self, params: Value) -> Result<Value, RpcError> {
        let id = text(&params, "chatId").map_err(failed)?;
        let command: SessionCommandPayload = serde_json::from_value(params["command"].clone())
            .map_err(|e| RpcError::BadParams(e.to_string()))?;
        let thread = self.thread(id).await?;
        let mut command_id = uuid::Uuid::new_v4().to_string();
        let mapped = match command {
            SessionCommandPayload::Run {
                request,
                message_id,
            } => {
                command_id = format!("zeron:{id}:{message_id}");
                if request.worktree.is_some()
                    || params["transfers"]
                        .as_array()
                        .is_some_and(|a| !a.is_empty())
                {
                    return Err(RpcError::BadParams(
                        "Use T3's worktree controls to prepare this thread.".into(),
                    ));
                }
                let providers = self.providers().await?;
                let requested_harness = request
                    .harness
                    .map(serde_json::to_value)
                    .transpose()
                    .map_err(failed)?;
                let explicit_model = request.model.is_some();
                let mut selection = model_selection(
                    &providers,
                    &thread["modelSelection"],
                    requested_harness.as_ref().and_then(Value::as_str),
                    request.model.as_deref(),
                )?;
                let current = providers
                    .iter()
                    .find(|p| p["instanceId"] == selection["instanceId"]);
                let mut options = if explicit_model {
                    Default::default()
                } else {
                    option_map(&selection["options"]).map_err(failed)?
                };
                for (id, value) in request.model_options {
                    options.insert(id, value);
                }
                let model = current.and_then(|provider| {
                    provider["models"]
                        .as_array()?
                        .iter()
                        .find(|model| model["slug"] == selection["model"])
                });
                if let Some(descriptors) =
                    model.and_then(|model| model["capabilities"]["optionDescriptors"].as_array())
                {
                    for descriptor in descriptors
                        .iter()
                        .filter(|descriptor| descriptor["type"] == "boolean")
                    {
                        if let Some(value) =
                            descriptor["id"].as_str().and_then(|id| options.get_mut(id))
                        {
                            if let Some(boolean) = match value.as_str() {
                                Some("true") => Some(true),
                                Some("false") => Some(false),
                                _ => None,
                            } {
                                *value = json!(boolean);
                            }
                        }
                    }
                }
                if let Some(reasoning) = request.reasoning {
                    let descriptor = model.and_then(|model| {
                        model["capabilities"]["optionDescriptors"]
                            .as_array()?
                            .iter()
                            .find(|descriptor| {
                                matches!(
                                    descriptor["id"].as_str(),
                                    Some("reasoningEffort" | "effort")
                                )
                            })
                    });
                    let id = descriptor
                        .and_then(|descriptor| descriptor["id"].as_str())
                        .ok_or_else(|| {
                            RpcError::BadParams(
                                "this T3 model does not advertise a reasoning option".into(),
                            )
                        })?;
                    options.insert(id.into(), serde_json::to_value(reasoning).map_err(failed)?);
                }
                selection["options"] = json!(
                    options
                        .into_iter()
                        .map(|(id, value)| json!({"id":id,"value":value}))
                        .collect::<Vec<_>>()
                );
                let attachments = request
                    .attachments
                    .iter()
                    .map(|path| {
                        parse_attachment_path(path).ok_or_else(|| {
                            RpcError::BadParams("Attach the file again before sending.".into())
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                let prompt =
                    attachments::prompt_without_refs(&request.prompt, &request.attachments);
                json!({"type":"message.dispatch","threadId":id,"messageId":message_id,"text":prompt,
                    "attachments":attachments,"modelSelection":selection,"deliveryIntent":"auto",
                    "dispatchMode":{"type":"queue_after_active"},"createdBy":"user","creationSource":"web"})
            }
            SessionCommandPayload::Steer { prompt, message_id } => {
                let active = text(&thread, "activeRunId").map_err(failed)?;
                json!({"type":"message.dispatch","threadId":id,"messageId":message_id.unwrap_or_else(||uuid::Uuid::new_v4().to_string()),
                    "text":prompt,"attachments":[],"dispatchMode":{"type":"steer_active","targetRunId":active},
                    "createdBy":"user","creationSource":"web"})
            }
            SessionCommandPayload::Interrupt {} => json!({"type":"run.interrupt","threadId":id,
                "runId":text(&thread,"activeRunId").map_err(failed)?,"holdQueue":true}),
            SessionCommandPayload::RespondInput {
                request_id,
                answers,
            } => {
                let projection = self
                    .client()
                    .await?
                    .call("orchestration.getThreadProjection", json!({"threadId":id}))
                    .await?;
                input_response(id, &request_id, answers, &projection)?
            }
        };
        let mut mapped = mapped;
        mapped["commandId"] = json!(command_id);
        self.dispatch(mapped).await?;
        Ok(json!({"commandId":command_id}))
    }
}

fn failed(error: impl std::fmt::Display) -> RpcError {
    RpcError::Failed(error.to_string())
}

fn visit_command(id: &str, thread: &Value) -> Result<Value, RpcError> {
    let seen = text(thread, "updatedAt").map_err(failed)?;
    chrono::DateTime::parse_from_rfc3339(seen).map_err(failed)?;
    Ok(json!({"type":"thread.visit","threadId":id,"visitedAt":seen}))
}

pub(crate) fn native_model_id(instance: &str, model: &str) -> String {
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    format!(
        "t3-model:{}",
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&(instance, model)).expect("string pair"))
    )
}

fn model_selection(
    providers: &[Value],
    fallback: &Value,
    requested_harness: Option<&str>,
    model: Option<&str>,
) -> Result<Value, RpcError> {
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    let encoded = model.and_then(|m| m.strip_prefix("t3-model:"));
    let decoded: Option<(String, String)> = encoded
        .map(|s| {
            if s.len() > 8192 {
                return Err(RpcError::BadParams("Invalid model selection".into()));
            }
            let bytes = URL_SAFE_NO_PAD.decode(s).map_err(failed)?;
            serde_json::from_slice(&bytes).map_err(failed)
        })
        .transpose()?;
    let instance = decoded.as_ref().map(|p| p.0.as_str());
    let slug = decoded
        .as_ref()
        .map(|p| p.1.as_str())
        .or(model)
        .or(fallback["model"].as_str());
    let eligible = |p: &&Value| {
        p["enabled"] == true
            && requested_harness
                .is_none_or(|h| harness(p["driver"].as_str().unwrap_or("")) == Some(h))
    };
    let provider = if let Some(instance) = instance {
        providers
            .iter()
            .filter(eligible)
            .find(|p| p["instanceId"] == instance)
    } else {
        providers
            .iter()
            .filter(eligible)
            .find(|p| {
                p["instanceId"] == fallback["instanceId"]
                    && slug.is_none_or(|slug| {
                        p["models"]
                            .as_array()
                            .is_some_and(|m| m.iter().any(|m| m["slug"] == slug))
                    })
            })
            .or_else(|| {
                let mut matches = providers.iter().filter(eligible).filter(|p| {
                    slug.is_none_or(|slug| {
                        p["models"]
                            .as_array()
                            .is_some_and(|m| m.iter().any(|m| m["slug"] == slug))
                    })
                });
                let first = matches.next()?;
                matches.next().is_none().then_some(first)
            })
    }
    .ok_or_else(|| RpcError::BadParams("Choose an available provider and model.".into()))?;
    let slug = slug
        .or_else(|| provider["models"].as_array()?.first()?["slug"].as_str())
        .ok_or_else(|| RpcError::BadParams("This provider has no models.".into()))?;
    if !provider["models"]
        .as_array()
        .is_some_and(|models| models.iter().any(|m| m["slug"] == slug))
    {
        return Err(RpcError::BadParams(
            "This model is unavailable on the selected provider.".into(),
        ));
    }
    let mut selected = if provider["instanceId"] == fallback["instanceId"]
        && slug == fallback["model"].as_str().unwrap_or("")
    {
        fallback.clone()
    } else {
        json!({})
    };
    selected["instanceId"] = provider["instanceId"].clone();
    selected["model"] = json!(slug);
    Ok(selected)
}

fn native_model(model: &Value) -> Result<Value> {
    let mut options = Vec::new();
    for descriptor in model["capabilities"]["optionDescriptors"]
        .as_array()
        .into_iter()
        .flatten()
    {
        let choices: Vec<Value> = match text(descriptor, "type")? {
            "select" => rows(descriptor, "options")?
                .iter()
                .map(|choice| json!({"id":choice["id"],"label":choice["label"]}))
                .collect(),
            "boolean" => vec![
                json!({"id":"false","label":"Off"}),
                json!({"id":"true","label":"On"}),
            ],
            _ => anyhow::bail!("unsupported T3 model option"),
        };
        let default = if descriptor["type"] == "boolean" {
            descriptor["currentValue"]
                .as_bool()
                .unwrap_or(false)
                .to_string()
        } else {
            descriptor["currentValue"]
                .as_str()
                .or_else(|| {
                    descriptor["options"]
                        .as_array()?
                        .iter()
                        .find(|choice| choice["isDefault"] == true)?["id"]
                        .as_str()
                })
                .or_else(|| choices.first()?["id"].as_str())
                .context("empty T3 option choices")?
                .into()
        };
        options.push(json!({"id":text(descriptor,"id")?,"label":text(descriptor,"label")?,"choices":choices,"defaultChoice":default}));
    }
    let native = json!({"id":text(model,"slug")?,"label":text(model,"name")?,"description":null,"reasoningLevels":[],"options":options});
    let _: zeron_proto::Model = serde_json::from_value(native.clone())?;
    Ok(native)
}

fn input_response(
    id: &str,
    request_id: &str,
    answers: Vec<UserInputAnswer>,
    projection: &Value,
) -> Result<Value, RpcError> {
    let request = rows(projection, "runtimeRequests")
        .map_err(failed)?
        .iter()
        .find(|r| r["id"] == request_id)
        .ok_or_else(|| RpcError::BadParams("T3 request no longer exists".into()))?;
    if request["status"] != "pending" {
        return Err(RpcError::BadParams(
            "T3 request is no longer pending".into(),
        ));
    }
    let approval = rows(projection, "visibleTurnItems")
        .map_err(failed)?
        .iter()
        .find(|row| {
            row["item"]["type"] == "approval_request" && row["item"]["requestId"] == request_id
        });
    let mut value = json!({"type":"runtime-request.respond","threadId":id,"requestId":request_id});
    if let Some(approval) = approval {
        if answers.len() != 1
            || answers[0].question_id != request_id
            || answers[0].labels.len() != 1
        {
            return Err(RpcError::BadParams(
                "choose one advertised approval option".into(),
            ));
        }
        let decision = approval_choices(&approval["item"])
            .map_err(failed)?
            .into_iter()
            .find(|(label, _)| label == &answers[0].labels[0])
            .ok_or_else(|| RpcError::BadParams("choose an advertised approval option".into()))?
            .1;
        value["decision"] = json!(decision);
    } else if request["kind"] == "user_input" {
        let item = rows(projection, "visibleTurnItems")
            .map_err(failed)?
            .iter()
            .find(|row| {
                row["item"]["type"] == "user_input_request"
                    && row["item"]["requestId"] == request_id
            })
            .ok_or_else(|| {
                RpcError::BadParams(
                    "The questions for this request are unavailable. Reopen the thread.".into(),
                )
            })?;
        let questions = rows(&item["item"], "questions").map_err(failed)?;
        let mut mapped = serde_json::Map::new();
        for question in questions {
            let question_id = text(question, "id").map_err(failed)?;
            let answer = answers.iter().find(|a| a.question_id == question_id);
            let labels = answer.map(|a| a.labels.as_slice()).unwrap_or_default();
            if labels.is_empty() || labels.iter().all(|label| label.trim().is_empty()) {
                if question["required"] == false {
                    continue;
                }
                return Err(RpcError::BadParams(format!(
                    "Answer {} before sending.",
                    question["header"].as_str().unwrap_or("each question")
                )));
            }
            let options = question["options"].as_array();
            let values = labels
                .iter()
                .map(|label| {
                    let option = options
                        .into_iter()
                        .flatten()
                        .find(|option| option["label"] == *label);
                    if option.is_none() && question["allowCustomAnswer"] == false {
                        return Err(RpcError::BadParams("Choose an advertised answer.".into()));
                    }
                    Ok(option
                        .and_then(|option| option["value"].as_str())
                        .unwrap_or(label)
                        .to_owned())
                })
                .collect::<Result<Vec<_>, RpcError>>()?;
            if values.len() > 1 && question["multiSelect"] != true {
                return Err(RpcError::BadParams(
                    "Choose one answer for this question.".into(),
                ));
            }
            // T3 owns provider-specific conversion. Asynchronous questions resume with text.
            let answer = if request["responseCapability"]["type"] == "message"
                || question["multiSelect"] != true
            {
                json!(values.join(", "))
            } else {
                json!(values)
            };
            mapped.insert(question_id.into(), answer);
        }
        if answers.iter().any(|answer| {
            !questions
                .iter()
                .any(|question| question["id"] == answer.question_id)
        }) {
            return Err(RpcError::BadParams(
                "An answer belongs to a different request.".into(),
            ));
        }
        value["answers"] = Value::Object(mapped);
    } else {
        return Err(RpcError::BadParams(
            "this T3 runtime request needs the T3 controls".into(),
        ));
    }
    Ok(value)
}

fn watch_values<T: Clone + Send + Sync + 'static>(
    rx: watch::Receiver<T>,
    map: impl Fn(&T) -> Result<Value> + Send + Sync + 'static,
) -> RpcReply {
    RpcReply::Stream(Box::pin(stream::unfold(
        (rx, true, map, None),
        |(mut rx, mut first, map, mut previous)| async move {
            loop {
                if !first && rx.changed().await.is_err() {
                    return None;
                }
                first = false;
                let source = rx.borrow_and_update().clone();
                match map(&source) {
                    Ok(value) => {
                        if previous.as_ref() == Some(&value) {
                            continue;
                        }
                        previous = Some(value.clone());
                        return Some((value, (rx, first, map, previous)));
                    }
                    Err(error) => {
                        tracing::warn!(%error,"invalid T3 native view projection");
                        return None;
                    }
                }
            }
        },
    )))
}

fn once(value: Value) -> RpcReply {
    RpcReply::Stream(Box::pin(
        stream::once(async move { value }).chain(stream::pending()),
    ))
}

#[async_trait]
impl RpcService for T3Service {
    async fn handle(&self, method: &str, params: Value) -> Result<RpcReply, RpcError> {
        let environment = self.engine_info.device_id.clone();
        match method {
            methods::ENGINE_INFO => RpcReply::value(&self.engine_info),
            methods::ENGINE_READY => RpcReply::value(&json!({"ready":true})),
            methods::LOCAL_DEVICE => RpcReply::value(&json!({"deviceId":environment})),
            methods::AUTH_STATUS => Ok(once(json!({"state":"signedOut"}))),
            methods::WATCH_CONNECTIVITY => {
                Ok(watch_values(self.connectivity.clone(), |v| Ok(v.clone())))
            }
            methods::WATCH_TRANSFERS => Ok(once(json!([]))),
            methods::WATCH_SIDEBAR_PREFERENCES => Ok(once(
                json!({"synced":false,"initialized":false,"sections":[]}),
            )),
            methods::WATCH_SPACES => Ok(watch_values(self.shell.clone(), move |s| {
                Ok(serde_json::to_value(s.spaces(&environment)?)?)
            })),
            methods::WATCH_SESSIONS => Ok(watch_values(self.shell.clone(), move |s| {
                Ok(serde_json::to_value(s.sessions(&environment)?)?)
            })),
            methods::WATCH_CHATS => {
                let providers = self.providers().await?;
                Ok(watch_values(self.shell.clone(), move |s| {
                    let metadata: std::collections::HashMap<_, _> = s
                        .all_threads()
                        .map(|thread| {
                            Ok((
                                text(thread, "id")?,
                                SidebarThread::from_shell(thread, &s.capabilities)?,
                            ))
                        })
                        .collect::<anyhow::Result<_>>()?;
                    let chats = s
                        .chats(&environment, &providers)?
                        .into_iter()
                        .map(|chat| {
                            let sidebar = metadata
                                .get(chat.id.as_str())
                                .context("missing T3 sidebar thread")?;
                            let mut value = serde_json::to_value(chat)?;
                            value["t3Sidebar"] = serde_json::to_value(sidebar)?;
                            Ok(value)
                        })
                        .collect::<anyhow::Result<Vec<_>>>()?;
                    Ok(json!(chats))
                }))
            }
            methods::WATCH_DEVICES => {
                let config = self
                    .client()
                    .await?
                    .call("server.getConfig", json!({}))
                    .await?;
                Ok(once(
                    json!([{"id":environment,"name":config["environment"]["label"],"platform":config["environment"]["platform"]["os"],
                    "lastSeenAt":chrono::Utc::now(),"createdAt":null,"version":null,"capabilities":[]}]),
                ))
            }
            methods::LIST_HARNESSES => {
                let providers = self.providers().await?;
                let mut descriptors = Vec::new();
                for provider in providers {
                    if let Some(id) = harness(provider["driver"].as_str().unwrap_or("")) {
                        if descriptors.iter().any(|v: &Value| v["id"] == id) {
                            continue;
                        }
                        descriptors.push(json!({"id":id,"name":provider["displayName"].as_str().unwrap_or(id),
                            "supportsSteering":true,"steeringMode":"step-boundary","reasoningLevels":[],
                            "installed":provider["installed"],"enabled":provider["enabled"],"canInstall":false}));
                    }
                }
                RpcReply::value(&descriptors)
            }
            methods::LIST_MODELS => {
                let providers = self.providers().await?;
                let mut models = Vec::new();
                for provider in providers.iter().filter(|p| {
                    harness(p["driver"].as_str().unwrap_or("")) == params["harness"].as_str()
                }) {
                    if provider["enabled"] != true {
                        continue;
                    }
                    for model in rows(provider, "models").map_err(failed)? {
                        let mut native = native_model(model).map_err(failed)?;
                        native["id"] = json!(native_model_id(
                            text(provider, "instanceId").map_err(failed)?,
                            text(model, "slug").map_err(failed)?
                        ));
                        native["label"] = json!(format!(
                            "{} · {}",
                            model["name"].as_str().unwrap_or("Model"),
                            provider["displayName"].as_str().unwrap_or("Provider")
                        ));
                        models.push(native);
                    }
                }
                RpcReply::value(&models)
            }
            methods::LIST_COMMANDS => {
                let providers = self.providers().await?;
                let commands: Vec<Value> = providers
                    .iter()
                    .filter(|p| {
                        harness(p["driver"].as_str().unwrap_or("")) == params["harness"].as_str()
                    })
                    .flat_map(|p| p["slashCommands"].as_array().into_iter().flatten().cloned())
                    .collect();
                RpcReply::value(&commands)
            }
            methods::LIST_SKILLS => RpcReply::value(&Value::Null),
            methods::FOCUS_CHAT => {
                let id = text(&params, "chatId").map_err(failed)?;
                self.dispatch(self.visit(id).await?).await?;
                RpcReply::value(&json!({"ok":true}))
            }
            methods::MUTATE => self.mutate(params).await.map(RpcReply::Value),
            methods::QUEUE_COMMAND => self.command(params).await.map(RpcReply::Value),
            methods::UPLOAD_CHUNK => self.attachments.chunk(params).await.map(RpcReply::Value),
            methods::UPLOAD_COMMIT => self
                .attachments
                .commit(self.client().await?.as_ref(), params)
                .await
                .map(RpcReply::Value),
            methods::READ_ATTACHMENT_CHUNK => self
                .attachments
                .read(self.client().await?.as_ref(), params)
                .await
                .map(RpcReply::Value),
            "T3AssetUrl" => self
                .attachments
                .url(self.client().await?.as_ref(), params["resource"].clone())
                .await
                .map(|url| RpcReply::Value(json!({"url":url}))),
            "T3BrowserBootstrap" => browser_bootstrap(&self.connection_path)
                .await
                .map(RpcReply::Value)
                .map_err(failed),
            methods::WATCH_DOC_MESSAGES => {
                let id = text(&params, "chatId").map_err(failed)?;
                let client = self.client().await?;
                let thread = self.thread(id).await?;
                let workspace = thread["worktreePath"]
                    .as_str()
                    .map(String::from)
                    .or_else(|| {
                        self.shell
                            .borrow()
                            .projects
                            .iter()
                            .find(|project| project["id"] == thread["projectId"])
                            .and_then(|project| project["workspaceRoot"].as_str().map(String::from))
                    });
                let subscription = client
                    .subscribe_checked(
                        "orchestration.subscribeThread",
                        json!({"threadId":id,"acceptBoundedSnapshot":true}),
                    )
                    .await?;
                let history_client = client.clone();
                let history_id = id.to_owned();
                let thread_events = stream::unfold(
                    (Some(subscription), false, history_client, history_id),
                    |(mut rx, upgrade, client, id)| async move {
                        if upgrade {
                            // Show the recent turn immediately, then fill history from
                            // a fresh authoritative snapshot without replaying writes.
                            rx.take();
                            rx = match client
                                .subscribe_checked(
                                    "orchestration.subscribeThread",
                                    json!({"threadId":id,"acceptBoundedSnapshot":false}),
                                )
                                .await
                            {
                                Ok(rx) => Some(rx),
                                Err(_) => return None,
                            };
                        }
                        let frame = rx.as_mut()?.recv().await?;
                        let upgrade =
                            frame["kind"] == "snapshot" && frame["hasMoreHistory"] == true;
                        Some(((false, Some(frame)), (rx, upgrade, client, id)))
                    },
                )
                .chain(stream::once(async { (false, None) }));
                // An optional Git read must not hold up the opening transcript.
                let git_events = stream::once(async move {
                    let cwd = workspace?;
                    client
                        .subscribe_checked(
                            "subscribeVcsStatus",
                            json!({"cwd":cwd,"includeRemote":false}),
                        )
                        .await
                        .ok()
                })
                .flat_map(|rx| {
                    stream::unfold(rx, |mut rx| async move {
                        Some(((true, Some(rx.as_mut()?.recv().await?)), rx))
                    })
                })
                .chain(stream::once(async { (true, None) }));
                let events = Box::pin(stream::select(thread_events, git_events));
                let workspace_binding =
                    (thread["projectId"].clone(), thread["worktreePath"].clone());
                let stream = stream::unfold(
                    (
                        events,
                        None::<Projection>,
                        Vec::new(),
                        environment,
                        None::<GitStats>,
                        workspace_binding,
                        false,
                    ),
                    |(
                        mut rx,
                        mut projection,
                        mut previous,
                        environment,
                        mut git,
                        workspace_binding,
                        mut history_pending,
                    )| async move {
                        loop {
                            let (git_event, frame) = rx.next().await?;
                            if !git_event && frame.is_none() {
                                return None;
                            }
                            let result: Result<Option<Value>> = (|| {
                                let empty = Value::Null;
                                let frame = frame.as_ref().unwrap_or(&empty);
                                let snapshot = !git_event && frame["kind"] == "snapshot";
                                if git_event {
                                    let changed = match GitStats::apply(
                                        &mut git,
                                        (!frame.is_null()).then_some(frame),
                                    ) {
                                        Ok(changed) => changed,
                                        Err(_) => {
                                            tracing::warn!(
                                                "invalid T3 Git status; clearing native counts"
                                            );
                                            git.take().is_some()
                                        }
                                    };
                                    if !changed {
                                        return Ok(None);
                                    }
                                } else if snapshot {
                                    projection = Some(Projection::snapshot(&frame)?);
                                    history_pending = frame["hasMoreHistory"] == true;
                                } else if let Some(p) = &mut projection {
                                    if !p.apply(&frame)? {
                                        return Ok(None);
                                    }
                                } else {
                                    return Ok(None);
                                }
                                let Some(projection) = projection.as_ref() else {
                                    return Ok(None);
                                };
                                // Rebind Git to the new checkout after a T3 worktree handoff.
                                ensure!(
                                    projection.value["thread"]["projectId"] == workspace_binding.0
                                        && projection.value["thread"]["worktreePath"]
                                            == workspace_binding.1,
                                    "T3 workspace changed; resubscribe"
                                );
                                let baseline = if snapshot {
                                    Some(projection.entries(&environment)?)
                                } else {
                                    None
                                };
                                let frame_update = if let Some(next) = &baseline {
                                    previous = next.clone();
                                    TranscriptFrame::reset(next)
                                } else if git_event {
                                    TranscriptFrame::Delta {
                                        upsert: vec![],
                                        append: vec![],
                                        remove: vec![],
                                        count: previous.len(),
                                    }
                                } else {
                                    projection.transcript_delta(
                                        &frame,
                                        &environment,
                                        &mut previous,
                                    )?
                                };
                                let update = TranscriptUpdate {
                                    frame: frame_update,
                                    context_usage: projection.context_usage(),
                                    replay_baseline: baseline
                                        .as_ref()
                                        .map(|next| TranscriptBaseline::capture(next)),
                                };
                                let mut value = serde_json::to_value(update)?;
                                if history_pending {
                                    value["historyPending"] = json!(true);
                                }
                                if git_event
                                    || snapshot
                                    || frame["event"]["type"] != "turn-item.updated"
                                {
                                    let mut details = projection.details()?;
                                    details.git = git.clone();
                                    value["t3Details"] = serde_json::to_value(details)?;
                                }
                                Ok(Some(value))
                            })();
                            match result {
                                Ok(Some(value)) => {
                                    return Some((
                                        value,
                                        (
                                            rx,
                                            projection,
                                            previous,
                                            environment,
                                            git,
                                            workspace_binding,
                                            history_pending,
                                        ),
                                    ));
                                }
                                Ok(None) => continue,
                                Err(error) => {
                                    tracing::warn!(%error,"invalid T3 transcript; resubscribing");
                                    return None;
                                }
                            }
                        }
                    },
                );
                Ok(RpcReply::Stream(Box::pin(stream)))
            }
            methods::LIST_PROJECT_ACTIONS
            | methods::UPSERT_PROJECT_ACTION
            | methods::DELETE_PROJECT_ACTION
            | methods::RUN_PROJECT_ACTION
            | methods::OPEN_TERMINAL
            | methods::WRITE_TERMINAL
            | methods::RESIZE_TERMINAL
            | methods::CLOSE_TERMINAL
            | methods::SUBSCRIBE_TERMINAL
            | "T3OpenInEditor"
            | "T3InitializeGit" => self.workspace(method, params).await,
            // ponytail: add remaining native controls with their T3 RPC mapping; never fall through to Zeron's engine.
            _ => Err(RpcError::UnknownMethod(format!(
                "{method} (not yet migrated to T3)"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn unchanged_shell_views_do_not_repaint_but_changes_are_delivered() {
        let (tx, rx) = watch::channel(0);
        let RpcReply::Stream(mut frames) = watch_values(rx, |n| Ok(json!(n))) else {
            panic!("expected stream");
        };
        assert_eq!(frames.next().await, Some(json!(0)));
        tx.send(1).unwrap();
        assert_eq!(frames.next().await, Some(json!(1)));
        tx.send(1).unwrap();
        drop(tx);
        assert_eq!(frames.next().await, None);
    }

    #[test]
    fn model_catalog_preserves_select_and_boolean_controls() {
        let model = native_model(&json!({"slug":"model","name":"Model","capabilities":{"optionDescriptors":[
            {"id":"effort","label":"Effort","type":"select","options":[{"id":"low","label":"Low"},{"id":"high","label":"High","isDefault":true}]},
            {"id":"fast","label":"Fast","type":"boolean","currentValue":true}]}})).unwrap();
        assert_eq!(model["options"][0]["defaultChoice"], "high");
        assert_eq!(model["options"][1]["defaultChoice"], "true");
        assert_eq!(model["options"][1]["choices"][0]["id"], "false");
    }

    #[test]
    fn provider_instances_with_the_same_model_remain_distinct() {
        let providers = json!([{"instanceId":"home","driver":"codex","enabled":true,"models":[{"slug":"same"}]},{"instanceId":"work","driver":"codex","enabled":true,"models":[{"slug":"same"}]}]);
        let providers = providers.as_array().unwrap();
        let current =
            json!({"instanceId":"home","model":"same","options":[{"id":"fast","value":true}]});
        assert_eq!(
            model_selection(providers, &current, Some("codex"), Some("same")).unwrap(),
            current
        );
        let picked = model_selection(
            providers,
            &current,
            Some("codex"),
            Some(&native_model_id("work", "same")),
        )
        .unwrap();
        assert_eq!(picked["instanceId"], "work");
        assert!(picked["options"].is_null());
        assert!(model_selection(providers, &Value::Null, Some("codex"), Some("same")).is_err());
        assert!(
            model_selection(
                providers,
                &current,
                Some("claude-code"),
                Some(&native_model_id("work", "same"))
            )
            .is_err()
        );
    }

    #[test]
    fn asynchronous_questions_use_text_and_live_multiselect_uses_values() {
        let mut projection = json!({"runtimeRequests":[{"id":"r","kind":"user_input","status":"pending","responseCapability":{"type":"message"}}],"visibleTurnItems":[{"item":{"type":"user_input_request","requestId":"r","questions":[{"id":"q","header":"Colors","multiSelect":true,"allowCustomAnswer":false,"options":[{"label":"Red","value":"red"},{"label":"Blue","value":"blue"}]}]}}]});
        let answer = || {
            vec![UserInputAnswer {
                question_id: "q".into(),
                labels: vec!["Red".into(), "Blue".into()],
            }]
        };
        assert_eq!(
            input_response("thread", "r", answer(), &projection).unwrap()["answers"]["q"],
            "red, blue"
        );
        projection["runtimeRequests"][0]["responseCapability"]["type"] = json!("live");
        assert_eq!(
            input_response("thread", "r", answer(), &projection).unwrap()["answers"]["q"],
            json!(["red", "blue"])
        );
        assert!(input_response("thread", "r", vec![], &projection).is_err());
        assert!(
            input_response(
                "thread",
                "r",
                vec![UserInputAnswer {
                    question_id: "q".into(),
                    labels: vec!["Unknown".into()]
                }],
                &projection
            )
            .is_err()
        );
    }

    #[test]
    fn visits_use_the_viewed_state_watermark() {
        let command =
            visit_command("thread", &json!({"updatedAt":"2026-10-09T00:00:00Z"})).unwrap();
        assert_eq!(command["visitedAt"], "2026-10-09T00:00:00Z");
        assert!(visit_command("thread", &json!({"updatedAt":"invalid"})).is_err());
    }

    #[test]
    fn responses_respect_request_type_and_pending_state() {
        let answer = || {
            vec![UserInputAnswer {
                question_id: "request".into(),
                labels: vec!["Allow once".into()],
            }]
        };
        let mut projection = json!({"runtimeRequests":[{"id":"request","kind":"mcp-elicitation","status":"pending"}],
            "visibleTurnItems":[{"item":{"type":"approval_request","requestId":"request"}}]});
        assert_eq!(
            input_response("thread", "request", answer(), &projection).unwrap()["decision"],
            "accept"
        );
        projection["visibleTurnItems"][0]["item"]["options"] =
            json!([{"decision":"decline","label":"Decline","warning":"Provider warning"}]);
        assert!(input_response("thread", "request", answer(), &projection).is_err());
        let advertised = vec![UserInputAnswer {
            question_id: "request".into(),
            labels: vec!["Decline — Provider warning".into()],
        }];
        assert_eq!(
            input_response("thread", "request", advertised, &projection).unwrap()["decision"],
            "decline"
        );
        projection["runtimeRequests"][0]["status"] = json!("resolved");
        assert!(input_response("thread", "request", answer(), &projection).is_err());
        projection["runtimeRequests"][0]["status"] = json!("pending");
        projection["visibleTurnItems"] = json!([]);
        assert!(input_response("thread", "request", answer(), &projection).is_err());
        projection["runtimeRequests"][0]["kind"] = json!("user_input");
        projection["visibleTurnItems"] = json!([{"item":{"type":"user_input_request","requestId":"request","questions":[{"id":"request","header":"Decision","options":[{"label":"Allow once","value":"allow"}]}]}}]);
        assert_eq!(
            input_response("thread", "request", answer(), &projection).unwrap()["answers"]["request"],
            json!("allow")
        );
    }
}
