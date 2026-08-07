//! Finnhub company-profile lookups for research universe enrichment.
//! Free-tier friendly: profile2 only, modest concurrency.

use serde::Deserialize;
use thiserror::Error;
use tokio::task::JoinSet;

const MAX_CONCURRENT: usize = 4;
const PROFILE_URL: &str = "https://finnhub.io/api/v1/stock/profile2";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapBand {
    Micro,
    Small,
    Mid,
    Large,
}

impl CapBand {
    pub fn as_str(self) -> &'static str {
        match self {
            CapBand::Micro => "Micro",
            CapBand::Small => "Small",
            CapBand::Mid => "Mid",
            CapBand::Large => "Large",
        }
    }
}

/// Map Finnhub `marketCapitalization` (USD millions) to a size band.
pub fn cap_band_from_millions(market_cap_m: f64) -> CapBand {
    if !market_cap_m.is_finite() || market_cap_m < 0.0 {
        return CapBand::Large;
    }
    if market_cap_m < 300.0 {
        CapBand::Micro
    } else if market_cap_m < 2000.0 {
        CapBand::Small
    } else if market_cap_m < 10_000.0 {
        CapBand::Mid
    } else {
        CapBand::Large
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct CompanyProfile {
    /// Symbol that resolved (may include exchange suffix).
    pub symbol: String,
    pub name: String,
    pub exchange: Option<String>,
    /// Market capitalization in USD millions.
    pub market_cap_m: f64,
    pub finnhub_industry: Option<String>,
    pub cap_band: CapBand,
}

#[derive(Debug, Error)]
pub enum FinnhubError {
    #[error("FINNHUB_API_KEY is not set")]
    MissingApiKey,
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("Finnhub HTTP {status} for {symbol}")]
    HttpStatus { symbol: String, status: u16 },
    #[error("invalid ticker symbol: {0}")]
    InvalidSymbol(String),
    #[error("no Finnhub profile for {0}")]
    NotFound(String),
    #[error("Finnhub returned an unexpected payload for {0}")]
    BadPayload(String),
}

#[derive(Debug, Clone)]
pub struct FinnhubClient {
    http: reqwest::Client,
    api_key: String,
}

impl FinnhubClient {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            http: reqwest::Client::new(),
            api_key: api_key.into(),
        }
    }

    pub fn from_env() -> Result<Self, FinnhubError> {
        let api_key = std::env::var("FINNHUB_API_KEY").map_err(|_| FinnhubError::MissingApiKey)?;
        if api_key.trim().is_empty() {
            return Err(FinnhubError::MissingApiKey);
        }
        Ok(Self::new(api_key))
    }

    /// Resolve a ticker via profile2, trying exchange-suffixed candidates when needed.
    pub async fn lookup_profile(&self, ticker: &str) -> Result<CompanyProfile, FinnhubError> {
        let candidates = finnhub_symbol_candidates(ticker)?;
        let mut last_err = FinnhubError::NotFound(ticker.to_string());
        for symbol in candidates {
            match self.fetch_profile2(&symbol).await {
                Ok(profile) => return Ok(profile),
                Err(e) => last_err = e,
            }
            // Gentle spacing under free-tier 60 req/min.
            tokio::time::sleep(std::time::Duration::from_millis(120)).await;
        }
        Err(last_err)
    }

    /// Look up many tickers with bounded concurrency.
    pub async fn lookup_profiles(
        &self,
        tickers: &[String],
    ) -> Vec<(String, Result<CompanyProfile, FinnhubError>)> {
        let mut set = JoinSet::new();
        let mut results = Vec::with_capacity(tickers.len());
        let mut iter = tickers.iter().cloned();

        for _ in 0..MAX_CONCURRENT.min(tickers.len()) {
            if let Some(ticker) = iter.next() {
                let client = self.clone();
                set.spawn(async move {
                    let result = client.lookup_profile(&ticker).await;
                    (ticker, result)
                });
            }
        }

        while let Some(joined) = set.join_next().await {
            match joined {
                Ok(pair) => results.push(pair),
                Err(e) => eprintln!("Finnhub: join error: {e}"),
            }
            if let Some(ticker) = iter.next() {
                let client = self.clone();
                set.spawn(async move {
                    let result = client.lookup_profile(&ticker).await;
                    (ticker, result)
                });
            }
        }

        results
    }

    async fn fetch_profile2(&self, symbol: &str) -> Result<CompanyProfile, FinnhubError> {
        let resp = self
            .http
            .get(PROFILE_URL)
            .query(&[("symbol", symbol), ("token", self.api_key.as_str())])
            .timeout(std::time::Duration::from_secs(10))
            .send()
            .await?;

        let status = resp.status();
        if status.as_u16() == 404 {
            return Err(FinnhubError::NotFound(symbol.to_string()));
        }
        if !status.is_success() {
            return Err(FinnhubError::HttpStatus {
                symbol: symbol.to_string(),
                status: status.as_u16(),
            });
        }

        let body: Profile2Response = resp
            .json()
            .await
            .map_err(|_| FinnhubError::BadPayload(symbol.to_string()))?;

        // Empty object `{}` when symbol is unknown.
        let name = body.name.filter(|s| !s.trim().is_empty());
        let ticker = body.ticker.filter(|s| !s.trim().is_empty());
        let market_cap_m = body.market_capitalization.filter(|m| m.is_finite() && *m > 0.0);

        let (Some(name), Some(resolved), Some(market_cap_m)) = (name, ticker, market_cap_m) else {
            return Err(FinnhubError::NotFound(symbol.to_string()));
        };

        Ok(CompanyProfile {
            symbol: resolved,
            name,
            exchange: body
                .exchange
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
            market_cap_m,
            finnhub_industry: body.finnhub_industry.filter(|s| !s.is_empty()),
            cap_band: cap_band_from_millions(market_cap_m),
        })
    }
}

/// Candidate symbols for Finnhub (bare + common exchange suffixes).
pub fn finnhub_symbol_candidates(ticker: &str) -> Result<Vec<String>, FinnhubError> {
    let t = ticker.trim().trim_start_matches('$').to_ascii_uppercase();
    if t.is_empty() || t.len() > 32 {
        return Err(FinnhubError::InvalidSymbol(ticker.to_string()));
    }
    if !t
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-'))
    {
        return Err(FinnhubError::InvalidSymbol(ticker.to_string()));
    }

    let mut out = vec![t.clone()];
    if !t.contains('.') {
        out.push(format!("{t}.TO"));
        out.push(format!("{t}.L"));
        out.push(format!("{t}.V")); // TSXV on some feeds
    }
    Ok(out)
}

#[derive(Debug, Deserialize)]
struct Profile2Response {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    ticker: Option<String>,
    #[serde(default)]
    exchange: Option<String>,
    #[serde(rename = "marketCapitalization", default)]
    market_capitalization: Option<f64>,
    #[serde(rename = "finnhubIndustry", default)]
    finnhub_industry: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cap_band_from_millions() {
        assert_eq!(cap_band_from_millions(50.0), CapBand::Micro);
        assert_eq!(cap_band_from_millions(299.9), CapBand::Micro);
        assert_eq!(cap_band_from_millions(300.0), CapBand::Small);
        assert_eq!(cap_band_from_millions(1999.0), CapBand::Small);
        assert_eq!(cap_band_from_millions(2000.0), CapBand::Mid);
        assert_eq!(cap_band_from_millions(9999.0), CapBand::Mid);
        assert_eq!(cap_band_from_millions(10_000.0), CapBand::Large);
        assert_eq!(cap_band_from_millions(500_000.0), CapBand::Large);
    }

    #[test]
    fn test_finnhub_symbol_candidates() {
        let c = finnhub_symbol_candidates("png").unwrap();
        assert_eq!(c[0], "PNG");
        assert!(c.contains(&"PNG.TO".to_string()));
        assert!(c.contains(&"PNG.L".to_string()));

        let already = finnhub_symbol_candidates("JDG.L").unwrap();
        assert_eq!(already, vec!["JDG.L".to_string()]);
    }

    #[test]
    fn test_parse_profile2_fixture() {
        let json = r#"{
            "country": "US",
            "currency": "USD",
            "exchange": "NASDAQ NMS - GLOBAL MARKET",
            "finnhubIndustry": "Technology",
            "ipo": "1980-12-12",
            "marketCapitalization": 450.5,
            "name": "Example Micro Inc",
            "ticker": "EXMP",
            "weburl": "https://example.com"
        }"#;
        let body: Profile2Response = serde_json::from_str(json).unwrap();
        assert_eq!(body.ticker.as_deref(), Some("EXMP"));
        assert_eq!(body.market_capitalization, Some(450.5));
        assert_eq!(
            cap_band_from_millions(body.market_capitalization.unwrap()),
            CapBand::Small
        );
    }
}
