use anyhow::{Context, Result};
use sqlx::SqlitePool;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Mutex;

mod parser;
mod watcher;

/// Main entry point to run the Janitor Agent.
pub async fn run(pool: SqlitePool, config: chotu_common::AppConfig) -> Result<()> {
    println!("Janitor Agent starting up...");

    let default_currency = config.currency().to_string();

    // 1. Resolve watched folders
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    let drop_dir = PathBuf::from(home).join("chotu_drop");
    let archive_dir = drop_dir.join("archive");

    println!("Ensuring watch directories exist...");
    tokio::fs::create_dir_all(&drop_dir)
        .await
        .with_context(|| format!("Failed to create drop directory: {:?}", drop_dir))?;
    tokio::fs::create_dir_all(&archive_dir)
        .await
        .with_context(|| format!("Failed to create archive directory: {:?}", archive_dir))?;

    println!("Watching directory for batch drops: {:?}", drop_dir);

    let active_paths = Arc::new(Mutex::new(HashSet::<PathBuf>::new()));

    // 1.5 Scan drop directory for existing files on startup
    println!("Scanning drop directory for existing files: {:?}", drop_dir);
    if let Ok(mut entries) = tokio::fs::read_dir(&drop_dir).await {
        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            if path.is_file() && !path.starts_with(&archive_dir) {
                println!("Found existing file on startup: {:?}", path);

                // Track in active_paths to prevent concurrent watcher processing
                {
                    let mut active = active_paths.lock().await;
                    active.insert(path.clone());
                }

                let path_clone = path.clone();
                let pool_clone = pool.clone();
                let archive_dir_clone = archive_dir.clone();
                let active_paths_clone = active_paths.clone();
                let default_currency_clone = default_currency.clone();

                tokio::spawn(async move {
                    if let Err(e) = watcher::wait_for_file_write_completion(&path_clone).await {
                        eprintln!(
                            "Error waiting for file stability for existing file {:?}: {:?}",
                            path_clone, e
                        );
                    } else if let Err(e) = process_dropped_file(&path_clone, &pool_clone, &archive_dir_clone, &default_currency_clone).await {
                        eprintln!("Failed to process existing file {:?}: {:?}", path_clone, e);
                    }
                    active_paths_clone.lock().await.remove(&path_clone);
                });
            }
        }
    }

    // 2. Start notify watcher
    let (_watcher, mut rx) =
        watcher::watch_directory(&drop_dir).context("Failed to start directory watcher")?;

    // 3. Process event stream
    while let Some(event_res) = rx.recv().await {
        let event = match event_res {
            Ok(e) => e,
            Err(e) => {
                eprintln!("Directory watcher error: {:?}", e);
                continue;
            }
        };

        if !watcher::is_file_write_event(&event) {
            continue;
        }

        for path in event.paths {
            // Ignore if path is not a file (directory/deleted) or is inside the archive folder
            if !path.is_file() || path.starts_with(&archive_dir) {
                continue;
            }

            // Check if already processing
            {
                let mut active = active_paths.lock().await;
                if active.contains(&path) {
                    continue;
                }
                active.insert(path.clone());
            }

            let path_clone = path.clone();
            let pool_clone = pool.clone();
            let archive_dir_clone = archive_dir.clone();
            let active_paths_clone = active_paths.clone();
            let default_currency_clone = default_currency.clone();

            // Spawn a task to process the file asynchronously, ensuring the main watcher thread doesn't block
            tokio::spawn(async move {
                println!("File mutation detected: {:?}", path_clone);

                // Wait for file write to complete
                if let Err(e) = watcher::wait_for_file_write_completion(&path_clone).await {
                    eprintln!(
                        "Error waiting for file stability for {:?}: {:?}",
                        path_clone, e
                    );
                    active_paths_clone.lock().await.remove(&path_clone);
                    return;
                }

                if let Err(e) =
                    process_dropped_file(&path_clone, &pool_clone, &archive_dir_clone, &default_currency_clone).await
                {
                    eprintln!("Failed to process file {:?}: {:?}", path_clone, e);
                }

                active_paths_clone.lock().await.remove(&path_clone);
            });
        }
    }

    Ok(())
}

async fn process_dropped_file(path: &Path, pool: &SqlitePool, archive_dir: &Path, default_currency: &str) -> Result<()> {
    let filename = path
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| anyhow::anyhow!("Invalid file name for path {:?}", path))?;

    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_lowercase();

    match ext.as_str() {
        "csv" => {
            println!("CSV file detected. Initiating parsing: {}", filename);
            let entries = parser::parse_csv_file(path, default_currency).context("CSV parsing failure")?;

            println!(
                "Parsed {} transactions from CSV. Saving to database...",
                entries.len()
            );
            let mut inserted_count = 0;

            for entry in entries {
                if let Err(reason) =
                    chotu_common::validate_ledger_amount(entry.amount, &entry.currency)
                {
                    println!(
                        "Skipping CSV row {} ({} {}): {}",
                        entry.merchant, entry.amount, entry.currency, reason
                    );
                    continue;
                }
                let res = sqlx::query(
                    "INSERT OR IGNORE INTO financial_ledger (id, timestamp, amount, currency, institution, merchant, category, source_type) VALUES (?, ?, ?, ?, ?, ?, ?, ?)"
                )
                .bind(&entry.id)
                .bind(entry.timestamp)
                .bind(entry.amount)
                .bind(&entry.currency)
                .bind(&entry.institution)
                .bind(&entry.merchant)
                .bind(&entry.category)
                .bind(&entry.source_type)
                .execute(pool)
                .await?;

                if res.rows_affected() > 0 {
                    inserted_count += 1;
                }
            }

            println!("Successfully inserted {} new transactions into financial_ledger (duplicates ignored).", inserted_count);

            // Move to archive
            watcher::safe_archive_file(path, archive_dir)
                .await
                .context("Failed to archive parsed CSV file")?;
            println!("CSV file moved to archive directory.");
        }
        "pdf" | "png" | "jpg" | "jpeg" => {
            println!("Document/Image file detected: {}. Initiating Tier 2 Gemini LLM parsing...", filename);
            let gemini_key = match std::env::var("GEMINI_API_KEY") {
                Ok(key) if !key.trim().is_empty() => key,
                _ => {
                    eprintln!("GEMINI_API_KEY environment variable not set. Cannot parse document. Logging to pending_documents...");
                    let id = uuid::Uuid::new_v4().to_string();
                    let filepath_str = path.to_string_lossy().to_string();
                    sqlx::query(
                        "INSERT INTO pending_documents (id, filename, filepath, status, received_at) VALUES (?, ?, ?, 'PENDING', ?)"
                    )
                    .bind(&id)
                    .bind(filename)
                    .bind(&filepath_str)
                    .bind(chrono::Utc::now())
                    .execute(pool)
                    .await?;
                    watcher::safe_archive_file(path, archive_dir)
                        .await
                        .context("Failed to archive unparsed document")?;
                    return Ok(());
                }
            };

            let gemini_client = chotu_common::GeminiClient::new(gemini_key);
            match gemini_client.extract_from_document(path).await {
                Ok(extraction) => {
                    match extraction.document_type {
                        chotu_common::DroppedDocumentType::Receipt => {
                            if let Some(tx) = extraction.receipt_transaction {
                                match chotu_common::validate_ledger_amount(tx.amount, &tx.currency)
                                {
                                    Ok(()) => {
                                        println!(
                                            "Extracted transaction: {} - {} {}",
                                            tx.merchant, tx.amount, tx.currency
                                        );
                                        let id = uuid::Uuid::new_v4().to_string();
                                        sqlx::query(
                                            "INSERT INTO financial_ledger (id, timestamp, amount, currency, institution, merchant, category, source_type) VALUES (?, ?, ?, ?, ?, ?, ?, ?)"
                                        )
                                        .bind(&id)
                                        .bind(chrono::Utc::now())
                                        .bind(tx.amount)
                                        .bind(&tx.currency)
                                        .bind("BATCH_DROP")
                                        .bind(&tx.merchant)
                                        .bind(&tx.category)
                                        .bind("BATCH_DROP")
                                        .execute(pool)
                                        .await?;
                                        println!("Saved extracted transaction to financial_ledger.");
                                    }
                                    Err(reason) => {
                                        println!(
                                            "Skipping receipt commit for {} ({} {}): {}",
                                            tx.merchant, tx.amount, tx.currency, reason
                                        );
                                    }
                                }
                            } else {
                                eprintln!("LLM classified document as RECEIPT but no receipt_transaction was populated.");
                            }
                        }
                        chotu_common::DroppedDocumentType::Portfolio => {
                            if let Some(holdings) = extraction.portfolio_holdings {
                                println!("Extracted {} portfolio holdings.", holdings.len());
                                let now = chrono::Utc::now();
                                for h in holdings {
                                    println!("Updating holding: {} ({} shares @ average cost ${:.2})", h.ticker, h.shares_owned, h.average_cost);
                                    let ticker_upper = h.ticker.to_uppercase();
                                    sqlx::query(
                                        "INSERT INTO portfolio_holdings (ticker, shares_owned, average_cost, last_updated) \
                                         VALUES (?, ?, ?, ?) \
                                         ON CONFLICT(ticker) DO UPDATE SET \
                                            shares_owned = excluded.shares_owned, \
                                            average_cost = excluded.average_cost, \
                                            last_updated = excluded.last_updated"
                                    )
                                    .bind(&ticker_upper)
                                    .bind(h.shares_owned)
                                    .bind(h.average_cost)
                                    .bind(now)
                                    .execute(pool)
                                    .await?;
                                }
                                println!("Successfully saved/updated holdings in database.");
                            } else {
                                eprintln!("LLM classified document as PORTFOLIO but no portfolio_holdings were populated.");
                            }
                        }
                    }
                }
                Err(e) => {
                    eprintln!("Failed to extract content from document using Gemini: {:?}", e);
                    let id = uuid::Uuid::new_v4().to_string();
                    let filepath_str = path.to_string_lossy().to_string();
                    sqlx::query(
                        "INSERT INTO pending_documents (id, filename, filepath, status, received_at) VALUES (?, ?, ?, 'PENDING', ?)"
                    )
                    .bind(&id)
                    .bind(filename)
                    .bind(&filepath_str)
                    .bind(chrono::Utc::now())
                    .execute(pool)
                    .await?;
                }
            }

            // Move to archive
            watcher::safe_archive_file(path, archive_dir)
                .await
                .context("Failed to archive processed document")?;
            println!("Document file moved to archive directory.");
        }
        _ => {
            println!("Ignoring file with unsupported extension: {}", filename);
        }
    }

    Ok(())
}
