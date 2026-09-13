use anyhow::{Context, Result};
use async_imap::extensions::idle::IdleResponse;
use chotu_common::{
    format_xoauth2_string, looks_like_non_transaction_alert, refresh_oauth2_token,
    validate_ledger_amount, ActionItemExtraction, AppConfig, ChatAddress, ChatClient,
    ChatMessageId, ChatProvider, ChotuLlm, ConversationKind, EmailClassification, EmailMetadata,
    LedgerExtraction, MemoryIndex, PersonalReferenceExtraction, TravelItineraryExtraction,
    UpcomingBillExtraction,
};
use futures::StreamExt;
use native_tls::TlsConnector;
use sqlx::{Sqlite, SqlitePool, Transaction};
use std::time::{Duration, Instant};
use tokio::time::sleep;

const CHAT_SEND_TIMEOUT: Duration = Duration::from_secs(15);
const CHAT_DELIVERY_LEASE_SECS: i64 = 30;
/// Keep worst-case drain under the IMAP IDLE keepalive.
const CHAT_DELIVERY_BATCH_SIZE: i64 = 8;
const CHAT_DRAIN_BUDGET: Duration = Duration::from_secs(90);
const IMAP_IDLE_KEEPALIVE: Duration = Duration::from_secs(300);

struct Xoauth2Authenticator {
    auth_string: String,
}

impl async_imap::Authenticator for Xoauth2Authenticator {
    type Response = String;

    fn process(&mut self, _challenge: &[u8]) -> Self::Response {
        self.auth_string.clone()
    }
}

pub async fn start_streamer(
    pool: SqlitePool,
    llm: ChotuLlm,
    config: AppConfig,
    chat: ChatClient,
) -> Result<()> {
    println!("IMAP Streamer Daemon starting up...");

    // Retrieve environment variables
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

    let mut reconnect_delay = Duration::from_secs(5);

    loop {
        if let Err(error) = drain_pending_chat_deliveries(&pool, &config, &chat).await {
            eprintln!("Failed to drain pending chat reminders: {error:?}");
        }

        println!("Refreshing OAuth2 access token...");
        let token_res = match refresh_oauth2_token(&client_id, &client_secret, &refresh_token).await
        {
            Ok(res) => res,
            Err(e) => {
                eprintln!(
                    "Failed to refresh OAuth2 token: {:?}. Retrying in {:?}...",
                    e, reconnect_delay
                );
                sleep(reconnect_delay).await;
                reconnect_delay = std::cmp::min(reconnect_delay * 2, Duration::from_secs(300));
                continue;
            }
        };

        println!(
            "Access token refreshed successfully. Connecting to IMAP server {}:{}...",
            imap_server, imap_port
        );
        let tcp_stream =
            match tokio::net::TcpStream::connect((imap_server.as_str(), imap_port)).await {
                Ok(stream) => stream,
                Err(e) => {
                    eprintln!(
                        "TCP connection failed: {:?}. Retrying in {:?}...",
                        e, reconnect_delay
                    );
                    sleep(reconnect_delay).await;
                    reconnect_delay = std::cmp::min(reconnect_delay * 2, Duration::from_secs(300));
                    continue;
                }
            };

        let ssl_connector = match TlsConnector::builder().build() {
            Ok(connector) => connector,
            Err(e) => {
                eprintln!(
                    "TLS builder failed: {:?}. Retrying in {:?}...",
                    e, reconnect_delay
                );
                sleep(reconnect_delay).await;
                continue;
            }
        };

        let tokio_connector = tokio_native_tls::TlsConnector::from(ssl_connector);
        let tls_stream = match tokio_connector.connect(&imap_server, tcp_stream).await {
            Ok(stream) => stream,
            Err(e) => {
                eprintln!(
                    "TLS handshake failed: {:?}. Retrying in {:?}...",
                    e, reconnect_delay
                );
                sleep(reconnect_delay).await;
                reconnect_delay = std::cmp::min(reconnect_delay * 2, Duration::from_secs(300));
                continue;
            }
        };

        let mut client = async_imap::Client::new(tls_stream);
        if let Err(e) = client.read_response().await {
            eprintln!(
                "Failed to read server greeting: {:?}. Retrying in {:?}...",
                e, reconnect_delay
            );
            sleep(reconnect_delay).await;
            reconnect_delay = std::cmp::min(reconnect_delay * 2, Duration::from_secs(300));
            continue;
        }

        // Reset reconnect delay on successful connection
        reconnect_delay = Duration::from_secs(5);

        let auth_string = format_xoauth2_string(&email_user, &token_res.access_token);
        let authenticator = Xoauth2Authenticator { auth_string };

        println!("Authenticating via SASL XOAUTH2...");
        let mut session = match client.authenticate("XOAUTH2", authenticator).await {
            Ok(s) => s,
            Err((e, _)) => {
                eprintln!("XOAUTH2 authentication failed: {:?}. Retrying in 10s...", e);
                sleep(Duration::from_secs(10)).await;
                continue;
            }
        };
        println!("Authenticated successfully.");

        // Check and create AI-Trash folder
        if let Err(e) = session.create("AI-Trash").await {
            println!("Note: AI-Trash folder creation returned: {:?} (usually means folder already exists)", e);
        }

        // Check and create AI-ReadingList folder
        if let Err(e) = session.create("AI-ReadingList").await {
            println!("Note: AI-ReadingList folder creation returned: {:?} (usually means folder already exists)", e);
        }

        if let Err(e) = session.select("INBOX").await {
            eprintln!("Failed to select INBOX: {:?}. Reconnecting...", e);
            continue;
        }

        println!("Subscribed to INBOX. Starting IDLE loop...");

        // Run the idle loop, periodically checking to refresh the token after 40 minutes (2400 seconds)
        let idle_timeout = Duration::from_secs(2400);
        let start_time = std::time::Instant::now();

        loop {
            if start_time.elapsed() >= idle_timeout {
                println!(
                    "Reached 40-minute session limit. Re-logging to refresh OAuth2 credentials."
                );
                break;
            }

            let mut idle = session.idle();
            if let Err(e) = idle.init().await {
                eprintln!("IDLE init failed: {:?}. Reconnecting...", e);
                break;
            }

            // Wait for events with a keepalive/timeout to prevent socket hanging
            let (wait_fut, _stop_source) = idle.wait_with_timeout(IMAP_IDLE_KEEPALIVE);
            match wait_fut.await {
                Ok(IdleResponse::NewData(_resp)) => {
                    // Alert received from server
                    session = match idle.done().await {
                        Ok(s) => s,
                        Err(e) => {
                            eprintln!("Failed to close IDLE state: {:?}. Reconnecting...", e);
                            break;
                        }
                    };

                    // Process new emails
                    if let Err(e) =
                        process_new_emails(&mut session, &llm, &pool, &config, chat.provider())
                            .await
                    {
                        eprintln!("Error processing incoming emails: {:?}. Reconnecting...", e);
                        break;
                    }
                    if let Err(error) = drain_pending_chat_deliveries(&pool, &config, &chat).await {
                        eprintln!("Failed to drain pending chat reminders: {error:?}");
                    }
                }
                Ok(IdleResponse::Timeout) => {
                    // Timeout (5 minutes) - send another IDLE keepalive
                    session = match idle.done().await {
                        Ok(s) => s,
                        Err(e) => {
                            eprintln!(
                                "Failed to close IDLE state after timeout: {:?}. Reconnecting...",
                                e
                            );
                            break;
                        }
                    };
                    if let Err(error) = drain_pending_chat_deliveries(&pool, &config, &chat).await {
                        eprintln!("Failed to drain pending chat reminders: {error:?}");
                    }
                }
                Ok(IdleResponse::ManualInterrupt) => {
                    session = match idle.done().await {
                        Ok(s) => s,
                        Err(e) => {
                            eprintln!("Failed to close IDLE state after manual interrupt: {:?}. Reconnecting...", e);
                            break;
                        }
                    };
                }
                Err(e) => {
                    eprintln!("IDLE connection error: {:?}. Reconnecting...", e);
                    break;
                }
            }
        }
    }
}

async fn process_new_emails<T>(
    session: &mut async_imap::Session<T>,
    llm: &ChotuLlm,
    pool: &SqlitePool,
    config: &AppConfig,
    provider: ChatProvider,
) -> Result<()>
where
    T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + Sync + std::fmt::Debug,
{
    let email_user = std::env::var("CHOTU_EMAIL_USER").ok();
    let mut assigned_to_member = None;
    if let Some(ref user) = email_user {
        for member in &config.family.members {
            if let Some(ref cal) = member.calendar {
                if cal.email.to_lowercase() == user.to_lowercase() {
                    assigned_to_member = Some(member.id.clone());
                    break;
                }
            }
        }
    }

    // Search for unseen emails
    let uids = session
        .uid_search("UNSEEN")
        .await
        .context("Failed to search UNSEEN messages")?;

    if uids.is_empty() {
        return Ok(());
    }

    println!("Found {} unseen emails to process.", uids.len());

    for uid in uids {
        let query = format!("{}", uid);
        let mut fetch_stream = session
            .uid_fetch(&query, "(RFC822.HEADER RFC822.TEXT INTERNALDATE)")
            .await
            .context("Failed to fetch email headers, body and date")?;

        let (metadata, email_date, message_id) = if let Some(msg_res) = fetch_stream.next().await {
            let msg = msg_res.context("Failed to fetch message from stream")?;
            let header_bytes = msg.header().unwrap_or_default();
            let body_bytes = msg.text().unwrap_or_default();
            let email_date = msg
                .internal_date()
                .map(|dt| dt.with_timezone(&chrono::Utc))
                .unwrap_or_else(chrono::Utc::now);

            let (sender, subject, msg_id) = parse_header(header_bytes);
            let body_preview = parse_body_preview(body_bytes);

            let message_id = if msg_id.is_empty() {
                format!("fallback-{}", uuid::Uuid::new_v4())
            } else {
                msg_id
            };

            let metadata = EmailMetadata {
                sender,
                subject,
                body_preview: Some(body_preview),
            };
            (metadata, email_date, message_id)
        } else {
            continue;
        };
        drop(fetch_stream);

        println!("Processing email from: {}", metadata.sender);
        println!("Subject: {}", metadata.subject);

        // Fetch unactionable feedback examples to guide the LLM (limited to 15 to prevent context blowup)
        let feedback_rows = sqlx::query("SELECT sender, subject, task_description FROM unactionable_emails_feedback ORDER BY created_at DESC LIMIT 15")
            .fetch_all(pool)
            .await;

        let mut unactionable_examples = Vec::new();
        if let Ok(rows) = feedback_rows {
            for row in rows {
                use sqlx::Row;
                let sender: String = row.get("sender");
                let subject: String = row.get("subject");
                let task_description: Option<String> = row.get("task_description");
                unactionable_examples.push(format!(
                    "From: {} | Subject: {} | Task: {}",
                    sender,
                    subject,
                    task_description.unwrap_or_default()
                ));
            }
        }

        match llm.classify_email(&metadata, &unactionable_examples).await {
            Ok(res) => {
                println!("Classification result: {:?}", res.classification);
                println!("Reason: {}", res.reason);

                match res.classification {
                    EmailClassification::Trash => {
                        println!("Moving message {} to AI-Trash...", uid);
                        session.uid_copy(&query, "AI-Trash").await?;
                        let mut delete_stream =
                            session.uid_store(&query, "+FLAGS (\\Deleted)").await?;
                        while delete_stream.next().await.is_some() {}
                        drop(delete_stream);
                        let mut expunge_stream = Box::pin(session.expunge().await?);
                        while expunge_stream.next().await.is_some() {}
                    }
                    EmailClassification::LedgerStream => {
                        println!("Parsing ledger transaction for message {}...", uid);
                        if looks_like_non_transaction_alert(
                            &metadata.subject,
                            metadata.body_preview.as_deref(),
                        ) || looks_like_non_transaction_alert(&metadata.sender, None)
                        {
                            println!(
                                "Skipping ledger commit — subject/body/sender looks like a non-transaction alert: {}",
                                metadata.subject
                            );
                        } else {
                            // Extract actual transaction details using local LLM
                            let ext = match llm.extract_ledger_transaction(&metadata).await {
                                Ok(e) => e,
                                Err(err) => {
                                    eprintln!("Failed to extract transaction details: {:?}", err);
                                    // Fallback to safe defaults
                                    LedgerExtraction {
                                        amount: 0.0,
                                        currency: config.currency().to_string(),
                                        merchant: metadata.subject.clone(),
                                        category: "Uncategorized".to_string(),
                                    }
                                }
                            };
                            match validate_ledger_amount(ext.amount, &ext.currency) {
                                Ok(()) => {
                                    let id = uuid::Uuid::new_v4().to_string();
                                    sqlx::query(
                                        "INSERT OR IGNORE INTO financial_ledger (id, timestamp, amount, currency, institution, merchant, category, source_type, message_id) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)"
                                    )
                                    .bind(&id)
                                    .bind(email_date)
                                    .bind(ext.amount)
                                    .bind(&ext.currency)
                                    .bind(&metadata.sender)
                                    .bind(&ext.merchant)
                                    .bind(&ext.category)
                                    .bind("EMAIL_STREAM")
                                    .bind(&message_id)
                                    .execute(pool)
                                    .await?;
                                    println!(
                                        "Transaction committed to database: {} - {} {}",
                                        ext.merchant, ext.amount, ext.currency
                                    );
                                }
                                Err(reason) => {
                                    println!(
                                        "Skipping ledger commit for {} ({} {}): {}",
                                        ext.merchant, ext.amount, ext.currency, reason
                                    );
                                }
                            }
                        }

                        // Mark as seen so we don't process it again
                        let mut seen_stream = session.uid_store(&query, "+FLAGS (\\Seen)").await?;
                        while seen_stream.next().await.is_some() {}
                    }
                    EmailClassification::Archive => {
                        println!("Archiving message {}...", uid);
                        // Mark as seen
                        let mut seen_stream = session.uid_store(&query, "+FLAGS (\\Seen)").await?;
                        while seen_stream.next().await.is_some() {}
                    }
                    EmailClassification::ActionItem => {
                        println!("Parsing action item for message {}...", uid);
                        let (task_desc, due_date) = match llm.extract_action_item(&metadata).await {
                            Ok(ext) => {
                                let ext: ActionItemExtraction = ext;
                                (ext.task_description, ext.due_date)
                            }
                            Err(err) => {
                                eprintln!("Failed to extract action item details: {:?}", err);
                                (metadata.subject.clone(), None)
                            }
                        };

                        // Only put dated commitments on the calendar. Undated action items
                        // used to all land on tomorrow at 09:00 and clutter the day.
                        let mut calendar_event_id: Option<String> = None;
                        if due_date.is_some() {
                            if let Some(ref member_id) = assigned_to_member {
                                if let Some(member) =
                                    config.family.members.iter().find(|m| &m.id == member_id)
                                {
                                    if let Some(cal_client) =
                                        chotu_common::build_calendar_client(member)
                                    {
                                        match chotu_common::schedule_timed_block(
                                            &cal_client,
                                            &task_desc,
                                            Some(&format!(
                                                "From email: {}\nSubject: {}",
                                                metadata.sender, metadata.subject
                                            )),
                                            due_date.as_deref(),
                                            30,
                                        )
                                        .await
                                        {
                                            Ok(event_id) => {
                                                println!(
                                                    "Scheduled action item on calendar: {}",
                                                    event_id
                                                );
                                                calendar_event_id = Some(event_id);
                                            }
                                            Err(e) => {
                                                eprintln!(
                                                    "Failed to schedule action item on calendar: {:?}",
                                                    e
                                                );
                                            }
                                        }
                                    }
                                }
                            }
                        }

                        let id = uuid::Uuid::new_v4().to_string();
                        let persisted = persist_inferred_task_and_deliveries(
                            pool,
                            NewInferredTask {
                                id: &id,
                                created_at: email_date,
                                title: &task_desc,
                                assigned_to: assigned_to_member.as_deref(),
                                due_date: due_date.as_deref(),
                                message_id: &message_id,
                                email_sender: &metadata.sender,
                                email_subject: &metadata.subject,
                                calendar_event_id: calendar_event_id.as_deref(),
                            },
                            config,
                            provider,
                        )
                        .await?;
                        if persisted.inserted {
                            println!("Action item committed to database: {}", task_desc);

                            let mem = MemoryIndex::from_env();
                            let created_at_str = email_date.to_rfc3339();
                            if let Err(e) = mem
                                .index_task(
                                    pool,
                                    &id,
                                    &task_desc,
                                    None,
                                    "open",
                                    due_date.as_deref(),
                                    assigned_to_member.as_deref(),
                                    Some(&created_at_str),
                                )
                                .await
                            {
                                eprintln!("Memory: failed to index new task: {:?}", e);
                            }
                        } else {
                            println!(
                                "Action item already exists as {}; ensured its chat reminder remains queued",
                                persisted.task_id
                            );
                        }

                        let mut seen_stream = session.uid_store(&query, "+FLAGS (\\Seen)").await?;
                        while seen_stream.next().await.is_some() {}
                    }
                    EmailClassification::TravelItinerary => {
                        println!("Parsing travel itinerary for message {}...", uid);
                        let ext = match llm.extract_travel_itinerary(&metadata).await {
                            Ok(e) => e,
                            Err(err) => {
                                eprintln!("Failed to extract travel itinerary: {:?}", err);
                                TravelItineraryExtraction {
                                    destination: "Unknown".to_string(),
                                    start_date: None,
                                    end_date: None,
                                    details: metadata.subject.clone(),
                                }
                            }
                        };

                        let dest_ok = {
                            let d = ext.destination.trim();
                            !d.is_empty() && !d.eq_ignore_ascii_case("unknown")
                        };
                        let has_dates = ext.start_date.is_some() || ext.end_date.is_some();
                        // Skip hollow extractions (e.g. parking/deal emails mislabeled as travel).
                        if !dest_ok || !has_dates {
                            println!(
                                "Skipping hollow travel itinerary (destination={:?}, start={:?}, end={:?})",
                                ext.destination, ext.start_date, ext.end_date
                            );
                            let mut seen_stream =
                                session.uid_store(&query, "+FLAGS (\\Seen)").await?;
                            while seen_stream.next().await.is_some() {}
                            continue;
                        }

                        if let Some(ref member_id) = assigned_to_member {
                            if let Some(member) =
                                config.family.members.iter().find(|m| &m.id == member_id)
                            {
                                if let Some(cal_client) =
                                    chotu_common::build_calendar_client(member)
                                {
                                    let title = format!("Travel: {}", ext.destination);
                                    let desc = Some(ext.details.as_str());
                                    if let Some(ref start) = ext.start_date {
                                        if let Err(e) = chotu_common::schedule_timed_block(
                                            &cal_client,
                                            &title,
                                            desc,
                                            Some(start),
                                            60,
                                        )
                                        .await
                                        {
                                            eprintln!(
                                                "Failed to schedule travel start on calendar: {:?}",
                                                e
                                            );
                                        }
                                    }
                                    if let Some(ref end) = ext.end_date {
                                        if Some(end) != ext.start_date.as_ref() {
                                            let return_title =
                                                format!("Travel return: {}", ext.destination);
                                            if let Err(e) = chotu_common::schedule_timed_block(
                                                &cal_client,
                                                &return_title,
                                                desc,
                                                Some(end),
                                                60,
                                            )
                                            .await
                                            {
                                                eprintln!(
                                                    "Failed to schedule travel return on calendar: {:?}",
                                                    e
                                                );
                                            }
                                        }
                                    }
                                }
                            }
                        }

                        let id = uuid::Uuid::new_v4().to_string();
                        sqlx::query(
                            "INSERT OR IGNORE INTO travel_itineraries (id, timestamp, destination, start_date, end_date, details, message_id) VALUES (?, ?, ?, ?, ?, ?, ?)"
                        )
                        .bind(&id)
                        .bind(email_date)
                        .bind(&ext.destination)
                        .bind(&ext.start_date)
                        .bind(&ext.end_date)
                        .bind(&ext.details)
                        .bind(&message_id)
                        .execute(pool)
                        .await?;
                        println!(
                            "Travel itinerary committed to database for: {}",
                            ext.destination
                        );

                        let mut seen_stream = session.uid_store(&query, "+FLAGS (\\Seen)").await?;
                        while seen_stream.next().await.is_some() {}
                    }
                    EmailClassification::FinancialBill => {
                        println!("Parsing financial bill for message {}...", uid);
                        let ext = match llm.extract_upcoming_bill(&metadata).await {
                            Ok(e) => e,
                            Err(err) => {
                                eprintln!("Failed to extract upcoming bill: {:?}", err);
                                UpcomingBillExtraction {
                                    biller: metadata.sender.clone(),
                                    amount: None,
                                    due_date: None,
                                }
                            }
                        };

                        if let (Some(due), Some(member_id)) =
                            (ext.due_date.as_ref(), assigned_to_member.as_ref())
                        {
                            if let Some(member) =
                                config.family.members.iter().find(|m| &m.id == member_id)
                            {
                                if let Some(cal_client) =
                                    chotu_common::build_calendar_client(member)
                                {
                                    let title = match ext.amount {
                                        Some(a) => format!("Bill due: {} (${:.2})", ext.biller, a),
                                        None => format!("Bill due: {}", ext.biller),
                                    };
                                    if let Err(e) = chotu_common::schedule_timed_block(
                                        &cal_client,
                                        &title,
                                        Some(&format!("From: {}", metadata.sender)),
                                        Some(due),
                                        30,
                                    )
                                    .await
                                    {
                                        eprintln!(
                                            "Failed to schedule bill due date on calendar: {:?}",
                                            e
                                        );
                                    }
                                }
                            }
                        }

                        let id = uuid::Uuid::new_v4().to_string();
                        sqlx::query(
                            "INSERT OR IGNORE INTO upcoming_bills (id, timestamp, biller, amount, due_date, status, message_id) VALUES (?, ?, ?, ?, ?, ?, ?)"
                        )
                        .bind(&id)
                        .bind(email_date)
                        .bind(&ext.biller)
                        .bind(ext.amount)
                        .bind(&ext.due_date)
                        .bind("unpaid")
                        .bind(&message_id)
                        .execute(pool)
                        .await?;
                        println!(
                            "Upcoming bill committed to database: {} (Due: {:?})",
                            ext.biller, ext.due_date
                        );

                        let mut seen_stream = session.uid_store(&query, "+FLAGS (\\Seen)").await?;
                        while seen_stream.next().await.is_some() {}
                    }
                    EmailClassification::StatementDocument => {
                        println!("Processing statement document email for message {}...", uid);
                        let mut fetch_stream = session
                            .uid_fetch(&query, "RFC822")
                            .await
                            .context("Failed to fetch full RFC822 message")?;

                        if let Some(msg_res) = fetch_stream.next().await {
                            let msg = msg_res.context("Failed to fetch message body")?;
                            if let Some(raw_body) = msg.body() {
                                match mailparse::parse_mail(raw_body) {
                                    Ok(parsed_mail) => {
                                        let mut pdfs = Vec::new();
                                        find_pdf_attachments(&parsed_mail, &mut pdfs);

                                        if pdfs.is_empty() {
                                            println!("No PDF attachments found in statement document email.");
                                        } else {
                                            let home = std::env::var("HOME")
                                                .unwrap_or_else(|_| "/Users/user".to_string());
                                            let drop_dir =
                                                std::path::PathBuf::from(home).join("chotu_drop");
                                            if let Err(e) =
                                                tokio::fs::create_dir_all(&drop_dir).await
                                            {
                                                eprintln!(
                                                    "Failed to create drop directory: {:?}",
                                                    e
                                                );
                                            } else {
                                                for (filename, content) in pdfs {
                                                    let unique_filename = format!(
                                                        "statement_{}_{}",
                                                        chrono::Utc::now().timestamp(),
                                                        filename
                                                    );
                                                    let file_path = drop_dir.join(&unique_filename);
                                                    if let Err(e) =
                                                        tokio::fs::write(&file_path, content).await
                                                    {
                                                        eprintln!(
                                                            "Failed to save PDF attachment: {:?}",
                                                            e
                                                        );
                                                    } else {
                                                        println!(
                                                            "Saved PDF attachment to: {:?}",
                                                            file_path
                                                        );
                                                    }
                                                }
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        eprintln!("Failed to parse raw mail: {:?}", e);
                                    }
                                }
                            } else {
                                eprintln!("No RFC822 body found in fetched message");
                            }
                        }
                        drop(fetch_stream);

                        let mut seen_stream = session.uid_store(&query, "+FLAGS (\\Seen)").await?;
                        while seen_stream.next().await.is_some() {}
                    }
                    EmailClassification::Newsletter => {
                        println!("Processing newsletter for message {}...", uid);
                        session.uid_copy(&query, "AI-ReadingList").await?;
                        let mut delete_stream =
                            session.uid_store(&query, "+FLAGS (\\Deleted)").await?;
                        while delete_stream.next().await.is_some() {}
                        drop(delete_stream);
                        let mut expunge_stream = Box::pin(session.expunge().await?);
                        while expunge_stream.next().await.is_some() {}
                        drop(expunge_stream);

                        let brain_dir_str = std::env::var("CHOTU_BRAIN_DIR")
                            .unwrap_or_else(|_| "~/chotu_brain".to_string());
                        let home =
                            std::env::var("HOME").unwrap_or_else(|_| "/Users/user".to_string());
                        let brain_path =
                            std::path::PathBuf::from(brain_dir_str.replace("~", &home));
                        let readings_dir = brain_path.join("Readings");

                        if let Err(e) = tokio::fs::create_dir_all(&readings_dir).await {
                            eprintln!("Failed to create Readings directory: {:?}", e);
                        } else {
                            let today = chrono::Local::now().format("%Y-%m-%d").to_string();
                            let file_path = readings_dir.join(format!("digest-{}.md", today));

                            let mut md_content = String::new();
                            if !file_path.exists() {
                                md_content.push_str(&format!(
                                    "# Daily Newsletter Digest - {}\n\n",
                                    today
                                ));
                            }
                            md_content.push_str(&format!(
                                "## {}\n- **Sender**: {}\n- **Received At**: {}\n- **Preview**: {}\n\n---\n\n",
                                metadata.subject,
                                metadata.sender,
                                chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
                                metadata.body_preview.as_deref().unwrap_or("[No body preview provided]")
                            ));

                            use tokio::io::AsyncWriteExt;
                            if let Ok(mut file) = tokio::fs::OpenOptions::new()
                                .create(true)
                                .append(true)
                                .open(&file_path)
                                .await
                            {
                                if let Err(e) = file.write_all(md_content.as_bytes()).await {
                                    eprintln!("Failed to write newsletter digest: {:?}", e);
                                } else {
                                    println!(
                                        "Appended newsletter to daily digest: {:?}",
                                        file_path
                                    );
                                }
                            }
                        }
                    }
                    EmailClassification::PersonalReference => {
                        println!("Parsing personal reference for message {}...", uid);
                        let ext = match llm.extract_personal_reference(&metadata).await {
                            Ok(e) => e,
                            Err(err) => {
                                eprintln!("Failed to extract personal reference: {:?}", err);
                                PersonalReferenceExtraction {
                                    title: metadata.subject.clone(),
                                    url: None,
                                    notes: metadata.body_preview.clone().unwrap_or_default(),
                                }
                            }
                        };

                        let id = uuid::Uuid::new_v4().to_string();
                        sqlx::query(
                            "INSERT OR IGNORE INTO personal_references (id, timestamp, title, url, notes, message_id) VALUES (?, ?, ?, ?, ?, ?)"
                        )
                        .bind(&id)
                        .bind(email_date)
                        .bind(&ext.title)
                        .bind(&ext.url)
                        .bind(&ext.notes)
                        .bind(&message_id)
                        .execute(pool)
                        .await?;
                        println!("Personal reference committed to database: {}", ext.title);

                        let mem = MemoryIndex::from_env();
                        let ts = email_date.to_rfc3339();
                        if let Err(e) = mem
                            .index_personal_reference(
                                pool,
                                &id,
                                &ext.title,
                                ext.url.as_deref(),
                                &ext.notes,
                                Some(&ts),
                            )
                            .await
                        {
                            eprintln!("Memory: failed to index personal reference: {:?}", e);
                        }

                        let brain_dir_str = std::env::var("CHOTU_BRAIN_DIR")
                            .unwrap_or_else(|_| "~/chotu_brain".to_string());
                        let home =
                            std::env::var("HOME").unwrap_or_else(|_| "/Users/user".to_string());
                        let brain_path =
                            std::path::PathBuf::from(brain_dir_str.replace("~", &home));
                        let references_dir = brain_path.join("References");

                        if let Err(e) = tokio::fs::create_dir_all(&references_dir).await {
                            eprintln!("Failed to create References directory: {:?}", e);
                        } else {
                            let file_path = references_dir.join(format!("ref_{}.md", id));
                            let md_content = format!(
                                "# {}\n- **Date**: {}\n- **URL**: {}\n\n## Notes\n{}\n",
                                ext.title,
                                chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
                                ext.url.as_deref().unwrap_or("N/A"),
                                ext.notes
                            );
                            if let Err(e) = tokio::fs::write(&file_path, md_content).await {
                                eprintln!("Failed to write personal reference file: {:?}", e);
                            } else {
                                println!("Saved personal reference markdown: {:?}", file_path);
                            }
                        }

                        let mut seen_stream = session.uid_store(&query, "+FLAGS (\\Seen)").await?;
                        while seen_stream.next().await.is_some() {}
                    }
                }
            }
            Err(e) => {
                eprintln!("Failed to classify email: {:?}", e);
            }
        }
    }

    Ok(())
}

fn parse_header(header_bytes: &[u8]) -> (String, String, String) {
    let header_str = String::from_utf8_lossy(header_bytes);
    let mut subject = String::new();
    let mut sender = String::new();
    let mut message_id = String::new();
    for line in header_str.lines() {
        let line_lower = line.to_lowercase();
        if line_lower.starts_with("subject:") {
            subject = line
                .strip_prefix("Subject:")
                .or_else(|| line.strip_prefix("subject:"))
                .unwrap_or(line)
                .trim()
                .to_string();
        } else if line_lower.starts_with("from:") {
            sender = line
                .strip_prefix("From:")
                .or_else(|| line.strip_prefix("from:"))
                .unwrap_or(line)
                .trim()
                .to_string();
        } else if line_lower.starts_with("message-id:") {
            message_id = line
                .strip_prefix("Message-ID:")
                .or_else(|| line.strip_prefix("message-id:"))
                .or_else(|| line.strip_prefix("Message-Id:"))
                .unwrap_or(line)
                .trim()
                .to_string();
        }
    }
    (sender, subject, message_id)
}

fn parse_body_preview(body_bytes: &[u8]) -> String {
    let body_str = String::from_utf8_lossy(body_bytes);
    let mut clean_text = String::new();
    let mut in_tag = false;
    let mut tag_content = String::new();

    // Track whether we are inside a style or script tag block
    let mut in_style = false;
    let mut in_script = false;

    let chars: Vec<char> = body_str.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '<' {
            in_tag = true;
            tag_content.clear();
            i += 1;
            continue;
        }
        if c == '>' {
            in_tag = false;
            let tag_lower = tag_content.to_lowercase();
            if tag_lower.starts_with("style") {
                in_style = true;
            } else if tag_lower.starts_with("/style") {
                in_style = false;
            } else if tag_lower.starts_with("script") {
                in_script = true;
            } else if tag_lower.starts_with("/script") {
                in_script = false;
            }
            i += 1;
            continue;
        }

        if in_tag {
            tag_content.push(c);
        } else if !in_style && !in_script {
            if c == '\r' || c == '\n' || c == '\t' {
                if !clean_text.ends_with(' ') {
                    clean_text.push(' ');
                }
            } else {
                clean_text.push(c);
            }
        }

        if clean_text.len() >= 300 {
            break;
        }
        i += 1;
    }

    let mut finalized = String::new();
    for line in clean_text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("--")
            || trimmed.to_lowercase().starts_with("content-type:")
            || trimmed
                .to_lowercase()
                .starts_with("content-transfer-encoding:")
        {
            continue;
        }
        finalized.push_str(trimmed);
        finalized.push(' ');
    }

    let mut output = String::new();
    let mut last_was_space = false;
    for c in finalized.chars() {
        if c.is_whitespace() {
            if !last_was_space {
                output.push(' ');
                last_was_space = true;
            }
        } else {
            output.push(c);
            last_was_space = false;
        }
    }

    output.trim().to_string()
}

fn action_item_reminder_message(task_id: &str, task_desc: &str) -> String {
    let short_id: String = task_id.chars().take(8).collect();
    format!(
        "Action Item Reminder:\n`{}` {}\n/tasks complete {} · /tasks snooze {} [days]",
        short_id, task_desc, short_id, short_id
    )
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ReminderTarget {
    kind: &'static str,
    id: String,
}

impl ReminderTarget {
    fn from_address(recipient: ChatAddress) -> Self {
        Self {
            kind: recipient.kind.as_str(),
            id: recipient.id,
        }
    }
}

fn reminder_delivery_targets(
    config: &AppConfig,
    provider: ChatProvider,
    assigned_to_member: Option<&str>,
    household_targets: &[ChatAddress],
) -> Vec<ReminderTarget> {
    if let Some(member_id) = assigned_to_member {
        if let Some(address) = chotu_common::chat_address_for_member(config, provider, member_id) {
            return vec![ReminderTarget::from_address(address)];
        }
        if let Some(group) = household_targets
            .iter()
            .find(|target| target.kind == ConversationKind::Group)
        {
            return vec![ReminderTarget::from_address(group.clone())];
        }
        // Keep a logical member target durable; never reroute it to a later-added group.
        return vec![ReminderTarget {
            kind: "member",
            id: member_id.to_string(),
        }];
    }

    let mut targets = Vec::new();
    for recipient in household_targets.iter().cloned() {
        let target = ReminderTarget::from_address(recipient);
        if !targets.contains(&target) {
            targets.push(target);
        }
    }
    targets
}

struct NewInferredTask<'a> {
    id: &'a str,
    created_at: chrono::DateTime<chrono::Utc>,
    title: &'a str,
    assigned_to: Option<&'a str>,
    due_date: Option<&'a str>,
    message_id: &'a str,
    email_sender: &'a str,
    email_subject: &'a str,
    calendar_event_id: Option<&'a str>,
}

#[derive(Debug)]
struct PersistedTask {
    task_id: String,
    inserted: bool,
}

async fn persist_inferred_task_and_deliveries(
    pool: &SqlitePool,
    task: NewInferredTask<'_>,
    config: &AppConfig,
    provider: ChatProvider,
) -> Result<PersistedTask> {
    let mut tx = pool.begin().await?;
    let inserted = sqlx::query(
        "INSERT OR IGNORE INTO tasks (id, created_at, updated_at, title, assigned_to, due_date, status, source, message_id, email_sender, email_subject, calendar_event_id) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"
    )
    .bind(task.id)
    .bind(task.created_at)
    .bind(task.created_at)
    .bind(task.title)
    .bind(task.assigned_to)
    .bind(task.due_date)
    .bind("open")
    .bind("inferred")
    .bind(task.message_id)
    .bind(task.email_sender)
    .bind(task.email_subject)
    .bind(task.calendar_event_id)
    .execute(&mut *tx)
    .await?;

    let inserted = inserted.rows_affected() == 1;
    let (task_id, assigned_to) = if inserted {
        (task.id.to_string(), task.assigned_to.map(ToOwned::to_owned))
    } else {
        sqlx::query_as::<_, (String, Option<String>)>(
            "SELECT id, assigned_to FROM tasks WHERE message_id = ?",
        )
        .bind(task.message_id)
        .fetch_optional(&mut *tx)
        .await?
        .with_context(|| {
            format!(
                "task insert was ignored but no task exists for message_id {}",
                task.message_id
            )
        })?
    };

    let household_targets = chotu_common::chat_delivery_targets(config, provider);
    let targets =
        reminder_delivery_targets(config, provider, assigned_to.as_deref(), &household_targets);
    let now = chrono::Utc::now().timestamp();
    for target in targets {
        sqlx::query(
            "INSERT INTO email_task_chat_deliveries \
             (task_id, provider, target_kind, target_id, state, attempts, next_attempt_at) \
             VALUES (?, ?, ?, ?, 'pending', 0, ?) \
             ON CONFLICT(task_id, provider, target_kind, target_id) DO NOTHING",
        )
        .bind(&task_id)
        .bind(provider.as_str())
        .bind(target.kind)
        .bind(&target.id)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        if provider == ChatProvider::Signal {
            sqlx::query(
                "INSERT INTO email_task_signal_deliveries \
                 (task_id, target_kind, target_id, state, attempts, next_attempt_at) \
                 VALUES (?, ?, ?, 'pending', 0, ?) \
                 ON CONFLICT(task_id, target_kind, target_id) DO NOTHING",
            )
            .bind(&task_id)
            .bind(target.kind)
            .bind(&target.id)
            .bind(now)
            .execute(&mut *tx)
            .await?;
        }
    }

    tx.commit().await?;
    Ok(PersistedTask { task_id, inserted })
}

#[derive(Debug)]
struct PendingDelivery {
    provider: ChatProvider,
    task_id: String,
    target_kind: String,
    target_id: String,
    attempts: i64,
    title: String,
}

fn resolve_delivery_recipient(
    delivery: &PendingDelivery,
    config: &AppConfig,
    provider: ChatProvider,
) -> Option<ChatAddress> {
    match delivery.target_kind.as_str() {
        "direct" => Some(ChatAddress::direct(provider, &delivery.target_id)),
        "group" => Some(ChatAddress::group(provider, &delivery.target_id)),
        "member" => chotu_common::chat_address_for_member(config, provider, &delivery.target_id),
        _ => None,
    }
}

fn chat_retry_delay_seconds(attempts: i64) -> i64 {
    const DELAYS: [i64; 6] = [5, 30, 120, 300, 900, 1800];
    let index = attempts.saturating_sub(1) as usize;
    DELAYS[index.min(DELAYS.len() - 1)]
}

async fn schedule_delivery_retry(
    pool: &SqlitePool,
    delivery: &PendingDelivery,
    attempts: i64,
    error: &str,
) -> Result<()> {
    let next_attempt_at =
        chrono::Utc::now().timestamp() + chat_retry_delay_seconds(attempts.max(1));
    let last_error: String = error.chars().take(1000).collect();
    let mut tx = pool.begin().await?;
    sqlx::query(
        "UPDATE email_task_chat_deliveries \
         SET state = 'pending', attempts = ?, next_attempt_at = ?, \
             lease_expires_at = NULL, last_error = ? \
         WHERE task_id = ? AND provider = ? AND target_kind = ? AND target_id = ? \
           AND state IN ('pending', 'sending')",
    )
    .bind(attempts)
    .bind(next_attempt_at)
    .bind(&last_error)
    .bind(&delivery.task_id)
    .bind(delivery.provider.as_str())
    .bind(&delivery.target_kind)
    .bind(&delivery.target_id)
    .execute(&mut *tx)
    .await?;
    if delivery.provider == ChatProvider::Signal {
        sqlx::query(
            "UPDATE email_task_signal_deliveries \
             SET state = 'pending', attempts = ?, next_attempt_at = ?, \
                 lease_expires_at = NULL, last_error = ? \
             WHERE task_id = ? AND target_kind = ? AND target_id = ? \
               AND state != 'delivered'",
        )
        .bind(attempts)
        .bind(next_attempt_at)
        .bind(&last_error)
        .bind(&delivery.task_id)
        .bind(&delivery.target_kind)
        .bind(&delivery.target_id)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

async fn fail_delivery(
    pool: &SqlitePool,
    delivery: &PendingDelivery,
    attempts: i64,
    error: &str,
) -> Result<()> {
    let last_error: String = error.chars().take(1000).collect();
    let mut tx = pool.begin().await?;
    sqlx::query(
        "UPDATE email_task_chat_deliveries \
         SET state = 'failed', attempts = ?, lease_expires_at = NULL, last_error = ? \
         WHERE task_id = ? AND provider = ? AND target_kind = ? AND target_id = ? \
           AND state = 'sending'",
    )
    .bind(attempts)
    .bind(&last_error)
    .bind(&delivery.task_id)
    .bind(delivery.provider.as_str())
    .bind(&delivery.target_kind)
    .bind(&delivery.target_id)
    .execute(&mut *tx)
    .await?;
    if delivery.provider == ChatProvider::Signal {
        sqlx::query(
            "DELETE FROM email_task_signal_deliveries \
             WHERE task_id = ? AND target_kind = ? AND target_id = ?",
        )
        .bind(&delivery.task_id)
        .bind(&delivery.target_kind)
        .bind(&delivery.target_id)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

async fn claim_delivery(pool: &SqlitePool, delivery: &PendingDelivery) -> Result<bool> {
    let now = chrono::Utc::now().timestamp();
    let mut tx = pool.begin().await?;
    let claimed = sqlx::query(
        "UPDATE email_task_chat_deliveries \
         SET state = 'sending', attempts = attempts + 1, \
             lease_expires_at = ?, last_error = NULL \
         WHERE task_id = ? AND provider = ? AND target_kind = ? AND target_id = ? \
           AND ((state = 'pending' AND next_attempt_at <= ?) \
                OR (state = 'sending' AND lease_expires_at <= ?))",
    )
    .bind(now + CHAT_DELIVERY_LEASE_SECS)
    .bind(&delivery.task_id)
    .bind(delivery.provider.as_str())
    .bind(&delivery.target_kind)
    .bind(&delivery.target_id)
    .bind(now)
    .bind(now)
    .execute(&mut *tx)
    .await?;
    if claimed.rows_affected() == 1 && delivery.provider == ChatProvider::Signal {
        sqlx::query(
            "UPDATE email_task_signal_deliveries \
             SET state = 'sending', attempts = attempts + 1, \
                 lease_expires_at = ?, last_error = NULL \
             WHERE task_id = ? AND target_kind = ? AND target_id = ?",
        )
        .bind(now + CHAT_DELIVERY_LEASE_SECS)
        .bind(&delivery.task_id)
        .bind(&delivery.target_kind)
        .bind(&delivery.target_id)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(claimed.rows_affected() == 1)
}

async fn pending_chat_deliveries(
    pool: &SqlitePool,
    provider: ChatProvider,
) -> Result<Vec<PendingDelivery>> {
    let now = chrono::Utc::now().timestamp();
    let rows: Vec<(String, String, String, i64, String)> = sqlx::query_as(
        "SELECT d.task_id, d.target_kind, d.target_id, d.attempts, t.title \
         FROM email_task_chat_deliveries d \
         JOIN tasks t ON t.id = d.task_id \
         WHERE d.provider = ? AND ( \
              (d.state = 'pending' AND d.next_attempt_at <= ?) \
           OR (d.state = 'sending' AND d.lease_expires_at <= ?)) \
         ORDER BY d.next_attempt_at, d.task_id, d.target_kind, d.target_id LIMIT ?",
    )
    .bind(provider.as_str())
    .bind(now)
    .bind(now)
    .bind(CHAT_DELIVERY_BATCH_SIZE)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(
            |(task_id, target_kind, target_id, attempts, title)| PendingDelivery {
                provider,
                task_id,
                target_kind,
                target_id,
                attempts,
                title,
            },
        )
        .collect())
}

async fn drain_pending_chat_deliveries(
    pool: &SqlitePool,
    config: &AppConfig,
    client: &ChatClient,
) -> Result<()> {
    let deliveries = pending_chat_deliveries(pool, client.provider()).await?;
    if deliveries.is_empty() {
        return Ok(());
    }

    let drain_deadline = Instant::now() + CHAT_DRAIN_BUDGET;
    for delivery in deliveries {
        if Instant::now() >= drain_deadline {
            break;
        }
        if !claim_delivery(pool, &delivery).await? {
            continue;
        }
        let attempts = delivery.attempts + 1;
        let recipient = match resolve_delivery_recipient(&delivery, config, client.provider()) {
            Some(recipient) => recipient,
            None => {
                schedule_delivery_retry(
                    pool,
                    &delivery,
                    attempts,
                    "assigned member has no configured direct chat",
                )
                .await?;
                continue;
            }
        };

        let message = action_item_reminder_message(&delivery.task_id, &delivery.title);
        match tokio::time::timeout(CHAT_SEND_TIMEOUT, client.send_text(&recipient, &message)).await
        {
            Ok(Ok(message_id)) => {
                if let Err(error) =
                    complete_chat_delivery(pool, &delivery, &recipient, &message_id).await
                {
                    schedule_delivery_retry(pool, &delivery, attempts, &error.to_string()).await?;
                    eprintln!(
                        "Failed to persist chat reminder mapping for {}: {error:?}",
                        delivery.task_id
                    );
                } else {
                    println!(
                        "Action item reminder {} sent to {recipient}.",
                        delivery.task_id
                    );
                }
            }
            Ok(Err(error)) if error.is_retryable() => {
                schedule_delivery_retry(
                    pool,
                    &delivery,
                    attempts,
                    &format!("chat send failed: {error}"),
                )
                .await?;
            }
            Ok(Err(error)) => {
                fail_delivery(
                    pool,
                    &delivery,
                    attempts,
                    &format!("chat send failed permanently: {error}"),
                )
                .await?;
                eprintln!(
                    "Chat reminder {} permanently failed for {recipient}: {error}",
                    delivery.task_id
                );
            }
            Err(_) => {
                schedule_delivery_retry(
                    pool,
                    &delivery,
                    attempts,
                    &format!("chat send timed out after {}s", CHAT_SEND_TIMEOUT.as_secs()),
                )
                .await?;
            }
        }
    }
    Ok(())
}

pub(crate) fn chat_mapping_parts(recipient: &ChatAddress) -> (&'static str, &'static str, String) {
    (
        recipient.provider.as_str(),
        recipient.kind.as_str(),
        recipient.id.clone(),
    )
}

#[cfg(test)]
async fn record_task_chat_message(
    pool: &SqlitePool,
    task_id: &str,
    recipient: &ChatAddress,
    message_id: &ChatMessageId,
) -> Result<()> {
    let mut tx = pool.begin().await?;
    record_task_chat_message_tx(&mut tx, task_id, recipient, message_id).await?;
    tx.commit().await?;
    Ok(())
}

async fn record_task_chat_message_tx(
    tx: &mut Transaction<'_, Sqlite>,
    task_id: &str,
    recipient: &ChatAddress,
    message_id: &ChatMessageId,
) -> Result<()> {
    let (provider, kind, recipient_id) = chat_mapping_parts(recipient);
    let inserted = sqlx::query(
        "INSERT INTO task_chat_messages \
         (task_id, provider, conversation_kind, conversation_id, message_id) \
         VALUES (?, ?, ?, ?, ?) \
         ON CONFLICT(provider, conversation_kind, conversation_id, message_id) DO NOTHING",
    )
    .bind(task_id)
    .bind(provider)
    .bind(kind)
    .bind(&recipient_id)
    .bind(&message_id.0)
    .execute(&mut **tx)
    .await?;
    if inserted.rows_affected() == 0 {
        let existing_task: Option<String> = sqlx::query_scalar(
            "SELECT task_id FROM task_chat_messages \
             WHERE provider = ? AND conversation_kind = ? \
               AND conversation_id = ? AND message_id = ?",
        )
        .bind(provider)
        .bind(kind)
        .bind(&recipient_id)
        .bind(&message_id.0)
        .fetch_optional(&mut **tx)
        .await?;
        match existing_task.as_deref() {
            Some(existing) if existing == task_id => {}
            Some(existing) => anyhow::bail!(
                "chat reminder mapping collision for {provider}:{kind}:{recipient_id}:{}: \
                 existing task {existing}, attempted task {task_id}",
                message_id.0
            ),
            None => {
                anyhow::bail!("chat reminder mapping insert was skipped without an existing row")
            }
        }
    }

    if recipient.provider == ChatProvider::Signal {
        let timestamp = message_id
            .0
            .parse::<i64>()
            .with_context(|| format!("Signal message id `{}` is not a timestamp", message_id.0))?;
        sqlx::query(
            "INSERT INTO task_signal_messages \
             (task_id, recipient_kind, recipient_id, message_timestamp) VALUES (?, ?, ?, ?) \
             ON CONFLICT(recipient_kind, recipient_id, message_timestamp) DO NOTHING",
        )
        .bind(task_id)
        .bind(kind)
        .bind(&recipient_id)
        .bind(timestamp)
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}

async fn complete_chat_delivery(
    pool: &SqlitePool,
    delivery: &PendingDelivery,
    recipient: &ChatAddress,
    message_id: &ChatMessageId,
) -> Result<()> {
    let mut tx = pool.begin().await?;
    record_task_chat_message_tx(&mut tx, &delivery.task_id, recipient, message_id).await?;
    let completed = sqlx::query(
        "UPDATE email_task_chat_deliveries \
         SET state = 'delivered', message_id = ?, delivered_at = CURRENT_TIMESTAMP, \
             lease_expires_at = NULL, last_error = NULL \
         WHERE task_id = ? AND provider = ? AND target_kind = ? AND target_id = ? \
           AND state = 'sending'",
    )
    .bind(&message_id.0)
    .bind(&delivery.task_id)
    .bind(delivery.provider.as_str())
    .bind(&delivery.target_kind)
    .bind(&delivery.target_id)
    .execute(&mut *tx)
    .await?;
    if completed.rows_affected() != 1 {
        anyhow::bail!(
            "chat delivery {}:{}:{}:{} was not in sending state",
            delivery.task_id,
            delivery.provider,
            delivery.target_kind,
            delivery.target_id
        );
    }
    if delivery.provider == ChatProvider::Signal {
        let timestamp = message_id.0.parse::<i64>()?;
        sqlx::query(
            "UPDATE email_task_signal_deliveries \
             SET state = 'delivered', message_timestamp = ?, delivered_at = CURRENT_TIMESTAMP, \
                 lease_expires_at = NULL, last_error = NULL \
             WHERE task_id = ? AND target_kind = ? AND target_id = ?",
        )
        .bind(timestamp)
        .bind(&delivery.task_id)
        .bind(&delivery.target_kind)
        .bind(&delivery.target_id)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

fn find_pdf_attachments(parsed: &mailparse::ParsedMail, pdfs: &mut Vec<(String, Vec<u8>)>) {
    if parsed.ctype.mimetype.to_lowercase() == "application/pdf" {
        if let Ok(body) = parsed.get_body_raw() {
            let filename = parsed
                .ctype
                .params
                .get("name")
                .cloned()
                .unwrap_or_else(|| "statement.pdf".to_string());
            pdfs.push((filename, body));
        }
    }
    for subpart in &parsed.subparts {
        find_pdf_attachments(subpart, pdfs);
    }
}

#[cfg(test)]
mod chat_delivery_tests {
    use super::*;
    use chotu_common::init_db;

    #[test]
    fn chat_drain_stays_under_imap_idle() {
        let worst = CHAT_SEND_TIMEOUT * CHAT_DELIVERY_BATCH_SIZE as u32;
        assert!(
            worst < IMAP_IDLE_KEEPALIVE,
            "batch drain {worst:?} must stay under IMAP IDLE {IMAP_IDLE_KEEPALIVE:?}"
        );
        assert!(
            CHAT_DRAIN_BUDGET + CHAT_SEND_TIMEOUT < IMAP_IDLE_KEEPALIVE,
            "drain budget plus one in-flight send must stay under IMAP IDLE"
        );
    }

    #[test]
    fn action_item_reminder_includes_id_and_commands() {
        let msg = action_item_reminder_message(
            "abcdef12-3456-7890-abcd-ef1234567890",
            "Reply to the HOA about parking",
        );
        assert!(msg.contains("`abcdef12`"));
        assert!(msg.contains("Reply to the HOA about parking"));
        assert!(msg.contains("/tasks complete abcdef12"));
        assert!(msg.contains("/tasks snooze abcdef12 [days]"));
    }

    #[test]
    fn mapping_parts_include_provider_kind_and_id() {
        assert_eq!(
            chat_mapping_parts(&ChatAddress::direct(ChatProvider::Telegram, "101")),
            ("telegram", "direct", "101".into())
        );
        assert_eq!(
            chat_mapping_parts(&ChatAddress::group(ChatProvider::Signal, "household")),
            ("signal", "group", "household".into())
        );
    }

    #[tokio::test]
    async fn successful_recipients_create_provider_scoped_mappings() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = dir.path().join("map.db");
        let pool = init_db(db.to_str().unwrap()).await.unwrap();
        sqlx::query(
            "INSERT INTO tasks (id, created_at, updated_at, title, status, source)
             VALUES ('task-1', CURRENT_TIMESTAMP, CURRENT_TIMESTAMP, 'buy milk', 'open', 'inferred')",
        )
        .execute(&pool)
        .await
        .unwrap();
        let recipients = [
            ChatAddress::direct(ChatProvider::Telegram, "101"),
            ChatAddress::group(ChatProvider::Telegram, "-100303"),
        ];
        for (index, recipient) in recipients.iter().enumerate() {
            record_task_chat_message(
                &pool,
                "task-1",
                recipient,
                &ChatMessageId((100 + index).to_string()),
            )
            .await
            .unwrap();
        }
        let rows: Vec<(String, String, String, String)> = sqlx::query_as(
            "SELECT provider, conversation_kind, conversation_id, message_id
             FROM task_chat_messages WHERE task_id = 'task-1'
             ORDER BY message_id",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            rows,
            vec![
                (
                    "telegram".into(),
                    "direct".into(),
                    "101".into(),
                    "100".into()
                ),
                (
                    "telegram".into(),
                    "group".into(),
                    "-100303".into(),
                    "101".into()
                ),
            ]
        );
    }

    #[tokio::test]
    async fn provider_namespace_allows_same_message_id() {
        let dir = tempfile::TempDir::new().unwrap();
        let pool = init_db(dir.path().join("namespace.db").to_str().unwrap())
            .await
            .unwrap();
        for id in ["task-1", "task-2"] {
            sqlx::query(
                "INSERT INTO tasks (id, created_at, updated_at, title, status, source)
                 VALUES (?, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP, ?, 'open', 'inferred')",
            )
            .bind(id)
            .bind(id)
            .execute(&pool)
            .await
            .unwrap();
        }

        record_task_chat_message(
            &pool,
            "task-1",
            &ChatAddress::direct(ChatProvider::Signal, "same-id"),
            &ChatMessageId("42".into()),
        )
        .await
        .unwrap();
        record_task_chat_message(
            &pool,
            "task-2",
            &ChatAddress::direct(ChatProvider::Telegram, "same-id"),
            &ChatMessageId("42".into()),
        )
        .await
        .unwrap();

        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM task_chat_messages WHERE conversation_id = 'same-id' AND message_id = '42'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(count, 2);
    }

    #[test]
    fn assigned_reminders_never_fan_out_to_other_member_dms() {
        let mut config = AppConfig::default();
        config.family.members[0].id = "alice".into();
        config.family.members[0].chat_ids.telegram = Some("101".into());
        let mut bob = config.family.members[0].clone();
        bob.id = "bob".into();
        bob.name = "Bob".into();
        bob.chat_ids.telegram = Some("202".into());
        config.family.members.push(bob);

        let household = vec![
            ChatAddress::direct(ChatProvider::Telegram, "101"),
            ChatAddress::direct(ChatProvider::Telegram, "202"),
            ChatAddress::group(ChatProvider::Telegram, "-100303"),
        ];

        assert_eq!(
            reminder_delivery_targets(&config, ChatProvider::Telegram, Some("alice"), &household),
            vec![ReminderTarget {
                kind: "direct",
                id: "101".into(),
            }]
        );
        assert_eq!(
            reminder_delivery_targets(
                &config,
                ChatProvider::Telegram,
                Some("unlinked"),
                &household
            ),
            vec![ReminderTarget {
                kind: "group",
                id: "-100303".into(),
            }]
        );
        assert_eq!(
            reminder_delivery_targets(
                &config,
                ChatProvider::Telegram,
                Some("unlinked"),
                &household[..2]
            ),
            vec![ReminderTarget {
                kind: "member",
                id: "unlinked".into(),
            }]
        );
    }

    #[test]
    fn member_targets_wait_for_selected_provider_link() {
        let mut config = AppConfig::default();
        config.family.members[0].id = "unlinked".into();
        let delivery = PendingDelivery {
            provider: ChatProvider::Telegram,
            task_id: "task-member".into(),
            target_kind: "member".into(),
            target_id: "unlinked".into(),
            attempts: 0,
            title: "renew insurance".into(),
        };

        assert_eq!(
            resolve_delivery_recipient(&delivery, &config, ChatProvider::Telegram),
            None
        );
        config.family.members[0].chat_ids.telegram = Some("101".into());
        assert_eq!(
            resolve_delivery_recipient(&delivery, &config, ChatProvider::Telegram),
            Some(ChatAddress::direct(ChatProvider::Telegram, "101"))
        );
    }

    #[tokio::test]
    async fn duplicate_email_keeps_one_task_and_provider_delivery() {
        let dir = tempfile::TempDir::new().unwrap();
        let pool = init_db(dir.path().join("same-task.db").to_str().unwrap())
            .await
            .unwrap();
        let mut config = AppConfig::default();
        config.family.members[0].chat_ids.telegram = Some("101".into());
        let now = chrono::Utc::now();

        let create = |id| NewInferredTask {
            id,
            created_at: now,
            title: "buy milk",
            assigned_to: Some(&config.family.members[0].id),
            due_date: None,
            message_id: "email-1",
            email_sender: "sender@example.com",
            email_subject: "milk",
            calendar_event_id: None,
        };
        let first = persist_inferred_task_and_deliveries(
            &pool,
            create("task-1"),
            &config,
            ChatProvider::Telegram,
        )
        .await
        .unwrap();
        let duplicate = persist_inferred_task_and_deliveries(
            &pool,
            create("task-2"),
            &config,
            ChatProvider::Telegram,
        )
        .await
        .unwrap();

        assert!(first.inserted);
        assert!(!duplicate.inserted);
        assert_eq!(duplicate.task_id, "task-1");
        let delivery_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM email_task_chat_deliveries
             WHERE task_id = 'task-1' AND provider = 'telegram'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(delivery_count, 1);
    }

    #[tokio::test]
    async fn retry_state_survives_database_reopen() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = dir.path().join("retry.db");
        let pool = init_db(db.to_str().unwrap()).await.unwrap();
        let mut config = AppConfig::default();
        config.family.members[0].chat_ids.telegram = Some("101".into());
        persist_inferred_task_and_deliveries(
            &pool,
            NewInferredTask {
                id: "task-retry",
                created_at: chrono::Utc::now(),
                title: "call dentist",
                assigned_to: Some(&config.family.members[0].id),
                due_date: None,
                message_id: "email-retry",
                email_sender: "sender@example.com",
                email_subject: "dentist",
                calendar_event_id: None,
            },
            &config,
            ChatProvider::Telegram,
        )
        .await
        .unwrap();
        let delivery = PendingDelivery {
            provider: ChatProvider::Telegram,
            task_id: "task-retry".into(),
            target_kind: "direct".into(),
            target_id: "101".into(),
            attempts: 0,
            title: "call dentist".into(),
        };
        schedule_delivery_retry(&pool, &delivery, 1, "transport unavailable")
            .await
            .unwrap();
        pool.close().await;

        let reopened = init_db(db.to_str().unwrap()).await.unwrap();
        let row: (String, i64, String) = sqlx::query_as(
            "SELECT state, attempts, last_error FROM email_task_chat_deliveries
             WHERE task_id = 'task-retry' AND provider = 'telegram'",
        )
        .fetch_one(&reopened)
        .await
        .unwrap();
        assert_eq!(row, ("pending".into(), 1, "transport unavailable".into()));
    }
    #[tokio::test]
    async fn permanent_delivery_failure_is_terminal() {
        let dir = tempfile::TempDir::new().unwrap();
        let pool = init_db(dir.path().join("failed.db").to_str().unwrap())
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO tasks (id, created_at, updated_at, title, status, source)
             VALUES ('task-failed', CURRENT_TIMESTAMP, CURRENT_TIMESTAMP, 'buy milk', 'open', 'inferred')",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO email_task_chat_deliveries
             (task_id, provider, target_kind, target_id, state, attempts, next_attempt_at)
             VALUES ('task-failed', 'telegram', 'direct', '101', 'sending', 1, 0)",
        )
        .execute(&pool)
        .await
        .unwrap();
        let delivery = PendingDelivery {
            provider: ChatProvider::Telegram,
            task_id: "task-failed".into(),
            target_kind: "direct".into(),
            target_id: "101".into(),
            attempts: 0,
            title: "buy milk".into(),
        };

        fail_delivery(&pool, &delivery, 1, "chat send failed permanently")
            .await
            .unwrap();

        let state: (String, String) = sqlx::query_as(
            "SELECT state, last_error FROM email_task_chat_deliveries
             WHERE task_id = 'task-failed' AND provider = 'telegram'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            state,
            ("failed".into(), "chat send failed permanently".into())
        );
        assert!(pending_chat_deliveries(&pool, ChatProvider::Telegram)
            .await
            .unwrap()
            .is_empty());
    }
}
