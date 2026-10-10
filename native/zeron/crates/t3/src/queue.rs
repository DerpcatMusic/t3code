//! Native queue rows are T3 user messages; mutations address their queued runs.

use super::*;
use zeron_doc::QueuedMessage;

pub(super) fn capabilities() -> Vec<String> {
    use zeron_proto::capabilities::*;
    [
        CAPABILITY,
        MESSAGE_QUEUE_V1,
        MESSAGE_QUEUE_ACTIONS_V1,
        MESSAGE_QUEUE_ATTACHMENTS_V1,
        MESSAGE_QUEUE_CLEAN_ATTACHMENT_TEXT_V1,
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}

fn queued_runs(projection: &Value) -> Result<Vec<&Value>> {
    let messages = rows(projection, "messages")?;
    let mut runs = rows(projection, "runs")?
        .iter()
        .filter(|run| {
            run["status"] == "queued"
                && messages.iter().any(|message| {
                    message["id"] == run["userMessageId"]
                        && message["role"] == "user"
                        && message["notification"].is_null()
                        && message["delegatedCompletion"].is_null()
                })
        })
        .collect::<Vec<_>>();
    runs.sort_by_key(|run| {
        (
            run["queuePosition"].as_u64().or(run["ordinal"].as_u64()),
            run["ordinal"].as_u64(),
        )
    });
    Ok(runs)
}

fn queue_rows(projection: &Value, environment: &str) -> Result<Vec<QueuedMessage>> {
    let messages = rows(projection, "messages")?;
    queued_runs(projection)?
        .into_iter()
        .map(|run| {
            let message = messages
                .iter()
                .find(|message| message["id"] == run["userMessageId"])
                .context("missing T3 queued message")?;
            Ok(QueuedMessage {
                id: text(message, "id")?.to_owned(),
                text: text(message, "text")?.to_owned(),
                attachments: rows(message, "attachments")?
                    .iter()
                    .map(attachment_path)
                    .collect::<Result<_>>()?,
                hold_for_turn_end: true,
                // T3 stores actors rather than originating native device IDs.
                issued_by: environment.to_owned(),
                issued_at: chrono::DateTime::parse_from_rfc3339(text(run, "requestedAt")?)?
                    .timestamp_millis(),
                edited_at: if message["updatedAt"] != message["createdAt"] {
                    message["updatedAt"]
                        .as_str()
                        .map(chrono::DateTime::parse_from_rfc3339)
                        .transpose()?
                        .map(|date| date.timestamp_millis())
                } else {
                    None
                },
                delivery_gate: None,
            })
        })
        .collect()
}

fn attachment_values(params: &Value) -> Result<Vec<Value>, RpcError> {
    let Some(paths) = params.get("attachments") else {
        return Ok(Vec::new());
    };
    let paths = paths
        .as_array()
        .ok_or_else(|| RpcError::BadParams("attachments must be an array".into()))?;
    paths
        .iter()
        .map(|path| {
            path.as_str()
                .and_then(parse_attachment_path)
                .ok_or_else(|| RpcError::BadParams("Attach the file again before sending.".into()))
        })
        .collect()
}

fn enqueue_command(params: &Value, message_id: &str) -> Result<Value, RpcError> {
    let id = text(params, "chatId").map_err(failed)?;
    let prompt = text(params, "text").map_err(failed)?;
    let attachments = attachment_values(params)?;
    if prompt.trim().is_empty() && attachments.is_empty() {
        return Err(RpcError::BadParams(
            "A queued message must have text or attachments".into(),
        ));
    }
    Ok(
        json!({"type":"message.dispatch","threadId":id,"messageId":message_id,
        "text":prompt,"attachments":attachments,"dispatchMode":{"type":"queue_after_active"},
        "createdBy":"user","creationSource":"web"}),
    )
}

fn queue_command(
    method: &str,
    params: &Value,
    projection: &Value,
) -> Result<Option<Value>, RpcError> {
    let thread_id = text(params, "chatId").map_err(failed)?;
    let id = text(params, "id").map_err(failed)?;
    let runs = queued_runs(projection).map_err(failed)?;
    let Some(run) = runs.iter().find(|run| run["userMessageId"] == id) else {
        return Ok(None);
    };
    let run_id = text(run, "id").map_err(failed)?;
    let command = match method {
        methods::REMOVE_QUEUED_MESSAGE => {
            json!({"type":"queued-run.cancel","threadId":thread_id,"runId":run_id})
        }
        methods::UPDATE_QUEUED_MESSAGE => {
            let prompt = text(params, "text").map_err(failed)?;
            if prompt.trim().is_empty() {
                json!({"type":"queued-run.cancel","threadId":thread_id,"runId":run_id})
            } else {
                let mut command = json!({"type":"queued-run.edit","threadId":thread_id,"runId":run_id,"text":prompt});
                // Omission preserves the server's attachments, including for old clients.
                if params.get("attachments").is_some() {
                    command["attachments"] = json!(attachment_values(params)?);
                }
                command
            }
        }
        methods::MOVE_QUEUED_MESSAGE => {
            let index = params["toIndex"].as_u64().ok_or_else(|| {
                RpcError::BadParams("toIndex must be a nonnegative integer".into())
            })?;
            let remaining = runs
                .iter()
                .filter(|candidate| candidate["id"] != run["id"])
                .collect::<Vec<_>>();
            let before = usize::try_from(index)
                .ok()
                .and_then(|index| remaining.get(index));
            json!({"type":"queued-run.reorder","threadId":thread_id,"runId":run_id,
                "beforeRunId":before.map(|run| &run["id"])})
        }
        methods::STEER_QUEUED_MESSAGE_NOW => {
            let active = active_run(projection).ok_or_else(|| {
                RpcError::BadParams("No active T3 turn to steer; resume the queue instead".into())
            })?;
            json!({"type":"queued-message.promote-to-steer","threadId":thread_id,
                "queuedRunId":run_id,"targetRunId":active["id"]})
        }
        _ => return Err(RpcError::UnknownMethod(method.into())),
    };
    Ok(Some(command))
}

pub(super) fn active_run(projection: &Value) -> Option<&Value> {
    projection["runs"].as_array()?.iter().rev().find(|run| {
        matches!(
            run["status"].as_str(),
            Some("preparing" | "starting" | "running" | "waiting")
        )
    })
}

impl T3Service {
    pub(super) async fn queue(&self, method: &str, params: Value) -> Result<RpcReply, RpcError> {
        let id = text(&params, "chatId").map_err(failed)?.to_owned();
        if method == methods::WATCH_QUEUE {
            let rx = self
                .client()
                .await?
                .subscribe_checked(
                    "orchestration.subscribeThread",
                    json!({"threadId":id,"acceptBoundedSnapshot":true}),
                )
                .await?;
            let environment = self.engine_info.device_id.clone();
            return Ok(RpcReply::Stream(Box::pin(stream::unfold(
                (
                    rx,
                    None::<Projection>,
                    None::<Vec<QueuedMessage>>,
                    environment,
                ),
                |(mut rx, mut projection, mut previous, environment)| async move {
                    loop {
                        let frame = rx.recv().await?;
                        let result: Result<Option<Vec<QueuedMessage>>> = (|| {
                            if frame["kind"] == "snapshot" {
                                projection = Some(Projection::snapshot(&frame)?);
                            } else if let Some(projection) = &mut projection {
                                if !projection.apply(&frame)? {
                                    return Ok(None);
                                }
                            } else {
                                return Ok(None);
                            }
                            let items = queue_rows(
                                &projection.as_ref().context("missing queue snapshot")?.value,
                                &environment,
                            )?;
                            Ok((previous.as_ref() != Some(&items)).then_some(items))
                        })(
                        );
                        match result {
                            Ok(Some(items)) => {
                                previous = Some(items.clone());
                                return Some((
                                    json!({"items":items}),
                                    (rx, projection, previous, environment),
                                ));
                            }
                            Ok(None) => continue,
                            Err(_) => {
                                tracing::warn!(
                                    category = "queue_projection",
                                    "invalid T3 queue; resubscribing"
                                );
                                return None;
                            }
                        }
                    }
                },
            ))));
        }
        if method == methods::QUEUE_MESSAGE {
            let message_id = uuid::Uuid::new_v4().to_string();
            self.dispatch(enqueue_command(&params, &message_id)?)
                .await?;
            return RpcReply::value(&json!({"id":message_id}));
        }
        let projection = self
            .client()
            .await?
            .call("orchestration.getThreadProjection", json!({"threadId":id}))
            .await?;
        if method == methods::SEND_QUEUED_MESSAGE_NOW {
            let message_id = text(&params, "id").map_err(failed)?;
            let runs = queued_runs(&projection).map_err(failed)?;
            let Some(run) = runs.iter().find(|run| run["userMessageId"] == message_id) else {
                return RpcReply::value(&json!({"sent":false}));
            };
            // Preserve the durable queued run and message (and their attachments).
            // T3 owns interruption and queue resumption; never cancel/recreate a send.
            if let Some(active) = active_run(&projection) {
                self.dispatch(json!({"type":"run.interrupt","threadId":id,"runId":active["id"],"holdQueue":true})).await?;
            }
            let before = runs.iter().find(|candidate| candidate["id"] != run["id"]);
            self.dispatch(
                json!({"type":"queued-run.reorder","threadId":id,"runId":run["id"],
                "beforeRunId":before.map(|run| &run["id"])}),
            )
            .await?;
            // Canonical resume releases the queue, rather than a native one-row lease.
            self.dispatch(json!({"type":"queue.resume","threadId":id}))
                .await?;
            return RpcReply::value(&json!({"sent":true}));
        }
        let command = queue_command(method, &params, &projection)?;
        let changed = command.is_some();
        if let Some(command) = command {
            self.dispatch(command).await?;
        }
        RpcReply::value(&if method == methods::STEER_QUEUED_MESSAGE_NOW {
            json!({"sent":changed})
        } else {
            json!({"changed":changed})
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn projection() -> Value {
        json!({"thread":{"id":"thread"},"runs":[
            {"id":"active","status":"running"},
            {"id":"run-b","status":"queued","userMessageId":"b","ordinal":3,"queuePosition":2,"requestedAt":"2026-10-08T00:00:00Z"},
            {"id":"run-a","status":"queued","userMessageId":"a","ordinal":2,"queuePosition":1,"requestedAt":"2026-10-08T00:00:00Z"},
            {"id":"auto","status":"queued","userMessageId":"notification","ordinal":4}],
            "messages":[{"id":"a","role":"user","text":"first","attachments":[]},
                {"id":"b","role":"user","text":"second","attachments":[]},
                {"id":"notification","role":"user","text":"wake","notification":{},"attachments":[]}]})
    }

    #[test]
    fn native_queue_filters_automatic_deliveries_and_uses_message_ids_in_server_order() {
        let items = queue_rows(&projection(), "env").unwrap();
        assert_eq!(
            items
                .iter()
                .map(|item| item.id.as_str())
                .collect::<Vec<_>>(),
            ["a", "b"]
        );
        assert_eq!(items[0].issued_at, 1791417600000);
    }

    #[test]
    fn moving_forward_addresses_the_run_and_uses_the_post_removal_slot() {
        let command = queue_command(
            methods::MOVE_QUEUED_MESSAGE,
            &json!({"chatId":"thread","id":"a","toIndex":1}),
            &projection(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            command,
            json!({"type":"queued-run.reorder","threadId":"thread","runId":"run-a","beforeRunId":null})
        );
    }

    #[test]
    fn edits_preserve_attachments_unless_explicitly_replaced_and_empty_text_cancels() {
        let command = queue_command(
            methods::UPDATE_QUEUED_MESSAGE,
            &json!({"chatId":"thread","id":"a","text":"new"}),
            &projection(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            command,
            json!({"type":"queued-run.edit","threadId":"thread","runId":"run-a","text":"new"})
        );
        let command = queue_command(
            methods::UPDATE_QUEUED_MESSAGE,
            &json!({"chatId":"thread","id":"a","text":" "}),
            &projection(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(command["type"], "queued-run.cancel");
    }

    #[test]
    fn steering_uses_authoritative_active_and_queued_run_ids() {
        let command = queue_command(
            methods::STEER_QUEUED_MESSAGE_NOW,
            &json!({"chatId":"thread","id":"b"}),
            &projection(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            command,
            json!({"type":"queued-message.promote-to-steer","threadId":"thread","queuedRunId":"run-b","targetRunId":"active"})
        );
    }

    #[test]
    fn enqueue_rejects_uncommitted_paths_and_leaves_permissions_alone() {
        assert!(
            enqueue_command(
                &json!({"chatId":"thread","text":"hello","attachments":["/private/path"]}),
                "message"
            )
            .is_err()
        );
        let command =
            enqueue_command(&json!({"chatId":"thread","text":"hello"}), "message").unwrap();
        assert_eq!(command["dispatchMode"]["type"], "queue_after_active");
        assert!(command.get("runtimeMode").is_none());
    }

    #[test]
    fn attachment_references_round_trip_through_queue_projection_and_explicit_edits() {
        let attachment = json!({"type":"file","id":"attachment","name":"notes.txt","mimeType":"text/plain","sizeBytes":3});
        let path = attachment_path(&attachment).unwrap();
        let mut projection = projection();
        projection["messages"][0]["attachments"] = json!([attachment]);
        projection["messages"][0]["createdAt"] = json!("2026-10-08T00:00:00Z");
        projection["messages"][0]["updatedAt"] = json!("2026-10-08T00:00:01Z");
        let items = queue_rows(&projection, "env").unwrap();
        assert_eq!(items[0].attachments, [path.clone()]);
        assert_eq!(items[0].edited_at, Some(items[0].issued_at + 1000));
        let command = queue_command(
            methods::UPDATE_QUEUED_MESSAGE,
            &json!({"chatId":"thread","id":"a","text":"new","attachments":[path]}),
            &projection,
        )
        .unwrap()
        .unwrap();
        assert_eq!(command["attachments"], json!([attachment]));
    }

    #[test]
    fn stale_rows_do_not_mutate_runs_and_steering_requires_an_active_turn() {
        assert!(
            queue_command(
                methods::REMOVE_QUEUED_MESSAGE,
                &json!({"chatId":"thread","id":"gone"}),
                &projection()
            )
            .unwrap()
            .is_none()
        );
        let mut projection = projection();
        projection["runs"][0]["status"] = json!("completed");
        assert!(
            queue_command(
                methods::STEER_QUEUED_MESSAGE_NOW,
                &json!({"chatId":"thread","id":"a"}),
                &projection
            )
            .is_err()
        );
    }

    #[test]
    fn no_edit_lease_is_advertised() {
        assert!(
            !capabilities()
                .iter()
                .any(|capability| capability
                    == zeron_proto::capabilities::MESSAGE_QUEUE_EDIT_LEASE_V1)
        );
    }
}
