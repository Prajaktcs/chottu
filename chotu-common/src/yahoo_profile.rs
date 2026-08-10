//! Yahoo Finance company profiles (market cap) for research enrichment.
//! Free Finnhub is US-only; international suffixes (`.TO` / `.V` / `.L`, …) 403 there.
//! Uses Yahoo's unofficial quote API (cookie + crumb). Personal use only.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use serde::Deserialize;
use tokio::sync::Mutex;

use crate::finnhub::{cap_band_from_millions, CompanyProfile};
use crate::quotes::{normalize_ticker, QuoteError};

const YAHOO_UA: &str = "Mozilla/5.0 (compatible; Chotu/0.1; +https://github.com/local/chotu)";
const QUOTE_BATCH_SIZE: usize = 20;

/// Exchanges Finnhub free-tier typically blocks; prefer Yahoo for market-cap banding.
pub fn prefers_yahoo_profile(ticker: &str) -> bool {
    let t = ticker.trim().trim_start_matches('$').to_ascii_uppercase();
    [
        ".TO", ".V", ".L", ".CN", ".NE", ".PA", ".DE", ".AS", ".SW", ".HK", ".AX", ".OL", ".ST",
    ]
    .iter()
    .any(|suffix| t.ends_with(suffix))
}

/// Candidate Yahoo symbols for profile / market-cap lookup.
pub fn yahoo_profile_candidates(ticker: &str) -> Result<Vec<String>, QuoteError> {
    let t = normalize_ticker(ticker)?;
    let mut out = vec![t.clone()];
    if !t.contains('.') {
        out.push(format!("{t}.TO"));
        out.push(format!("{t}.V"));
        out.push(format!("{t}.L"));
    }
    Ok(out)
}

/// Map Yahoo quote currency to the unit Yahoo uses for `marketCap`.
/// LSE quotes often report price in `GBp` (pence) while `marketCap` is in GBP.
pub fn market_cap_currency_code(currency: &str) -> String {
    match currency.trim().to_ascii_uppercase().as_str() {
        "GBP" | "GBX" => "GBP".to_string(),
        "ILA" => "ILS".to_string(),
        "ZAC" => "ZAR".to_string(),
        other => other.to_string(),
    }
}

/// Convert Yahoo absolute market cap into USD millions using `rates` from `latest/USD`
/// (units of foreign currency per 1 USD).
pub fn yahoo_market_cap_usd_millions(
    market_cap: f64,
    currency: &str,
    usd_rates: &HashMap<String, f64>,
) -> Option<f64> {
    if !market_cap.is_finite() || market_cap <= 0.0 {
        return None;
    }
    let ccy = market_cap_currency_code(currency);
    if ccy == "USD" {
        return Some(market_cap / 1_000_000.0);
    }
    let rate = usd_rates
        .get(&ccy)
        .copied()
        .filter(|r| r.is_finite() && *r > 0.0)?;
    Some((market_cap / rate) / 1_000_000.0)
}

#[derive(Debug, Deserialize)]
struct QuoteApiResponse {
    #[serde(rename = "quoteResponse")]
    quote_response: QuoteApiBody,
}

#[derive(Debug, Deserialize)]
struct QuoteApiBody {
    result: Option<Vec<YahooQuoteRow>>,
    error: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize, Clone)]
struct YahooQuoteRow {
    symbol: Option<String>,
    #[serde(rename = "shortName")]
    short_name: Option<String>,
    #[serde(rename = "longName")]
    long_name: Option<String>,
    #[serde(rename = "fullExchangeName")]
    full_exchange_name: Option<String>,
    currency: Option<String>,
    #[serde(rename = "marketCap")]
    market_cap: Option<f64>,
}

/// Cookie + crumb Yahoo client for market-cap / profile lookups.
#[derive(Clone)]
pub struct YahooProfileClient {
    http: reqwest::Client,
    crumb: Arc<Mutex<Option<String>>>,
}

impl YahooProfileClient {
    pub fn new() -> Result<Self, QuoteError> {
        let http = reqwest::Client::builder()
            .cookie_store(true)
            .user_agent(YAHOO_UA)
            .timeout(std::time::Duration::from_secs(12))
            .build()?;
        Ok(Self {
            http,
            crumb: Arc::new(Mutex::new(None)),
        })
    }

    async fn ensure_crumb(&self) -> Result<String, QuoteError> {
        {
            let guard = self.crumb.lock().await;
            if let Some(c) = guard.as_ref() {
                if !c.is_empty() {
                    return Ok(c.clone());
                }
            }
        }

        // Establishes the A3 cookie Yahoo expects before getcrumb.
        let _ = self.http.get("https://fc.yahoo.com").send().await;

        let resp = self
            .http
            .get("https://query1.finance.yahoo.com/v1/test/getcrumb")
            .send()
            .await?;
        let status = resp.status();
        let crumb = resp.text().await?.trim().to_string();
        if !status.is_success() || crumb.is_empty() || crumb.contains(' ') || crumb.len() > 128 {
            return Err(QuoteError::Auth(format!(
                "getcrumb HTTP {status}: {}",
                crumb.chars().take(80).collect::<String>()
            )));
        }

        let mut guard = self.crumb.lock().await;
        *guard = Some(crumb.clone());
        Ok(crumb)
    }

    async fn invalidate_crumb(&self) {
        let mut guard = self.crumb.lock().await;
        *guard = None;
    }

    async fn fetch_quote_rows(
        &self,
        symbols: &[String],
    ) -> Result<HashMap<String, YahooQuoteRow>, QuoteError> {
        if symbols.is_empty() {
            return Ok(HashMap::new());
        }

        let mut out = HashMap::new();
        for chunk in symbols.chunks(QUOTE_BATCH_SIZE) {
            let rows = self.fetch_quote_rows_chunk(chunk).await?;
            out.extend(rows);
        }
        Ok(out)
    }

    async fn fetch_quote_rows_chunk(
        &self,
        symbols: &[String],
    ) -> Result<HashMap<String, YahooQuoteRow>, QuoteError> {
        let joined = symbols.join(",");
        for attempt in 0..2u8 {
            let crumb = self.ensure_crumb().await?;
            let resp = self
                .http
                .get("https://query1.finance.yahoo.com/v7/finance/quote")
                .query(&[
                    ("symbols", joined.as_str()),
                    (
                        "fields",
                        "symbol,shortName,longName,marketCap,currency,fullExchangeName",
                    ),
                    ("crumb", crumb.as_str()),
                ])
                .send()
                .await?;

            let status = resp.status();
            if status.as_u16() == 401 || status.as_u16() == 403 {
                self.invalidate_crumb().await;
                if attempt == 0 {
                    continue;
                }
                return Err(QuoteError::Auth(format!("quote HTTP {status}")));
            }
            if !status.is_success() {
                return Err(QuoteError::Auth(format!("quote HTTP {status}")));
            }

            let body: QuoteApiResponse = resp
                .json()
                .await
                .map_err(|_| QuoteError::BadPayload(joined.clone()))?;
            if body.quote_response.error.is_some() {
                self.invalidate_crumb().await;
                if attempt == 0 {
                    continue;
                }
                return Err(QuoteError::BadPayload(joined));
            }

            let mut map = HashMap::new();
            for row in body.quote_response.result.unwrap_or_default() {
                if let Some(sym) = row.symbol.clone() {
                    map.insert(sym.to_ascii_uppercase(), row);
                }
            }
            return Ok(map);
        }
        Err(QuoteError::Auth("quote auth retry exhausted".into()))
    }

    /// Resolve ticker → [`CompanyProfile`] with `market_cap_m` in **USD millions**.
    pub async fn lookup_profile(
        &self,
        ticker: &str,
        usd_rates: &HashMap<String, f64>,
    ) -> Result<CompanyProfile, QuoteError> {
        let candidates = yahoo_profile_candidates(ticker)?;
        let rows = self.fetch_quote_rows(&candidates).await?;

        let mut last_err = QuoteError::NotFound(ticker.to_string());
        for symbol in candidates {
            let Some(row) = rows.get(&symbol) else {
                last_err = QuoteError::NotFound(symbol);
                continue;
            };
            match row_to_company_profile(row, usd_rates) {
                Ok(profile) => return Ok(profile),
                Err(e) => last_err = e,
            }
        }
        Err(last_err)
    }

    /// Look up many tickers with one crumb session and batched quote calls.
    pub async fn lookup_profiles(
        &self,
        tickers: &[String],
        usd_rates: &HashMap<String, f64>,
    ) -> Vec<(String, Result<CompanyProfile, QuoteError>)> {
        let mut candidate_map: HashMap<String, Vec<String>> = HashMap::new();
        let mut all_symbols: Vec<String> = Vec::new();
        let mut seen = HashSet::new();

        for ticker in tickers {
            match yahoo_profile_candidates(ticker) {
                Ok(cands) => {
                    for c in &cands {
                        if seen.insert(c.clone()) {
                            all_symbols.push(c.clone());
                        }
                    }
                    candidate_map.insert(ticker.clone(), cands);
                }
                Err(_) => {
                    candidate_map.insert(ticker.clone(), Vec::new());
                }
            }
        }

        let rows = match self.fetch_quote_rows(&all_symbols).await {
            Ok(r) => r,
            Err(e) => {
                let msg = e.to_string();
                return tickers
                    .iter()
                    .cloned()
                    .map(|t| (t, Err(QuoteError::Auth(msg.clone()))))
                    .collect();
            }
        };

        tickers
            .iter()
            .map(|ticker| {
                let normalized = match normalize_ticker(ticker) {
                    Ok(t) => t,
                    Err(e) => return (ticker.clone(), Err(e)),
                };
                let cands = candidate_map
                    .get(ticker)
                    .cloned()
                    .unwrap_or_else(|| vec![normalized.clone()]);
                if cands.is_empty() {
                    return (
                        ticker.clone(),
                        Err(QuoteError::InvalidSymbol(ticker.clone())),
                    );
                }
                let mut last_err = QuoteError::NotFound(normalized);
                for symbol in cands {
                    let Some(row) = rows.get(&symbol) else {
                        last_err = QuoteError::NotFound(symbol);
                        continue;
                    };
                    match row_to_company_profile(row, usd_rates) {
                        Ok(profile) => return (ticker.clone(), Ok(profile)),
                        Err(e) => last_err = e,
                    }
                }
                (ticker.clone(), Err(last_err))
            })
            .collect()
    }
}

fn row_to_company_profile(
    row: &YahooQuoteRow,
    usd_rates: &HashMap<String, f64>,
) -> Result<CompanyProfile, QuoteError> {
    let symbol = row
        .symbol
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| QuoteError::BadPayload("missing symbol".into()))?
        .to_ascii_uppercase();

    let name = row
        .long_name
        .as_ref()
        .or(row.short_name.as_ref())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| QuoteError::NotFound(symbol.clone()))?;

    let currency = row
        .currency
        .as_deref()
        .unwrap_or("USD")
        .trim()
        .to_string();

    let market_cap = row
        .market_cap
        .filter(|m| m.is_finite() && *m > 0.0)
        .ok_or_else(|| QuoteError::NotFound(symbol.clone()))?;

    let market_cap_m = yahoo_market_cap_usd_millions(market_cap, &currency, usd_rates).ok_or_else(
        || {
            QuoteError::BadPayload(format!(
                "no FX rate to convert {currency} market cap for {symbol}"
            ))
        },
    )?;

    Ok(CompanyProfile {
        symbol,
        name,
        exchange: row
            .full_exchange_name
            .as_ref()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()),
        market_cap_m,
        finnhub_industry: None,
        cap_band: cap_band_from_millions(market_cap_m),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn prefers_yahoo_for_international_suffixes() {
        assert!(prefers_yahoo_profile("KITS.TO"));
        assert!(prefers_yahoo_profile("png.v"));
        assert!(prefers_yahoo_profile("SDI.L"));
        assert!(!prefers_yahoo_profile("AAPL"));
        assert!(!prefers_yahoo_profile("BRK.B"));
    }

    #[test]
    fn profile_candidates_add_common_suffixes() {
        assert_eq!(
            yahoo_profile_candidates("png").unwrap(),
            vec![
                "PNG".to_string(),
                "PNG.TO".to_string(),
                "PNG.V".to_string(),
                "PNG.L".to_string()
            ]
        );
        assert_eq!(
            yahoo_profile_candidates("KITS.TO").unwrap(),
            vec!["KITS.TO".to_string()]
        );
    }

    #[test]
    fn gbp_pence_currency_maps_for_market_cap() {
        assert_eq!(market_cap_currency_code("GBp"), "GBP");
        assert_eq!(market_cap_currency_code("gbx"), "GBP");
        assert_eq!(market_cap_currency_code("CAD"), "CAD");
    }

    #[test]
    fn converts_yahoo_market_cap_to_usd_millions() {
        let mut rates = HashMap::new();
        rates.insert("CAD".to_string(), 1.37);
        rates.insert("GBP".to_string(), 0.79);

        let cad = yahoo_market_cap_usd_millions(484_435_360.0, "CAD", &rates).unwrap();
        assert!((cad - (484_435_360.0 / 1.37 / 1_000_000.0)).abs() < 1e-6);

        let gbp = yahoo_market_cap_usd_millions(95_161_552.0, "GBp", &rates).unwrap();
        assert!((gbp - (95_161_552.0 / 0.79 / 1_000_000.0)).abs() < 1e-6);

        let usd = yahoo_market_cap_usd_millions(2_000_000_000.0, "USD", &rates).unwrap();
        assert!((usd - 2000.0).abs() < 1e-9);
    }
}
