//! Exercises sidebar writes against a disposable T3 server. Never use live userdata.
use anyhow::{Context, Result, ensure};
use chrono::{Duration, Utc};
use serde_json::{Value, json};
use zeron_proto::{Chat, SidebarPinChange};
use zeron_rpc::{RpcClient, memory_client, methods};
use zeron_t3::{SidebarThread, sidebar_pins};

async fn observe(client: &RpcClient, predicate: impl Fn(&Value) -> bool) -> Result<Value> {
    let mut watch = client
        .subscribe_checked(methods::WATCH_CHATS, json!({}))
        .await?;
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let frame = watch.recv().await.context("sidebar watch ended")?;
            if predicate(&frame) {
                return Ok(frame);
            }
        }
    })
    .await
    .context("sidebar state did not converge")?
}

fn row<'a>(frame: &'a Value, id: &str) -> &'a Value {
    frame
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == id)
        .unwrap()
}

async fn change(
    client: &RpcClient,
    id: &str,
    op: &str,
    fields: Value,
    predicate: impl Fn(&Value) -> bool,
) -> Result<Value> {
    let mut params = fields;
    params["op"] = json!(op);
    params["chatId"] = json!(id);
    client.call(methods::MUTATE, params).await?;
    observe(client, |frame| predicate(&row(frame, id)["t3Sidebar"])).await
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = std::env::args_os().skip(1);
    let path = args
        .next()
        .context("usage: sidebar <connection.json> <disposable-existing-thread-id>")?;
    let existing = args
        .next()
        .context("a disposable existing thread is required")?
        .into_string()
        .map_err(|_| anyhow::anyhow!("invalid thread id"))?;
    let service = zeron_t3::T3Service::connect(std::path::Path::new(&path)).await?;
    let client = memory_client(service);
    let opening = observe(&client, |frame| {
        frame
            .as_array()
            .is_some_and(|rows| rows.iter().any(|row| row["id"] == existing))
    })
    .await?;
    let chat: Chat = serde_json::from_value(row(&opening, &existing).clone())?;
    let mut created = Vec::new();
    let result: Result<()> = async {
        for title in ["Native sidebar check A", "Native sidebar check B"] {
            let id = uuid::Uuid::new_v4().to_string();
            client.call(methods::MUTATE, json!({"op":"createChat","chatId":id,"spaceId":chat.space_id,"title":title,"config":chat.config})).await?;
            created.push(id);
        }
        observe(&client, |frame| created.iter().all(|id| frame.as_array().unwrap().iter().any(|row| row["id"] == *id))).await?;
        let id = &created[0];
        change(&client, id, "pinChat", json!({}), |meta| !meta["pinnedAt"].is_null()).await?;
        let settled = change(&client, id, "settleChat", json!({}), |meta| meta["settledOverride"] == "settled").await?;
        ensure!(row(&settled, id)["t3Sidebar"]["pinnedAt"].is_null(), "settling must clear the pin");
        change(&client, id, "unsettleChat", json!({}), |meta| meta["settledOverride"] != "settled").await?;
        change(&client, id, "pinChat", json!({}), |meta| !meta["pinnedAt"].is_null()).await?;
        let until = Utc::now() + Duration::hours(1);
        let snoozed = change(&client, id, "snoozeChat", json!({"until":until}), |meta| !meta["snoozedUntil"].is_null()).await?;
        let meta: SidebarThread = serde_json::from_value(row(&snoozed, id)["t3Sidebar"].clone())?;
        ensure!(meta.is_snoozed(Utc::now()) && !meta.visible_pin(Utc::now()) && meta.pinned_at.is_some(), "snooze must retain and hide its pin");
        let woken = change(&client, id, "wakeChat", json!({}), |meta| meta["snoozedUntil"].is_null()).await?;
        let meta: SidebarThread = serde_json::from_value(row(&woken, id)["t3Sidebar"].clone())?;
        ensure!(meta.visible_pin(Utc::now()), "waking must restore the retained pin");
        change(&client, id, "setChatAutoSettle", json!({"enabled":false}), |meta| !meta["autoSettleDisabledAt"].is_null()).await?;
        change(&client, id, "setChatAutoSettle", json!({"enabled":true}), |meta| meta["autoSettleDisabledAt"].is_null()).await?;
        change(&client, &created[1], "pinChat", json!({}), |meta| !meta["pinnedAt"].is_null()).await?;
        let reordered = change(&client, id, "changeChatPin", json!({"change":SidebarPinChange::Move {
            session_id:id.clone(),after:None,before:Some(created[1].clone())
        }}), |meta| meta["pinOrderKey"].is_string()).await?;
        let threads = reordered.as_array().unwrap().iter().map(|row| Ok((row["id"].as_str().unwrap().to_owned(), serde_json::from_value::<SidebarThread>(row["t3Sidebar"].clone())?)))
            .collect::<Result<std::collections::HashMap<_, _>>>()?;
        let pins = sidebar_pins(&threads, Utc::now());
        ensure!(pins.iter().position(|item| item == id).context("first pin missing")? < pins.iter().position(|item| item == &created[1]).context("second pin missing")?, "drag order did not persist");
        change(&client, &created[1], "snoozeChat", json!({"until":until}), |meta| !meta["snoozedUntil"].is_null()).await?;
        let repinned = change(&client, &created[1], "changeChatPin", json!({"change":SidebarPinChange::Pin {
            session_id:created[1].clone(),after:None,before:Some(id.clone())
        }}), |meta| meta["snoozedUntil"].is_null() && !meta["pinnedAt"].is_null()).await?;
        ensure!(row(&repinned, &created[1])["t3Sidebar"]["pinOrderKey"].as_str().context("second pin key missing")?
            < row(&repinned, id)["t3Sidebar"]["pinOrderKey"].as_str().context("first pin key missing")?, "snoozed pin drop did not wake into the requested position");
        change(&client, id, "unpinChat", json!({}), |meta| meta["pinnedAt"].is_null()).await?;
        client.call(methods::FOCUS_CHAT, json!({"chatId":existing})).await?;
        let visited = observe(&client, |frame| !row(frame, &existing)["t3Sidebar"]["lastVisitedAt"].is_null()).await?;
        let last_visit = row(&visited, &existing)["t3Sidebar"]["lastVisitedAt"].clone();
        change(&client, &existing, "markChatUnread", json!({}), |meta| meta["lastVisitedAt"] != last_visit).await?;
        client.call(methods::FOCUS_CHAT, json!({"chatId":existing})).await?;
        println!("Native T3 settle/un-settle, pin/unpin/reorder, snooze/wake, auto-settle and unread writes passed.");
        Ok(())
    }.await;
    for id in &created {
        client
            .call(methods::MUTATE, json!({"op":"deleteChat","chatId":id}))
            .await?;
    }
    observe(&client, |frame| {
        created
            .iter()
            .all(|id| frame.as_array().unwrap().iter().all(|row| row["id"] != *id))
    })
    .await?;
    result
}
