use anyhow::{anyhow, Context, Result};
use chotu_common::FinancialLedgerEntry;
use chrono::{DateTime, TimeZone, Utc};
use sha2::{Digest, Sha256};
use std::fs::File;
use std::path::Path;

/// Parses a CSV file, dynamically inferring column mapping, and returns ledger entries.
pub fn parse_csv_file(file_path: &Path, default_currency: &str) -> Result<Vec<FinancialLedgerEntry>> {
    let file = File::open(file_path)
        .with_context(|| format!("Failed to open CSV file: {:?}", file_path))?;
    let mut reader = csv::ReaderBuilder::new()
        .has_headers(true)
        .from_reader(file);

    // Extract headers
    let headers = reader.headers().context("Failed to read CSV headers")?;

    // Inferred column indices
    let mut date_idx = None;
    let mut amount_idx = None;
    let mut merchant_idx = None;
    let mut category_idx = None;
    let mut currency_idx = None;

    for (i, header) in headers.iter().enumerate() {
        let h_lower = header.to_lowercase();
        if date_idx.is_none()
            && (h_lower.contains("date")
                || h_lower.contains("time")
                || h_lower.contains("timestamp"))
        {
            date_idx = Some(i);
        } else if amount_idx.is_none()
            && (h_lower.contains("amount")
                || h_lower.contains("charge")
                || h_lower.contains("value")
                || h_lower.contains("sum"))
        {
            amount_idx = Some(i);
        } else if merchant_idx.is_none()
            && (h_lower.contains("merchant")
                || h_lower.contains("payee")
                || h_lower.contains("description")
                || h_lower.contains("name")
                || h_lower.contains("memo")
                || h_lower.contains("details")
                || h_lower.contains("detail"))
        {
            merchant_idx = Some(i);
        } else if category_idx.is_none()
            && (h_lower.contains("category") || h_lower.contains("type"))
        {
            category_idx = Some(i);
        } else if currency_idx.is_none()
            && (h_lower.contains("currency") || h_lower.contains("curr"))
        {
            currency_idx = Some(i);
        }
    }

    // Enforce required columns: Date, Amount, and Merchant
    let date_idx = date_idx.ok_or_else(|| {
        anyhow!(
            "Could not infer 'date' column in CSV headers: {:?}",
            headers
        )
    })?;
    let amount_idx = amount_idx.ok_or_else(|| {
        anyhow!(
            "Could not infer 'amount' column in CSV headers: {:?}",
            headers
        )
    })?;
    let merchant_idx = merchant_idx.ok_or_else(|| {
        anyhow!(
            "Could not infer 'merchant/description' column in CSV headers: {:?}",
            headers
        )
    })?;

    // Infer institution from filename
    let filename = file_path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("Dropped CSV");
    let institution = infer_institution(filename);

    let lower_filename = filename.to_lowercase();
    let is_credit_card = lower_filename.contains("credit-card")
        || lower_filename.contains("credit_card")
        || lower_filename.contains("cc-")
        || lower_filename.contains("visa")
        || lower_filename.contains("mastercard")
        || lower_filename.contains("amex");

    let mut entries = Vec::new();

    for result in reader.records() {
        let record = result.context("Failed to read CSV record")?;

        let raw_date = record.get(date_idx).unwrap_or_default().trim();
        let raw_amount = record.get(amount_idx).unwrap_or_default().trim();
        let raw_merchant = record.get(merchant_idx).unwrap_or_default().trim();
        let raw_category = category_idx
            .and_then(|idx| record.get(idx))
            .unwrap_or("Uncategorized")
            .trim();
        let raw_currency = currency_idx
            .and_then(|idx| record.get(idx))
            .unwrap_or(default_currency)
            .trim();

        if raw_date.is_empty() || raw_amount.is_empty() || raw_merchant.is_empty() {
            continue; // Skip empty rows
        }

        // Parse Date
        let timestamp = parse_flexible_date(raw_date)
            .with_context(|| format!("Failed to parse date '{}' in CSV record", raw_date))?;

        // Parse Amount (clean currency symbols like $)
        let clean_amount_str: String = raw_amount
            .chars()
            .filter(|c| c.is_ascii_digit() || *c == '.' || *c == '-')
            .collect();
        let mut amount = clean_amount_str
            .parse::<f64>()
            .with_context(|| format!("Failed to parse amount '{}' in CSV record", raw_amount))?;

        if is_credit_card {
            amount = -amount;
        }

        // Generate deterministic transaction signature hash to prevent duplicates
        let transaction_id = generate_transaction_signature(
            timestamp,
            amount,
            raw_merchant,
            raw_currency,
            &institution,
        );

        entries.push(FinancialLedgerEntry {
            id: transaction_id,
            timestamp,
            amount,
            currency: raw_currency.to_string(),
            institution: institution.clone(),
            merchant: raw_merchant.to_string(),
            category: raw_category.to_string(),
            source_type: "BATCH_DROP".to_string(),
        });
    }

    Ok(entries)
}

fn infer_institution(filename: &str) -> String {
    let lower = filename.to_lowercase();
    if lower.contains("chase") {
        "Chase".to_string()
    } else if lower.contains("citi") {
        "Citibank".to_string()
    } else if lower.contains("scotia") {
        "Scotiabank".to_string()
    } else if lower.contains("amex") {
        "Amex".to_string()
    } else {
        "Dropped CSV".to_string()
    }
}

/// Tries to parse date formats like YYYY-MM-DD, MM/DD/YYYY, or RFC3339
fn parse_flexible_date(raw_date: &str) -> Result<DateTime<Utc>> {
    // 1. Try RFC3339
    if let Ok(dt) = DateTime::parse_from_rfc3339(raw_date) {
        return Ok(dt.with_timezone(&Utc));
    }

    // 2. Try YYYY-MM-DD
    if let Ok(naive) = chrono::NaiveDate::parse_from_str(raw_date, "%Y-%m-%d") {
        if let Some(dt) = naive.and_hms_opt(0, 0, 0) {
            return Ok(Utc.from_utc_datetime(&dt));
        }
    }

    // 3. Try MM/DD/YYYY
    if let Ok(naive) = chrono::NaiveDate::parse_from_str(raw_date, "%m/%d/%Y") {
        if let Some(dt) = naive.and_hms_opt(0, 0, 0) {
            return Ok(Utc.from_utc_datetime(&dt));
        }
    }

    // 4. Try DD-MM-YYYY
    if let Ok(naive) = chrono::NaiveDate::parse_from_str(raw_date, "%d-%m-%Y") {
        if let Some(dt) = naive.and_hms_opt(0, 0, 0) {
            return Ok(Utc.from_utc_datetime(&dt));
        }
    }

    Err(anyhow!("Unsupported date format: {}", raw_date))
}

/// Generates a deterministic SHA256 transaction hash to serve as the PRIMARY KEY
fn generate_transaction_signature(
    timestamp: DateTime<Utc>,
    amount: f64,
    merchant: &str,
    currency: &str,
    institution: &str,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(timestamp.to_rfc3339().as_bytes());
    hasher.update(format!("{:.2}", amount).as_bytes());
    hasher.update(merchant.to_lowercase().trim().as_bytes());
    hasher.update(currency.to_lowercase().trim().as_bytes());
    hasher.update(institution.to_lowercase().trim().as_bytes());

    let result = hasher.finalize();
    // Convert hash bytes to hex string
    result
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect::<String>()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn test_csv_inference_and_parsing() {
        let mut tmp_file = NamedTempFile::new().unwrap();
        writeln!(
            tmp_file,
            "Transaction Date,Amount,Category,Merchant Name\n2026-05-30,-12.50,Food,Whole Foods\n05/31/2026,-42.00,Transport,Uber"
        )
        .unwrap();

        let entries = parse_csv_file(tmp_file.path(), "USD").unwrap();
        assert_eq!(entries.len(), 2);

        let entry1 = &entries[0];
        assert_eq!(entry1.amount, -12.50);
        assert_eq!(entry1.merchant, "Whole Foods");
        assert_eq!(entry1.category, "Food");
        assert_eq!(entry1.currency, "USD");
        assert_eq!(entry1.source_type, "BATCH_DROP");

        let entry2 = &entries[1];
        assert_eq!(entry2.amount, -42.00);
        assert_eq!(entry2.merchant, "Uber");
        assert_eq!(entry2.category, "Transport");
    }

    #[test]
    fn test_csv_details_header_parsing() {
        let mut tmp_file = NamedTempFile::new().unwrap();
        writeln!(
            tmp_file,
            "transaction_date,post_date,type,details,amount,currency\n2026-05-01,2026-05-02,DEBIT,Supermarket,-55.20,CAD"
        )
        .unwrap();

        let entries = parse_csv_file(tmp_file.path(), "USD").unwrap();
        assert_eq!(entries.len(), 1);
        let entry = &entries[0];
        assert_eq!(entry.amount, -55.20);
        assert_eq!(entry.merchant, "Supermarket");
        assert_eq!(entry.currency, "CAD");
    }

    #[test]
    fn test_date_parsing() {
        assert!(parse_flexible_date("2026-05-30").is_ok());
        assert!(parse_flexible_date("05/30/2026").is_ok());
        assert!(parse_flexible_date("30-05-2026").is_ok());
        assert!(parse_flexible_date("2026-05-30T10:00:00Z").is_ok());
        assert!(parse_flexible_date("invalid-date").is_err());
    }

    #[test]
    fn test_transaction_deduplication_hashing() {
        let ts = Utc.with_ymd_and_hms(2026, 5, 30, 0, 0, 0).unwrap();
        let hash1 = generate_transaction_signature(ts, -12.50, "Whole Foods", "USD", "Chase");
        let hash2 = generate_transaction_signature(ts, -12.50, "Whole Foods  ", "usd", "chase");
        let hash3 = generate_transaction_signature(ts, -12.50, "Citibank", "USD", "Chase");

        assert_eq!(
            hash1, hash2,
            "Hashing must be whitespace and case insensitive"
        );
        assert_ne!(hash1, hash3, "Hashing must differ on merchant changes");
    }

    #[test]
    fn test_credit_card_statement_sign_flipping() {
        let tmp_file = NamedTempFile::new().unwrap();
        let file_path = tmp_file.path().parent().unwrap().join("credit-card-test.csv");
        let mut cc_file = File::create(&file_path).unwrap();
        
        writeln!(
            cc_file,
            "transaction_date,amount,merchant\n2026-05-01,150.00,Best Buy\n2026-05-02,-150.00,Payment Received"
        )
        .unwrap();

        let entries = parse_csv_file(&file_path, "USD").unwrap();
        assert_eq!(entries.len(), 2);
        
        // Purchase (originally positive 150.00) should be flipped to negative -150.00
        assert_eq!(entries[0].amount, -150.00);
        assert_eq!(entries[0].merchant, "Best Buy");
        
        // Payment (originally negative -150.00) should be flipped to positive 150.00
        assert_eq!(entries[1].amount, 150.00);
        assert_eq!(entries[1].merchant, "Payment Received");
        
        let _ = std::fs::remove_file(&file_path);
    }
}
