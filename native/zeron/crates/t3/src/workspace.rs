use super::*;
use base64::{Engine, engine::general_purpose::STANDARD};

fn terminal_ref(value: &Value) -> Result<(&str, &str), RpcError> {
    let id = text(value, "terminalId").map_err(failed)?;
    let (thread, terminal) = id
        .strip_prefix("t3-terminal:")
        .and_then(|s| s.split_once('/'))
        .ok_or_else(|| RpcError::BadParams("Invalid T3 terminal".into()))?;
    if uuid::Uuid::parse_str(thread).is_err() || uuid::Uuid::parse_str(terminal).is_err() {
        return Err(RpcError::BadParams("Invalid T3 terminal".into()));
    }
    Ok((thread, terminal))
}

pub fn terminal_session(snapshot: &Value) -> Result<Value, RpcError> {
    Ok(
        json!({"id":format!("t3-terminal:{}/{}", text(snapshot,"threadId").map_err(failed)?, text(snapshot,"terminalId").map_err(failed)?),
        "cwd":snapshot["cwd"],"shell":snapshot["label"].as_str().filter(|s|!s.is_empty()).unwrap_or("Shell")}),
    )
}

fn project_snapshot(project: &Value) -> Result<Value, RpcError> {
    let actions = rows(project, "scripts")
        .map_err(failed)?
        .iter()
        .map(|script| {
            let mut action = script.clone();
            action["spaceId"] = project["id"].clone();
            action
        })
        .collect::<Vec<_>>();
    Ok(
        json!({"spaceId":project["id"],"actions":actions,"importableActions":[],"projectFileIssue":null}),
    )
}

impl T3Service {
    pub(super) async fn workspace(
        &self,
        method: &str,
        params: Value,
    ) -> Result<RpcReply, RpcError> {
        let client = self.client().await?;
        match method {
            methods::LIST_PROJECT_ACTIONS
            | methods::UPSERT_PROJECT_ACTION
            | methods::DELETE_PROJECT_ACTION
            | methods::RUN_PROJECT_ACTION => {
                let _write = self.project_write.lock().await;
                let project_id = text(&params, "spaceId").map_err(failed)?;
                // Read the authoritative project immediately before changing its script list.
                let projects = client.call("projects.list", json!({})).await?;
                let projects = projects
                    .as_array()
                    .or_else(|| projects["projects"].as_array())
                    .ok_or_else(|| failed("Invalid T3 project list"))?;
                let project = projects
                    .iter()
                    .find(|p| p["id"] == project_id)
                    .ok_or_else(|| RpcError::BadParams("Project no longer exists".into()))?;
                if method == methods::LIST_PROJECT_ACTIONS {
                    return project_snapshot(project).map(RpcReply::Value);
                }
                if method == methods::RUN_PROJECT_ACTION {
                    let action_id = text(&params, "actionId").map_err(failed)?;
                    let action = rows(project, "scripts")
                        .map_err(failed)?
                        .iter()
                        .find(|s| s["id"] == action_id)
                        .ok_or_else(|| RpcError::BadParams("Script no longer exists".into()))?;
                    let chat = text(&params, "chatId").map_err(failed)?;
                    let thread = self.thread(chat).await?;
                    if thread["projectId"] != project_id {
                        return Err(RpcError::BadParams(
                            "Script belongs to another project".into(),
                        ));
                    }
                    let session = self
                        .open_terminal(&client, &params, &thread, project)
                        .await?;
                    let id = text(&session, "id").map_err(failed)?;
                    let terminal = id
                        .rsplit_once('/')
                        .ok_or_else(|| failed("Invalid terminal session"))?
                        .1;
                    client.call("terminal.write",json!({"threadId":chat,"terminalId":terminal,"data":format!("{}\r", text(action,"command").map_err(failed)?)})).await?;
                    return Ok(RpcReply::Value(
                        json!({"actionId":action_id,"actionName":action["name"],"terminal":session}),
                    ));
                }
                let mut scripts = rows(project, "scripts").map_err(failed)?.to_vec();
                let action_id = params["actionId"]
                    .as_str()
                    .map(String::from)
                    .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
                let index = scripts.iter().position(|s| s["id"] == action_id);
                if method == methods::DELETE_PROJECT_ACTION {
                    let index = index
                        .ok_or_else(|| RpcError::BadParams("Script no longer exists".into()))?;
                    scripts.remove(index);
                } else {
                    let mut action = index
                        .map(|index| scripts[index].clone())
                        .unwrap_or_else(|| json!({}));
                    action["id"] = json!(action_id);
                    for key in ["name", "command", "icon", "runOnWorktreeCreate"] {
                        action[key] = params["action"][key].clone();
                    }
                    if let Some(index) = index {
                        scripts[index] = action;
                    } else {
                        scripts.push(action);
                    }
                }
                client.call("projects.mutate",json!({"type":"project.update","commandId":uuid::Uuid::new_v4(),"projectId":project_id,"scripts":scripts})).await?;
                let mut project = project.clone();
                project["scripts"] = json!(scripts);
                project_snapshot(&project).map(RpcReply::Value)
            }
            methods::OPEN_TERMINAL => {
                let chat = text(&params, "chatId").map_err(failed)?;
                let thread = self.thread(chat).await?;
                let project = self
                    .shell
                    .borrow()
                    .projects
                    .iter()
                    .find(|p| p["id"] == thread["projectId"])
                    .cloned()
                    .ok_or_else(|| failed("Project unavailable"))?;
                self.open_terminal(&client, &params, &thread, &project)
                    .await
                    .map(RpcReply::Value)
            }
            methods::WRITE_TERMINAL | methods::RESIZE_TERMINAL | methods::CLOSE_TERMINAL => {
                let (thread, terminal) = terminal_ref(&params)?;
                let mut input = json!({"threadId":thread,"terminalId":terminal});
                let mapped = match method {
                    methods::WRITE_TERMINAL => {
                        let bytes = STANDARD
                            .decode(text(&params, "data").map_err(failed)?)
                            .map_err(failed)?;
                        input["data"] = json!(String::from_utf8(bytes).map_err(failed)?);
                        "terminal.write"
                    }
                    methods::RESIZE_TERMINAL => {
                        input["cols"] = params["cols"].clone();
                        input["rows"] = params["rows"].clone();
                        "terminal.resize"
                    }
                    _ => {
                        input["deleteHistory"] = json!(false);
                        "terminal.close"
                    }
                };
                client.call(mapped, input).await?;
                Ok(RpcReply::Value(json!({"ok":true})))
            }
            methods::SUBSCRIBE_TERMINAL => {
                let (thread, terminal) = terminal_ref(&params)?;
                let rx = client
                    .subscribe_checked(
                        "terminal.observe",
                        json!({"threadId":thread,"terminalId":terminal}),
                    )
                    .await?;
                let after = params["afterSeq"].as_u64().unwrap_or(0);
                let initial = after == 0;
                let stream = stream::unfold(
                    (rx, initial, after),
                    |(mut rx, mut initial, mut seq)| async move {
                        loop {
                            let frame = rx.recv().await?;
                            let event = match frame["type"].as_str() {
                                Some("snapshot" | "restarted") => {
                                    let history =
                                        frame["snapshot"]["history"].as_str().unwrap_or("");
                                    // Reconnect from T3's authoritative buffer, without duplicating old output.
                                    let data = if initial {
                                        history.to_owned()
                                    } else {
                                        format!("\x1bc{history}")
                                    };
                                    initial = false;
                                    Some(
                                        json!({"type":"data","data":STANDARD.encode(data),"seq":seq+1}),
                                    )
                                }
                                Some("output") => Some(
                                    json!({"type":"data","data":STANDARD.encode(frame["data"].as_str().unwrap_or("")),"seq":seq+1}),
                                ),
                                Some("exited" | "closed") => Some(
                                    json!({"type":"exit","seq":seq+1,"exitCode":frame["exitCode"].as_i64().unwrap_or(0),"signal":frame["exitSignal"]}),
                                ),
                                Some("cleared") => Some(
                                    json!({"type":"data","data":STANDARD.encode("\x1bc"),"seq":seq+1}),
                                ),
                                Some("error") => Some(
                                    json!({"type":"data","data":STANDARD.encode(format!("\r\nTerminal error: {}\r\n",frame["message"].as_str().unwrap_or("Connection failed"))),"seq":seq+1}),
                                ),
                                _ => None,
                            };
                            if let Some(event) = event {
                                seq += 1;
                                return Some((event, (rx, initial, seq)));
                            }
                        }
                    },
                );
                Ok(RpcReply::Stream(Box::pin(stream)))
            }
            "T3OpenInEditor" | "T3InitializeGit" => {
                let thread = self
                    .thread(text(&params, "chatId").map_err(failed)?)
                    .await?;
                let project = self
                    .shell
                    .borrow()
                    .projects
                    .iter()
                    .find(|p| p["id"] == thread["projectId"])
                    .cloned()
                    .ok_or_else(|| failed("Project unavailable"))?;
                let cwd = thread["worktreePath"]
                    .as_str()
                    .unwrap_or(text(&project, "workspaceRoot").map_err(failed)?);
                let (mapped, input) = if method == "T3OpenInEditor" {
                    (
                        "shell.openInEditor",
                        json!({"cwd":cwd,"editor":params["editor"].as_str().unwrap_or("zed")}),
                    )
                } else {
                    ("vcs.init", json!({"cwd":cwd}))
                };
                client.call(mapped, input).await.map(RpcReply::Value)
            }
            _ => Err(RpcError::UnknownMethod(method.into())),
        }
    }

    async fn open_terminal(
        &self,
        client: &RpcClient,
        params: &Value,
        thread: &Value,
        project: &Value,
    ) -> Result<Value, RpcError> {
        let cwd = thread["worktreePath"]
            .as_str()
            .unwrap_or(text(project, "workspaceRoot").map_err(failed)?);
        let snapshot = client.call("terminal.open",json!({"threadId":thread["id"],"terminalId":uuid::Uuid::new_v4(),"cwd":cwd,"worktreePath":thread["worktreePath"],"cols":params["cols"].as_u64().unwrap_or(80),"rows":params["rows"].as_u64().unwrap_or(24)})).await?;
        terminal_session(&snapshot)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn scripts_keep_existing_t3_fields_and_terminal_references_require_both_ids() {
        let project = json!({"id":"project","scripts":[{"id":"run","name":"Run","command":"npm run dev","icon":"play","runOnWorktreeCreate":false,"previewUrl":"http://localhost:3000"}]});
        let result = project_snapshot(&project).unwrap();
        assert_eq!(result["actions"][0]["previewUrl"], "http://localhost:3000");
        assert_eq!(result["actions"][0]["spaceId"], "project");
        assert!(terminal_ref(&json!({"terminalId":"../other"})).is_err());
        let thread = uuid::Uuid::new_v4();
        let terminal = uuid::Uuid::new_v4();
        let result = terminal_session(
            &json!({"threadId":thread,"terminalId":terminal,"cwd":"/workspace","label":"bash"}),
        )
        .unwrap();
        assert_eq!(
            terminal_ref(&json!({"terminalId":result["id"]})).unwrap().0,
            thread.to_string()
        );
    }
}
