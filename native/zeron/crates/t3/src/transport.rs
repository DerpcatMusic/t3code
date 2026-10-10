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
    pub async fn browser_bootstrap(&self) -> Result<Value> {
        let base = self.base_url()?;
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(15))
            .build()?;
        let descriptor: Value = http
            .get(base.join(".well-known/t3/environment")?)
            .send()
            .await
            .context("T3 environment is unavailable")?
            .error_for_status()?
            .json()
            .await?;
        validate_descriptor(&descriptor, &self.environment_id)?;
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
        let session: Value = http
            .get(base.join("api/auth/session")?)
            .bearer_auth(token.trim())
            .send()
            .await
            .context("T3 authentication is unavailable")?
            .error_for_status()?
            .json()
            .await?;
        ensure!(
            session["authenticated"] == true,
            "T3 login expired; restart Z3-code to renew it"
        );
        let cookie = session["auth"]["sessionCookieName"]
            .as_str()
            .context("T3 did not advertise browser authentication")?;
        ensure!(
            !cookie.is_empty()
                && cookie
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'),
            "Invalid T3 cookie name"
        );
        Ok(
            json!({"origin":self.origin,"cookieName":cookie,"accessToken":token.trim(),"label":descriptor["label"]}),
        )
    }

    pub async fn projects(&self, mutation: Option<Value>) -> Result<Value> {
        let session = self.browser_bootstrap().await?;
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(15))
            .build()?;
        let base = self.base_url()?;
        let request = match mutation {
            Some(body) => http.post(base.join("api/projects/mutate")?).json(&body),
            None => http.get(base.join("api/projects")?),
        };
        let response = request
            .bearer_auth(
                session["accessToken"]
                    .as_str()
                    .context("T3 authentication missing")?,
            )
            .send()
            .await
            .map_err(|_| anyhow::anyhow!("T3 project request failed"))?;
        ensure!(
            response.status().is_success(),
            "T3 project request rejected (HTTP {})",
            response.status().as_u16()
        );
        response.json().await.context("Invalid T3 project response")
    }

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
        let task = tokio::spawn(async move {
            let (mut writer, mut reader) = ws.split();
            let now = tokio::time::Instant::now();
            let mut heartbeat =
                tokio::time::interval_at(now + Duration::from_secs(20), Duration::from_secs(20));
            heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            let mut last_pong = now;
            let mut category = "peer_closed";
            let mut close_code = None::<u16>;
            let result: Result<()> = async {
                loop {
                    tokio::select! {
                        outgoing = outgoing.recv() => {
                            let Some(outgoing) = outgoing else { category = "client_closed"; break; };
                            let frame: ClientFrame = serde_json::from_str(&outgoing).map_err(|error| { category = "request_encoding"; error })?;
                            let request = request_frame(frame).inspect_err(|_| category = "request_encoding")?;
                            writer.send(Message::Text(request.to_string())).await.inspect_err(|_| category = "socket_write")?;
                        }
                        message = reader.next() => {
                            let Some(message) = message else { break; };
                            match message.inspect_err(|_| category = "socket_read")? {
                                message @ (Message::Text(_) | Message::Binary(_)) => {
                                    // Effect layerJson accepts UTF-8 bytes as well as strings.
                                    // WebSocket opcodes are transport details, not the RPC codec.
                                    let frames = payload_frames(message).inspect_err(|_| category = "payload_decode")?;
                                    for frame in frames {
                                        if frame["_tag"] == "Pong" { last_pong = tokio::time::Instant::now(); continue; }
                                        let (replies, ack) = response_frames(&frame).inspect_err(|_| category = "rpc_envelope")?;
                                        for reply in replies {
                                            if incoming.send(reply.to_string()).await.is_err() { category = "client_closed"; return Ok(()); }
                                        }
                                        if let Some(ack) = ack { writer.send(Message::Text(ack.to_string())).await.inspect_err(|_| category = "socket_write")?; }
                                    }
                                }
                                Message::Ping(bytes) => writer.send(Message::Pong(bytes)).await.inspect_err(|_| category = "socket_write")?,
                                Message::Close(frame) => {
                                    close_code = frame.map(|frame| u16::from(frame.code));
                                    break;
                                },
                                Message::Pong(_) => {},
                                _ => { category = "websocket_opcode"; bail!("unsupported T3 websocket frame"); },
                            }
                        }
                        _ = heartbeat.tick() => {
                            if last_pong.elapsed() >= Duration::from_secs(60) {
                                category = "heartbeat_timeout";
                                bail!("T3 heartbeat timed out");
                            }
                            writer.send(Message::Text(json!({"_tag":"Ping"}).to_string())).await.inspect_err(|_| category = "socket_write")?;
                        }
                    }
                }
                Ok(())
            }.await;
            // Errors may contain arbitrary server data; don't log credential-bearing frames.
            if result.is_err() {
                tracing::warn!(
                    category,
                    last_pong_age_seconds = last_pong.elapsed().as_secs(),
                    "T3 transport disconnected"
                );
            } else {
                // Close reason text is untrusted and may contain credentials.
                tracing::debug!(category, close_code, "T3 transport closed");
            }
            let _ = closed_tx.send(true);
        });
        Ok(Session {
            client,
            closed,
            task,
        })
    }
}

pub struct Session {
    pub client: Arc<RpcClient>,
    pub closed: watch::Receiver<bool>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Session {
    fn drop(&mut self) {
        self.task.abort();
    }
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

fn payload_frames(message: Message) -> Result<Vec<Value>> {
    let value: Value = match message {
        Message::Text(text) => serde_json::from_str(&text)?,
        Message::Binary(bytes) => serde_json::from_slice(&bytes)?,
        _ => bail!("expected T3 JSON payload"),
    };
    Ok(if let Value::Array(values) = value {
        values
    } else {
        vec![value]
    })
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

    #[tokio::test]
    async fn dropping_a_session_stops_its_background_transport() {
        struct SignalOnDrop(Option<tokio::sync::oneshot::Sender<()>>);
        impl Drop for SignalOnDrop {
            fn drop(&mut self) {
                if let Some(signal) = self.0.take() {
                    let _ = signal.send(());
                }
            }
        }
        let (started, ready) = tokio::sync::oneshot::channel();
        let (stopped, done) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let _signal = SignalOnDrop(Some(stopped));
            let _ = started.send(());
            std::future::pending::<()>().await;
        });
        ready.await.unwrap();
        let (out, _outgoing) = mpsc::channel(1);
        let (_incoming, input) = mpsc::channel(1);
        let session = Session {
            client: Arc::new(RpcClient::new(out, input)),
            closed: watch::channel(false).1,
            task,
        };
        drop(session);
        tokio::time::timeout(Duration::from_secs(1), done)
            .await
            .unwrap()
            .unwrap();
    }

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
    fn effect_json_heartbeat_and_chunks_accept_text_and_binary_payloads() {
        for envelope in [
            json!({"_tag":"Pong"}),
            json!([{"_tag":"Pong"}, {"_tag":"Chunk","requestId":"12","values":[{"kind":"synchronized"}]}]),
        ] {
            let encoded = envelope.to_string();
            let text = payload_frames(Message::Text(encoded.clone())).unwrap();
            let binary = payload_frames(Message::Binary(encoded.into_bytes())).unwrap();
            assert_eq!(text, binary);
            assert_eq!(binary[0]["_tag"], "Pong");
            if binary.len() > 1 {
                let (_, ack) = response_frames(&binary[1]).unwrap();
                assert_eq!(ack.unwrap()["requestId"], "12");
            }
        }
    }

    #[test]
    fn malformed_binary_json_is_not_accepted_as_a_heartbeat() {
        assert!(payload_frames(Message::Binary(vec![0xff])).is_err());
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
