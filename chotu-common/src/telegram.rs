use std::time::Duration;

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use reqwest::StatusCode;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::chat::{
    ChatAddress, ChatAttachment, ChatError, ChatInbound, ChatMessageId, ChatProvider,
    ConversationKind,
};

const TELEGRAM_API_BASE: &str = "https://api.telegram.org";
const LONG_POLL_SECONDS: u64 = 50;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(LONG_POLL_SECONDS + 10);

#[derive(Clone)]
pub struct TelegramAdapter {
    inner: Arc<TelegramInner>,
}

struct TelegramInner {
    client: reqwest::Client,
    api_base: String,
    token: String,
    username: String,
    polling: AtomicBool,
}

impl TelegramAdapter {
    pub async fn connect(token: String) -> Result<Self, ChatError> {
        let api_base = std::env::var("TELEGRAM_BOT_API_BASE_URL")
            .unwrap_or_else(|_| TELEGRAM_API_BASE.to_string());
        Self::connect_with_base_url(token, api_base).await
    }

    async fn connect_with_base_url(
        token: String,
        api_base: impl Into<String>,
    ) -> Result<Self, ChatError> {
        if token.trim().is_empty() {
            return Err(ChatError::Configuration(
                "TELEGRAM_BOT_TOKEN must not be blank".into(),
            ));
        }
        let provisional = Self {
            inner: Arc::new(TelegramInner {
                client: reqwest::Client::builder()
                    .connect_timeout(CONNECT_TIMEOUT)
                    .timeout(REQUEST_TIMEOUT)
                    .build()
                    .map_err(classify_reqwest)?,
                api_base: api_base.into().trim_end_matches('/').to_string(),
                token,
                username: String::new(),
                polling: AtomicBool::new(false),
            }),
        };
        let me: TelegramUser = provisional.call("getMe", &Empty {}).await?;
        let username = me.username.ok_or_else(|| {
            ChatError::Protocol("Telegram getMe response omitted the bot username".into())
        })?;
        let adapter = Self {
            inner: Arc::new(TelegramInner {
                client: provisional.inner.client.clone(),
                api_base: provisional.inner.api_base.clone(),
                token: provisional.inner.token.clone(),
                username,
                polling: AtomicBool::new(false),
            }),
        };
        // A zero-timeout probe detects a configured webhook before other agents start.
        let _: Vec<TelegramUpdate> = adapter
            .call(
                "getUpdates",
                &GetUpdatesRequest {
                    offset: None,
                    timeout: 0,
                    limit: Some(1),
                    allowed_updates: ["message"],
                },
            )
            .await?;
        Ok(adapter)
    }

    pub(crate) async fn start_polling(
        &self,
        sender: mpsc::Sender<Result<ChatInbound, ChatError>>,
    ) -> Result<(), ChatError> {
        if self.inner.polling.swap(true, Ordering::AcqRel) {
            return Err(ChatError::Configuration(
                "Telegram inbound updates already have an active subscriber".into(),
            ));
        }
        let adapter = self.clone();
        tokio::spawn(async move {
            let result = adapter.poll(sender.clone()).await;
            adapter.inner.polling.store(false, Ordering::Release);
            if let Err(error) = result {
                let _ = sender.send(Err(error)).await;
            }
        });
        Ok(())
    }

    pub(crate) async fn send_text(
        &self,
        recipient: &ChatAddress,
        text: &str,
    ) -> Result<ChatMessageId, ChatError> {
        if recipient.provider != ChatProvider::Telegram {
            return Err(ChatError::PermanentRequest(format!(
                "Telegram adapter received {} address",
                recipient.provider
            )));
        }
        let message: SentMessage = self
            .call(
                "sendMessage",
                &SendMessageRequest {
                    chat_id: &recipient.id,
                    text,
                },
            )
            .await?;
        Ok(ChatMessageId(message.message_id.to_string()))
    }

    pub(crate) async fn download_attachment(
        &self,
        attachment_id: &str,
    ) -> Result<Vec<u8>, ChatError> {
        let file: TelegramFile = self
            .call(
                "getFile",
                &GetFileRequest {
                    file_id: attachment_id,
                },
            )
            .await?;
        let path = file.file_path.ok_or_else(|| {
            ChatError::Protocol("Telegram getFile response omitted file_path".into())
        })?;
        let url = format!(
            "{}/file/bot{}/{}",
            self.inner.api_base,
            self.inner.token,
            path.trim_start_matches('/')
        );
        let response = self
            .inner
            .client
            .get(url)
            .send()
            .await
            .map_err(classify_reqwest)?;
        if !response.status().is_success() {
            return Err(classify_status(response.status(), "Telegram file download"));
        }
        response
            .bytes()
            .await
            .map(|bytes| bytes.to_vec())
            .map_err(classify_reqwest)
    }

    async fn poll(
        &self,
        sender: mpsc::Sender<Result<ChatInbound, ChatError>>,
    ) -> Result<(), ChatError> {
        let mut offset = 0_i64;
        loop {
            let updates: Vec<TelegramUpdate> = match self
                .call(
                    "getUpdates",
                    &GetUpdatesRequest {
                        offset: (offset > 0).then_some(offset),
                        timeout: LONG_POLL_SECONDS,
                        limit: None,
                        allowed_updates: ["message"],
                    },
                )
                .await
            {
                Ok(updates) => updates,
                Err(error) if error.is_retryable() => {
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                    continue;
                }
                Err(error) => return Err(error),
            };

            for update in updates {
                let next_offset = update.update_id.saturating_add(1);
                if let Some(inbound) = normalize_update(update, &self.inner.username)? {
                    sender.send(Ok(inbound)).await.map_err(|_| {
                        ChatError::Protocol("Telegram inbound receiver closed".into())
                    })?;
                }
                offset = offset.max(next_offset);
            }
        }
    }

    async fn call<R: DeserializeOwned, B: Serialize + ?Sized>(
        &self,
        method: &str,
        body: &B,
    ) -> Result<R, ChatError> {
        let url = format!("{}/bot{}/{}", self.inner.api_base, self.inner.token, method);
        let response = self
            .inner
            .client
            .post(url)
            .json(body)
            .send()
            .await
            .map_err(classify_reqwest)?;
        let status = response.status();
        let bytes = response.bytes().await.map_err(classify_reqwest)?;
        let envelope: ApiResponse<R> = match serde_json::from_slice(&bytes) {
            Ok(envelope) => envelope,
            Err(_) if !status.is_success() => {
                return Err(classify_status(status, &format!("Telegram {method}")));
            }
            Err(error) => {
                return Err(ChatError::Protocol(format!(
                    "Telegram {method} returned malformed JSON: {error}"
                )));
            }
        };
        if status.is_success() && envelope.ok {
            return envelope.result.ok_or_else(|| {
                ChatError::Protocol(format!("Telegram {method} response omitted result"))
            });
        }
        Err(classify_api_error(
            status,
            envelope.error_code,
            envelope.description.as_deref(),
            method,
        ))
    }
}

fn normalize_update(
    update: TelegramUpdate,
    bot_username: &str,
) -> Result<Option<ChatInbound>, ChatError> {
    let Some(message) = update.message else {
        return Ok(None);
    };
    let Some(sender) = message.from else {
        return Ok(None);
    };
    let kind = match message.chat.kind.as_str() {
        "private" => ConversationKind::Direct,
        "group" | "supergroup" => ConversationKind::Group,
        other => {
            return Err(ChatError::Protocol(format!(
                "unsupported Telegram chat type `{other}`"
            )))
        }
    };
    let caption = message.caption.filter(|caption| !caption.is_empty());
    let text = message
        .text
        .or_else(|| caption.clone())
        .map(|text| normalize_bot_command(text, bot_username));
    let mut attachments = Vec::new();
    if let Some(photo) = message
        .photo
        .unwrap_or_default()
        .into_iter()
        .max_by_key(|photo| photo.file_size.unwrap_or(0))
    {
        attachments.push(ChatAttachment {
            id: photo.file_id,
            content_type: "image/jpeg".into(),
            size: photo.file_size,
            caption: caption.clone(),
        });
    } else if let Some(document) = message.document {
        let content_type = document
            .mime_type
            .unwrap_or_else(|| "application/octet-stream".into());
        if content_type.starts_with("image/") {
            attachments.push(ChatAttachment {
                id: document.file_id,
                content_type,
                size: document.file_size,
                caption: caption.clone(),
            });
        }
    }

    Ok(Some(ChatInbound {
        sender_id: sender.id.to_string(),
        conversation: ChatAddress {
            provider: ChatProvider::Telegram,
            kind,
            id: message.chat.id.to_string(),
        },
        text,
        reply_to: message
            .reply_to_message
            .map(|reply| ChatMessageId(reply.message_id.to_string())),
        attachments,
    }))
}

fn normalize_bot_command(text: String, bot_username: &str) -> String {
    let Some(first_end) = text.find(char::is_whitespace) else {
        return normalize_command_token(&text, bot_username).unwrap_or(text);
    };
    let (first, rest) = text.split_at(first_end);
    match normalize_command_token(first, bot_username) {
        Some(command) => format!("{command}{rest}"),
        None => text,
    }
}

fn normalize_command_token(token: &str, bot_username: &str) -> Option<String> {
    let (command, addressed_to) = token.strip_prefix('/')?.split_once('@')?;
    if addressed_to.eq_ignore_ascii_case(bot_username) && !command.is_empty() {
        Some(format!("/{command}"))
    } else {
        None
    }
}

fn classify_reqwest(error: reqwest::Error) -> ChatError {
    let transient =
        error.is_connect() || error.is_timeout() || error.is_request() || error.is_body();
    let message = error.without_url().to_string();
    if transient {
        ChatError::TransientTransport(message)
    } else {
        ChatError::Protocol(message)
    }
}

fn classify_status(status: StatusCode, context: &str) -> ChatError {
    if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
        ChatError::Authentication(format!("{context} failed with HTTP {status}"))
    } else if status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
        ChatError::TransientTransport(format!("{context} failed with HTTP {status}"))
    } else {
        ChatError::PermanentRequest(format!("{context} failed with HTTP {status}"))
    }
}

fn classify_api_error(
    status: StatusCode,
    error_code: Option<i64>,
    description: Option<&str>,
    method: &str,
) -> ChatError {
    let code = error_code.unwrap_or(i64::from(status.as_u16()));
    let description = description.unwrap_or("unknown Bot API error");
    let message = format!("Telegram {method} failed ({code}): {description}");
    if code == 401 || code == 403 {
        ChatError::Authentication(message)
    } else if code == 429 || code >= 500 || status.is_server_error() {
        ChatError::TransientTransport(message)
    } else {
        ChatError::PermanentRequest(message)
    }
}

#[derive(Serialize)]
struct Empty {}

#[derive(Serialize)]
struct GetUpdatesRequest<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    offset: Option<i64>,
    timeout: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    limit: Option<u8>,
    allowed_updates: [&'a str; 1],
}

#[derive(Serialize)]
struct SendMessageRequest<'a> {
    chat_id: &'a str,
    text: &'a str,
}

#[derive(Serialize)]
struct GetFileRequest<'a> {
    file_id: &'a str,
}

#[derive(Deserialize)]
struct ApiResponse<T> {
    ok: bool,
    result: Option<T>,
    error_code: Option<i64>,
    description: Option<String>,
}

#[derive(Deserialize)]
struct TelegramUser {
    #[allow(dead_code)]
    id: i64,
    username: Option<String>,
}

#[derive(Deserialize)]
struct TelegramUpdate {
    update_id: i64,
    message: Option<TelegramMessage>,
}

#[derive(Deserialize)]
struct TelegramMessage {
    #[allow(dead_code)]
    message_id: i64,
    from: Option<TelegramUser>,
    chat: TelegramChat,
    text: Option<String>,
    caption: Option<String>,
    photo: Option<Vec<TelegramPhotoSize>>,
    document: Option<TelegramDocument>,
    reply_to_message: Option<Box<TelegramMessageRef>>,
}

#[derive(Deserialize)]
struct TelegramMessageRef {
    message_id: i64,
}

#[derive(Deserialize)]
struct TelegramChat {
    id: i64,
    #[serde(rename = "type")]
    kind: String,
}

#[derive(Deserialize)]
struct TelegramPhotoSize {
    file_id: String,
    file_size: Option<u64>,
}

#[derive(Deserialize)]
struct TelegramDocument {
    file_id: String,
    file_size: Option<u64>,
    mime_type: Option<String>,
}

#[derive(Deserialize)]
struct SentMessage {
    message_id: i64,
}

#[derive(Deserialize)]
struct TelegramFile {
    file_path: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    async fn stub_server(
        replies: Vec<(&'static str, u16, &'static [u8])>,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            for (expected_path, status, response_body) in replies {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let mut buffer = [0_u8; 2048];
                let header_end = loop {
                    let count = stream.read(&mut buffer).await.unwrap();
                    assert!(count > 0, "client closed before sending HTTP headers");
                    request.extend_from_slice(&buffer[..count]);
                    if let Some(end) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                        break end + 4;
                    }
                };
                let headers = String::from_utf8_lossy(&request[..header_end]);
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        line.strip_prefix("content-length: ")
                            .or_else(|| line.strip_prefix("Content-Length: "))
                    })
                    .and_then(|value| value.parse::<usize>().ok())
                    .unwrap_or(0);
                while request.len() < header_end + content_length {
                    let count = stream.read(&mut buffer).await.unwrap();
                    assert!(count > 0, "client closed before sending HTTP body");
                    request.extend_from_slice(&buffer[..count]);
                }
                let request = String::from_utf8_lossy(&request);
                let request_line = request.lines().next().unwrap();
                assert!(
                    request_line.contains(expected_path),
                    "expected path {expected_path}, got {request_line}"
                );
                let reason = match status {
                    200 => "OK",
                    401 => "Unauthorized",
                    409 => "Conflict",
                    _ => "Error",
                };
                let header = format!(
                    "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    response_body.len()
                );
                stream.write_all(header.as_bytes()).await.unwrap();
                stream.write_all(response_body).await.unwrap();
            }
        });
        (format!("http://{address}"), handle)
    }

    #[test]
    fn normalizes_addressed_commands_and_images() {
        let inbound = normalize_update(
            TelegramUpdate {
                update_id: 7,
                message: Some(TelegramMessage {
                    message_id: 12,
                    from: Some(TelegramUser {
                        id: 42,
                        username: None,
                    }),
                    chat: TelegramChat {
                        id: -1009,
                        kind: "supergroup".into(),
                    },
                    text: None,
                    caption: Some("/food@ChotuBot lunch".into()),
                    photo: Some(vec![
                        TelegramPhotoSize {
                            file_id: "small".into(),
                            file_size: Some(10),
                        },
                        TelegramPhotoSize {
                            file_id: "large".into(),
                            file_size: Some(20),
                        },
                    ]),
                    document: None,
                    reply_to_message: Some(Box::new(TelegramMessageRef { message_id: 11 })),
                }),
            },
            "chotubot",
        )
        .unwrap()
        .unwrap();

        assert_eq!(inbound.sender_id, "42");
        assert_eq!(
            inbound.conversation,
            ChatAddress::group(ChatProvider::Telegram, "-1009")
        );
        assert_eq!(inbound.text.as_deref(), Some("/food lunch"));
        assert_eq!(inbound.reply_to, Some(ChatMessageId("11".into())));
        assert_eq!(inbound.attachments[0].id, "large");
        assert_eq!(
            inbound.attachments[0].caption.as_deref(),
            Some("/food@ChotuBot lunch")
        );
    }

    #[test]
    fn leaves_commands_for_other_bots_unchanged() {
        assert_eq!(
            normalize_bot_command("/tasks@OtherBot open".into(), "ChotuBot"),
            "/tasks@OtherBot open"
        );
    }

    #[test]
    fn classifies_bot_api_failures() {
        assert!(matches!(
            classify_api_error(
                StatusCode::CONFLICT,
                Some(409),
                Some("webhook active"),
                "getUpdates"
            ),
            ChatError::PermanentRequest(_)
        ));
        assert!(matches!(
            classify_api_error(
                StatusCode::TOO_MANY_REQUESTS,
                Some(429),
                None,
                "sendMessage"
            ),
            ChatError::TransientTransport(_)
        ));
        assert!(matches!(
            classify_api_error(StatusCode::UNAUTHORIZED, Some(401), None, "getMe"),
            ChatError::Authentication(_)
        ));
    }
    #[tokio::test]
    async fn bot_api_client_connects_sends_and_downloads() {
        let (base_url, server) = stub_server(vec![
            (
                "/botTEST/getMe",
                200,
                br#"{"ok":true,"result":{"id":1,"username":"ChotuBot"}}"#,
            ),
            ("/botTEST/getUpdates", 200, br#"{"ok":true,"result":[]}"#),
            (
                "/botTEST/sendMessage",
                200,
                br#"{"ok":true,"result":{"message_id":55}}"#,
            ),
            (
                "/botTEST/sendMessage",
                200,
                br#"{"ok":true,"result":{"message_id":56}}"#,
            ),
            (
                "/botTEST/getFile",
                200,
                br#"{"ok":true,"result":{"file_path":"photos/test.jpg"}}"#,
            ),
            ("/file/botTEST/photos/test.jpg", 200, b"image-bytes"),
        ])
        .await;

        let adapter = TelegramAdapter::connect_with_base_url("TEST".into(), base_url)
            .await
            .unwrap();
        let client = crate::chat::ChatClient::Telegram(adapter);
        let recipient = ChatAddress::direct(ChatProvider::Telegram, "101");
        let message_id = client
            .send_text(&recipient, &"x".repeat(4_097))
            .await
            .unwrap();
        let image = client
            .download_attachment(&recipient, "photo-id")
            .await
            .unwrap();

        assert_eq!(message_id, ChatMessageId("56".into()));
        assert_eq!(image, b"image-bytes");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn startup_reports_authentication_and_webhook_conflicts() {
        let (auth_url, auth_server) = stub_server(vec![(
            "/botBAD/getMe",
            401,
            br#"{"ok":false,"error_code":401,"description":"Unauthorized"}"#,
        )])
        .await;
        let auth_error = TelegramAdapter::connect_with_base_url("BAD".into(), auth_url)
            .await
            .err()
            .expect("invalid token must fail startup");
        assert!(matches!(auth_error, ChatError::Authentication(_)));
        auth_server.await.unwrap();
        let (outage_url, outage_server) =
            stub_server(vec![("/botTEST/getMe", 500, b"upstream unavailable")]).await;
        let outage_error = TelegramAdapter::connect_with_base_url("TEST".into(), outage_url)
            .await
            .err()
            .expect("server outage must fail startup");
        assert!(matches!(outage_error, ChatError::TransientTransport(_)));
        outage_server.await.unwrap();

        let (webhook_url, webhook_server) = stub_server(vec![
            (
                "/botTEST/getMe",
                200,
                br#"{"ok":true,"result":{"id":1,"username":"ChotuBot"}}"#,
            ),
            (
                "/botTEST/getUpdates",
                409,
                br#"{"ok":false,"error_code":409,"description":"Conflict: webhook is active"}"#,
            ),
        ])
        .await;
        let webhook_error = TelegramAdapter::connect_with_base_url("TEST".into(), webhook_url)
            .await
            .err()
            .expect("active webhook must fail startup");
        assert!(matches!(webhook_error, ChatError::PermanentRequest(_)));
        webhook_server.await.unwrap();
    }
}
