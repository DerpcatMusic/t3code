use anyhow::{Context, Result, ensure};
use base64::{
    Engine,
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap},
    time::{Duration, Instant},
};
use tokio::sync::Mutex;
use url::Url;
use zeron_rpc::{RpcClient, RpcError};

const MAX_BYTES: usize = 24 * 1024 * 1024;
const MAX_BUFFER_BYTES: usize = 128 * 1024 * 1024;
const CHUNK_CHARS: usize = 680_000;
const LIFETIME: Duration = Duration::from_secs(180);

pub fn attachment_path(attachment: &Value) -> Result<String> {
    let name = attachment["name"]
        .as_str()
        .context("Attachment name is missing")?;
    ensure!(
        !name.is_empty() && name.len() <= 255 && !name.contains(['\n', '\r', '/', '\\']),
        "Invalid attachment name"
    );
    Ok(format!(
        "t3-attachment:{}/{name}",
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(attachment)?)
    ))
}

pub fn parse_attachment_path(path: &str) -> Option<Value> {
    let encoded = path.strip_prefix("t3-attachment:")?.split_once('/')?.0;
    if encoded.len() > 4096 {
        return None;
    }
    let value: Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(encoded).ok()?).ok()?;
    let id = value["id"].as_str()?;
    let name = value["name"].as_str()?;
    let mime = value["mimeType"].as_str()?;
    (matches!(value["type"].as_str(), Some("image" | "file"))
        && !id.is_empty()
        && id.len() <= 256
        && !name.is_empty()
        && name.len() <= 255
        && !name.contains(['\n', '\r', '/', '\\'])
        && !mime.is_empty()
        && mime.len() <= 100
        && value["sizeBytes"].as_u64().is_some())
    .then_some(value)
}

pub fn prompt_without_refs(prompt: &str, paths: &[String]) -> String {
    let trailer = format!(
        "\n\nAttached images (local files — open them to view):\n{}",
        paths
            .iter()
            .map(|p| format!("- {p}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    if !paths.is_empty() {
        prompt.strip_suffix(&trailer).unwrap_or(prompt).to_owned()
    } else {
        prompt.to_owned()
    }
}

// MCP results arrive as compact objects, JSON strings, or content envelopes.
pub fn visual_reference(output: &Value) -> Option<Value> {
    fn read(value: &Value, depth: usize) -> Option<Value> {
        if depth > 6 || value["isError"] == true || value["is_error"] == true {
            return None;
        }
        if let Some(text) = value.as_str().filter(|s| s.len() <= 256 * 1024) {
            return read(&serde_json::from_str::<Value>(text).ok()?, depth + 1);
        }
        if let Some(rows) = value.as_array().filter(|r| r.len() <= 256) {
            return rows
                .iter()
                .find_map(|row| read(row.get("text").unwrap_or(row), depth + 1));
        }
        for key in ["structuredContent", "content"] {
            if let Some(content) = value.get(key)
                && let Some(found) = read(content, depth + 1)
            {
                return Some(found);
            }
        }
        let reference = &value["htmlRender"];
        let id = reference["attachmentId"].as_str()?;
        let title = reference["title"].as_str()?;
        (!id.is_empty() && id.len() <= 256 && title.len() <= 1024).then(|| reference.clone())
    }
    read(output, 0)
}

struct Upload {
    chunks: BTreeMap<u64, String>,
    touched: Instant,
}
struct Download {
    name: String,
    mime: String,
    data: String,
    touched: Instant,
}
#[derive(Default)]
struct Buffers {
    uploads: HashMap<String, Upload>,
    downloads: HashMap<String, Download>,
}
impl Buffers {
    fn clean(&mut self) {
        self.uploads
            .retain(|_, row| row.touched.elapsed() < LIFETIME);
        self.downloads
            .retain(|_, row| row.touched.elapsed() < LIFETIME);
    }
    fn bytes(&self) -> usize {
        self.uploads
            .values()
            .flat_map(|row| row.chunks.values())
            .map(String::len)
            .sum::<usize>()
            + self
                .downloads
                .values()
                .map(|row| row.data.len())
                .sum::<usize>()
    }
}

pub(super) struct Attachments {
    origin: Url,
    http: reqwest::Client,
    buffers: Mutex<Buffers>,
}
impl Attachments {
    pub fn new(origin: &str) -> Result<Self> {
        Ok(Self {
            origin: Url::parse(origin)?,
            http: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(150))
                .build()?,
            buffers: Mutex::new(Buffers::default()),
        })
    }

    fn signed_url(&self, relative: &str) -> Result<Url, RpcError> {
        let url = self
            .origin
            .join(relative)
            .map_err(|_| bad("Invalid T3 asset URL"))?;
        if !relative.starts_with("/api/")
            || relative.starts_with("//")
            || url.origin() != self.origin.origin()
            || !url.username().is_empty()
            || url.password().is_some()
        {
            return Err(bad("The asset URL belongs to another environment"));
        }
        Ok(url)
    }

    pub async fn url(&self, client: &RpcClient, resource: Value) -> Result<String, RpcError> {
        let minted = client
            .call("assets.createUrl", json!({"resource":resource}))
            .await?;
        Ok(self
            .signed_url(
                minted["relativeUrl"]
                    .as_str()
                    .ok_or_else(|| bad("T3 did not return an asset URL"))?,
            )?
            .to_string())
    }

    pub async fn chunk(&self, params: Value) -> Result<Value, RpcError> {
        let id = field(&params, "uploadId")?;
        if uuid::Uuid::parse_str(id).is_err() {
            return Err(bad("Invalid upload id"));
        }
        let seq = params["seq"]
            .as_u64()
            .filter(|s| *s < 100)
            .ok_or_else(|| bad("Invalid upload sequence"))?;
        let data = field(&params, "data")?;
        if data.len() > CHUNK_CHARS
            || !data
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'='))
        {
            return Err(bad("Invalid upload chunk"));
        }
        let mut buffers = self.buffers.lock().await;
        buffers.clean();
        if buffers.bytes().saturating_add(data.len()) > MAX_BUFFER_BYTES {
            return Err(bad(
                "Too many attachments are transferring. Try again shortly.",
            ));
        }
        let row = buffers.uploads.entry(id.into()).or_insert_with(|| Upload {
            chunks: BTreeMap::new(),
            touched: Instant::now(),
        });
        if let Some(old) = row.chunks.get(&seq) {
            if old != data {
                return Err(bad("Upload chunk changed; attach the file again"));
            }
        } else {
            if row.chunks.values().map(String::len).sum::<usize>() + data.len()
                > MAX_BYTES.div_ceil(3) * 4
            {
                return Err(bad("Attachment exceeds 24 MB"));
            }
            row.chunks.insert(seq, data.into());
        }
        row.touched = Instant::now();
        Ok(json!({"ok":true}))
    }

    pub async fn commit(&self, client: &RpcClient, params: Value) -> Result<Value, RpcError> {
        let id = field(&params, "uploadId")?;
        let name = field(&params, "fileName")?;
        if name.is_empty() || name.len() > 255 || name.contains(['\n', '\r', '/', '\\']) {
            return Err(bad("Invalid attachment name"));
        }
        let encoded = {
            let mut buffers = self.buffers.lock().await;
            buffers.clean();
            let row = buffers
                .uploads
                .get_mut(id)
                .ok_or_else(|| bad("Upload expired; attach the file again"))?;
            if row
                .chunks
                .keys()
                .enumerate()
                .any(|(n, seq)| n as u64 != *seq)
            {
                return Err(bad("The upload is incomplete"));
            }
            row.touched = Instant::now();
            row.chunks.values().cloned().collect::<String>()
        };
        let bytes = STANDARD
            .decode(encoded)
            .map_err(|_| bad("Invalid attachment bytes"))?;
        if bytes.is_empty() || bytes.len() > MAX_BYTES {
            return Err(bad("Attachment must contain between 1 byte and 24 MB"));
        }
        let mime = mime_type(name, &bytes);
        let kind = if matches!(
            mime,
            "image/png" | "image/jpeg" | "image/webp" | "image/gif"
        ) {
            "image"
        } else {
            "file"
        };
        let minted = client
            .call(
                "attachments.createUploadUrl",
                json!({"type":kind,"name":name,"mimeType":mime,"sizeBytes":bytes.len()}),
            )
            .await?;
        let attachment_id = field(&minted, "attachmentId")?;
        let url = self.signed_url(field(&minted, "relativeUrl")?)?;
        // Signed URLs contain credentials: never include reqwest errors in user-facing diagnostics.
        let response = self
            .http
            .post(url)
            .header("Content-Type", mime)
            .body(bytes.clone())
            .send()
            .await;
        if !response.is_ok_and(|r| r.status().is_success()) {
            let _ = client
                .call("attachments.delete", json!({"attachmentId":attachment_id}))
                .await;
            return Err(bad("Attachment upload failed. Try sending again."));
        }
        let attachment = json!({"type":kind,"id":attachment_id,"name":name,"mimeType":mime,"sizeBytes":bytes.len()});
        let path = attachment_path(&attachment).map_err(|_| bad("Invalid attachment metadata"))?;
        let mut buffers = self.buffers.lock().await;
        buffers.uploads.remove(id);
        if buffers.bytes() + bytes.len().div_ceil(3) * 4 <= MAX_BUFFER_BYTES {
            buffers.downloads.insert(
                path.clone(),
                Download {
                    name: name.into(),
                    mime: mime.into(),
                    data: STANDARD.encode(bytes),
                    touched: Instant::now(),
                },
            );
        }
        Ok(json!({"path":path}))
    }

    pub async fn read(&self, client: &RpcClient, params: Value) -> Result<Value, RpcError> {
        let path = field(&params, "path")?;
        let attachment =
            parse_attachment_path(path).ok_or_else(|| bad("Invalid attachment reference"))?;
        let offset = params["offset"]
            .as_u64()
            .and_then(|n| usize::try_from(n).ok())
            .ok_or_else(|| bad("Invalid attachment offset"))?;
        {
            let mut buffers = self.buffers.lock().await;
            buffers.clean();
            if let Some(row) = buffers.downloads.get_mut(path) {
                return download_chunk(row, offset);
            }
        }
        let url = self.url(client, json!({"_tag":"attachment","attachmentId":attachment["id"],"fileName":attachment["name"],"mimeType":attachment["mimeType"]})).await?;
        let mut response = self
            .http
            .get(url)
            .send()
            .await
            .map_err(|_| bad("Could not download the attachment"))?;
        if !response.status().is_success()
            || response
                .content_length()
                .is_some_and(|n| n > MAX_BYTES as u64)
        {
            return Err(bad("Attachment unavailable or larger than 24 MB"));
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| bad("Attachment transfer interrupted"))?
        {
            if bytes.len().saturating_add(chunk.len()) > MAX_BYTES {
                return Err(bad("Attachment exceeds 24 MB"));
            }
            bytes.extend_from_slice(&chunk);
        }
        let mut row = Download {
            name: field(&attachment, "name")?.into(),
            mime: field(&attachment, "mimeType")?.into(),
            data: STANDARD.encode(bytes),
            touched: Instant::now(),
        };
        let result = download_chunk(&mut row, offset)?;
        let mut buffers = self.buffers.lock().await;
        if buffers.bytes() + row.data.len() <= MAX_BUFFER_BYTES {
            buffers.downloads.insert(path.into(), row);
        }
        Ok(result)
    }
}

fn bad(message: &str) -> RpcError {
    RpcError::BadParams(message.into())
}
fn field<'a>(value: &'a Value, key: &str) -> Result<&'a str, RpcError> {
    value[key]
        .as_str()
        .ok_or_else(|| bad("Missing attachment metadata"))
}
fn download_chunk(row: &mut Download, offset: usize) -> Result<Value, RpcError> {
    if offset > row.data.len() || offset % 4 != 0 {
        return Err(bad("Invalid attachment offset"));
    }
    let next = (offset + CHUNK_CHARS).min(row.data.len());
    row.touched = Instant::now();
    Ok(
        json!({"name":row.name,"mimeType":row.mime,"data":&row.data[offset..next],"done":next == row.data.len(),"nextOffset":next}),
    )
}
fn mime_type(name: &str, bytes: &[u8]) -> &'static str {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return "image/png";
    }
    if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        return "image/jpeg";
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return "image/gif";
    }
    if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        return "image/webp";
    }
    match name
        .rsplit('.')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "txt" | "md" | "log" => "text/plain",
        "html" | "htm" => "text/html",
        "json" => "application/json",
        "pdf" => "application/pdf",
        "csv" => "text/csv",
        "svg" => "image/svg+xml",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn references_preserve_names_and_keep_remote_paths_out_of_prompts() {
        let attachment = json!({"type":"image","id":"remote-id","name":"my image.png","mimeType":"image/png","sizeBytes":12});
        let path = attachment_path(&attachment).unwrap();
        assert_eq!(parse_attachment_path(&path), Some(attachment));
        let prompt =
            format!("Review this\n\nAttached images (local files — open them to view):\n- {path}");
        assert_eq!(prompt_without_refs(&prompt, &[path]), "Review this");
        assert!(parse_attachment_path("/etc/passwd").is_none());
        assert!(attachment_path(&json!({"name":"bad\nname.png"})).is_err());
    }
    #[tokio::test]
    async fn uploads_are_idempotent_bounded_and_do_not_accept_changed_chunks() {
        let store = Attachments::new("http://127.0.0.1:39741").unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let chunk = json!({"uploadId":id,"seq":0,"data":"YWJj"});
        store.chunk(chunk.clone()).await.unwrap();
        store.chunk(chunk).await.unwrap();
        assert!(
            store
                .chunk(json!({"uploadId":id,"seq":0,"data":"ZGVm"}))
                .await
                .is_err()
        );
        assert!(
            store
                .chunk(json!({"uploadId":id,"seq":100,"data":"YWJj"}))
                .await
                .is_err()
        );
        assert!(
            store
                .signed_url("https://other.example/api/asset?token=x")
                .is_err()
        );
        assert!(
            store
                .signed_url("//other.example/api/asset?token=x")
                .is_err()
        );
        assert!(store.signed_url("/api/asset?token=x").is_ok());
    }
    #[test]
    fn published_visuals_survive_mcp_envelopes_and_failed_results_stay_hidden() {
        let output = json!({"content":[{"type":"text","text":json!({"htmlRender":{"attachmentId":"page","title":"Interactive chart","height":420}}).to_string()}]});
        assert_eq!(visual_reference(&output).unwrap()["attachmentId"], "page");
        assert!(visual_reference(&json!({"isError":true,"structuredContent":{"htmlRender":{"attachmentId":"page","title":"bad"}}})).is_none());
    }
}
