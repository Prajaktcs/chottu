use std::{fmt, str::FromStr};

use tokio::sync::mpsc;

use crate::{
    signal::{SignalClient, SignalError, SignalInbound, SignalRecipient},
    telegram::TelegramAdapter,
};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ChatProvider {
    Signal,
    Telegram,
}

impl ChatProvider {
    pub fn from_env() -> Result<Self, ChatError> {
        match std::env::var("CHOTU_CHAT_PROVIDER") {
            Ok(value) => value.parse(),
            Err(std::env::VarError::NotPresent) => Ok(Self::Signal),
            Err(error) => Err(ChatError::Configuration(format!(
                "failed to read CHOTU_CHAT_PROVIDER: {error}"
            ))),
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Signal => "signal",
            Self::Telegram => "telegram",
        }
    }
}

impl fmt::Display for ChatProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for ChatProvider {
    type Err = ChatError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "signal" => Ok(Self::Signal),
            "telegram" => Ok(Self::Telegram),
            other => Err(ChatError::Configuration(format!(
                "unsupported CHOTU_CHAT_PROVIDER `{other}`; expected `signal` or `telegram`"
            ))),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ConversationKind {
    Direct,
    Group,
}

impl ConversationKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::Group => "group",
        }
    }
}

impl fmt::Display for ConversationKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for ConversationKind {
    type Err = ChatError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "direct" => Ok(Self::Direct),
            "group" => Ok(Self::Group),
            other => Err(ChatError::Protocol(format!(
                "invalid conversation kind `{other}`"
            ))),
        }
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ChatAddress {
    pub provider: ChatProvider,
    pub kind: ConversationKind,
    pub id: String,
}

impl ChatAddress {
    pub fn direct(provider: ChatProvider, id: impl Into<String>) -> Self {
        Self {
            provider,
            kind: ConversationKind::Direct,
            id: id.into(),
        }
    }

    pub fn group(provider: ChatProvider, id: impl Into<String>) -> Self {
        Self {
            provider,
            kind: ConversationKind::Group,
            id: id.into(),
        }
    }
}

impl fmt::Display for ChatAddress {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}:{}:{}", self.provider, self.kind, self.id)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChatAttachment {
    pub id: String,
    pub content_type: String,
    pub size: Option<u64>,
    pub caption: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChatInbound {
    pub sender_id: String,
    pub conversation: ChatAddress,
    pub text: Option<String>,
    pub reply_to: Option<ChatMessageId>,
    pub attachments: Vec<ChatAttachment>,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ChatMessageId(pub String);

impl fmt::Display for ChatMessageId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ChatError {
    #[error("chat configuration error: {0}")]
    Configuration(String),
    #[error("chat authentication error: {0}")]
    Authentication(String),
    #[error("transient chat transport error: {0}")]
    TransientTransport(String),
    #[error("permanent chat request error: {0}")]
    PermanentRequest(String),
    #[error("chat protocol error: {0}")]
    Protocol(String),
}

impl ChatError {
    pub const fn is_retryable(&self) -> bool {
        matches!(self, Self::TransientTransport(_))
    }
}

#[derive(Clone)]
pub enum ChatClient {
    Signal(SignalClient),
    Telegram(TelegramAdapter),
}

pub struct ChatReceiver {
    receiver: mpsc::Receiver<Result<ChatInbound, ChatError>>,
}

impl ChatReceiver {
    pub async fn recv(&mut self) -> Result<ChatInbound, ChatError> {
        self.receiver
            .recv()
            .await
            .unwrap_or_else(|| Err(ChatError::Protocol("chat subscription closed".into())))
    }
}

impl ChatClient {
    pub async fn connect(provider: ChatProvider) -> Result<Self, ChatError> {
        match provider {
            ChatProvider::Signal => {
                let socket = required_env("SIGNAL_CLI_SOCKET", provider)?;
                SignalClient::connect(&socket)
                    .await
                    .map(Self::Signal)
                    .map_err(chat_error_from_signal)
            }
            ChatProvider::Telegram => {
                let token = required_env("TELEGRAM_BOT_TOKEN", provider)?;
                TelegramAdapter::connect(token).await.map(Self::Telegram)
            }
        }
    }

    pub const fn provider(&self) -> ChatProvider {
        match self {
            Self::Signal(_) => ChatProvider::Signal,
            Self::Telegram(_) => ChatProvider::Telegram,
        }
    }

    pub const fn max_text_chars(&self) -> usize {
        match self {
            Self::Signal(_) => 4_000,
            Self::Telegram(_) => 4_096,
        }
    }

    pub async fn subscribe(&self) -> Result<ChatReceiver, ChatError> {
        let (sender, receiver) = mpsc::channel(64);
        match self {
            Self::Signal(client) => {
                let mut signal_receiver = client
                    .subscribe_receive()
                    .await
                    .map_err(chat_error_from_signal)?;
                tokio::spawn(async move {
                    loop {
                        let result = match signal_receiver.recv().await {
                            Ok(inbound) => Ok(chat_inbound_from_signal(inbound)),
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                                Err(ChatError::TransientTransport(format!(
                                    "signal receive buffer lagged by {skipped} messages"
                                )))
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                                let _ = sender
                                    .send(Err(ChatError::Protocol(
                                        "signal receive subscription closed".into(),
                                    )))
                                    .await;
                                break;
                            }
                        };
                        if sender.send(result).await.is_err() {
                            break;
                        }
                    }
                });
            }
            Self::Telegram(client) => client.start_polling(sender).await?,
        }
        Ok(ChatReceiver { receiver })
    }

    pub async fn send_text(
        &self,
        recipient: &ChatAddress,
        text: &str,
    ) -> Result<ChatMessageId, ChatError> {
        self.ensure_provider(recipient)?;
        match self {
            Self::Signal(client) => client
                .send_text(&signal_recipient(recipient)?, text)
                .await
                .map(|timestamp| ChatMessageId(timestamp.to_string()))
                .map_err(chat_error_from_signal),
            Self::Telegram(client) => client.send_text(recipient, text).await,
        }
    }

    pub async fn download_attachment(
        &self,
        recipient: &ChatAddress,
        attachment_id: &str,
    ) -> Result<Vec<u8>, ChatError> {
        self.ensure_provider(recipient)?;
        match self {
            Self::Signal(client) => client
                .get_attachment(&signal_recipient(recipient)?, attachment_id)
                .await
                .map_err(chat_error_from_signal),
            Self::Telegram(client) => client.download_attachment(attachment_id).await,
        }
    }

    fn ensure_provider(&self, recipient: &ChatAddress) -> Result<(), ChatError> {
        if recipient.provider == self.provider() {
            Ok(())
        } else {
            Err(ChatError::PermanentRequest(format!(
                "{} client cannot send to {} address",
                self.provider(),
                recipient.provider
            )))
        }
    }
}

fn required_env(name: &str, provider: ChatProvider) -> Result<String, ChatError> {
    std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            ChatError::Configuration(format!(
                "{name} is required when CHOTU_CHAT_PROVIDER={provider}"
            ))
        })
}

fn signal_recipient(address: &ChatAddress) -> Result<SignalRecipient, ChatError> {
    if address.provider != ChatProvider::Signal {
        return Err(ChatError::PermanentRequest(format!(
            "Signal adapter received {} address",
            address.provider
        )));
    }
    Ok(match address.kind {
        ConversationKind::Direct => SignalRecipient::Direct {
            aci: address.id.clone(),
        },
        ConversationKind::Group => SignalRecipient::Group {
            group_id: address.id.clone(),
        },
    })
}

fn chat_inbound_from_signal(inbound: SignalInbound) -> ChatInbound {
    let conversation = match inbound.recipient {
        SignalRecipient::Direct { aci } => ChatAddress::direct(ChatProvider::Signal, aci),
        SignalRecipient::Group { group_id } => ChatAddress::group(ChatProvider::Signal, group_id),
    };
    ChatInbound {
        sender_id: inbound.sender_aci,
        conversation,
        text: inbound.text,
        reply_to: inbound
            .quote_timestamp
            .map(|timestamp| ChatMessageId(timestamp.to_string())),
        attachments: inbound
            .attachments
            .into_iter()
            .map(|attachment| ChatAttachment {
                id: attachment.id,
                content_type: attachment.content_type,
                size: attachment.size,
                caption: attachment.caption,
            })
            .collect(),
    }
}

fn chat_error_from_signal(error: SignalError) -> ChatError {
    match error {
        SignalError::Io(_)
        | SignalError::Eof
        | SignalError::Reconnecting
        | SignalError::Timeout => ChatError::TransientTransport(error.to_string()),
        SignalError::Rpc { .. } => ChatError::PermanentRequest(error.to_string()),
        SignalError::Utf8(_)
        | SignalError::Json(_)
        | SignalError::Protocol(_)
        | SignalError::MissingTimestamp
        | SignalError::Base64(_) => ChatError::Protocol(error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_parse_defaults_are_explicit() {
        assert_eq!(
            "signal".parse::<ChatProvider>().unwrap(),
            ChatProvider::Signal
        );
        assert_eq!(
            "telegram".parse::<ChatProvider>().unwrap(),
            ChatProvider::Telegram
        );
        assert!("discord".parse::<ChatProvider>().is_err());
    }

    #[test]
    fn provider_is_part_of_address_identity() {
        let signal = ChatAddress::direct(ChatProvider::Signal, "42");
        let telegram = ChatAddress::direct(ChatProvider::Telegram, "42");
        assert_ne!(signal, telegram);
    }
}
