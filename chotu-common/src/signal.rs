use std::{
    collections::HashMap,
    io,
    path::{Path, PathBuf},
    str,
    sync::Arc,
    time::Duration,
};

use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde_json::{json, Value};
use thiserror::Error;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{unix::OwnedWriteHalf, UnixStream},
    sync::{broadcast, mpsc, oneshot},
    time::Instant,
};

const RECEIVE_BUFFER: usize = 64;
const RECONNECT_BACKOFF: [Duration; 5] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(4),
    Duration::from_secs(8),
    Duration::from_secs(60),
];

#[derive(Debug, Error)]
pub enum SignalError {
    #[error("signal socket I/O error: {0}")]
    Io(#[source] io::Error),
    #[error("signal socket closed")]
    Eof,
    #[error("signal client is reconnecting")]
    Reconnecting,
    #[error("malformed UTF-8 JSON-RPC frame: {0}")]
    Utf8(String),
    #[error("malformed JSON-RPC frame: {0}")]
    Json(String),
    #[error("signal JSON-RPC error ({code:?}): {message}")]
    Rpc {
        code: Option<i64>,
        message: String,
        data: Option<Value>,
    },
    #[error("signal protocol error: {0}")]
    Protocol(String),
    #[error("signal response did not contain a timestamp")]
    MissingTimestamp,
    #[error("invalid base64 attachment: {0}")]
    Base64(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum SignalRecipient {
    Direct { aci: String },
    Group { group_id: String },
}

impl SignalRecipient {
    pub fn lookup_aci(&self) -> &str {
        match self {
            Self::Direct { aci } => aci,
            Self::Group { .. } => "",
        }
    }

    pub fn group_id(&self) -> Option<&str> {
        match self {
            Self::Group { group_id } => Some(group_id),
            Self::Direct { .. } => None,
        }
    }
}

impl std::fmt::Display for SignalRecipient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Direct { aci } => write!(f, "direct:{aci}"),
            Self::Group { group_id } => write!(f, "group:{group_id}"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignalAttachment {
    pub id: String,
    pub content_type: String,
    pub size: Option<u64>,
    pub caption: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignalInbound {
    pub sender_aci: String,
    pub recipient: SignalRecipient,
    pub text: Option<String>,
    pub quote_timestamp: Option<i64>,
    pub attachments: Vec<SignalAttachment>,
}

#[derive(Clone)]
pub struct SignalClient {
    inner: Arc<SignalInner>,
}

struct SignalInner {
    commands: mpsc::UnboundedSender<Command>,
    inbound: broadcast::Sender<SignalInbound>,
}

enum Command {
    Request {
        method: &'static str,
        params: Value,
        response: oneshot::Sender<Result<Value, SignalError>>,
    },
}

enum ReaderEvent {
    Frame { generation: u64, frame: Vec<u8> },
    Closed { generation: u64, error: SignalError },
}

impl SignalClient {
    pub async fn connect(socket_path: impl AsRef<Path>) -> Result<Self, SignalError> {
        let socket_path = socket_path.as_ref().to_path_buf();
        let stream = UnixStream::connect(&socket_path)
            .await
            .map_err(SignalError::Io)?;
        let (inbound, _) = broadcast::channel(RECEIVE_BUFFER);
        let (commands, command_rx) = mpsc::unbounded_channel();
        let client = Self {
            inner: Arc::new(SignalInner {
                commands,
                inbound,
            }),
        };
        tokio::spawn(run_io_loop(
            socket_path,
            stream,
            command_rx,
            client.inner.inbound.clone(),
        ));
        Ok(client)
    }

    pub async fn subscribe_receive(
        &self,
    ) -> Result<broadcast::Receiver<SignalInbound>, SignalError> {
        let receiver = self.inner.inbound.subscribe();
        let _ = self.request("subscribeReceive", json!({})).await?;
        Ok(receiver)
    }

    pub async fn send_text(
        &self,
        recipient: &SignalRecipient,
        text: &str,
    ) -> Result<i64, SignalError> {
        let params = match recipient {
            SignalRecipient::Direct { aci } => json!({
                "recipient": [aci],
                "message": text,
            }),
            SignalRecipient::Group { group_id } => json!({
                "groupId": group_id,
                "message": text,
            }),
        };
        let result = self.request("send", params).await?;
        timestamp_from_result(&result)
    }

    pub async fn get_attachment(
        &self,
        recipient: &SignalRecipient,
        attachment_id: &str,
    ) -> Result<Vec<u8>, SignalError> {
        let params = match recipient {
            SignalRecipient::Direct { aci } => json!({
                "id": attachment_id,
                "recipient": aci,
            }),
            SignalRecipient::Group { group_id } => json!({
                "id": attachment_id,
                "groupId": group_id,
            }),
        };
        let result = self.request("getAttachment", params).await?;
        let encoded = result
            .as_str()
            .or_else(|| result.get("data").and_then(Value::as_str))
            .ok_or_else(|| SignalError::Protocol("attachment result was not base64".into()))?;
        STANDARD
            .decode(encoded)
            .map_err(|error| SignalError::Base64(error.to_string()))
    }

    async fn request(&self, method: &'static str, params: Value) -> Result<Value, SignalError> {
        let (response_tx, response_rx) = oneshot::channel();
        self.inner
            .commands
            .send(Command::Request {
                method,
                params,
                response: response_tx,
            })
            .map_err(|_| SignalError::Eof)?;
        response_rx.await.map_err(|_| SignalError::Eof)?
    }
}

async fn run_io_loop(
    socket_path: PathBuf,
    initial_stream: UnixStream,
    mut commands: mpsc::UnboundedReceiver<Command>,
    inbound: broadcast::Sender<SignalInbound>,
) {
    let (initial_reader, initial_writer) = initial_stream.into_split();
    let (events, mut event_rx) = mpsc::unbounded_channel();
    let mut writer = Some(initial_writer);
    let mut generation = 0_u64;
    spawn_reader(initial_reader, events.clone(), generation);

    let mut pending: HashMap<u64, oneshot::Sender<Result<Value, SignalError>>> = HashMap::new();
    let mut next_id = 1_u64;
    let mut subscription_requested = false;
    let mut reconnect_deadline: Option<Instant> = None;
    let mut backoff_index = 0_usize;

    loop {
        tokio::select! {
            command = commands.recv() => {
                let Some(Command::Request { method, params, response }) = command else {
                    return;
                };
                if method == "subscribeReceive" {
                    subscription_requested = true;
                }
                if let Some(current) = writer.as_mut() {
                    let request_id = next_id;
                    next_id = next_id.wrapping_add(1).max(1);
                    pending.insert(request_id, response);
                    if let Err(error) = write_request(current, request_id, method, params).await {
                        if let Some(waiter) = pending.remove(&request_id) {
                            let _ = waiter.send(Err(SignalError::Io(error)));
                        }
                        disconnect(
                            &mut writer,
                            &mut pending,
                            &mut reconnect_deadline,
                            &mut backoff_index,
                            SignalError::Eof,
                        );
                    }
                } else {
                    let _ = response.send(Err(SignalError::Reconnecting));
                }
            }
            event = event_rx.recv() => {
                let Some(event) = event else {
                    return;
                };
                match event {
                    ReaderEvent::Frame { generation: event_generation, frame }
                        if event_generation == generation && writer.is_some() =>
                    {
                        if let Err(error) = dispatch_frame(&frame, &mut pending, &inbound) {
                            disconnect(
                                &mut writer,
                                &mut pending,
                                &mut reconnect_deadline,
                                &mut backoff_index,
                                error,
                            );
                        }
                    }
                    ReaderEvent::Closed { generation: event_generation, error }
                        if event_generation == generation && writer.is_some() =>
                    {
                        disconnect(
                            &mut writer,
                            &mut pending,
                            &mut reconnect_deadline,
                            &mut backoff_index,
                            error,
                        );
                    }
                    _ => {}
                }
            }
            _ = tokio::time::sleep_until(reconnect_deadline.unwrap_or_else(|| Instant::now() + Duration::from_secs(86_400))), if writer.is_none() && reconnect_deadline.is_some() => {
                match UnixStream::connect(&socket_path).await {
                    Ok(stream) => {
                        let (reader, new_writer) = stream.into_split();
                        writer = Some(new_writer);
                        generation = generation.wrapping_add(1);
                        reconnect_deadline = None;
                        backoff_index = 0;
                        spawn_reader(reader, events.clone(), generation);
                        if subscription_requested {
                            let request_id = next_id;
                            next_id = next_id.wrapping_add(1).max(1);
                            let (waiter, _ignored) = oneshot::channel();
                            pending.insert(request_id, waiter);
                            let current = writer.as_mut().expect("just connected");
                            if let Err(error) = write_request(current, request_id, "subscribeReceive", json!({})).await {
                                pending.remove(&request_id);
                                disconnect(
                                    &mut writer,
                                    &mut pending,
                                    &mut reconnect_deadline,
                                    &mut backoff_index,
                                    SignalError::Io(error),
                                );
                            }
                        }
                    }
                    Err(_) => {
                        reconnect_deadline = Some(Instant::now() + RECONNECT_BACKOFF[backoff_index]);
                        backoff_index = (backoff_index + 1).min(RECONNECT_BACKOFF.len() - 1);
                    }
                }
            }
        }

        if writer.is_none() && reconnect_deadline.is_none() {
            reconnect_deadline = Some(Instant::now() + RECONNECT_BACKOFF[backoff_index]);
        }
    }
}

fn spawn_reader(
    reader: tokio::net::unix::OwnedReadHalf,
    events: mpsc::UnboundedSender<ReaderEvent>,
    generation: u64,
) {
    tokio::spawn(async move {
        let mut reader = BufReader::new(reader);
        loop {
            let mut frame = Vec::new();
            match reader.read_until(b'\n', &mut frame).await {
                Ok(0) => {
                    let _ = events.send(ReaderEvent::Closed {
                        generation,
                        error: SignalError::Eof,
                    });
                    return;
                }
                Ok(_) => {
                    if events
                        .send(ReaderEvent::Frame { generation, frame })
                        .is_err()
                    {
                        return;
                    }
                }
                Err(error) => {
                    let _ = events.send(ReaderEvent::Closed {
                        generation,
                        error: SignalError::Io(error),
                    });
                    return;
                }
            }
        }
    });
}


async fn write_request(
    writer: &mut OwnedWriteHalf,
    id: u64,
    method: &'static str,
    params: Value,
) -> Result<(), io::Error> {
    let mut frame = serde_json::to_vec(&json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    }))
    .map_err(|error| io::Error::new(io::ErrorKind::Other, error))?;
    frame.push(b'\n');
    writer.write_all(&frame).await
}

fn dispatch_frame(
    frame: &[u8],
    pending: &mut HashMap<u64, oneshot::Sender<Result<Value, SignalError>>>,
    inbound: &broadcast::Sender<SignalInbound>,
) -> Result<(), SignalError> {
    let frame = str::from_utf8(frame)
        .map_err(|error| SignalError::Utf8(error.to_string()))?
        .trim_end_matches(['\r', '\n']);
    let message: Value = serde_json::from_str(frame)
        .map_err(|error| SignalError::Json(error.to_string()))?;

    if message.get("method").and_then(Value::as_str) == Some("receive") {
        if let Some(receive) = parse_receive(&message)? {
            let _ = inbound.send(receive);
        }
        return Ok(());
    }

    let id = message
        .get("id")
        .and_then(Value::as_u64)
        .ok_or_else(|| SignalError::Protocol("response did not contain an integer id".into()))?;
    let Some(waiter) = pending.remove(&id) else {
        return Ok(());
    };
    if let Some(error) = message.get("error") {
        let code = error.get("code").and_then(Value::as_i64);
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("unknown signal daemon error")
            .to_string();
        let _ = waiter.send(Err(SignalError::Rpc {
            code,
            message,
            data: error.get("data").cloned(),
        }));
    } else {
        let _ = waiter.send(Ok(message.get("result").cloned().unwrap_or(Value::Null)));
    }
    Ok(())
}

fn parse_receive(message: &Value) -> Result<Option<SignalInbound>, SignalError> {
    let envelope = message
        .get("params")
        .and_then(|params| params.get("result"))
        .and_then(|result| result.get("envelope"))
        .ok_or_else(|| SignalError::Protocol("receive notification lacked result.envelope".into()))?;
    let Some(sender_aci) = envelope.get("sourceUuid").and_then(Value::as_str) else {
        return Ok(None);
    };
    if sender_aci.is_empty() {
        return Ok(None);
    }
    let Some(data_message) = envelope.get("dataMessage") else {
        return Ok(None);
    };

    let recipient = data_message
        .get("groupInfo")
        .and_then(|group_info| group_info.get("groupId"))
        .and_then(Value::as_str)
        .filter(|group_id| !group_id.is_empty())
        .map(|group_id| SignalRecipient::Group {
            group_id: group_id.to_string(),
        })
        .unwrap_or_else(|| SignalRecipient::Direct {
            aci: sender_aci.to_string(),
        });

    let text = data_message
        .get("message")
        .and_then(Value::as_str)
        .map(str::to_string);
    let quote_timestamp = data_message
        .get("quote")
        .and_then(|quote| quote.get("id"))
        .and_then(Value::as_i64);

    let mut attachments = Vec::new();
    if let Some(raw_attachments) = data_message.get("attachments") {
        let Some(raw_attachments) = raw_attachments.as_array() else {
            return Ok(None);
        };
        for attachment in raw_attachments {
            attachments.push(SignalAttachment {
                id: attachment
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                content_type: attachment
                    .get("contentType")
                    .and_then(Value::as_str)
                    .unwrap_or("application/octet-stream")
                    .to_string(),
                size: attachment.get("size").and_then(Value::as_u64),
                caption: attachment
                    .get("caption")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            });
        }
    }

    if text.is_none() && attachments.is_empty() {
        return Ok(None);
    }
    Ok(Some(SignalInbound {
        sender_aci: sender_aci.to_string(),
        recipient,
        text,
        quote_timestamp,
        attachments,
    }))
}

fn timestamp_from_result(result: &Value) -> Result<i64, SignalError> {
    result
        .as_i64()
        .or_else(|| result.get("timestamp").and_then(Value::as_i64))
        .ok_or(SignalError::MissingTimestamp)
}

fn disconnect(
    writer: &mut Option<OwnedWriteHalf>,
    pending: &mut HashMap<u64, oneshot::Sender<Result<Value, SignalError>>>,
    reconnect_deadline: &mut Option<Instant>,
    backoff_index: &mut usize,
    error: SignalError,
) {
    *writer = None;
    for (_, waiter) in pending.drain() {
        let _ = waiter.send(Err(error_for_pending(&error)));
    }
    *reconnect_deadline = Some(Instant::now() + RECONNECT_BACKOFF[*backoff_index]);
    *backoff_index = (*backoff_index + 1).min(RECONNECT_BACKOFF.len() - 1);
}

fn error_for_pending(error: &SignalError) -> SignalError {
    match error {
        SignalError::Io(error) => SignalError::Io(io::Error::new(error.kind(), error.to_string())),
        SignalError::Eof => SignalError::Eof,
        SignalError::Reconnecting => SignalError::Reconnecting,
        SignalError::Utf8(error) => SignalError::Utf8(error.clone()),
        SignalError::Json(error) => SignalError::Json(error.clone()),
        SignalError::Rpc {
            code,
            message,
            data,
        } => SignalError::Rpc {
            code: *code,
            message: message.clone(),
            data: data.clone(),
        },
        SignalError::Protocol(error) => SignalError::Protocol(error.clone()),
        SignalError::MissingTimestamp => SignalError::MissingTimestamp,
        SignalError::Base64(error) => SignalError::Base64(error.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixListener;

    async fn socket() -> (tempfile::TempDir, UnixListener, PathBuf) {
        let dir = tempdir().unwrap();
        let path = dir.path().join("signal.sock");
        let listener = UnixListener::bind(&path).unwrap();
        (dir, listener, path)
    }

    async fn request(reader: &mut BufReader<tokio::net::unix::OwnedReadHalf>) -> Value {
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        serde_json::from_str(&line).unwrap()
    }

    #[tokio::test]
    async fn interleaves_receive_notification_and_matches_response() {
        let (_dir, listener, path) = socket().await;
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read, mut write) = stream.into_split();
            let mut reader = BufReader::new(read);
            let subscribe = request(&mut reader).await;
            assert_eq!(subscribe["method"], "subscribeReceive");
            write.write_all(format!("{}\n", json!({"jsonrpc":"2.0","id":subscribe["id"],"result":{}})).as_bytes()).await.unwrap();
            let send = request(&mut reader).await;
            let notification = json!({
                "jsonrpc":"2.0", "method":"receive", "params":{"result":{"envelope":{
                    "sourceUuid":"aci-1", "dataMessage":{"message":"hello","quote":{"id":42}}
                }}}
            });
            write.write_all(format!("{}\n{}\n", notification, json!({"jsonrpc":"2.0","id":send["id"],"result":{"timestamp":17}})).as_bytes()).await.unwrap();
        });
        let client = SignalClient::connect(&path).await.unwrap();
        let mut receives = client.subscribe_receive().await.unwrap();
        let timestamp = client.send_text(&SignalRecipient::Direct { aci: "aci-2".into() }, "out").await.unwrap();
        assert_eq!(timestamp, 17);
        let received = receives.recv().await.unwrap();
        assert_eq!(received.sender_aci, "aci-1");
        assert_eq!(received.quote_timestamp, Some(42));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn sends_direct_and_group_payloads() {
        let (_dir, listener, path) = socket().await;
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read, mut write) = stream.into_split();
            let mut reader = BufReader::new(read);
            for (timestamp, expected) in [(3, json!({"recipient":["aci"],"message":"one"})), (4, json!({"groupId":"group","message":"two"}))] {
                let request = request(&mut reader).await;
                assert_eq!(request["method"], "send");
                assert_eq!(request["params"], expected);
                write.write_all(format!("{}\n", json!({"jsonrpc":"2.0","id":request["id"],"result":{"timestamp":timestamp}})).as_bytes()).await.unwrap();
            }
        });
        let client = SignalClient::connect(&path).await.unwrap();
        assert_eq!(client.send_text(&SignalRecipient::Direct { aci: "aci".into() }, "one").await.unwrap(), 3);
        assert_eq!(client.send_text(&SignalRecipient::Group { group_id: "group".into() }, "two").await.unwrap(), 4);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn gets_and_decodes_attachment_for_direct_and_group() {
        let (_dir, listener, path) = socket().await;
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read, mut write) = stream.into_split();
            let mut reader = BufReader::new(read);
            for expected in [json!({"id":"one","recipient":"aci"}), json!({"id":"two","groupId":"group"})] {
                let request = request(&mut reader).await;
                assert_eq!(request["method"], "getAttachment");
                assert_eq!(request["params"], expected);
                write.write_all(format!("{}\n", json!({"jsonrpc":"2.0","id":request["id"],"result":"aGVsbG8="})).as_bytes()).await.unwrap();
            }
        });
        let client = SignalClient::connect(&path).await.unwrap();
        assert_eq!(client.get_attachment(&SignalRecipient::Direct { aci: "aci".into() }, "one").await.unwrap(), b"hello");
        assert_eq!(client.get_attachment(&SignalRecipient::Group { group_id: "group".into() }, "two").await.unwrap(), b"hello");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn propagates_rpc_and_malformed_frame_errors() {
        let (_dir, listener, path) = socket().await;
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read, mut write) = stream.into_split();
            let mut reader = BufReader::new(read);
            let first_request = request(&mut reader).await;
            write.write_all(format!("{}\n", json!({"jsonrpc":"2.0","id":first_request["id"],"error":{"code":-1,"message":"nope"}})).as_bytes()).await.unwrap();
            let second_request = request(&mut reader).await;
            assert_eq!(second_request["method"], "send");
            write.write_all(b"not-json\n").await.unwrap();
        });
        let client = SignalClient::connect(&path).await.unwrap();
        let error = client.subscribe_receive().await.unwrap_err();
        assert!(matches!(error, SignalError::Rpc { code: Some(-1), .. }));
        let error = client.send_text(&SignalRecipient::Direct { aci: "aci".into() }, "x").await.unwrap_err();
        assert!(matches!(error, SignalError::Json(_)));
        server.await.unwrap();
    }
}
