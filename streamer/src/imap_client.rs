use anyhow::{Context, Result};
use async_imap::extensions::idle::IdleResponse;
use chotu_common::{
    format_xoauth2_string, looks_like_non_transaction_alert, refresh_oauth2_token,
    validate_ledger_amount, AppConfig, ChotuLlm, EmailClassification, EmailMetadata,
    LedgerExtraction, ActionItemExtraction, TravelItineraryExtraction, UpcomingBillExtraction,
    MemoryIndex, PersonalReferenceExtraction,
};
use futures::StreamExt;
use native_tls::TlsConnector;
use sqlx::SqlitePool;
use std::time::Duration;
use tokio::time::sleep;
struct Xoauth2Authenticator {
    auth_string: String,
}

impl async_imap::Authenticator for Xoauth2Authenticator {
    type Response = String;

    fn process(&mut self, _challenge: &[u8]) -> Self::Response {
        self.auth_string.clone()
    }
}

pub async fn start_streamer(pool: SqlitePool, llm: ChotuLlm, config: AppConfig) -> Result<()> {
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
            let (wait_fut, _stop_source) = idle.wait_with_timeout(Duration::from_secs(300));
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
                    if let Err(e) = process_new_emails(&mut session, &llm, &pool, &config).await {
                        eprintln!("Error processing incoming emails: {:?}. Reconnecting...", e);
                        break;
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
                        let inserted = sqlx::query(
                            "INSERT OR IGNORE INTO tasks (id, created_at, updated_at, title, assigned_to, due_date, status, source, message_id, email_sender, email_subject, calendar_event_id) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"
                        )
                        .bind(&id)
                        .bind(email_date)
                        .bind(email_date)
                        .bind(&task_desc)
                        .bind(assigned_to_member.as_deref())
                        .bind(due_date.as_deref())
                        .bind("open")
                        .bind("inferred")
                        .bind(&message_id)
                        .bind(&metadata.sender)
                        .bind(&metadata.subject)
                        .bind(calendar_event_id.as_deref())
                        .execute(pool)
                        .await?;
                        if inserted.rows_affected() > 0 {
                            send_signal_reminder(pool, &id, &task_desc, config).await;
                        }
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
                        println!("Travel itinerary committed to database for: {}", ext.destination);

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

                        if let (Some(ref due), Some(member_id)) =
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
                        println!("Upcoming bill committed to database: {} (Due: {:?})", ext.biller, ext.due_date);

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
                                            let home = std::env::var("HOME").unwrap_or_else(|_| "/Users/user".to_string());
                                            let drop_dir = std::path::PathBuf::from(home).join("chotu_drop");
                                            if let Err(e) = tokio::fs::create_dir_all(&drop_dir).await {
                                                eprintln!("Failed to create drop directory: {:?}", e);
                                            } else {
                                                for (filename, content) in pdfs {
                                                    let unique_filename = format!(
                                                        "statement_{}_{}",
                                                        chrono::Utc::now().timestamp(),
                                                        filename
                                                    );
                                                    let file_path = drop_dir.join(&unique_filename);
                                                    if let Err(e) = tokio::fs::write(&file_path, content).await {
                                                        eprintln!("Failed to save PDF attachment: {:?}", e);
                                                    } else {
                                                        println!("Saved PDF attachment to: {:?}", file_path);
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
                        let mut delete_stream = session.uid_store(&query, "+FLAGS (\\Deleted)").await?;
                        while delete_stream.next().await.is_some() {}
                        drop(delete_stream);
                        let mut expunge_stream = Box::pin(session.expunge().await?);
                        while expunge_stream.next().await.is_some() {}
                        drop(expunge_stream);

                        let brain_dir_str = std::env::var("CHOTU_BRAIN_DIR").unwrap_or_else(|_| "~/chotu_brain".to_string());
                        let home = std::env::var("HOME").unwrap_or_else(|_| "/Users/user".to_string());
                        let brain_path = std::path::PathBuf::from(brain_dir_str.replace("~", &home));
                        let readings_dir = brain_path.join("Readings");

                        if let Err(e) = tokio::fs::create_dir_all(&readings_dir).await {
                            eprintln!("Failed to create Readings directory: {:?}", e);
                        } else {
                            let today = chrono::Local::now().format("%Y-%m-%d").to_string();
                            let file_path = readings_dir.join(format!("digest-{}.md", today));

                            let mut md_content = String::new();
                            if !file_path.exists() {
                                md_content.push_str(&format!("# Daily Newsletter Digest - {}\n\n", today));
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
                                    println!("Appended newsletter to daily digest: {:?}", file_path);
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

                        let brain_dir_str = std::env::var("CHOTU_BRAIN_DIR").unwrap_or_else(|_| "~/chotu_brain".to_string());
                        let home = std::env::var("HOME").unwrap_or_else(|_| "/Users/user".to_string());
                        let brain_path = std::path::PathBuf::from(brain_dir_str.replace("~", &home));
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
            || trimmed.to_lowercase().starts_with("content-transfer-encoding:")
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

async fn send_signal_reminder(
    pool: &SqlitePool,
    task_id: &str,
    task_desc: &str,
    config: &AppConfig,
) {
    let socket = match std::env::var("SIGNAL_CLI_SOCKET") {
        Ok(path) if !path.trim().is_empty() => path,
        _ => {
            println!("SIGNAL_CLI_SOCKET is missing; skipping notification push.");
            return;
        }
    };
    let targets = chotu_common::signal_delivery_targets(config);
    if targets.is_empty() {
        println!("Signal delivery targets empty; skipping notification push.");
        return;
    }
    let client = match chotu_common::SignalClient::connect(&socket).await {
        Ok(client) => client,
        Err(error) => {
            eprintln!("SIGNAL_CLI_SOCKET is unreachable ({socket}): {error:?}");
            return;
        }
    };
    let message = format!("Action Item Reminder:\n{}", task_desc);
    for recipient in targets {
        match client.send_text(&recipient, &message).await {
            Ok(timestamp) => {
                if let Err(error) = record_task_signal_message(pool, task_id, &recipient, timestamp).await {
                    eprintln!("Failed to persist Signal reminder mapping for {task_id}: {error:?}");
                } else {
                    println!("Action item reminder sent to Signal {recipient}.");
                }
            }
            Err(error) => {
                eprintln!("Failed to send action item reminder to {recipient}: {error:?}");
            }
        }
    }
}

pub(crate) fn signal_mapping_parts(
    recipient: &chotu_common::SignalRecipient,
) -> (&'static str, String) {
    match recipient {
        chotu_common::SignalRecipient::Direct { aci } => ("direct", aci.clone()),
        chotu_common::SignalRecipient::Group { group_id } => ("group", group_id.clone()),
    }
}

async fn record_task_signal_message(
    pool: &SqlitePool,
    task_id: &str,
    recipient: &chotu_common::SignalRecipient,
    timestamp: i64,
) -> Result<(), sqlx::Error> {
    let (kind, recipient_id) = signal_mapping_parts(recipient);
    sqlx::query(
        "INSERT OR IGNORE INTO task_signal_messages (task_id, recipient_kind, recipient_id, message_timestamp) \
         VALUES (?, ?, ?, ?)",
    )
    .bind(task_id)
    .bind(kind)
    .bind(recipient_id)
    .bind(timestamp)
    .execute(pool)
    .await?;
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
mod signal_mapping_tests {
    use super::*;
    use chotu_common::{init_db, SignalRecipient};

    #[test]
    fn mapping_parts_cover_direct_and_group() {
        assert_eq!(
            signal_mapping_parts(&SignalRecipient::Direct { aci: "aci-1".into() }),
            ("direct", "aci-1".into())
        );
        assert_eq!(
            signal_mapping_parts(&SignalRecipient::Group { group_id: "g1".into() }),
            ("group", "g1".into())
        );
    }

    #[tokio::test]
    async fn each_successful_recipient_creates_one_mapping() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = dir.path().join("map.db");
        let pool = init_db(db.to_str().unwrap()).await.unwrap();
        sqlx::query(
            "INSERT INTO tasks (id, created_at, updated_at, title, status, source) VALUES ('task-1', CURRENT_TIMESTAMP, CURRENT_TIMESTAMP, 'buy milk', 'open', 'inferred')"
        )
        .execute(&pool)
        .await
        .unwrap();
        let recipients = [
            SignalRecipient::Direct { aci: "aci-1".into() },
            SignalRecipient::Group { group_id: "household".into() },
        ];
        for (idx, recipient) in recipients.iter().enumerate() {
            record_task_signal_message(&pool, "task-1", recipient, 100 + idx as i64)
                .await
                .unwrap();
        }
        let rows: Vec<(String, String, i64)> = sqlx::query_as(
            "SELECT recipient_kind, recipient_id, message_timestamp FROM task_signal_messages WHERE task_id = 'task-1' ORDER BY message_timestamp"
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            rows,
            vec![
                ("direct".into(), "aci-1".into(), 100),
                ("group".into(), "household".into(), 101),
            ]
        );
    }
}
