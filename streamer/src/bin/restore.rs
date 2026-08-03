use anyhow::{Context, Result};
use chotu_common::{format_xoauth2_string, refresh_oauth2_token};
use futures::StreamExt;
use native_tls::TlsConnector;

struct Xoauth2Authenticator {
    auth_string: String,
}

impl async_imap::Authenticator for Xoauth2Authenticator {
    type Response = String;

    fn process(&mut self, _challenge: &[u8]) -> Self::Response {
        self.auth_string.clone()
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // Load .env file
    dotenvy::dotenv().ok();

    println!("Starting restoration tool: moving messages from AI-Trash back to INBOX in bulk...");

    let email_user = std::env::var("CHOTU_EMAIL_USER")
        .context("CHOTU_EMAIL_USER environment variable not set")?;
    let client_id = std::env::var("CHOTU_OAUTH_CLIENT_ID")
        .context("CHOTU_OAUTH_CLIENT_ID environment variable not set")?;
    let client_secret = std::env::var("CHOTU_OAUTH_CLIENT_SECRET")
        .context("CHOTU_OAUTH_CLIENT_SECRET environment variable not set")?;
    let refresh_token = std::env::var("CHOTU_OAUTH_REFRESH_TOKEN")
        .context("CHOTU_OAUTH_REFRESH_TOKEN environment variable not set")?;
    let imap_server =
        std::env::var("CHOTU_IMAP_SERVER").unwrap_or_else(|_| "imap.gmail.com".to_string());
    let imap_port = std::env::var("CHOTU_IMAP_PORT")
        .unwrap_or_else(|_| "993".to_string())
        .parse::<u16>()
        .unwrap_or(993);

    println!("Refreshing OAuth2 access token...");
    let token_res = refresh_oauth2_token(&client_id, &client_secret, &refresh_token).await?;

    println!("Connecting to IMAP server {}:{}...", imap_server, imap_port);
    let tcp_stream = tokio::net::TcpStream::connect((imap_server.as_str(), imap_port)).await?;
    let ssl_connector = TlsConnector::builder().build()?;
    let tokio_connector = tokio_native_tls::TlsConnector::from(ssl_connector);
    let tls_stream = tokio_connector.connect(&imap_server, tcp_stream).await?;

    let mut client = async_imap::Client::new(tls_stream);
    client.read_response().await?;

    let auth_string = format_xoauth2_string(&email_user, &token_res.access_token);
    let authenticator = Xoauth2Authenticator { auth_string };

    println!("Authenticating...");
    let mut session = match client.authenticate("XOAUTH2", authenticator).await {
        Ok(s) => s,
        Err((e, _)) => return Err(anyhow::anyhow!("Authentication failed: {:?}", e)),
    };

    println!("Selecting AI-Trash mailbox...");
    session.select("AI-Trash").await?;

    // Search for all messages in AI-Trash
    let uids = session.uid_search("ALL").await?;
    if uids.is_empty() {
        println!("No emails found in AI-Trash folder.");
        return Ok(());
    }

    println!("Found {} emails in AI-Trash.", uids.len());

    let uid_strs: Vec<String> = uids.iter().map(|uid| uid.to_string()).collect();
    let query = uid_strs.join(",");

    println!("Copying all {} emails to INBOX in bulk...", uids.len());
    session.uid_copy(&query, "INBOX").await?;

    println!("Marking all source emails in AI-Trash for deletion...");
    let mut delete_stream = session.uid_store(&query, "+FLAGS (\\Deleted)").await?;
    while delete_stream.next().await.is_some() {}
    drop(delete_stream);

    println!("Expunging AI-Trash folder...");
    let mut expunge_stream = Box::pin(session.expunge().await?);
    while expunge_stream.next().await.is_some() {}
    drop(expunge_stream);

    println!("Bulk restoration completed successfully!");

    Ok(())
}
