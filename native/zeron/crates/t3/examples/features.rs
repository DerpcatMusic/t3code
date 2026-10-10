//! Exercise native controls only against the private disposable test server.
use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::{path::Path, time::Duration};
use zeron_proto::{Chat, ProjectActionRun, ProjectActionsSnapshot, TerminalEvent, TerminalSession};
use zeron_rpc::{memory_client, methods};

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let path = args
        .next()
        .context("features <isolated-connection.json> <test-thread>")?;
    let id = args.next().context("test thread required")?;
    let config: Value = serde_json::from_slice(&std::fs::read(&path)?)?;
    ensure!(
        config["origin"] == "http://127.0.0.1:39741",
        "use the isolated test server"
    );
    let client = memory_client(zeron_t3::T3Service::connect(Path::new(&path)).await?);
    let mut chats = client
        .subscribe_checked(methods::WATCH_CHATS, json!({}))
        .await?;
    let chats: Vec<Chat> = serde_json::from_value(chats.recv().await.context("chats ended")?)?;
    let chat = chats
        .iter()
        .find(|chat| chat.id == id)
        .context("test thread missing")?;
    ensure!(
        chat.title.as_deref() == Some("Native integration test"),
        "use the disposable fixture thread"
    );
    let project = chat.space_id.as_deref().context("test project missing")?;
    let _: ProjectActionsSnapshot = client
        .call_as(methods::LIST_PROJECT_ACTIONS, json!({"spaceId":project}))
        .await
        .context("list project scripts")?;
    let bootstrap = client
        .call("T3BrowserBootstrap", json!({}))
        .await
        .context("authenticated settings")?;
    ensure!(
        bootstrap["origin"] == config["origin"]
            && bootstrap["accessToken"]
                .as_str()
                .is_some_and(|s| !s.is_empty()),
        "settings bootstrap failed"
    );
    let mut paths = Vec::new();
    for (name,bytes) in [("z3-check.txt",b"Z3_ATTACHMENT_OK".to_vec()),("z3-check.png",STANDARD.decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+aWZkAAAAASUVORK5CYII=")?)] {
        let upload = uuid::Uuid::new_v4().to_string();
        client.call(methods::UPLOAD_CHUNK,json!({"uploadId":upload,"seq":0,"data":STANDARD.encode(&bytes)})).await?;
        let committed = client.call(methods::UPLOAD_COMMIT,json!({"uploadId":upload,"fileName":name})).await.context("commit attachment")?;
        let path = committed["path"].as_str().context("attachment reference missing")?;
        let metadata = zeron_t3::parse_attachment_path(path).context("invalid attachment reference")?;
        let read = client.call(methods::READ_ATTACHMENT_CHUNK,json!({"path":path,"offset":0})).await?;
        ensure!(STANDARD.decode(read["data"].as_str().context("attachment bytes missing")?)? == bytes && read["done"] == true,"attachment changed");
        let url = client.call("T3AssetUrl",json!({"resource":{"_tag":"attachment","attachmentId":metadata["id"],"fileName":name,"mimeType":metadata["mimeType"]}})).await?;
        let response = reqwest::get(url["url"].as_str().context("asset URL missing")?).await.map_err(|_|anyhow::anyhow!("asset download failed"))?;
        ensure!(response.status().is_success(), "asset download was rejected");
        ensure!(response.bytes().await.map_err(|_|anyhow::anyhow!("asset read failed"))?.as_ref() == bytes,"stored asset changed");
        paths.push(path.to_owned());
    }
    let terminal: TerminalSession = client
        .call_as(
            methods::OPEN_TERMINAL,
            json!({"chatId":id,"cols":90,"rows":26}),
        )
        .await?;
    let result: Result<()> = async {
        let mut output = client.subscribe_checked(methods::SUBSCRIBE_TERMINAL,json!({"terminalId":terminal.id,"afterSeq":0})).await?;
        client.call(methods::WRITE_TERMINAL,json!({"terminalId":terminal.id,"data":STANDARD.encode("printf 'Z3_TERMINAL_OK\\n'\r")})).await?;
        tokio::time::timeout(Duration::from_secs(15),async {
            loop {
                let event: TerminalEvent = serde_json::from_value(output.recv().await.context("terminal stream ended")?)?;
                if let TerminalEvent::Data {data,..} = event && String::from_utf8_lossy(&STANDARD.decode(data)?).contains("Z3_TERMINAL_OK") {break;}
            }
            Ok::<_,anyhow::Error>(())
        }).await??;
        client.call(methods::RESIZE_TERMINAL,json!({"terminalId":terminal.id,"cols":100,"rows":30})).await?;
        let mut resumed = client.subscribe_checked(methods::SUBSCRIBE_TERMINAL,json!({"terminalId":terminal.id,"afterSeq":100})).await?;
        let event: TerminalEvent = serde_json::from_value(tokio::time::timeout(Duration::from_secs(10),resumed.recv()).await?.context("terminal resume ended")?)?;
        let TerminalEvent::Data {seq,data} = event else {anyhow::bail!("resume has no snapshot");};
        ensure!(seq > 100 && STANDARD.decode(data)?.starts_with(b"\x1bc"),"resume did not resynchronize terminal history");
        Ok(())
    }.await;
    client
        .call(methods::CLOSE_TERMINAL, json!({"terminalId":terminal.id}))
        .await?;
    result.context("terminal streaming and replay")?;
    let action_id = uuid::Uuid::new_v4().to_string();
    let created: ProjectActionsSnapshot = client.call_as(methods::UPSERT_PROJECT_ACTION,json!({"spaceId":project,"actionId":action_id,"action":{"name":"Z3 isolated check","command":"printf 'Z3_SCRIPT_OK\\n'","icon":"test","runOnWorktreeCreate":false}})).await?;
    ensure!(
        created.actions.iter().any(|a| a.id == action_id),
        "script did not persist"
    );
    let run: Result<ProjectActionRun, _> = client
        .call_as(
            methods::RUN_PROJECT_ACTION,
            json!({"spaceId":project,"chatId":id,"actionId":action_id}),
        )
        .await;
    client
        .call(
            methods::DELETE_PROJECT_ACTION,
            json!({"spaceId":project,"actionId":action_id}),
        )
        .await?;
    let run = run?;
    client
        .call(
            methods::CLOSE_TERMINAL,
            json!({"terminalId":run.terminal.id}),
        )
        .await?;
    let config = chat
        .config
        .as_ref()
        .context("model configuration missing")?;
    client
        .call(
            methods::MUTATE,
            json!({"op":"setChatConfig","chatId":id,"config":config}),
        )
        .await?;
    println!(
        "Native attachment upload/download, model selection, authenticated settings, terminal replay and project scripts passed."
    );
    Ok(())
}
