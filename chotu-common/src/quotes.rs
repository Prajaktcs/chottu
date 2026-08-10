//! Live market quotes via Yahoo Finance's unofficial chart API.
//! No API key. Suitable for personal/household net-worth checks; not for trading.

use serde::Deserialize;
use thiserror::Error;
use tokio::task::JoinSet;

const MAX_CONCURRENT_QUOTES: usize = 6;

#[derive(Debug, Clone, PartialEq)]
pub struct StockQuote {
    /// Symbol as stored in the portfolio (e.g. `VFV`).
    pub ticker: String,
    /// Yahoo symbol that resolved (e.g. `VFV.TO`).
    pub yahoo_symbol: String,
    pub price: f64,
    pub currency: String,
}

#[derive(Debug, Error)]
pub enum QuoteError {
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("invalid ticker symbol: {0}")]
    InvalidSymbol(String),
    #[error("no usable quote for {0}")]
    NotFound(String),
    #[error("yahoo finance returned an unexpected payload for {0}")]
    BadPayload(String),
}

#[derive(Debug, Deserialize)]
struct ChartResponse {
    chart: ChartBody,
}

#[derive(Debug, Deserialize)]
struct ChartBody {
    result: Option<Vec<ChartResult>>,
    error: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct ChartResult {
    meta: ChartMeta,
}

#[derive(Debug, Deserialize)]
struct ChartMeta {
    currency: Option<String>,
    symbol: Option<String>,
    #[serde(rename = "regularMarketPrice")]
    regular_market_price: Option<f64>,
}

/// Normalize and validate a ticker before hitting Yahoo.
/// Allows letters, digits, `.`, `-`, and `=` (Yahoo class shares / some indices).
pub fn normalize_ticker(ticker: &str) -> Result<String, QuoteError> {
    let t = ticker.trim().to_uppercase();
    if t.is_empty() || t.len() > 32 {
        return Err(QuoteError::InvalidSymbol(ticker.to_string()));
    }
    if !t
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '='))
    {
        return Err(QuoteError::InvalidSymbol(ticker.to_string()));
    }
    Ok(t)
}

/// Known Yahoo exchange suffixes (not share-class letters).
fn is_exchange_suffix(suffix: &str) -> bool {
    matches!(
        suffix,
        "TO" | "V" | "L" | "CN" | "NE" | "PA" | "DE" | "AS" | "SW" | "HK" | "AX" | "OL" | "ST"
            | "NY" | "NQ" | "OB" | "OTC"
    )
}

/// Broker statements use `BRK.B` / `BTCC.B`; Yahoo wants `BRK-B` / `BTCC-B.TO`.
fn is_share_class_suffix(suffix: &str) -> bool {
    let mut chars = suffix.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic()) && chars.next().is_none()
}

fn push_unique(out: &mut Vec<String>, symbol: String) {
    if !out.iter().any(|s| s == &symbol) {
        out.push(symbol);
    }
}

/// Candidate Yahoo symbols for a portfolio ticker.
/// - Bare Canadian ETFs like `VFV` / `QQC` need `.TO` / `.V` on Yahoo.
/// - Class shares like `BRK.B` / `HPS.A` need the dash form (`BRK-B`, `HPS-A.TO`).
pub fn yahoo_symbol_candidates(ticker: &str) -> Result<Vec<String>, QuoteError> {
    let t = normalize_ticker(ticker)?;
    let mut out = vec![t.clone()];

    if let Some((base, suffix)) = t.rsplit_once('.') {
        if is_share_class_suffix(suffix) && !base.is_empty() {
            let dashed = format!("{base}-{suffix}");
            push_unique(&mut out, dashed.clone());
            push_unique(&mut out, format!("{dashed}.TO"));
            push_unique(&mut out, format!("{dashed}.V"));
        } else if !is_exchange_suffix(suffix) {
            // Unknown dotted form — still try dash + Canadian suffixes.
            let dashed = format!("{base}-{suffix}");
            push_unique(&mut out, dashed.clone());
            push_unique(&mut out, format!("{dashed}.TO"));
        }
    } else {
        push_unique(&mut out, format!("{t}.TO"));
        push_unique(&mut out, format!("{t}.V"));
    }
    Ok(out)
}

/// When several Yahoo symbols resolve, prefer the price closest to book cost
/// (stops US microcaps hijacking Canadian ETF tickers like `QQC` / `HURA`).
fn pick_best_quote(quotes: Vec<StockQuote>, average_cost: Option<f64>) -> StockQuote {
    debug_assert!(!quotes.is_empty());
    if quotes.len() == 1 {
        return quotes.into_iter().next().expect("non-empty");
    }
    if let Some(cost) = average_cost.filter(|c| c.is_finite() && *c > 0.0) {
        return quotes
            .into_iter()
            .min_by(|a, b| {
                let da = (a.price / cost).ln().abs();
                let db = (b.price / cost).ln().abs();
                da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
            })
            .expect("non-empty");
    }
    quotes.into_iter().next().expect("non-empty")
}

fn chart_url(symbol: &str) -> Result<reqwest::Url, QuoteError> {
    let mut url = reqwest::Url::parse("https://query1.finance.yahoo.com/v8/finance/chart/")
        .expect("static Yahoo chart base URL");
    {
        let mut segments = url
            .path_segments_mut()
            .map_err(|_| QuoteError::InvalidSymbol(symbol.to_string()))?;
        // Ensure we append a single encoded path segment (not query injection).
        segments.pop_if_empty();
        segments.push(symbol);
    }
    url.query_pairs_mut()
        .append_pair("interval", "1d")
        .append_pair("range", "1d");
    Ok(url)
}

async fn fetch_one_yahoo_symbol(
    client: &reqwest::Client,
    symbol: &str,
) -> Result<(f64, String, String), QuoteError> {
    let url = chart_url(symbol)?;
    let resp = client
        .get(url)
        .header(
            reqwest::header::USER_AGENT,
            "Mozilla/5.0 (compatible; Chotu/0.1; +https://github.com/local/chotu)",
        )
        .timeout(std::time::Duration::from_secs(8))
        .send()
        .await?;

    if !resp.status().is_success() {
        return Err(QuoteError::NotFound(symbol.to_string()));
    }

    let body: ChartResponse = resp
        .json()
        .await
        .map_err(|_| QuoteError::BadPayload(symbol.to_string()))?;

    if body.chart.error.is_some() {
        return Err(QuoteError::NotFound(symbol.to_string()));
    }

    let meta = body
        .chart
        .result
        .and_then(|mut r| r.pop())
        .map(|r| r.meta)
        .ok_or_else(|| QuoteError::NotFound(symbol.to_string()))?;

    let price = meta
        .regular_market_price
        .filter(|p| p.is_finite() && *p > 0.0)
        .ok_or_else(|| QuoteError::NotFound(symbol.to_string()))?;

    let currency = meta
        .currency
        .filter(|c| !c.is_empty())
        .ok_or_else(|| QuoteError::BadPayload(symbol.to_string()))?;

    let resolved = meta.symbol.unwrap_or_else(|| symbol.to_string());
    Ok((price, currency.to_uppercase(), resolved))
}

/// Resolve a single portfolio ticker to a live Yahoo quote.
pub async fn fetch_stock_quote(
    client: &reqwest::Client,
    ticker: &str,
) -> Result<StockQuote, QuoteError> {
    fetch_stock_quote_near_cost(client, ticker, None).await
}

/// Resolve a ticker, disambiguating multiple Yahoo hits with `average_cost` when set.
pub async fn fetch_stock_quote_near_cost(
    client: &reqwest::Client,
    ticker: &str,
    average_cost: Option<f64>,
) -> Result<StockQuote, QuoteError> {
    let normalized = normalize_ticker(ticker)?;
    let candidates = yahoo_symbol_candidates(&normalized)?;

    let mut found = Vec::new();
    let mut last_err = QuoteError::NotFound(normalized.clone());
    for symbol in candidates {
        match fetch_one_yahoo_symbol(client, &symbol).await {
            Ok((price, currency, yahoo_symbol)) => {
                found.push(StockQuote {
                    ticker: normalized.clone(),
                    yahoo_symbol,
                    price,
                    currency,
                });
            }
            Err(e) => last_err = e,
        }
    }
    if found.is_empty() {
        return Err(last_err);
    }
    Ok(pick_best_quote(found, average_cost))
}

/// Fetch quotes for many tickers with bounded concurrency.
/// Missing symbols are omitted (caller falls back to book cost).
pub async fn fetch_stock_quotes(tickers: &[String]) -> Result<Vec<StockQuote>, QuoteError> {
    let keyed: Vec<(String, Option<f64>)> = tickers.iter().cloned().map(|t| (t, None)).collect();
    fetch_stock_quotes_near_cost(&keyed).await
}

/// Like [`fetch_stock_quotes`], but uses each holding's average cost to pick among
/// ambiguous Yahoo symbols (e.g. US `QQC` vs TSX `QQC.TO`).
pub async fn fetch_stock_quotes_near_cost(
    tickers: &[(String, Option<f64>)],
) -> Result<Vec<StockQuote>, QuoteError> {
    if tickers.is_empty() {
        return Ok(Vec::new());
    }

    let client = reqwest::Client::new();
    let mut quotes = Vec::with_capacity(tickers.len());
    let mut errors = Vec::new();
    let mut set: JoinSet<Result<StockQuote, QuoteError>> = JoinSet::new();
    let mut next = 0usize;

    while next < tickers.len() || !set.is_empty() {
        while set.len() < MAX_CONCURRENT_QUOTES && next < tickers.len() {
            let client = client.clone();
            let (ticker, average_cost) = tickers[next].clone();
            next += 1;
            set.spawn(async move {
                fetch_stock_quote_near_cost(&client, &ticker, average_cost).await
            });
        }

        let Some(joined) = set.join_next().await else {
            break;
        };
        match joined {
            Ok(Ok(q)) => quotes.push(q),
            Ok(Err(e)) => {
                eprintln!("Quote lookup failed: {}", e);
                errors.push(e);
            }
            Err(e) => {
                eprintln!("Quote task join failed: {}", e);
                errors.push(QuoteError::NotFound("join".to_string()));
            }
        }
    }

    if quotes.is_empty() && !errors.is_empty() {
        return Err(errors.remove(0));
    }

    Ok(quotes)
}

#[cfg(test)]
mod tests {
    use super::{
        chart_url, is_share_class_suffix, normalize_ticker, pick_best_quote,
        yahoo_symbol_candidates, StockQuote,
    };

    #[test]
    fn candidates_add_tsx_suffix_for_bare_symbols() {
        assert_eq!(
            yahoo_symbol_candidates("vfv").unwrap(),
            vec![
                "VFV".to_string(),
                "VFV.TO".to_string(),
                "VFV.V".to_string()
            ]
        );
    }

    #[test]
    fn candidates_keep_explicit_exchange_suffix() {
        assert_eq!(
            yahoo_symbol_candidates("VFV.TO").unwrap(),
            vec!["VFV.TO".to_string()]
        );
    }

    #[test]
    fn candidates_expand_class_shares_to_yahoo_dash_form() {
        assert_eq!(
            yahoo_symbol_candidates("BRK.B").unwrap(),
            vec![
                "BRK.B".to_string(),
                "BRK-B".to_string(),
                "BRK-B.TO".to_string(),
                "BRK-B.V".to_string()
            ]
        );
        assert_eq!(
            yahoo_symbol_candidates("BTCC.B").unwrap(),
            vec![
                "BTCC.B".to_string(),
                "BTCC-B".to_string(),
                "BTCC-B.TO".to_string(),
                "BTCC-B.V".to_string()
            ]
        );
        assert_eq!(
            yahoo_symbol_candidates("HPS.A").unwrap(),
            vec![
                "HPS.A".to_string(),
                "HPS-A".to_string(),
                "HPS-A.TO".to_string(),
                "HPS-A.V".to_string()
            ]
        );
        assert!(is_share_class_suffix("B"));
        assert!(!is_share_class_suffix("TO"));
    }

    #[test]
    fn pick_best_quote_prefers_price_near_book_cost() {
        let quotes = vec![
            StockQuote {
                ticker: "QQC".into(),
                yahoo_symbol: "QQC".into(),
                price: 24.46,
                currency: "USD".into(),
            },
            StockQuote {
                ticker: "QQC".into(),
                yahoo_symbol: "QQC.TO".into(),
                price: 49.25,
                currency: "CAD".into(),
            },
        ];
        let best = pick_best_quote(quotes, Some(40.321));
        assert_eq!(best.yahoo_symbol, "QQC.TO");
    }

    #[test]
    fn rejects_injection_characters() {
        assert!(normalize_ticker("AAPL?evil=1").is_err());
        assert!(normalize_ticker("VFV&x=1").is_err());
        assert!(normalize_ticker("../etc").is_err());
    }

    #[test]
    fn chart_url_encodes_path_segment() {
        let url = chart_url("BRK.B").unwrap();
        assert_eq!(url.path(), "/v8/finance/chart/BRK.B");
        assert_eq!(url.query(), Some("interval=1d&range=1d"));

        let url = chart_url("A B").unwrap();
        assert_eq!(url.path(), "/v8/finance/chart/A%20B");
        assert_eq!(url.query(), Some("interval=1d&range=1d"));
    }
}
