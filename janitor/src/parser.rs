use anyhow::{anyhow, bail, Context, Result};
use chotu_common::FinancialLedgerEntry;
use chrono::{DateTime, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Utc};
use sha2::{Digest, Sha256};
use std::fs::File;
use std::path::Path;

#[derive(Debug, Clone)]
pub(crate) struct CsvTransaction {
    pub(crate) entry: FinancialLedgerEntry,
    pub(crate) account_id: String,
    pub(crate) kind: String,
    pub(crate) posting_date: Option<NaiveDate>,
    pub(crate) has_time: bool,
    pub(crate) date_quality: u8,
    pub(crate) merchant_quality: u8,
    pub(crate) category_quality: u8,
}

#[derive(Debug)]
pub(crate) struct ParsedCsv {
    pub(crate) transactions: Vec<CsvTransaction>,
    pub(crate) legacy_ids: Vec<String>,
    pub(crate) blank_rows: usize,
    pub(crate) non_posted_rows: usize,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Schema {
    CardStatement,
    CardActivities,
    AccountActivities,
    MonthlyStatement,
    Generic,
}

struct Columns {
    names: Vec<String>,
}

impl Columns {
    fn exact(&self, names: &[&str]) -> Option<usize> {
        names
            .iter()
            .find_map(|name| self.names.iter().position(|h| h == name))
    }

    fn required(&self, names: &[&str], label: &str) -> Result<usize> {
        self.exact(names)
            .ok_or_else(|| anyhow!("Missing {label} column in CSV headers: {:?}", self.names))
    }

    fn preferred(&self, names: &[&str], fragments: &[&str]) -> Option<usize> {
        self.exact(names).or_else(|| {
            self.names
                .iter()
                .position(|h| fragments.iter().any(|f| h.contains(f)))
        })
    }

    fn has(&self, names: &[&str]) -> bool {
        names.iter().all(|name| self.exact(&[name]).is_some())
    }
}

pub(crate) fn parse_csv_file(path: &Path, default_currency: &str) -> Result<ParsedCsv> {
    let file =
        File::open(path).with_context(|| format!("Failed to open CSV file: {}", path.display()))?;
    let mut reader = csv::ReaderBuilder::new()
        .has_headers(true)
        .from_reader(file);
    let headers = reader
        .headers()
        .context("Failed to read CSV headers")?
        .clone();
    let columns = Columns {
        names: headers
            .iter()
            .map(|h| {
                h.trim()
                    .trim_start_matches('\u{feff}')
                    .to_lowercase()
                    .replace([' ', '-'], "_")
            })
            .collect(),
    };
    let schema = if columns.has(&["transaction_date", "post_date", "type", "details"]) {
        Schema::CardStatement
    } else if columns.has(&[
        "transaction_date",
        "transaction_type",
        "merchant",
        "amount",
        "category",
    ]) {
        Schema::CardActivities
    } else if columns.has(&["effective_date", "net_cash_amount", "account_id"]) {
        Schema::AccountActivities
    } else if columns.has(&[
        "date",
        "transaction",
        "description",
        "amount",
        "balance",
        "currency",
    ]) {
        Schema::MonthlyStatement
    } else {
        Schema::Generic
    };
    let date_idx = match schema {
        Schema::AccountActivities => columns.required(&["effective_date"], "effective date")?,
        Schema::CardStatement | Schema::CardActivities => {
            columns.required(&["transaction_date"], "transaction date")?
        }
        Schema::MonthlyStatement => columns.required(&["date"], "date")?,
        Schema::Generic => columns
            .preferred(
                &["date", "transaction_date", "timestamp", "effective_date"],
                &["date", "time", "timestamp"],
            )
            .ok_or_else(|| anyhow!("Missing date column in CSV headers: {:?}", headers))?,
    };
    let amount_idx = match schema {
        Schema::AccountActivities => columns.required(&["net_cash_amount"], "net cash amount")?,
        _ => columns
            .preferred(&["amount"], &["amount", "charge", "value", "sum"])
            .ok_or_else(|| anyhow!("Missing amount column in CSV headers: {:?}", headers))?,
    };
    let merchant_idx = match schema {
        Schema::CardStatement => columns.exact(&["details"]),
        Schema::CardActivities => columns.exact(&["merchant"]),
        Schema::AccountActivities | Schema::MonthlyStatement => columns.exact(&["description"]),
        Schema::Generic => columns.preferred(
            &["merchant", "description", "payee", "merchant_name"],
            &["merchant", "payee", "description", "name", "memo", "detail"],
        ),
    };
    let category_idx = columns.exact(&["category"]).or_else(|| {
        columns
            .names
            .iter()
            .position(|h| h.contains("category") && !h.ends_with("_type"))
    });
    let currency_idx = columns.preferred(&["currency"], &["currency", "curr"]);
    let account_idx = columns.exact(&["account_id", "account"]);
    let type_idx = match schema {
        Schema::CardStatement => columns.exact(&["type"]),
        Schema::CardActivities => columns.exact(&["transaction_type"]),
        Schema::AccountActivities => columns.exact(&["activity_type"]),
        Schema::MonthlyStatement => columns.exact(&["transaction"]),
        Schema::Generic => {
            columns.exact(&["transaction_type", "activity_type", "type", "transaction"])
        }
    };
    let subtype_idx = columns.exact(&["activity_sub_type", "activity_subtype"]);
    let status_idx = if schema == Schema::CardActivities {
        Some(columns.required(&["status"], "status")?)
    } else {
        None
    };
    let time_idx = if schema == Schema::AccountActivities {
        columns.exact(&["effective_time"])
    } else {
        None
    };
    let posting_idx = columns.exact(&["post_date", "posting_date"]);
    let filename = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("Dropped CSV");
    let inferred_institution = infer_institution(filename);
    let filename_account = filename_account_id(filename);
    let legacy = LegacyColumns::new(&headers, filename);
    let mut parsed = ParsedCsv {
        transactions: Vec::new(),
        legacy_ids: Vec::new(),
        blank_rows: 0,
        non_posted_rows: 0,
    };

    for (index, result) in reader.records().enumerate() {
        let record =
            result.with_context(|| format!("{}: CSV row {}", path.display(), index + 2))?;
        let row = record
            .position()
            .map(|p| p.line())
            .unwrap_or((index + 2) as u64);
        if record.iter().all(|cell| cell.trim().is_empty()) {
            parsed.blank_rows += 1;
            continue;
        }
        // Migration ownership only: reproduce the old parser's signature, never use it for new IDs.
        if let Some(id) = legacy.id(&record, default_currency) {
            parsed.legacy_ids.push(id);
        }
        if let Some(status_idx) = status_idx {
            let status = record.get(status_idx).unwrap_or("").trim();
            if !status.eq_ignore_ascii_case("completed") && !status.eq_ignore_ascii_case("posted") {
                if [
                    "pending",
                    "declined",
                    "cancelled",
                    "canceled",
                    "failed",
                    "void",
                    "voided",
                    "reversed",
                    "authorized",
                    "authorised",
                ]
                .iter()
                .any(|state| status.eq_ignore_ascii_case(state))
                {
                    parsed.non_posted_rows += 1;
                    continue;
                }
                bail!(
                    "{}: CSV row {row}: Unknown or empty card activity status: {status:?}",
                    path.display()
                );
            }
        }
        let transaction = (|| -> Result<CsvTransaction> {
            let cell = |idx: Option<usize>| idx.and_then(|i| record.get(i)).unwrap_or("").trim();
            let raw_date = cell(Some(date_idx));
            let raw_amount = cell(Some(amount_idx));
            if raw_date.is_empty() {
                bail!("Missing transaction date");
            }
            if raw_amount.is_empty() {
                bail!("Missing transaction amount");
            }
            let (mut timestamp, has_time) = parse_timestamp(raw_date, cell(time_idx))?;
            let mut posting_date = if cell(posting_idx).is_empty() {
                None
            } else {
                Some(parse_flexible_date(cell(posting_idx))?.date_naive())
            };
            let mut amount = parse_amount(raw_amount)?;
            if schema == Schema::CardStatement {
                amount = -amount;
            }
            let currency = if cell(currency_idx).is_empty() {
                default_currency.trim()
            } else {
                cell(currency_idx)
            }
            .to_uppercase();
            if currency.is_empty() {
                bail!("Missing currency and default currency");
            }
            let is_card = matches!(schema, Schema::CardStatement | Schema::CardActivities);
            let account_id = if is_card {
                "wealthsimple-credit-card".to_string()
            } else if !cell(account_idx).is_empty() {
                normalize_whitespace(cell(account_idx))
            } else if schema == Schema::AccountActivities {
                bail!("Missing account_id");
            } else if let Some(account) = &filename_account {
                account.clone()
            } else if schema == Schema::MonthlyStatement {
                // An unidentifiable monthly statement cannot safely be merged across accounts.
                bail!("Missing account identity in monthly statement filename: {filename}");
            } else {
                bail!("Missing account identity: generic CSV requires a nonempty account_id/account column or a recognized account identity in the filename");
            };
            let institution = if is_card {
                "Wealthsimple:credit-card".to_string()
            } else if matches!(schema, Schema::MonthlyStatement | Schema::AccountActivities)
                || filename_account.is_some()
            {
                format!("Wealthsimple:{account_id}")
            } else {
                inferred_institution.clone()
            };
            let raw_merchant = normalize_whitespace(cell(merchant_idx));
            let kind = transaction_kind(
                cell(type_idx),
                cell(subtype_idx),
                &raw_merchant,
                is_card,
                amount,
            );
            if schema == Schema::MonthlyStatement && matches!(kind.as_str(), "buy" | "sell") {
                if let Some(executed_date) = execution_date(&raw_merchant)? {
                    posting_date = Some(timestamp.date_naive());
                    timestamp = Utc.from_utc_datetime(
                        &executed_date
                            .and_hms_opt(0, 0, 0)
                            .expect("midnight is valid"),
                    );
                }
            }
            let merchant_quality = if raw_merchant.is_empty()
                || (is_card && kind == "transfer")
                || (schema == Schema::AccountActivities
                    && is_generic_description(&raw_merchant, &kind))
            {
                1
            } else if schema == Schema::AccountActivities {
                2
            } else {
                3
            };
            let merchant = if is_card && kind == "transfer" {
                "Credit card payment".to_string()
            } else if !raw_merchant.is_empty() {
                raw_merchant
            } else if schema != Schema::Generic && kind != "unknown" {
                kind_label(&kind).to_string()
            } else {
                bail!("Missing merchant/description");
            };
            let inferred = inferred_category(&kind, amount);
            // Structural kinds outrank export labels, including metadata from
            // earlier imports; asset movements must not become household spend.
            let (category, category_quality) = if inferred != "Uncategorized" {
                (inferred.to_string(), 3)
            } else {
                let real_category = normalize_whitespace(cell(category_idx));
                if !real_category.is_empty()
                    && !real_category.eq_ignore_ascii_case("Uncategorized")
                {
                    (real_category, 2)
                } else {
                    (inferred.to_string(), 0)
                }
            };
            Ok(CsvTransaction {
                entry: FinancialLedgerEntry {
                    id: String::new(),
                    timestamp,
                    amount,
                    currency,
                    institution,
                    merchant,
                    category,
                    source_type: "CSV_IMPORT".to_string(),
                },
                account_id,
                kind,
                posting_date,
                has_time,
                date_quality: match schema {
                    Schema::CardActivities | Schema::AccountActivities => 2,
                    _ => 1,
                },
                merchant_quality,
                category_quality,
            })
        })()
        .with_context(|| format!("{}: CSV row {row}", path.display()))?;
        parsed.transactions.push(transaction);
    }
    Ok(parsed)
}

fn normalize_whitespace(value: &str) -> String {
    let mut normalized = String::with_capacity(value.len());
    for word in value.split_whitespace() {
        if !normalized.is_empty() {
            normalized.push(' ');
        }
        normalized.push_str(word);
    }
    normalized
}

fn filename_account_id(filename: &str) -> Option<String> {
    filename
        .split(|c: char| !c.is_ascii_alphanumeric())
        .find(|part| {
            part.len() == 12
                && (part.ends_with("CAD") || part.ends_with("USD"))
                && part[..9]
                    .chars()
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
                && part[..9].chars().any(|c| c.is_ascii_digit())
        })
        .map(str::to_string)
}

fn execution_date(description: &str) -> Result<Option<NaiveDate>> {
    let Some((_, suffix)) = description.split_once("(executed at ") else {
        return Ok(None);
    };
    let raw = suffix.split(')').next().unwrap_or(suffix);
    Ok(Some(
        NaiveDate::parse_from_str(raw, "%Y-%m-%d")
            .with_context(|| format!("Invalid trade execution date: {raw:?}"))?,
    ))
}

fn transaction_kind(
    raw_type: &str,
    raw_subtype: &str,
    merchant: &str,
    is_card: bool,
    amount: f64,
) -> String {
    let normalize = |s: &str| s.trim().to_uppercase().replace([' ', '-'], "_");
    let classify = |value: &str| -> Option<&'static str> {
        Some(match value {
            "PURCHASE" => "purchase",
            "REFUND" => "refund",
            "PAYMENT"
            | "PAYMENT_RECEIVED"
            | "CREDIT_CARD_PAYMENT"
            | "TRFOUT"
            | "TRFIN"
            | "TRFOUTTF"
            | "TRFINTF"
            | "TRANSFER"
            | "TRANSFER_TF"
            | "INTERNAL_TRANSFER"
            | "FX"
            | "FXEXCHANGE"
            | "FOREIGN_EXCHANGE" => "transfer",
            "INT" | "INTEREST" | "INTEREST_EARNED" => "interest",
            "INTEREST_CHARGE" | "INTEREST_CHARGES" | "INTERESTCHARGED" | "FEE" | "FEES" => "fee",
            "DIV" | "DIVIDEND" => "dividend",
            "BUY" => "buy",
            "SELL" => "sell",
            "EFT" | "EFTOUT" | "EFT_TF" => "eft",
            "AFT_IN" | "DIRECT_DEPOSIT" | "SALARY" => "direct-deposit",
            "AFT_OUT" | "PREAUTHORIZED_DEBIT" | "PRE_AUTHORIZED_DEBIT" => "preauthorized-debit",
            "SPEND" => "spend",
            "OBP_OUT" | "BILL_PAYMENT" => "bill-payment",
            "E_TRFOUT" | "E_TRFIN" | "ETRANSFER" | "E_TRANSFER" | "INTERAC_E_TRANSFER" => {
                "etransfer"
            }
            "NRT" | "TAX" | "NON_RESIDENT_TAX" => "tax",
            "CASHBACK" | "CASH_BACK" => "cashback",
            "BONUS" | "BONUSPAYMENT" | "GIVEAWAY" => "bonus",
            _ => return None,
        })
    };
    let kind = classify(&normalize(raw_subtype)).or_else(|| classify(&normalize(raw_type)));
    if let Some(kind) = kind {
        return kind.to_string();
    }
    if is_card {
        if normalize(merchant).contains("PAYMENT") {
            return "transfer".to_string();
        }
        return if amount < 0.0 { "purchase" } else { "refund" }.to_string();
    }
    if raw_type.is_empty() {
        "unknown".to_string()
    } else {
        normalize(raw_type).to_lowercase()
    }
}

fn kind_label(kind: &str) -> &str {
    match kind {
        "transfer" => "Transfer",
        "interest" => "Interest",
        "dividend" => "Dividend",
        "direct-deposit" => "Direct deposit",
        "preauthorized-debit" => "Pre-authorized Debit",
        "bill-payment" => "Bill payment",
        "etransfer" => "E-transfer",
        "eft" => "EFT",
        "spend" => "Spend",
        "buy" => "Buy",
        "sell" => "Sell",
        "tax" => "Tax",
        "cashback" => "Cashback",
        "bonus" => "Bonus",
        "fee" => "Fee",
        _ => kind,
    }
}

fn is_generic_description(description: &str, kind: &str) -> bool {
    let label = description
        .split(" (executed at ")
        .next()
        .unwrap_or(description);
    label.eq_ignore_ascii_case(kind_label(kind))
        || label.eq_ignore_ascii_case(&kind.replace('-', " "))
        || [
            "Purchase",
            "Refund",
            "Spend",
            "Pre-authorized Debit",
            "Preauthorized Debit",
            "Deposit",
            "Withdrawal",
            "Direct deposit received",
            "Interest received",
            "Money transfer into the account",
            "Money transfer out of the account",
            "Online bill payment",
            "Interac e-Transfer® Out",
            "Interac e-Transfer® In",
        ]
        .iter()
        .any(|generic| label.eq_ignore_ascii_case(generic))
}

fn inferred_category(kind: &str, amount: f64) -> &'static str {
    match kind {
        "interest" if amount < 0.0 => "Fees",
        "interest" | "dividend" | "direct-deposit" | "cashback" | "bonus" => "Income",
        "transfer" | "eft" | "etransfer" => "Transfer",
        "buy" | "sell" => "Investment",
        "fee" => "Fees",
        "tax" => "Taxes",
        _ => "Uncategorized",
    }
}

fn parse_amount(raw: &str) -> Result<f64> {
    let raw = raw.trim();
    let parenthesized = raw.starts_with('(') && raw.ends_with(')');
    if raw.contains(['(', ')']) && !parenthesized {
        bail!("Malformed amount: {raw:?}");
    }
    let inner = if parenthesized {
        &raw[1..raw.len() - 1]
    } else {
        raw
    };
    let mut cleaned = inner.trim().to_string();
    for code in ["CAD", "USD", "EUR", "GBP"] {
        if let Some(rest) = cleaned.strip_prefix(code) {
            cleaned = rest.trim().to_string();
        }
        if let Some(rest) = cleaned.strip_suffix(code) {
            cleaned = rest.trim().to_string();
        }
    }
    cleaned.retain(|c| !matches!(c, '$' | '€' | '£' | ',') && !c.is_whitespace());
    if parenthesized && cleaned.starts_with(['-', '+']) {
        bail!("Conflicting signs in amount: {raw:?}");
    }
    let mut amount = cleaned
        .parse::<f64>()
        .with_context(|| format!("Malformed amount: {raw:?}"))?;
    if !amount.is_finite() {
        bail!("Nonfinite amount: {raw:?}");
    }
    if parenthesized {
        amount = -amount;
    }
    Ok(amount)
}

fn parse_timestamp(date: &str, time: &str) -> Result<(DateTime<Utc>, bool)> {
    if !time.is_empty() {
        let date = parse_flexible_date(date)?.date_naive();
        let combined = format!("{date}T{time}");
        if let Ok(timestamp) = DateTime::parse_from_rfc3339(&combined) {
            return Ok((timestamp.with_timezone(&Utc), true));
        }
        let time = NaiveTime::parse_from_str(time, "%H:%M:%S%.f")
            .or_else(|_| NaiveTime::parse_from_str(time, "%H:%M"))
            .with_context(|| format!("Unsupported effective_time: {time:?}"))?;
        return Ok((Utc.from_utc_datetime(&date.and_time(time)), true));
    }
    let timestamp = parse_flexible_date(date)?;
    Ok((timestamp, date.contains('T') || date.contains(':')))
}

fn parse_flexible_date(raw: &str) -> Result<DateTime<Utc>> {
    if let Ok(dt) = DateTime::parse_from_rfc3339(raw) {
        return Ok(dt.with_timezone(&Utc));
    }
    for format in ["%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%dT%H:%M:%S%.f"] {
        if let Ok(dt) = NaiveDateTime::parse_from_str(raw, format) {
            return Ok(Utc.from_utc_datetime(&dt));
        }
    }
    legacy_date(raw)
}

// MIGRATION ONLY. These first-match columns, filename sign guesses and lossy amount
// cleaning deliberately reproduce deployed hashes, including their old bugs.
struct LegacyColumns {
    date: Option<usize>,
    amount: Option<usize>,
    merchant: Option<usize>,
    currency: Option<usize>,
    institution: String,
    flip_sign: bool,
}

impl LegacyColumns {
    fn new(headers: &csv::StringRecord, filename: &str) -> Self {
        let mut columns = Self {
            date: None,
            amount: None,
            merchant: None,
            currency: None,
            institution: infer_institution(filename),
            flip_sign: false,
        };
        let mut category = None;
        for (i, header) in headers.iter().enumerate() {
            let h = header.to_lowercase();
            if columns.date.is_none() && ["date", "time", "timestamp"].iter().any(|s| h.contains(s))
            {
                columns.date = Some(i);
            } else if columns.amount.is_none()
                && ["amount", "charge", "value", "sum"]
                    .iter()
                    .any(|s| h.contains(s))
            {
                columns.amount = Some(i);
            } else if columns.merchant.is_none()
                && [
                    "merchant",
                    "payee",
                    "description",
                    "name",
                    "memo",
                    "details",
                    "detail",
                ]
                .iter()
                .any(|s| h.contains(s))
            {
                columns.merchant = Some(i);
            } else if category.is_none() && ["category", "type"].iter().any(|s| h.contains(s)) {
                category = Some(i);
            } else if columns.currency.is_none()
                && ["currency", "curr"].iter().any(|s| h.contains(s))
            {
                columns.currency = Some(i);
            }
        }
        let filename = filename.to_lowercase();
        columns.flip_sign = [
            "credit-card",
            "credit_card",
            "cc-",
            "visa",
            "mastercard",
            "amex",
        ]
        .iter()
        .any(|s| filename.contains(s));
        columns
    }

    fn id(&self, record: &csv::StringRecord, default_currency: &str) -> Option<String> {
        let date = record.get(self.date?)?.trim();
        let amount = record.get(self.amount?)?.trim();
        let merchant = record.get(self.merchant?)?.trim();
        let currency = self
            .currency
            .and_then(|i| record.get(i))
            .unwrap_or(default_currency)
            .trim();
        if date.is_empty() || amount.is_empty() || merchant.is_empty() {
            return None;
        }
        let timestamp = legacy_date(date).ok()?;
        let cleaned: String = amount
            .chars()
            .filter(|c| c.is_ascii_digit() || *c == '.' || *c == '-')
            .collect();
        let mut amount = cleaned.parse::<f64>().ok()?;
        if self.flip_sign {
            amount = -amount;
        }
        let mut hasher = Sha256::new();
        hasher.update(timestamp.to_rfc3339().as_bytes());
        hasher.update(format!("{amount:.2}").as_bytes());
        hasher.update(merchant.to_lowercase().trim().as_bytes());
        hasher.update(currency.to_lowercase().trim().as_bytes());
        hasher.update(self.institution.to_lowercase().trim().as_bytes());
        Some(format!("{:x}", hasher.finalize()))
    }
}

fn legacy_date(raw: &str) -> Result<DateTime<Utc>> {
    if let Ok(dt) = DateTime::parse_from_rfc3339(raw) {
        return Ok(dt.with_timezone(&Utc));
    }
    for format in ["%Y-%m-%d", "%m/%d/%Y", "%d-%m-%Y"] {
        if let Ok(date) = NaiveDate::parse_from_str(raw, format) {
            return Ok(
                Utc.from_utc_datetime(&date.and_hms_opt(0, 0, 0).expect("midnight is valid"))
            );
        }
    }
    bail!("Unsupported date format: {raw}")
}

fn infer_institution(filename: &str) -> String {
    let lower = filename.to_lowercase();
    if lower.contains("chase") {
        "Chase"
    } else if lower.contains("citi") {
        "Citibank"
    } else if lower.contains("scotia") {
        "Scotiabank"
    } else if lower.contains("amex") {
        "Amex"
    } else {
        "Dropped CSV"
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn csv(filename: &str, content: &str) -> (TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(filename);
        std::fs::write(&path, content).unwrap();
        (dir, path)
    }

    #[test]
    fn generic_exact_columns_win_and_filename_does_not_flip_signs() {
        let (_dir, path) = csv("credit-card-generic.csv", "posting_date,charge_amount,merchant_name,transaction_type,date,amount,merchant,category,currency,account\n2026-05-03,999,Wrong,PURCHASE,2026-05-01,-12.50,  Whole   Foods ,Food, ,household\n2026-05-03,999,Wrong,REFUND,05/31/2026,42.00,Uber,Transport,USD,household\n");
        let parsed = parse_csv_file(&path, "CAD").unwrap();
        let first = &parsed.transactions[0];
        assert_eq!(first.entry.amount, -12.50);
        assert_eq!(
            first.entry.timestamp.date_naive(),
            NaiveDate::from_ymd_opt(2026, 5, 1).unwrap()
        );
        assert_eq!(first.entry.merchant, "Whole Foods");
        assert_eq!(first.entry.category, "Food");
        assert_eq!(first.entry.currency, "CAD");
        assert_eq!(first.entry.source_type, "CSV_IMPORT");
        assert_eq!(first.account_id, "household");
        assert_eq!(parsed.transactions[1].entry.amount, 42.0);
    }

    #[test]
    fn card_statement_flips_charges_and_retains_merchantless_payment() {
        let (_dir, path) = csv("statement.csv", "transaction_date,post_date,type,details,amount,currency\n2026-05-01,2026-05-02,Purchase,Supermarket,55.20,CAD\n2026-05-02,2026-05-03,Refund,Supermarket,-5.20,CAD\n2026-05-03,2026-05-04,Payment,,-50.00,CAD\n");
        let parsed = parse_csv_file(&path, "USD").unwrap();
        assert_eq!(
            parsed
                .transactions
                .iter()
                .map(|t| t.entry.amount)
                .collect::<Vec<_>>(),
            [-55.2, 5.2, 50.0]
        );
        assert_eq!(parsed.transactions[0].entry.category, "Uncategorized");
        assert_eq!(parsed.transactions[0].kind, "purchase");
        assert_eq!(
            parsed.transactions[0].posting_date,
            NaiveDate::from_ymd_opt(2026, 5, 2)
        );
        assert_eq!(parsed.transactions[2].entry.merchant, "Credit card payment");
        assert_eq!(parsed.transactions[2].entry.category, "Transfer");
        assert_eq!(parsed.transactions[2].kind, "transfer");
        assert_eq!(
            parsed.transactions[2].account_id,
            "wealthsimple-credit-card"
        );
        assert_eq!(
            parsed.transactions[2].entry.institution,
            "Wealthsimple:credit-card"
        );
        assert_eq!(
            parsed.legacy_ids.len(),
            2,
            "old parser skipped merchantless payments"
        );
        assert_eq!(
            parsed.legacy_ids[0],
            "aefaaf1f73d42cb7d50b96ff99c0f6395cd40f84b2efe41253af834ae1451940"
        );
    }

    #[test]
    fn card_activities_preserve_cash_flow_and_real_categories() {
        let (_dir, path) = csv("credit-card-activities.csv", "transaction_date,transaction_type,status,merchant,amount,category,currency\n2026-05-01,Purchase,Posted,Grocer,-20.00,Groceries,CAD\n2026-05-02,Refund,Posted,Grocer,5.00,Groceries,CAD\n2026-05-03,Payment,Posted,,15.00,Uncategorized,CAD\n");
        let parsed = parse_csv_file(&path, "USD").unwrap();
        assert_eq!(
            parsed
                .transactions
                .iter()
                .map(|t| t.entry.amount)
                .collect::<Vec<_>>(),
            [-20.0, 5.0, 15.0]
        );
        assert_eq!(parsed.transactions[0].entry.category, "Groceries");
        assert_eq!(parsed.transactions[0].category_quality, 2);
        assert_eq!(parsed.transactions[2].entry.category, "Transfer");
        assert_eq!(parsed.transactions[2].entry.merchant, "Credit card payment");
    }

    #[test]
    fn monthly_accounts_and_repeated_transactions_remain_distinct() {
        let content = "date,transaction,description,amount,balance,currency\n2026-05-01,AFT_IN,Employer,1000.00,1000.00,CAD\n2026-05-02,SPEND,Corner shop,-12.50,987.50,CAD\n2026-05-02,SPEND,Corner shop,-12.50,975.00,CAD\n2026-05-03,INT,Interest,1.00,976.00,CAD\n2026-05-04,TRFOUT,TFSA,-100.00,876.00,CAD\n2026-05-05,BUY,ETF,-50.00,826.00,CAD\n";
        let (_dir, path) = csv("2026-05-WK0000001CAD.csv", content);
        let parsed = parse_csv_file(&path, "USD").unwrap();
        assert_eq!(parsed.transactions[0].account_id, "WK0000001CAD");
        assert_eq!(
            parsed.transactions[0].entry.institution,
            "Wealthsimple:WK0000001CAD"
        );
        assert_eq!(parsed.transactions[0].entry.category, "Income");
        assert_eq!(parsed.transactions[1].entry.amount, -12.50);
        assert_eq!(parsed.transactions[3].entry.category, "Income");
        assert_eq!(parsed.transactions[4].entry.category, "Transfer");
        assert_eq!(parsed.transactions[5].entry.category, "Investment");
        let (_other_dir, other_path) = csv("HQ0000001CAD-2026-05.csv", content);
        let other = parse_csv_file(&other_path, "USD").unwrap();
        assert_eq!(other.transactions[0].account_id, "HQ0000001CAD");
    }

    #[test]
    fn monthly_trade_execution_date_matches_detailed_export_without_changing_old_hash() {
        let (_dir, path) = csv("RRSP-monthly-statement-transactions-HQ0000001CAD-2026-05-01.csv",
            "date,transaction,description,amount,balance,currency\n2026-05-08,BUY,CASH - Global X High Interest Savings ETF: Bought 0.0009 shares at $50.01 per share (executed at 2026-05-07),-0.05,0.01,CAD\n,,,,,\n");
        let parsed = parse_csv_file(&path, "USD").unwrap();
        let trade = &parsed.transactions[0];
        assert_eq!(
            trade.entry.timestamp.date_naive(),
            NaiveDate::from_ymd_opt(2026, 5, 7).unwrap()
        );
        assert_eq!(trade.posting_date, NaiveDate::from_ymd_opt(2026, 5, 8));
        assert_eq!(trade.entry.category, "Investment");
        assert_eq!(trade.kind, "buy");
        assert_eq!(parsed.blank_rows, 1);
        assert_eq!(
            parsed.legacy_ids,
            ["42b258bf6e06207e78006b81d596d37b2e424d1065e5c3aaf9b34d20e5769edf"]
        );
    }

    #[test]
    fn activities_use_effective_time_net_cash_account_and_exact_currency() {
        let (_dir, path) = csv("activities.csv", "transaction_date,amount,account_name,activity_type,activity_sub_type,description,account_id,effective_date,effective_time,net_cash_amount,settlement_currency,currency\n2026-04-30,999,Wrong,SPEND,,Spend,WK0000001CAD,2026-05-01,10:30:01,-12.50,USD,CAD\n2026-04-30,999,Wrong,SPEND,,Spend,WK0000001CAD,2026-05-01,10:30:02,-12.50,USD,CAD\n2026-04-30,999,Wrong,SPEND,,Spend,HQ0000001CAD,2026-05-01,10:30:01,-12.50,USD,CAD\n2026-04-30,999,Wrong,TRANSFER,TRANSFER_TF,,WK0000001CAD,2026-05-01,11:00:00,-100,USD,CAD\n");
        let parsed = parse_csv_file(&path, "USD").unwrap();
        let first = &parsed.transactions[0];
        assert_eq!(
            first.entry.timestamp,
            Utc.with_ymd_and_hms(2026, 5, 1, 10, 30, 1).unwrap()
        );
        assert!(first.has_time);
        assert_eq!(first.entry.amount, -12.5);
        assert_eq!(first.entry.currency, "CAD");
        assert_eq!(first.account_id, "WK0000001CAD");
        assert_ne!(
            first.entry.timestamp,
            parsed.transactions[1].entry.timestamp
        );
        assert_ne!(first.account_id, parsed.transactions[2].account_id);
        assert_eq!(first.merchant_quality, 1);
        assert_eq!(parsed.transactions[3].kind, "transfer");
        assert_eq!(parsed.transactions[3].entry.category, "Transfer");
    }

    #[test]
    fn actual_activity_labels_infer_nonspending_and_income_categories() {
        let (_dir, path) = csv("activities-export.csv",
            "effective_date,effective_time,settlement_date,account_id,account_type,activity_type,activity_sub_type,description,direction,symbol,name,currency,quantity,unit_price,commission,net_cash_amount\n\
             2026-05-01,10:30:01,,WK0000001CAD,Chequing,MoneyMovement,SPEND,Spend,,,,CAD,12.5,,,-12.50\n\
             2026-05-02,10:30:01,,WK0000001CAD,Chequing,MoneyMovement,AFT_IN,Direct deposit received,,,,CAD,1000,,,1000\n\
             2026-05-03,10:30:01,,HQ0000001CAD,RRSP,FxExchange,-,Convert USD (executed at 2026-05-03),,,,USD,100,,,100\n\
             2026-05-04,10:30:01,,HQ0000001CAD,RRSP,InterestCharged,-,Margin Interest Charges,,,,CAD,1,,,-1\n\
             2026-05-05,10:30:01,,HQ0000001CAD,RRSP,BonusPayment,GIVEAWAY,Giveaway received,,,,CAD,10,,,10\n\
             2026-05-06,10:30:01,,WK0000001CAD,Chequing,MoneyMovement,EFT_TF,Withdrawal (executed at 2026-05-06),,,,CAD,100,,,-100\n");
        let parsed = parse_csv_file(&path, "CAD").unwrap();
        assert_eq!(
            parsed
                .transactions
                .iter()
                .map(|t| t.entry.category.as_str())
                .collect::<Vec<_>>(),
            [
                "Uncategorized",
                "Income",
                "Transfer",
                "Fees",
                "Income",
                "Transfer"
            ]
        );
        assert_eq!(parsed.transactions[1].merchant_quality, 1);
        assert_eq!(parsed.transactions[3].kind, "fee");
        assert_eq!(parsed.transactions[5].kind, "eft");
        assert_eq!(
            parsed.legacy_ids[0],
            "fd41d86550b7118d7f3146f162c26dc6c3217ed490b126cda15e10c37a134712"
        );
    }

    #[test]
    fn parentheses_amounts_are_negative_and_bad_rows_report_context() {
        let (_dir, path) = csv(
            "generic.csv",
            "date,amount,merchant,currency,account_id\n2026-05-01,\"($1,234.50)\",Grocer,,household\n , , , , \n",
        );
        let parsed = parse_csv_file(&path, "CAD").unwrap();
        assert_eq!(parsed.transactions[0].entry.amount, -1234.50);
        assert_eq!(parsed.blank_rows, 1);
        for bad in ["NaN", "inf", "12oops", "(12", "(-12)", ""] {
            let (_dir, path) = csv(
                "malformed.csv",
                &format!("date,amount,merchant,account_id\n2026-05-01,{bad},Grocer,household\n"),
            );
            let error = format!("{:#}", parse_csv_file(&path, "CAD").unwrap_err());
            assert!(error.contains("CSV row 2"), "{error}");
            assert!(error.to_lowercase().contains("amount"), "{error}");
        }
        let (_dir, path) = csv(
            "bad-date.csv",
            "date,amount,merchant,account_id\ninvalid,12,Grocer,household\n",
        );
        let error = format!("{:#}", parse_csv_file(&path, "CAD").unwrap_err());
        assert!(
            error.contains("CSV row 2") && error.contains("Unsupported date"),
            "{error}"
        );
    }

    #[test]
    fn legacy_ids_reproduce_first_match_filename_flip_and_parentheses_bug() {
        let (_dir, path) = csv("credit-card-activities.csv", "transaction_date,transaction_type,status,merchant,amount,category,currency\n2026-05-01,Purchase,Posted,Grocer,-20.00,Groceries,CAD\n");
        let parsed = parse_csv_file(&path, "USD").unwrap();
        assert_eq!(
            parsed.legacy_ids,
            ["aad351c7c812b127d045d8c0e307ce7c7a20520af008c4b0464bf5d806dcad21"]
        );
        let (_dir, path) = csv("activity.csv", "transaction_date,amount,description,effective_date,net_cash_amount,currency,account\n2026-04-30,\"($12.50)\",Spend,2026-05-01,-30,CAD,household\n");
        let parsed = parse_csv_file(&path, "CAD").unwrap();
        assert_eq!(
            parsed.legacy_ids,
            ["d57b2039d47c5fb2e7f19b75a389fb3ad518fd2b92fc7aa4c44c78b5fd564884"]
        );
        assert_eq!(parsed.transactions[0].entry.amount, -12.5);
    }

    #[test]
    fn generic_rows_require_account_identity_and_preserve_distinct_accounts() {
        for content in [
            "date,amount,merchant\n2026-05-01,-20,Grocer\n",
            "date,amount,merchant,account_id\n2026-05-01,-20,Grocer,   \n",
        ] {
            let (_dir, path) = csv("wealthsimple-generic.csv", content);
            let error = format!("{:#}", parse_csv_file(&path, "CAD").unwrap_err());
            assert!(error.contains("CSV row 2"), "{error}");
            assert!(error.contains("account_id/account column"), "{error}");
        }
        let (_dir, path) = csv("generic.csv",
            "date,amount,merchant,account\n2026-05-01,-20,Grocer,household\n2026-05-01,-20,Grocer,business\n");
        let parsed = parse_csv_file(&path, "CAD").unwrap();
        assert_eq!(parsed.transactions[0].account_id, "household");
        assert_eq!(parsed.transactions[1].account_id, "business");
        assert_eq!(
            parsed.transactions[0].entry.amount,
            parsed.transactions[1].entry.amount
        );
        let (_dir, path) = csv(
            "2026-05-WK0000001CAD.csv",
            "date,amount,merchant\n2026-05-01,-20,Grocer\n",
        );
        let parsed = parse_csv_file(&path, "CAD").unwrap();
        assert_eq!(parsed.transactions[0].account_id, "WK0000001CAD");
    }

    #[test]
    fn card_status_filters_nonposted_spending_but_retains_legacy_cleanup_ids() {
        let header = "transaction_date,transaction_type,status,merchant,amount,category,currency\n";
        let mut content = format!("{header}2026-05-01,Purchase, Completed ,Grocer,-20,Groceries,CAD\n2026-05-02,Purchase,pOsTeD,Shop,-10,,CAD\n");
        let states = [
            "Pending",
            "Declined",
            "Cancelled",
            "Canceled",
            "Failed",
            "Void",
            "Voided",
            "Reversed",
            "Authorized",
            "Authorised",
        ];
        for (index, status) in states.iter().enumerate() {
            content.push_str(&format!(
                "2026-05-{:02},Purchase,{status},Excluded merchant,-99,Shopping,CAD\n",
                index + 3
            ));
        }
        let (_dir, path) = csv("credit-card-activities.csv", &content);
        let parsed = parse_csv_file(&path, "CAD").unwrap();
        assert_eq!(
            parsed
                .transactions
                .iter()
                .map(|t| (t.entry.merchant.as_str(), t.entry.amount))
                .collect::<Vec<_>>(),
            [("Grocer", -20.0), ("Shop", -10.0)]
        );
        assert_eq!(parsed.non_posted_rows, states.len());
        // The deployed parser ignored status. Repair must still own each excluded row.
        let headers = csv::StringRecord::from(header.trim_end().split(',').collect::<Vec<_>>());
        let legacy = LegacyColumns::new(&headers, "credit-card-activities.csv");
        let mut old_reader = csv::Reader::from_reader(content.as_bytes());
        let old_ids: Vec<_> = old_reader
            .records()
            .map(|row| legacy.id(&row.unwrap(), "CAD").unwrap())
            .collect();
        assert_eq!(parsed.legacy_ids, old_ids);
        assert_eq!(parsed.legacy_ids.len(), states.len() + 2);
        assert_ne!(
            parsed.transactions[0].category_quality,
            parsed.transactions[1].category_quality
        );
        assert_eq!(parsed.transactions[0].date_quality, 2);
        assert_eq!(parsed.transactions[1].date_quality, 2);
    }

    #[test]
    fn unknown_or_empty_card_status_reports_row_context_and_missing_column_errors() {
        for status in ["Processing", ""] {
            let (_dir, path) = csv("card.csv", &format!(
                "transaction_date,transaction_type,status,merchant,amount,category\n2026-05-01,Purchase,{status},Grocer,-20,Food\n"
            ));
            let error = format!("{:#}", parse_csv_file(&path, "CAD").unwrap_err());
            assert!(error.contains("CSV row 2"), "{error}");
            assert!(error.contains("status"), "{error}");
        }
        let (_dir, path) = csv("card.csv",
            "transaction_date,transaction_type,merchant,amount,category\n2026-05-01,Purchase,Grocer,-20,Food\n");
        let error = format!("{:#}", parse_csv_file(&path, "CAD").unwrap_err());
        assert!(error.contains("Missing status column"), "{error}");
    }

    #[test]
    fn account_activity_dates_outrank_monthly_dates_independently_of_categories() {
        let (_dir, path) = csv("2026-05-WK0000001CAD.csv",
            "date,transaction,description,amount,balance,currency,category\n2026-05-02,SPEND,Grocer,-20,80,CAD,Groceries\n");
        let monthly = parse_csv_file(&path, "CAD").unwrap();
        let (_dir, path) = csv("activities.csv",
            "effective_date,net_cash_amount,account_id,activity_type,description,category\n2026-05-01,-20,WK0000001CAD,SPEND,Grocer,\n2026-05-03,-10,WK0000001CAD,SPEND,Shop,Shopping\n");
        let activities = parse_csv_file(&path, "CAD").unwrap();
        let statement = &monthly.transactions[0];
        let activity = &activities.transactions[0];
        assert_eq!(statement.account_id, activity.account_id);
        assert_eq!(statement.entry.amount, activity.entry.amount);
        assert_ne!(statement.entry.timestamp, activity.entry.timestamp);
        assert!(statement.category_quality > activity.category_quality);
        assert!(activity.date_quality > statement.date_quality);
        assert!(!activity.has_time);
        assert_eq!(
            activity.date_quality,
            activities.transactions[1].date_quality
        );
        assert_ne!(
            activity.category_quality,
            activities.transactions[1].category_quality
        );
    }
}
