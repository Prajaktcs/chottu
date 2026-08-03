//! Guards for financial_ledger inserts — blocks absurd / non-transaction parses.

/// Absolute raw-amount ceiling (any currency). Catches volume/balance hallucinations.
pub const LEDGER_ABS_AMOUNT_HARD_MAX: f64 = 1_000_000.0;

/// Soft USD-equivalent ceiling for a single personal ledger commit.
pub const LEDGER_USD_EQUIV_MAX: f64 = 100_000.0;

#[derive(Debug, Clone, PartialEq)]
pub enum LedgerAmountReject {
    ZeroOrNonFinite,
    HardMax { amount: f64 },
    UsdEquivalentMax { usd_equiv: f64 },
}

impl std::fmt::Display for LedgerAmountReject {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ZeroOrNonFinite => write!(f, "amount is zero or non-finite"),
            Self::HardMax { amount } => {
                write!(
                    f,
                    "amount {:.2} exceeds hard max {}",
                    amount, LEDGER_ABS_AMOUNT_HARD_MAX
                )
            }
            Self::UsdEquivalentMax { usd_equiv } => {
                write!(
                    f,
                    "≈${:.2} USD exceeds soft max ${}",
                    usd_equiv, LEDGER_USD_EQUIV_MAX
                )
            }
        }
    }
}

/// Rough static FX so ingest does not depend on a live rate fetch.
pub fn approximate_usd(amount: f64, currency: &str) -> f64 {
    let rate = match currency.trim().to_uppercase().as_str() {
        "USD" => 1.0,
        "CAD" => 0.74,
        "EUR" => 1.08,
        "GBP" => 1.27,
        "INR" => 0.012,
        "AUD" => 0.65,
        "JPY" => 0.0067,
        // Unknown → treat as USD (stricter).
        _ => 1.0,
    };
    amount.abs() * rate
}

pub fn validate_ledger_amount(amount: f64, currency: &str) -> Result<(), LedgerAmountReject> {
    if !amount.is_finite() || amount == 0.0 {
        return Err(LedgerAmountReject::ZeroOrNonFinite);
    }
    let abs = amount.abs();
    if abs > LEDGER_ABS_AMOUNT_HARD_MAX {
        return Err(LedgerAmountReject::HardMax { amount: abs });
    }
    let usd_equiv = approximate_usd(amount, currency);
    if usd_equiv > LEDGER_USD_EQUIV_MAX {
        return Err(LedgerAmountReject::UsdEquivalentMax { usd_equiv });
    }
    Ok(())
}

/// Subject/body patterns that look like alerts, cancels, or loan spam — not ledger txs.
pub fn looks_like_non_transaction_alert(subject: &str, body_preview: Option<&str>) -> bool {
    let text = format!("{} {}", subject, body_preview.unwrap_or("")).to_lowercase();
    const NEEDLES: &[&str] = &[
        "traded above high volume",
        "high volume",
        "available balance is",
        "available balance",
        "smart alert for",
        "price alert",
        "went up by",
        "went down by",
        "we've canceled your order",
        "we have canceled your order",
        "order canceled",
        "order cancelled",
        "authorization on your credit card",
        "direct deposit greater than",
        "personalized loan",
        "pre-approved loan",
        "preapproved loan",
        "superfast approval",
        "loan for rs",
        "loan for ₹",
    ];
    if NEEDLES.iter().any(|n| text.contains(n)) {
        return true;
    }
    // Forum digests / alert senders (check From: as well as subject).
    text.contains("redditmail.com")
        || text.contains("noreply@reddit")
        || text.contains("iqalerts@questrade")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_zero_and_absurd() {
        assert!(validate_ledger_amount(0.0, "USD").is_err());
        assert!(validate_ledger_amount(f64::NAN, "USD").is_err());
        assert!(validate_ledger_amount(6.53e18, "USD").is_err());
        assert!(validate_ledger_amount(1_416_444.0, "USD").is_err());
    }

    #[test]
    fn allows_normal_and_inr_emi() {
        assert!(validate_ledger_amount(42.50, "USD").is_ok());
        assert!(validate_ledger_amount(94_768.0, "INR").is_ok());
        assert!(validate_ledger_amount(43_890.0, "USD").is_ok());
    }

    #[test]
    fn detects_alert_subjects() {
        assert!(looks_like_non_transaction_alert(
            "Alert: DDOG traded above high volume 6,250,982",
            None
        ));
        assert!(looks_like_non_transaction_alert(
            "Your Checking Account Available Balance Is Less Than $100.00",
            None
        ));
        assert!(looks_like_non_transaction_alert(
            "We've canceled your order for 0.52 ETH",
            None
        ));
        assert!(looks_like_non_transaction_alert(
            "Authorization on your credit card outside of Canada",
            None
        ));
        assert!(looks_like_non_transaction_alert(
            "Direct Deposit Greater Than $1.00 Credited To Your Checking Account",
            None
        ));
        assert!(!looks_like_non_transaction_alert(
            "Your Amazon.ca order of USB-C cable",
            Some("Total: $19.99")
        ));
    }
}
