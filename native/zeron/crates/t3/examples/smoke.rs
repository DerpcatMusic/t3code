//! Read-only integration check against a paired, isolated T3 environment.

use anyhow::{Context, Result, ensure};
use serde_json::json;
use zeron_proto::{Chat, Session, Space};
use zeron_rpc::{memory_client, methods};

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = std::env::args_os().skip(1);
    let path = args
        .next()
        .context("usage: smoke <connection.json> [thread-id]")?;
    let thread_id = args
        .next()
        .map(|id| id.into_string())
        .transpose()
        .map_err(|_| anyhow::anyhow!("invalid thread id"))?;
    tokio::time::timeout(std::time::Duration::from_secs(45), async {
        let service = zeron_t3::T3Service::connect(std::path::Path::new(&path)).await?;
        let client = memory_client(service);
        let _: zeron_proto::EngineInfo = client.call_as(methods::ENGINE_INFO, json!({})).await?;
        let mut auth = client
            .subscribe_checked(methods::AUTH_STATUS, json!({}))
            .await?;
        let auth: zeron_proto::AuthState =
            serde_json::from_value(auth.recv().await.context("auth stream ended")?)?;
        ensure!(
            matches!(auth, zeron_proto::AuthState::SignedOut),
            "T3 mode must not enable Zeron sync"
        );
        let mut connectivity = client
            .subscribe_checked(methods::WATCH_CONNECTIVITY, json!({}))
            .await?;
        let connectivity: zeron_proto::Connectivity = serde_json::from_value(
            connectivity
                .recv()
                .await
                .context("connectivity stream ended")?,
        )?;
        ensure!(
            matches!(
                connectivity.state,
                zeron_proto::ConnectivityState::Connected
            ),
            "native connection is not ready"
        );
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
        let chat = if let Some(id) = thread_id {
            chats
                .iter()
                .find(|chat| chat.id == id)
                .context("test thread not found")?
        } else {
            &chats[0]
        };
        {
            let mut transcript = client
                .subscribe_checked(methods::WATCH_DOC_MESSAGES, json!({"chatId":chat.id}))
                .await?;
            let value = transcript.recv().await.context("transcript stream ended")?;
            let mut details: zeron_t3::ThreadDetails =
                serde_json::from_value(value["t3Details"].clone())?;
            let update: zeron_doc::TranscriptUpdate = serde_json::from_value(value)?;
            let mut entries = Vec::new();
            zeron_doc::apply_transcript_frame(&mut entries, update.frame)?;
            println!("Native transcript decoded: {} entries", entries.len());
            let git = tokio::time::timeout(std::time::Duration::from_secs(10), async {
                while details.git.is_none() {
                    let value = transcript.recv().await.context("transcript stream ended")?;
                    if let Some(next) = value.get("t3Details") {
                        details = serde_json::from_value(next.clone())?;
                    }
                    let update: zeron_doc::TranscriptUpdate = serde_json::from_value(value)?;
                    zeron_doc::apply_transcript_frame(&mut entries, update.frame)?;
                }
                Ok::<_, anyhow::Error>(details.git.unwrap())
            })
            .await;
            match git {
                Ok(Ok(git)) => println!(
                    "Git status decoded: {} +{} / -{}",
                    git.branch.as_deref().unwrap_or("Detached"),
                    git.additions,
                    git.deletions
                ),
                Ok(Err(error)) => return Err(error),
                Err(_) => println!("Git status not reported by this environment."),
            }
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
