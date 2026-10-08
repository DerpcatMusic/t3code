//! Read-only integration check against a paired, isolated T3 environment.

use anyhow::{Context, Result, ensure};
use serde_json::json;
use zeron_proto::{Chat, Session, Space};
use zeron_rpc::{memory_client, methods};

#[tokio::main]
async fn main() -> Result<()> {
    let path = std::env::args_os()
        .nth(1)
        .context("usage: smoke <connection.json>")?;
    tokio::time::timeout(std::time::Duration::from_secs(45), async {
        let service = zeron_t3::T3Service::connect(std::path::Path::new(&path)).await?;
        let client = memory_client(service);
        let mut spaces = client
            .subscribe_checked(methods::WATCH_SPACES, json!({}))
            .await?;
        let spaces: Vec<Space> =
            serde_json::from_value(spaces.recv().await.context("spaces stream ended")?)?;
        let mut chats = client
            .subscribe_checked(methods::WATCH_CHATS, json!({}))
            .await?;
        let chats: Vec<Chat> =
            serde_json::from_value(chats.recv().await.context("chats stream ended")?)?;
        ensure!(
            !chats.is_empty(),
            "seed a test thread before running the native integration check"
        );
        let harnesses: Vec<serde_json::Value> =
            client.call_as(methods::LIST_HARNESSES, json!({})).await?;
        for harness in harnesses
            .iter()
            .filter(|harness| harness["enabled"] == true && harness["installed"] == true)
        {
            let _: zeron_proto::HarnessId = serde_json::from_value(harness["id"].clone())?;
            let _: Vec<zeron_proto::Model> = client
                .call_as(methods::LIST_MODELS, json!({"harness":harness["id"]}))
                .await?;
        }
        let mut sessions = client
            .subscribe_checked(methods::WATCH_SESSIONS, json!({}))
            .await?;
        let sessions: Vec<Session> =
            serde_json::from_value(sessions.recv().await.context("sessions stream ended")?)?;
        for chat in &chats {
            ensure!(
                spaces
                    .iter()
                    .any(|space| Some(&space.id) == chat.space_id.as_ref()),
                "thread lost its project"
            );
        }
        if let Some(chat) = chats.first() {
            let mut transcript = client
                .subscribe_checked(methods::WATCH_DOC_MESSAGES, json!({"chatId":chat.id}))
                .await?;
            let update: zeron_doc::TranscriptUpdate = serde_json::from_value(
                transcript.recv().await.context("transcript stream ended")?,
            )?;
            let mut entries = Vec::new();
            zeron_doc::apply_transcript_frame(&mut entries, update.frame)?;
            println!("Native transcript decoded: {} entries", entries.len());
        }
        println!(
            "T3 native adapter: {} projects, {} threads, {} sessions",
            spaces.len(),
            chats.len(),
            sessions.len()
        );
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    Ok(())
}
