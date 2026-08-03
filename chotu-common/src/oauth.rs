use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum OAuthError {
    #[error("HTTP client error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("Failed to refresh token: status code {0}, response: {1}")]
    TokenExchangeFailed(u16, String),
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct GoogleTokenResponse {
    pub access_token: String,
    pub expires_in: u64,
    pub token_type: String,
    pub scope: Option<String>,
}

/// Refreshes a Google/Generic OAuth2 token using client credentials and a refresh token.
pub async fn refresh_oauth2_token(
    client_id: &str,
    client_secret: &str,
    refresh_token: &str,
) -> Result<GoogleTokenResponse, OAuthError> {
    let client = reqwest::Client::new();
    let params = [
        ("client_id", client_id),
        ("client_secret", client_secret),
        ("refresh_token", refresh_token),
        ("grant_type", "refresh_token"),
    ];

    let response = client
        .post("https://oauth2.googleapis.com/token")
        .form(&params)
        .send()
        .await?;

    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(OAuthError::TokenExchangeFailed(status.as_u16(), body));
    }

    let parsed = response.json::<GoogleTokenResponse>().await?;
    Ok(parsed)
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct FitbitTokenResponse {
    pub access_token: String,
    pub expires_in: u64,
    pub token_type: String,
    pub refresh_token: String,
    pub scope: String,
    pub user_id: String,
}

/// Refreshes a Fitbit OAuth2 token using client credentials and a refresh token.
pub async fn refresh_fitbit_token(
    client_id: &str,
    client_secret: &str,
    refresh_token: &str,
) -> Result<FitbitTokenResponse, OAuthError> {
    let client = reqwest::Client::new();
    let params = [
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
    ];

    let response = client
        .post("https://api.fitbit.com/oauth2/token")
        .basic_auth(client_id, Some(client_secret))
        .form(&params)
        .send()
        .await?;

    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(OAuthError::TokenExchangeFailed(status.as_u16(), body));
    }

    let parsed = response.json::<FitbitTokenResponse>().await?;
    Ok(parsed)
}

/// Saves the new Google Health refresh token to the `.env` file and updates the current process environment variable.
///
/// Legacy single-account path (`FITBIT_REFRESH_TOKEN`). Prefer
/// [`save_health_refresh_token`] for per-member tokens.
pub fn save_google_health_refresh_token(new_token: &str) -> Result<(), std::io::Error> {
    std::env::set_var("FITBIT_REFRESH_TOKEN", new_token);
    update_env_file(".env", "FITBIT_REFRESH_TOKEN", new_token)
}

/// Saves a per-member Google Health refresh token (e.g. `HEALTH_REFRESH_TOKEN_ALEX`).
///
/// Also writes `FITBIT_REFRESH_TOKEN` when `member_id` is the primary member so
/// older single-token readers keep working.
pub fn save_health_refresh_token(
    member_id: &str,
    new_token: &str,
    is_primary: bool,
) -> Result<(), std::io::Error> {
    let key = format!("HEALTH_REFRESH_TOKEN_{}", member_id.to_uppercase());
    std::env::set_var(&key, new_token);
    update_env_file(".env", &key, new_token)?;
    if is_primary {
        save_google_health_refresh_token(new_token)?;
    }
    Ok(())
}

fn update_env_file(path: &str, key: &str, value: &str) -> Result<(), std::io::Error> {
    update_env_file_pub(path, key, value)
}

/// Public version of `update_env_file` for use by other modules (e.g., coordinator).
pub fn update_env_file_pub(path: &str, key: &str, value: &str) -> Result<(), std::io::Error> {
    use std::fs::File;
    use std::io::{Read, Write};

    if !std::path::Path::new(path).exists() {
        let mut file = File::create(path)?;
        writeln!(file, "{}={}", key, value)?;
        return Ok(());
    }

    let mut content = String::new();
    {
        let mut file = File::open(path)?;
        file.read_to_string(&mut content)?;
    }

    let mut lines: Vec<String> = content.lines().map(|s| s.to_string()).collect();
    let mut found = false;
    let prefix = format!("{}=", key);

    for line in &mut lines {
        if line.starts_with(&prefix) {
            *line = format!("{}={}", key, value);
            found = true;
            break;
        }
    }

    if !found {
        lines.push(format!("{}={}", key, value));
    }

    let mut file = File::create(path)?;
    for line in lines {
        writeln!(file, "{}", line)?;
    }

    Ok(())
}

/// Formats the XOAUTH2 SASL payload:
/// `user={email}\x01auth=Bearer {access_token}\x01\x01`
pub fn format_xoauth2_string(email: &str, access_token: &str) -> String {
    format!("user={}\x01auth=Bearer {}\x01\x01", email, access_token)
}

/// Saves the new Google refresh token to the `.env` file and updates the current process environment variable.
pub fn save_google_refresh_token(new_token: &str) -> Result<(), std::io::Error> {
    std::env::set_var("CHOTU_OAUTH_REFRESH_TOKEN", new_token);
    update_env_file(".env", "CHOTU_OAUTH_REFRESH_TOKEN", new_token)
}

/// Saves a per-member Google Calendar refresh token (e.g. `CALENDAR_REFRESH_TOKEN_ALEX`).
pub fn save_calendar_refresh_token(member_id: &str, new_token: &str) -> Result<(), std::io::Error> {
    let key = format!("CALENDAR_REFRESH_TOKEN_{}", member_id.to_uppercase());
    std::env::set_var(&key, new_token);
    update_env_file(".env", &key, new_token)
}

/// Exchanges the Fitbit authorization code for access and refresh tokens.
pub async fn exchange_fitbit_code(
    client_id: &str,
    client_secret: &str,
    code: &str,
    redirect_uri: &str,
) -> Result<FitbitTokenResponse, OAuthError> {
    let client = reqwest::Client::new();
    let params = [
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", redirect_uri),
    ];

    let response = client
        .post("https://api.fitbit.com/oauth2/token")
        .basic_auth(client_id, Some(client_secret))
        .form(&params)
        .send()
        .await?;

    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(OAuthError::TokenExchangeFailed(status.as_u16(), body));
    }

    let parsed = response.json::<FitbitTokenResponse>().await?;
    Ok(parsed)
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct GoogleInitialTokenResponse {
    pub access_token: String,
    pub expires_in: u64,
    pub token_type: String,
    pub refresh_token: String,
    pub scope: Option<String>,
}

/// Exchanges the Google authorization code for access and refresh tokens.
pub async fn exchange_google_code(
    client_id: &str,
    client_secret: &str,
    code: &str,
    redirect_uri: &str,
) -> Result<GoogleInitialTokenResponse, OAuthError> {
    let client = reqwest::Client::new();
    let params = [
        ("client_id", client_id),
        ("client_secret", client_secret),
        ("code", code),
        ("redirect_uri", redirect_uri),
        ("grant_type", "authorization_code"),
    ];

    let response = client
        .post("https://oauth2.googleapis.com/token")
        .form(&params)
        .send()
        .await?;

    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(OAuthError::TokenExchangeFailed(status.as_u16(), body));
    }

    let parsed = response.json::<GoogleInitialTokenResponse>().await?;
    Ok(parsed)
}

/// Starts a temporary TCP redirect listener on the given port to capture the OAuth authorization code.
pub async fn start_redirect_listener(port: u16) -> Result<String, anyhow::Error> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    // Try binding to IPv6 dual-stack (accepts both IPv6 and IPv4 connections on macOS/Linux),
    // and fallback to IPv4-only if binding to [::] fails.
    let listener = match TcpListener::bind(format!("[::]:{}", port)).await {
        Ok(l) => l,
        Err(_) => TcpListener::bind(format!("127.0.0.1:{}", port)).await?,
    };

    loop {
        if let Ok((mut stream, _)) = listener.accept().await {
            let mut buffer = [0; 4096];
            let bytes_read = match stream.read(&mut buffer).await {
                Ok(n) => n,
                Err(_) => continue,
            };

            if bytes_read == 0 {
                continue;
            }

            let request = String::from_utf8_lossy(&buffer[..bytes_read]);

            // Check if user denied access or another OAuth error occurred
            if let Some(error) = extract_error_from_request(&request) {
                let body = format!(
                    "<html>\
                    <head><style>\
                        body {{ font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, Helvetica, Arial, sans-serif; text-align: center; padding-top: 60px; background: #0f172a; color: #f8fafc; }}\
                        .container {{ max-width: 500px; margin: 0 auto; background: #1e293b; padding: 40px; border-radius: 12px; box-shadow: 0 4px 6px -1px rgb(0 0 0 / 0.1); }}\
                        h1 {{ color: #ef4444; margin-bottom: 16px; }}\
                        p {{ color: #94a3b8; font-size: 16px; line-height: 1.5; }}\
                    </style></head>\
                    <body>\
                        <div class='container'>\
                            <h1>✗ Authentication Failed</h1>\
                            <p>OAuth flow returned an error: <code>{}</code>. You can close this tab and try again.</p>\
                        </div>\
                    </body>\
                    </html>",
                    error
                );
                let response = format!(
                    "HTTP/1.1 400 Bad Request\r\nContent-Length: {}\r\nContent-Type: text/html\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.flush().await;
                return Err(anyhow::anyhow!("OAuth authorization error: {}", error));
            }

            // Try to extract the authorization code
            if let Some(code) = extract_code_from_request(&request) {
                let body = "<html>\
                    <head><style>\
                        body { font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, Helvetica, Arial, sans-serif; text-align: center; padding-top: 60px; background: #0f172a; color: #f8fafc; }\
                        .container { max-width: 500px; margin: 0 auto; background: #1e293b; padding: 40px; border-radius: 12px; box-shadow: 0 4px 6px -1px rgb(0 0 0 / 0.1); }\
                        h1 { color: #10b981; margin-bottom: 16px; }\
                        p { color: #94a3b8; font-size: 16px; line-height: 1.5; }\
                    </style></head>\
                    <body>\
                        <div class='container'>\
                            <h1>✓ Authentication Successful!</h1>\
                            <p>You have successfully authorized the application. You can close this browser tab now and return to your agent.</p>\
                        </div>\
                    </body>\
                    </html>";
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: text/html\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.flush().await;
                return Ok(code);
            }

            // For non-oauth/helper requests, respond to avoid browser hangs
            if request.contains("GET /favicon.ico") {
                let response = "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.flush().await;
            } else if request.starts_with("GET ") {
                let body = "<html><body><h1>Authentication Pending</h1><p>Waiting for authorization code...</p></body></html>";
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: text/html\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.flush().await;
            }
        }
    }
}

fn extract_error_from_request(request: &str) -> Option<String> {
    let first_line = request.lines().next()?;
    let parts: Vec<&str> = first_line.split_whitespace().collect();
    if parts.len() < 2 {
        return None;
    }
    let path = parts[1];
    let query_start = path.find('?')?;
    let query = &path[query_start + 1..];
    for param in query.split('&') {
        let mut kv = param.split('=');
        if let (Some(k), Some(v)) = (kv.next(), kv.next()) {
            if k == "error" {
                return Some(v.to_string());
            }
        }
    }
    None
}

fn extract_code_from_request(request: &str) -> Option<String> {
    let first_line = request.lines().next()?;
    let parts: Vec<&str> = first_line.split_whitespace().collect();
    if parts.len() < 2 {
        return None;
    }
    let path = parts[1];
    let query_start = path.find('?')?;
    let query = &path[query_start + 1..];
    for param in query.split('&') {
        let mut kv = param.split('=');
        if let (Some(k), Some(v)) = (kv.next(), kv.next()) {
            if k == "code" {
                return Some(v.to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_xoauth2_string() {
        let email = "praj@example.com";
        let token = "ya29.abcdefg";
        let formatted = format_xoauth2_string(email, token);
        assert_eq!(
            formatted,
            "user=praj@example.com\x01auth=Bearer ya29.abcdefg\x01\x01"
        );
    }

    #[test]
    fn test_parse_token_response() {
        let raw_json =
            r#"{"access_token": "ya29.12345", "expires_in": 3600, "token_type": "Bearer"}"#;
        let parsed: Result<GoogleTokenResponse, _> = serde_json::from_str(raw_json);
        assert!(parsed.is_ok());
        let res = parsed.unwrap();
        assert_eq!(res.access_token, "ya29.12345");
        assert_eq!(res.expires_in, 3600);
        assert_eq!(res.token_type, "Bearer");
    }

    #[test]
    fn test_parse_fitbit_token_response() {
        let raw_json = r#"{"access_token": "fitbit123", "expires_in": 28800, "token_type": "Bearer", "refresh_token": "ref456", "scope": "nutrition", "user_id": "usr789"}"#;
        let parsed: Result<FitbitTokenResponse, _> = serde_json::from_str(raw_json);
        assert!(parsed.is_ok());
        let res = parsed.unwrap();
        assert_eq!(res.access_token, "fitbit123");
        assert_eq!(res.refresh_token, "ref456");
        assert_eq!(res.user_id, "usr789");
    }

    #[test]
    fn test_update_env_file() {
        use std::io::Read;
        use tempfile::NamedTempFile;

        let temp_file = NamedTempFile::new().unwrap();
        let path = temp_file.path().to_str().unwrap();

        // 1. Write new variable
        update_env_file(path, "TEST_KEY", "value1").unwrap();

        let mut content = String::new();
        std::fs::File::open(path)
            .unwrap()
            .read_to_string(&mut content)
            .unwrap();
        assert!(content.contains("TEST_KEY=value1"));

        // 2. Overwrite existing variable
        update_env_file(path, "TEST_KEY", "value2").unwrap();

        let mut content2 = String::new();
        std::fs::File::open(path)
            .unwrap()
            .read_to_string(&mut content2)
            .unwrap();
        assert!(content2.contains("TEST_KEY=value2"));
        assert!(!content2.contains("TEST_KEY=value1"));
    }

    #[test]
    fn test_extract_code_from_request() {
        let request = "GET /callback?code=abc123xyz&state=state_value HTTP/1.1\r\nHost: localhost:8080\r\n\r\n";
        let code = extract_code_from_request(request);
        assert_eq!(code, Some("abc123xyz".to_string()));
    }

    #[test]
    fn test_extract_error_from_request() {
        let request = "GET /callback?error=access_denied&error_description=User+denied+consent HTTP/1.1\r\nHost: localhost:8080\r\n\r\n";
        let error = extract_error_from_request(request);
        assert_eq!(error, Some("access_denied".to_string()));
    }
}
