//! Adapt Effect RPC envelopes to Zeron's existing bounded request multiplexer.

use anyhow::{Context, Result, bail, ensure};
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::sync::{mpsc, watch};
use tokio_tungstenite::tungstenite::Message;
use url::Url;
use zeron_rpc::{ClientFrame, RpcClient};

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ConnectionConfig {
    pub origin: String,
    pub environment_id: String,
    /// An existing paired T3 bearer credential. Never included in a socket URL.
    pub access_token_file: PathBuf,
}

impl ConnectionConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let config: Self = serde_json::from_slice(&std::fs::read(path)?)?;
        config.base_url()?;
        ensure!(
            !config.environment_id.trim().is_empty(),
            "environmentId is required"
        );
        ensure!(
            config.access_token_file.is_absolute(),
            "accessTokenFile must be absolute"
        );
        Ok(config)
    }

    fn base_url(&self) -> Result<Url> {
        let url = Url::parse(&self.origin).context("invalid T3 origin")?;
        ensure!(
            url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none()
                && url.path() == "/",
            "origin must contain only scheme, host and port"
        );
        let loopback = match url.host() {
            Some(url::Host::Domain("localhost")) => true,
            Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
            Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
            _ => false,
        };
        ensure!(
            url.scheme() == "https" || (url.scheme() == "http" && loopback),
            "remote T3 connections require HTTPS; use an SSH forward for plain HTTP"
        );
        Ok(url)
    }

    pub async fn connect(&self) -> Result<Session> {
        let base = self.base_url()?;
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(15))
            .build()?;
        let descriptor: Value = http
            .get(base.join(".well-known/t3/environment")?)
            .send()
            .await
            .context("T3 descriptor unavailable")?
            .error_for_status()?
            .json()
            .await?;
        validate_descriptor(&descriptor, &self.environment_id)?;
        // Check identity before reading or sending a credential, on every dial.
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            ensure!(
                std::fs::metadata(&self.access_token_file)?.mode() & 0o077 == 0,
                "T3 access token file must be private (chmod 600)"
            );
        }
        let token = std::fs::read_to_string(&self.access_token_file)?;
        ensure!(!token.trim().is_empty(), "T3 access token file is empty");
        let ticket: Value = http
            .post(base.join("api/auth/websocket-ticket")?)
            .bearer_auth(token.trim())
            .send()
            .await
            .context("T3 ticket request failed")?
            .error_for_status()?
            .json()
            .await?;
        let ticket = ticket
            .get("ticket")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .context("T3 did not issue a websocket ticket")?;
        let mut socket = base.join("ws")?;
        socket
            .set_scheme(if base.scheme() == "https" {
                "wss"
            } else {
                "ws"
            })
            .unwrap();
        socket
            .query_pairs_mut()
            .append_pair("wsTicket", ticket)
            .append_pair("orchestrationProtocol", "2");
        // The handshake error can contain its URL and ticket. Report only its class.
        let (ws, _) = tokio::time::timeout(
            Duration::from_secs(15),
            tokio_tungstenite::connect_async(socket.as_str()),
        )
        .await
        .context("T3 websocket timed out")?
        .map_err(|_| anyhow::anyhow!("T3 websocket handshake failed"))?;
        let (out, mut outgoing) = mpsc::channel::<String>(256);
        let (incoming, input) = mpsc::channel::<String>(256);
        let (closed_tx, closed) = watch::channel(false);
        let client = Arc::new(RpcClient::new(out, input));
        tokio::spawn(async move {
            let (mut writer, mut reader) = ws.split();
            let mut heartbeat = tokio::time::interval(Duration::from_secs(20));
            let mut last_pong = tokio::time::Instant::now();
            let result: Result<()> = async {
                loop {
                    tokio::select! {
                        outgoing = outgoing.recv() => {
                            let Some(outgoing) = outgoing else { break; };
                            let frame: ClientFrame = serde_json::from_str(&outgoing)?;
                            writer.send(Message::Text(request_frame(frame)?.to_string())).await?;
                        }
                        message = reader.next() => {
                            let Some(message) = message else { break; };
                            match message? {
                                Message::Text(text) => {
                                    let value: Value = serde_json::from_str(&text)?;
                                    let frames = if let Value::Array(values) = value { values } else { vec![value] };
                                    for frame in frames {
                                        if frame["_tag"] == "Pong" { last_pong = tokio::time::Instant::now(); continue; }
                                        let (replies, ack) = response_frames(&frame)?;
                                        for reply in replies {
                                            if incoming.send(reply.to_string()).await.is_err() { return Ok(()); }
                                        }
                                        if let Some(ack) = ack { writer.send(Message::Text(ack.to_string())).await?; }
                                    }
                                }
                                Message::Ping(bytes) => writer.send(Message::Pong(bytes)).await?,
                                Message::Close(_) => break,
                                Message::Pong(_) => {},
                                _ => bail!("unsupported T3 websocket frame"),
                            }
                        }
                        _ = heartbeat.tick() => {
                            ensure!(last_pong.elapsed() < Duration::from_secs(60), "T3 heartbeat timed out");
                            writer.send(Message::Text(json!({"_tag":"Ping"}).to_string())).await?;
                        }
                    }
                }
                Ok(())
            }.await;
            // Errors may contain arbitrary server data; don't log credential-bearing frames.
            if result.is_err() {
                tracing::warn!("T3 transport disconnected");
            }
            let _ = closed_tx.send(true);
        });
        Ok(Session { client, closed })
    }
}

pub struct Session {
    pub client: Arc<RpcClient>,
    pub closed: watch::Receiver<bool>,
}

pub fn validate_descriptor(descriptor: &Value, expected: &str) -> Result<()> {
    ensure!(
        descriptor["environmentId"].as_str() == Some(expected),
        "T3 environment identity mismatch"
    );
    ensure!(
        descriptor["orchestrationProtocolVersion"].as_u64() == Some(2),
        "this native client requires T3 orchestration protocol 2"
    );
    Ok(())
}

fn request_frame(frame: ClientFrame) -> Result<Value> {
    if frame.cancel {
        return Ok(json!({"_tag":"Interrupt", "requestId":frame.id.to_string()}));
    }
    let method = frame.method.context("missing RPC method")?;
    Ok(
        json!({"_tag":"Request", "id":frame.id.to_string(), "tag":method,
        "payload":frame.params, "headers":[]}),
    )
}

fn response_frames(frame: &Value) -> Result<(Vec<Value>, Option<Value>)> {
    let tag = frame["_tag"].as_str().context("missing Effect RPC tag")?;
    if matches!(tag, "Defect" | "ClientProtocolError") {
        bail!("T3 protocol failure");
    }
    let id = frame["requestId"]
        .as_u64()
        .or_else(|| frame["requestId"].as_str()?.parse().ok())
        .context("invalid Effect RPC request id")?;
    match tag {
        "Chunk" => {
            let values = frame["values"]
                .as_array()
                .context("invalid Effect RPC chunk")?;
            ensure!(!values.is_empty(), "empty Effect RPC chunk");
            Ok((
                values
                    .iter()
                    .map(|item| json!({"id":id,"item":item}))
                    .collect(),
                Some(json!({"_tag":"Ack", "requestId":frame["requestId"]})),
            ))
        }
        "Exit" if frame["exit"]["_tag"] == "Success" => Ok((
            vec![
                json!({"id":id,"ok":frame["exit"]["value"]}),
                json!({"id":id,"done":true}),
            ],
            None,
        )),
        "Exit" if frame["exit"]["_tag"] == "Failure" => {
            let error = frame["exit"]["cause"]
                .as_array()
                .and_then(|causes| {
                    causes
                        .iter()
                        .find_map(|cause| cause["error"]["message"].as_str())
                })
                .unwrap_or("T3 request failed or was interrupted");
            Ok((vec![json!({"id":id,"err":error})], None))
        }
        _ => bail!("unsupported Effect RPC envelope"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effect_frames_preserve_errors_acknowledgements_and_cancellation() {
        let request = request_frame(ClientFrame {
            id: 12,
            method: Some("server.probe".into()),
            params: json!({}),
            cancel: false,
        })
        .unwrap();
        assert_eq!(request["id"], "12");
        assert_eq!(request["headers"], json!([]));
        let (replies, ack) =
            response_frames(&json!({"_tag":"Chunk","requestId":"12","values":[1,2]})).unwrap();
        assert_eq!(
            replies,
            vec![json!({"id":12,"item":1}), json!({"id":12,"item":2})]
        );
        assert_eq!(ack.unwrap(), json!({"_tag":"Ack","requestId":"12"}));
        let (replies, _) = response_frames(&json!({"_tag":"Exit","requestId":12,
            "exit":{"_tag":"Failure","cause":[{"_tag":"Fail","error":{"message":"permission denied"}}]}})).unwrap();
        assert_eq!(replies[0]["err"], "permission denied");
        assert_eq!(
            request_frame(ClientFrame {
                id: 12,
                method: None,
                params: Value::Null,
                cancel: true
            })
            .unwrap(),
            json!({"_tag":"Interrupt","requestId":"12"})
        );
        assert!(response_frames(&json!({"_tag":"Chunk","requestId":"12","values":[]})).is_err());
    }

    #[test]
    fn identity_protocol_and_secure_origin_are_mandatory() {
        assert!(
            validate_descriptor(
                &json!({"environmentId":"other","orchestrationProtocolVersion":2}),
                "env"
            )
            .is_err()
        );
        assert!(validate_descriptor(&json!({"environmentId":"env"}), "env").is_err());
        assert!(
            validate_descriptor(
                &json!({"environmentId":"env","orchestrationProtocolVersion":2}),
                "env"
            )
            .is_ok()
        );
        for origin in [
            "http://remote.test",
            "https://user:secret@remote.test",
            "https://remote.test?token=x",
        ] {
            let config = ConnectionConfig {
                origin: origin.into(),
                environment_id: "env".into(),
                access_token_file: "/token".into(),
            };
            assert!(config.base_url().is_err());
        }
        for origin in [
            "http://localhost:39741",
            "http://127.0.0.1:39741",
            "http://[::1]:39741",
            "https://remote.test",
        ] {
            let config = ConnectionConfig {
                origin: origin.into(),
                environment_id: "env".into(),
                access_token_file: "/token".into(),
            };
            assert!(config.base_url().is_ok());
        }
    }
}
