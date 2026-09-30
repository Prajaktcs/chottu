use crate::parser::{self, CsvTransaction, ParsedCsv};
use anyhow::{bail, Context, Result};
use chrono::{DateTime, NaiveDate, Utc};
use sha2::{Digest, Sha256};
use sqlx::{Sqlite, SqlitePool, Transaction};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

const SCHEMA: &str =
    include_str!("../../chotu-common/migrations/20260929000000_csv_import_identity.sql");

#[derive(Debug, Default)]
pub struct ImportStats {
    pub files: usize,
    pub source_rows: usize,
    pub blank_rows: usize,
    pub non_posted_rows: usize,
    pub inserted: usize,
    pub matched: usize,
    pub updated: usize,
    pub legacy_removed: u64,
}

#[derive(sqlx::FromRow)]
struct ExistingRecord {
    id: String,
    timestamp: DateTime<Utc>,
    merchant: String,
    category: String,
    has_time: bool,
    date_quality: i64,
    merchant_quality: i64,
    category_quality: i64,
}

/// Import one complete CSV atomically. Errors leave both the file and ledger unchanged.
pub async fn import_csv_file(
    path: &Path,
    pool: &SqlitePool,
    default_currency: &str,
) -> Result<ImportStats> {
    let path = path.to_owned();
    let currency = default_currency.to_owned();
    let parsed =
        tokio::task::spawn_blocking(move || parser::parse_csv_file(&path, &currency)).await??;
    validate(&parsed)?;
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    ensure_schema(&mut tx).await?;
    let mut stats = ImportStats {
        legacy_removed: remove_legacy_rows(&mut tx, &parsed.legacy_ids).await?,
        ..ImportStats::default()
    };
    import_parsed(&mut tx, parsed, &mut stats).await?;
    tx.commit().await?;
    Ok(stats)
}

/// Rebuild only legacy CSV rows proven by archived source hashes. The caller must back up first.
/// Existing CSV_IMPORT rows are reconciled, making reruns idempotent; receipts/emails are untouched.
pub async fn rebuild_archived_csvs(
    pool: &SqlitePool,
    archive: &Path,
    default_currency: &str,
) -> Result<ImportStats> {
    let archive = archive.to_owned();
    let currency = default_currency.to_owned();
    let sources = tokio::task::spawn_blocking(move || parse_archive(&archive, &currency)).await??;
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    ensure_schema(&mut tx).await?;
    let mut stats = ImportStats::default();
    let legacy_ids: HashSet<_> = sources
        .iter()
        .flat_map(|source| &source.legacy_ids)
        .collect();
    for id in legacy_ids {
        stats.legacy_removed += remove_legacy_row(&mut tx, id).await?;
    }
    for source in sources {
        import_parsed(&mut tx, source, &mut stats).await?;
    }
    tx.commit().await?;
    Ok(stats)
}

async fn ensure_schema(tx: &mut Transaction<'_, Sqlite>) -> Result<()> {
    sqlx::raw_sql(SCHEMA).execute(&mut **tx).await?;
    let has_date_quality: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('csv_import_records') WHERE name = 'date_quality')",
    )
    .fetch_one(&mut **tx)
    .await?;
    let has_foreign_keys: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM pragma_foreign_key_list('csv_import_records')) OR EXISTS(SELECT 1 FROM pragma_foreign_key_list('csv_import_keys'))",
    )
    .fetch_one(&mut **tx)
    .await?;
    if !has_date_quality || has_foreign_keys {
        // The standalone repair created preview metadata before this unmerged migration ran.
        // Rebuild only those two tables, retaining identities and any known date authority.
        let save_records = if has_date_quality {
            "CREATE TEMP TABLE csv_saved_records AS SELECT ledger_id, has_time, date_quality, merchant_quality, category_quality FROM csv_import_records"
        } else {
            "CREATE TEMP TABLE csv_saved_records AS SELECT ledger_id, has_time, CASE WHEN has_time THEN 2 ELSE 1 END AS date_quality, merchant_quality, category_quality FROM csv_import_records"
        };
        sqlx::query(save_records).execute(&mut **tx).await?;
        sqlx::raw_sql(
            "CREATE TEMP TABLE csv_saved_keys AS SELECT match_key, ledger_id FROM csv_import_keys;
             DROP TABLE csv_import_keys;
             DROP TABLE csv_import_records;",
        )
        .execute(&mut **tx)
        .await?;
        sqlx::raw_sql(SCHEMA).execute(&mut **tx).await?;
        sqlx::raw_sql(
            "INSERT INTO csv_import_records SELECT r.* FROM csv_saved_records r
                 JOIN financial_ledger l ON l.id = r.ledger_id WHERE l.source_type = 'CSV_IMPORT';
             INSERT INTO csv_import_keys SELECT k.* FROM csv_saved_keys k
                 JOIN csv_import_records r ON r.ledger_id = k.ledger_id;
             DROP TABLE csv_saved_keys;
             DROP TABLE csv_saved_records;",
        )
        .execute(&mut **tx)
        .await?;
    }
    // Logical references follow the repo convention: cleanup must also work with FK checks off.
    sqlx::raw_sql(
        "DELETE FROM csv_import_keys WHERE NOT EXISTS (
             SELECT 1 FROM csv_import_records r JOIN financial_ledger l ON l.id = r.ledger_id
             WHERE r.ledger_id = csv_import_keys.ledger_id AND l.source_type = 'CSV_IMPORT');
         DELETE FROM csv_import_records WHERE NOT EXISTS (
             SELECT 1 FROM financial_ledger l
             WHERE l.id = csv_import_records.ledger_id AND l.source_type = 'CSV_IMPORT');",
    )
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn remove_legacy_row(tx: &mut Transaction<'_, Sqlite>, id: &str) -> Result<u64> {
    Ok(sqlx::query(
        "DELETE FROM financial_ledger WHERE id = ? AND source_type = 'BATCH_DROP' AND message_id IS NULL",
    )
    .bind(id)
    .execute(&mut **tx)
    .await?
    .rows_affected())
}

async fn remove_legacy_rows(tx: &mut Transaction<'_, Sqlite>, ids: &[String]) -> Result<u64> {
    let mut removed = 0;
    for id in ids {
        removed += remove_legacy_row(tx, id).await?;
    }
    Ok(removed)
}

fn parse_archive(archive: &Path, currency: &str) -> Result<Vec<ParsedCsv>> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(archive)
        .with_context(|| format!("Cannot read CSV archive {}", archive.display()))?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<std::io::Result<_>>()?;
    files.retain(|path| {
        path.is_file()
            && path
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("csv"))
    });
    // Statements establish transaction/posting-date aliases before richer activities arrive.
    files.sort_by_key(|path| {
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        (name.contains("activities"), path.clone())
    });
    if files.is_empty() {
        bail!("No CSV sources in {}", archive.display());
    }
    files
        .into_iter()
        .map(|path| {
            let source = parser::parse_csv_file(&path, currency)
                .with_context(|| format!("Parsing {}", path.display()))?;
            validate(&source).with_context(|| format!("Validating {}", path.display()))?;
            Ok(source)
        })
        .collect()
}

fn validate(source: &ParsedCsv) -> Result<()> {
    for row in &source.transactions {
        chotu_common::validate_ledger_amount(row.entry.amount, &row.entry.currency).map_err(
            |reason| anyhow::anyhow!("Invalid CSV transaction {}: {}", row.entry.merchant, reason),
        )?;
    }
    Ok(())
}

fn hash_parts(parts: &[&str]) -> String {
    let mut hash = Sha256::new();
    for part in parts {
        hash.update((part.len() as u64).to_be_bytes());
        hash.update(part.as_bytes());
    }
    format!("{:x}", hash.finalize())
}

fn normalize_merchant(value: &str) -> String {
    let mut normalized = String::with_capacity(value.len());
    for word in value.split_whitespace() {
        if !normalized.is_empty() {
            normalized.push(' ');
        }
        normalized.extend(word.chars().flat_map(char::to_lowercase));
    }
    normalized
}

fn match_key(row: &CsvTransaction, date: NaiveDate) -> String {
    let merchant = if row.account_id == "wealthsimple-credit-card"
        || !row.entry.institution.starts_with("Wealthsimple:")
    {
        normalize_merchant(&row.entry.merchant)
    } else if matches!(row.kind.as_str(), "buy" | "sell" | "dividend") {
        // Statements omit FX suffixes present in activities, but identify the same security.
        normalize_merchant(
            row.entry
                .merchant
                .split(" - ")
                .next()
                .unwrap_or(&row.entry.merchant),
        )
    } else {
        // Wealthsimple activity exports omit counterparties that statements retain.
        // Account, event kind, date, signed amount, and per-source multiplicity define the match.
        String::new()
    };
    hash_parts(&[
        &row.account_id,
        &date.to_string(),
        &row.kind,
        &row.entry.amount.to_string(),
        &row.entry.currency,
        &merchant,
    ])
}

async fn import_parsed(
    tx: &mut Transaction<'_, Sqlite>,
    mut source: ParsedCsv,
    stats: &mut ImportStats,
) -> Result<()> {
    stats.files += 1;
    stats.source_rows += source.transactions.len();
    stats.blank_rows += source.blank_rows;
    stats.non_posted_rows += source.non_posted_rows;
    source.transactions.sort_by(|a, b| {
        a.entry
            .timestamp
            .cmp(&b.entry.timestamp)
            .then_with(|| a.entry.merchant.cmp(&b.entry.merchant))
    });
    // One existing transaction may match at most one row in this source. Repeated rows are real
    // occurrences, not duplicates. Separate overlapping source files may reuse those occurrences.
    let mut claimed = HashSet::new();
    for row in source.transactions {
        let primary = match_key(&row, row.entry.timestamp.date_naive());
        let posting = row
            .posting_date
            .map(|date| match_key(&row, date))
            .unwrap_or_else(|| primary.clone());
        let existing: Vec<ExistingRecord> = sqlx::query_as(
            "SELECT DISTINCT l.id, l.timestamp, l.merchant, l.category, r.has_time, r.date_quality, r.merchant_quality, r.category_quality \
             FROM csv_import_keys k JOIN csv_import_records r ON r.ledger_id = k.ledger_id \
             JOIN financial_ledger l ON l.id = r.ledger_id \
             WHERE (k.match_key = ? OR k.match_key = ?) AND l.source_type = 'CSV_IMPORT' \
             ORDER BY l.timestamp, l.id",
        )
        .bind(&primary)
        .bind(&posting)
        .fetch_all(&mut **tx)
        .await?;
        let matched = existing.iter().find(|candidate| {
            !claimed.contains(&candidate.id)
                && (!row.has_time
                    || !candidate.has_time
                    || candidate.timestamp == row.entry.timestamp)
        });
        let id = if let Some(candidate) = matched {
            stats.matched += 1;
            let replace_timestamp = row.date_quality as i64 > candidate.date_quality
                || (row.date_quality as i64 == candidate.date_quality
                    && !candidate.has_time
                    && row.has_time);
            let timestamp = if replace_timestamp {
                row.entry.timestamp
            } else {
                candidate.timestamp
            };
            let has_time = if replace_timestamp {
                row.has_time
            } else {
                candidate.has_time
            };
            let merchant = if row.merchant_quality as i64 > candidate.merchant_quality {
                &row.entry.merchant
            } else {
                &candidate.merchant
            };
            let category = if row.category_quality as i64 > candidate.category_quality {
                &row.entry.category
            } else {
                &candidate.category
            };
            if timestamp != candidate.timestamp
                || merchant != &candidate.merchant
                || category != &candidate.category
            {
                sqlx::query("UPDATE financial_ledger SET timestamp = ?, merchant = ?, category = ? WHERE id = ?")
                    .bind(timestamp).bind(merchant).bind(category).bind(&candidate.id)
                    .execute(&mut **tx).await?;
                stats.updated += 1;
            }
            if has_time != candidate.has_time
                || row.date_quality as i64 > candidate.date_quality
                || row.merchant_quality as i64 > candidate.merchant_quality
                || row.category_quality as i64 > candidate.category_quality
            {
                sqlx::query("UPDATE csv_import_records SET has_time = ?, date_quality = MAX(date_quality, ?), merchant_quality = MAX(merchant_quality, ?), category_quality = MAX(category_quality, ?) WHERE ledger_id = ?")
                    .bind(has_time).bind(row.date_quality).bind(row.merchant_quality).bind(row.category_quality)
                    .bind(&candidate.id).execute(&mut **tx).await?;
            }
            candidate.id.clone()
        } else {
            let ordinal = existing.len().to_string();
            let timestamp = if row.has_time {
                row.entry.timestamp.to_rfc3339()
            } else {
                String::new()
            };
            let id = format!("csv-{}", hash_parts(&[&primary, &timestamp, &ordinal]));
            sqlx::query("INSERT INTO financial_ledger (id, timestamp, amount, currency, institution, merchant, category, source_type) VALUES (?, ?, ?, ?, ?, ?, ?, 'CSV_IMPORT')")
                .bind(&id).bind(row.entry.timestamp).bind(row.entry.amount).bind(&row.entry.currency)
                .bind(&row.entry.institution).bind(&row.entry.merchant).bind(&row.entry.category)
                .execute(&mut **tx).await?;
            sqlx::query("INSERT INTO csv_import_records (ledger_id, has_time, date_quality, merchant_quality, category_quality) VALUES (?, ?, ?, ?, ?)")
                .bind(&id).bind(row.has_time).bind(row.date_quality).bind(row.merchant_quality).bind(row.category_quality)
                .execute(&mut **tx).await?;
            stats.inserted += 1;
            id
        };
        claimed.insert(id.clone());
        for key in [&primary, &posting] {
            sqlx::query(
                "INSERT OR IGNORE INTO csv_import_keys (match_key, ledger_id) VALUES (?, ?)",
            )
            .bind(key)
            .bind(&id)
            .execute(&mut **tx)
            .await?;
        }
    }
    Ok(())
}
