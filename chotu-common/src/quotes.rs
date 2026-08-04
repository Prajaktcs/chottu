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

/// Candidate Yahoo symbols for a portfolio ticker.
/// Bare Canadian ETFs like `VFV` / `QQC` need the `.TO` TSX suffix on Yahoo.
pub fn yahoo_symbol_candidates(ticker: &str) -> Result<Vec<String>, QuoteError> {
    let t = normalize_ticker(ticker)?;
    let mut out = vec![t.clone()];
    if !t.contains('.') {
        out.push(format!("{t}.TO"));
    }
    Ok(out)
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
    let normalized = normalize_ticker(ticker)?;
    let candidates = yahoo_symbol_candidates(&normalized)?;

    let mut last_err = QuoteError::NotFound(normalized.clone());
    for symbol in candidates {
        match fetch_one_yahoo_symbol(client, &symbol).await {
            Ok((price, currency, yahoo_symbol)) => {
                return Ok(StockQuote {
                    ticker: normalized,
                    yahoo_symbol,
                    price,
                    currency,
                });
            }
            Err(e) => last_err = e,
        }
    }
    Err(last_err)
}

/// Fetch quotes for many tickers with bounded concurrency.
/// Missing symbols are omitted (caller falls back to book cost).
pub async fn fetch_stock_quotes(tickers: &[String]) -> Result<Vec<StockQuote>, QuoteError> {
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
            let ticker = tickers[next].clone();
            next += 1;
            set.spawn(async move { fetch_stock_quote(&client, &ticker).await });
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
    use super::{chart_url, normalize_ticker, yahoo_symbol_candidates};

    #[test]
    fn candidates_add_tsx_suffix_for_bare_symbols() {
        assert_eq!(
            yahoo_symbol_candidates("vfv").unwrap(),
            vec!["VFV".to_string(), "VFV.TO".to_string()]
        );
    }

    #[test]
    fn candidates_keep_explicit_exchange_suffix() {
        assert_eq!(
            yahoo_symbol_candidates("VFV.TO").unwrap(),
            vec!["VFV.TO".to_string()]
        );
        assert_eq!(
            yahoo_symbol_candidates("BRK.B").unwrap(),
            vec!["BRK.B".to_string()]
        );
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
