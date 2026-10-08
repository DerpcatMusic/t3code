//! Mutating integration check. Run only against a disposable T3 thread.

use anyhow::{Context, Result, ensure};
use serde_json::json;
use std::time::Duration;
use zeron_doc::{SessionCommandPayload, TranscriptUpdate, apply_transcript_frame};
use zeron_proto::{Chat, RunRequest, SandboxLevel};
use zeron_rpc::{memory_client, methods};

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = std::env::args_os().skip(1);
    let path = args
        .next()
        .context("usage: exercise <connection.json> <disposable-thread-id>")?;
    let id = args
        .next()
        .context("a disposable thread id is required")?
        .into_string()
        .map_err(|_| anyhow::anyhow!("invalid thread id"))?;
    let service = zeron_t3::T3Service::connect(std::path::Path::new(&path)).await?;
    let client = memory_client(service);
    let mut chats = client
        .subscribe_checked(methods::WATCH_CHATS, json!({}))
        .await?;
    let chats: Vec<Chat> =
        serde_json::from_value(chats.recv().await.context("chat stream ended")?)?;
    let chat = chats
        .iter()
        .find(|chat| chat.id == id)
        .context("test thread not found")?;
    ensure!(
        !chat.archived,
        "unarchive the disposable thread before testing"
    );
    client
        .call(methods::FOCUS_CHAT, json!({"chatId":id}))
        .await?;
    client
        .call(
            methods::MUTATE,
            json!({"op":"renameChat","chatId":id,"title":chat.title}),
        )
        .await?;
    let mut transcript = client
        .subscribe_checked(methods::WATCH_DOC_MESSAGES, json!({"chatId":id}))
        .await?;
    let opening: TranscriptUpdate =
        serde_json::from_value(transcript.recv().await.context("transcript ended")?)?;
    let mut entries = Vec::new();
    apply_transcript_frame(&mut entries, opening.frame)?;
    let message_id = uuid::Uuid::new_v4().to_string();
    let payload = json!({"chatId":id,"command":SessionCommandPayload::Run {
        message_id:message_id.clone(),
        request:RunRequest {
            prompt:"For a UI integration check, reply exactly Native bridge OK. Do not use tools or edit files.".into(),
            harness:None,model:None,reasoning:None,model_options:Default::default(),
            cwd:chat.cwd.clone().context("test thread has no workspace")?,
            sandbox:SandboxLevel::WorkspaceWrite,auto_approve:false,resume:None,
            attachments:vec![],worktree:None,mcp:None,
        }
    }});
    let result: Result<()> = async {
        let first = client.call(methods::QUEUE_COMMAND, payload.clone()).await?;
        let retry = client.call(methods::QUEUE_COMMAND, payload).await?;
        ensure!(
            first["commandId"] == retry["commandId"],
            "send retry changed its receipt identity"
        );
        tokio::time::timeout(Duration::from_secs(90), async {
            loop {
                let update: TranscriptUpdate = serde_json::from_value(
                    transcript
                        .recv()
                        .await
                        .context("transcript ended before the response")?,
                )?;
                apply_transcript_frame(&mut entries, update.frame)?;
                if entries.iter().any(|entry| {
                    let value = serde_json::to_value(entry).unwrap();
                    value["role"] == "assistant"
                        && value["status"] == "complete"
                        && value["parts"].as_array().is_some_and(|parts| {
                            parts.iter().any(|part| {
                                part["text"]
                                    .as_str()
                                    .is_some_and(|text| text.contains("Native bridge OK"))
                            })
                        })
                }) {
                    break;
                }
            }
            Ok::<_, anyhow::Error>(())
        })
        .await??;
        ensure!(
            entries
                .iter()
                .filter(|entry| entry.id == message_id)
                .count()
                == 1,
            "a retried send duplicated its user message"
        );
        client
            .call(
                methods::MUTATE,
                json!({"op":"setChatArchived","chatId":id,"archived":true}),
            )
            .await?;
        client
            .call(
                methods::MUTATE,
                json!({"op":"setChatArchived","chatId":id,"archived":false}),
            )
            .await?;
        println!("Native T3 send/retry, transcript, visit, rename and archive/unarchive passed.");
        Ok(())
    }
    .await;
    if result.is_err() {
        let _ = tokio::time::timeout(
            Duration::from_secs(10),
            client.call(
                methods::QUEUE_COMMAND,
                json!({"chatId":id,"command":SessionCommandPayload::Interrupt {}}),
            ),
        )
        .await;
    }
    result
}
