//! HTTP API for the Chotu app. Listens on loopback only; `tailscale serve`
//! terminates HTTPS in front of it and is the only way in (see `docs/app-api.md`).

use crate::scope::CallerScope;
use anyhow::{anyhow, Context, Result};
use axum::extract::{FromRequestParts, MatchedPath, Request, State};
use axum::http::header::{AUTHORIZATION, WWW_AUTHENTICATE};
use axum::http::request::Parts;
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Extension, Json, Router};
use chotu_common::api_token::{
    api_token_env_key, hash_api_token, mint_api_token, parse_token_hash, token_device,
    verify_api_token, TokenHash,
};
use chotu_common::{ApiConfig, AppConfig, FamilyMember};
use serde::Serialize;
use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::task::{JoinError, JoinHandle};

/// How long shutdown waits for in-flight requests before giving up.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

/// Set by `tailscale serve` to the tailnet login of the caller.
const TAILSCALE_USER_LOGIN: &str = "Tailscale-User-Login";

/// The profile the app's picker chose, as a `family.members[].id`.
const CHOTU_MEMBER: &str = "X-Chotu-Member";

/// The app's own version, e.g. `1.4.2`, sent on every request.
const CHOTU_APP_VERSION: &str = "X-Chotu-App-Version";

/// Oldest app version this server supports. Bump it only when the server drops
/// or changes something an older app relies on (see `docs/app-api.md`).
const MIN_APP_VERSION: AppVersion = AppVersion(0, 1, 0);

/// Crate version plus the git commit it was built from, e.g. `0.1.0+87dd739`.
const SERVER_VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), "+", env!("CHOTU_GIT_SHA"));

pub struct ApiServer {
    task: JoinHandle<Result<()>>,
    shutdown: oneshot::Sender<()>,
}

impl ApiServer {
    /// Binds the listener before returning so a bad address, a busy port, or a
    /// malformed token hash fails startup.
    pub async fn start(api: &ApiConfig, config: &AppConfig) -> Result<Self> {
        api.validate(&config.family.members)
            .map_err(anyhow::Error::msg)?;
        let auth = DeviceAuth::from_env(api, &config.family.members)?;
        let listener = TcpListener::bind(api.bind)
            .await
            .with_context(|| format!("Failed to bind API server to {}", api.bind))?;
        println!(
            "API server listening on http://{} (expose with `tailscale serve`)",
            api.bind
        );
        Ok(Self::spawn(listener, router(Arc::new(auth))))
    }

    fn spawn(listener: TcpListener, app: Router) -> Self {
        let (shutdown, shutdown_rx) = oneshot::channel::<()>();
        let task = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = shutdown_rx.await;
                })
                .await
                .context("API server failed")
        });
        Self { task, shutdown }
    }

    /// Resolves when the server task ends on its own.
    pub async fn exited(&mut self) -> Result<Result<()>, JoinError> {
        (&mut self.task).await
    }

    /// Stops accepting connections and waits up to `SHUTDOWN_GRACE` for in-flight requests.
    pub async fn shutdown(self) {
        let _ = self.shutdown.send(());
        match tokio::time::timeout(SHUTDOWN_GRACE, self.task).await {
            Ok(Ok(Ok(()))) => println!("API server stopped."),
            Ok(res) => eprintln!("API server stopped with error: {:?}", res),
            Err(_) => eprintln!(
                "API server did not stop within {}s; abandoning in-flight requests.",
                SHUTDOWN_GRACE.as_secs()
            ),
        }
    }
}

fn router(auth: Arc<DeviceAuth>) -> Router {
    with_auth(auth, Router::new().route("/v1/me", get(me)))
}

/// Mounts `protected` behind the app version check and device-token auth, next
/// to the open health check. The version check runs first, so an outdated app
/// is told to update even before its token is checked.
fn with_auth(auth: Arc<DeviceAuth>, protected: Router) -> Router {
    let protected = protected
        .route_layer(middleware::from_fn_with_state(auth, authenticate))
        .route_layer(middleware::from_fn(check_app_version));
    Router::new()
        .route("/v1/health", get(health))
        .merge(protected)
        .layer(middleware::from_fn(log_request))
}

/// A `major.minor.patch` app version. Ordered numerically, so `0.10.0 > 0.9.0`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct AppVersion(u32, u32, u32);

impl AppVersion {
    /// Parses `1.4.2`. A pre-release or build suffix (`1.4.2-beta`, `1.4.2+7`)
    /// is ignored; anything else that is not three numbers is rejected.
    fn parse(value: &str) -> Option<Self> {
        let core = value.trim().split(['-', '+']).next()?;
        let mut parts = core.split('.').map(|part| part.parse::<u32>().ok());
        let version = Self(parts.next()??, parts.next()??, parts.next()??);
        parts.next().is_none().then_some(version)
    }
}

impl fmt::Display for AppVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.0, self.1, self.2)
    }
}

/// `426` when `X-Chotu-App-Version` is below `MIN_APP_VERSION`, `400` when it
/// is not a version. A request without the header passes: the app always
/// sends it, and curl or scripts need not.
async fn check_app_version(request: Request, next: Next) -> Response {
    if let Some(value) = request.headers().get(CHOTU_APP_VERSION) {
        match value.to_str().ok().and_then(AppVersion::parse) {
            None => return ApiError::InvalidAppVersion.into_response(),
            Some(version) if version < MIN_APP_VERSION => {
                return ApiError::UpgradeRequired.into_response()
            }
            Some(_) => {}
        }
    }
    next.run(request).await
}

#[derive(Debug, Clone, Serialize, PartialEq)]
struct Profile {
    id: String,
    name: String,
}

/// No `Debug`: it holds a token hash.
struct Device {
    name: String,
    token_hash: TokenHash,
    profiles: Vec<Profile>,
}

/// Configured devices with their token hashes. Built once at startup.
/// No `Debug`: it holds token hashes.
struct DeviceAuth {
    devices: Vec<Arc<Device>>,
    /// Verified against when a token names no known device, so an unknown
    /// device costs the same Argon2 work as a wrong secret.
    decoy: TokenHash,
}

impl DeviceAuth {
    /// Reads each device's `API_TOKEN_<NAME>` hash. A device without one cannot
    /// sign in (that is how a token is revoked); a malformed hash fails startup.
    fn from_env(api: &ApiConfig, members: &[FamilyMember]) -> Result<Self> {
        let mut devices = Vec::new();
        for device in &api.devices {
            let key = api_token_env_key(&device.name);
            let hash = match std::env::var(&key) {
                Ok(value) if !value.trim().is_empty() => {
                    parse_token_hash(&value).map_err(|e| anyhow!("{key}: {e}"))?
                }
                _ => {
                    eprintln!(
                        "API device {:?} has no {key} in .env and cannot sign in. Mint one with `just api-token {}`.",
                        device.name, device.name
                    );
                    continue;
                }
            };
            devices.push((device.name.clone(), hash, device.members.clone()));
        }
        Self::new(devices, members)
    }

    /// `devices` are `(name, token hash, member ids)`, already validated against `members`.
    fn new(
        devices: Vec<(String, TokenHash, Vec<String>)>,
        members: &[FamilyMember],
    ) -> Result<Self> {
        let decoy = mint_api_token("decoy")
            .and_then(|token| hash_api_token(&token))
            .and_then(|hash| parse_token_hash(&hash))
            .map_err(anyhow::Error::msg)?;
        let devices = devices
            .into_iter()
            .map(|(name, token_hash, member_ids)| {
                let profiles = members
                    .iter()
                    .filter(|m| member_ids.iter().any(|id| id.eq_ignore_ascii_case(&m.id)))
                    .map(|m| Profile {
                        id: m.id.clone(),
                        name: m.name.clone(),
                    })
                    .collect();
                Arc::new(Device {
                    name,
                    token_hash,
                    profiles,
                })
            })
            .collect();
        Ok(Self { devices, decoy })
    }

    /// Bearer token → device; 401 when missing or not matching its device's hash.
    /// Argon2 verification runs on the blocking pool.
    async fn device(&self, headers: &HeaderMap) -> Result<Arc<Device>, ApiError> {
        let token = headers
            .get(AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| {
                let (scheme, token) = value.split_once(' ')?;
                scheme.eq_ignore_ascii_case("bearer").then(|| token.trim())
            })
            .filter(|token| !token.is_empty())
            .ok_or(ApiError::Unauthorized)?
            .to_string();
        let device = token_device(&token).and_then(|name| {
            self.devices
                .iter()
                .find(|device| device.name.eq_ignore_ascii_case(name))
                .cloned()
        });
        let hash = device
            .as_ref()
            .map_or_else(|| self.decoy.clone(), |device| device.token_hash.clone());
        let verified = tokio::task::spawn_blocking(move || verify_api_token(&token, &hash))
            .await
            .unwrap_or(false);
        device.filter(|_| verified).ok_or(ApiError::Unauthorized)
    }
}

impl Device {
    /// `X-Chotu-Member`, when sent, must be one of this device's profiles (else 403).
    /// Returns the canonical member id.
    fn member(&self, headers: &HeaderMap) -> Result<Option<String>, ApiError> {
        let Some(value) = headers.get(CHOTU_MEMBER) else {
            return Ok(None);
        };
        let requested = value.to_str().map_err(|_| ApiError::Forbidden)?.trim();
        self.profiles
            .iter()
            .find(|p| p.id.eq_ignore_ascii_case(requested))
            .map(|p| Some(p.id.clone()))
            .ok_or(ApiError::Forbidden)
    }
}

/// An authenticated device, plus the profile it chose for this request (if any).
#[derive(Clone)]
struct Session {
    device: Arc<Device>,
    member_id: Option<String>,
}

/// The member a request acts for. Always `LinkedDm`: the app never speaks
/// for the household group. Rejects with 400 when no profile was chosen.
#[cfg_attr(not(test), allow(dead_code))] // Used by the first member-data routes.
pub(crate) struct Caller(pub CallerScope);

impl<S: Send + Sync> FromRequestParts<S> for Caller {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let session = parts
            .extensions
            .get::<Session>()
            .ok_or(ApiError::Unauthorized)?;
        let member_id = session.member_id.clone().ok_or(ApiError::MemberRequired)?;
        Ok(Self(CallerScope::LinkedDm { member_id }))
    }
}

#[derive(Debug)]
pub(crate) enum ApiError {
    Unauthorized,
    Forbidden,
    #[cfg_attr(not(test), allow(dead_code))] // Only `Caller` rejects with it.
    MemberRequired,
    InvalidAppVersion,
    UpgradeRequired,
}

#[derive(Serialize)]
struct ErrorBody {
    error: &'static str,
    /// Set on `426`, so the app can show which version it needs.
    #[serde(skip_serializing_if = "Option::is_none")]
    min_app_version: Option<String>,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, error) = match self {
            Self::Unauthorized => (StatusCode::UNAUTHORIZED, "missing or invalid device token"),
            Self::Forbidden => (StatusCode::FORBIDDEN, "member not allowed for this device"),
            Self::MemberRequired => (StatusCode::BAD_REQUEST, "X-Chotu-Member header required"),
            Self::InvalidAppVersion => (
                StatusCode::BAD_REQUEST,
                "X-Chotu-App-Version must be major.minor.patch",
            ),
            Self::UpgradeRequired => (StatusCode::UPGRADE_REQUIRED, "app update required"),
        };
        let min_app_version =
            (status == StatusCode::UPGRADE_REQUIRED).then(|| MIN_APP_VERSION.to_string());
        let body = ErrorBody {
            error,
            min_app_version,
        };
        let mut response = (status, Json(body)).into_response();
        if status == StatusCode::UNAUTHORIZED {
            response
                .headers_mut()
                .insert(WWW_AUTHENTICATE, "Bearer".parse().expect("static header"));
        }
        response
    }
}

/// Copied onto the response so `log_request` can name the device and member.
#[derive(Clone)]
struct LoggedCaller {
    device: String,
    member_id: Option<String>,
}

async fn authenticate(
    State(auth): State<Arc<DeviceAuth>>,
    mut request: Request,
    next: Next,
) -> Response {
    let device = match auth.device(request.headers()).await {
        Ok(device) => device,
        Err(err) => return err.into_response(),
    };
    // The device is known from here on, so even a 403 logs which device asked.
    let (member_id, mut response) = match device.member(request.headers()) {
        Ok(member_id) => {
            request.extensions_mut().insert(Session {
                device: Arc::clone(&device),
                member_id: member_id.clone(),
            });
            (member_id, next.run(request).await)
        }
        Err(err) => (None, err.into_response()),
    };
    response.extensions_mut().insert(LoggedCaller {
        device: device.name.clone(),
        member_id,
    });
    response
}

#[derive(Serialize)]
struct Health {
    status: &'static str,
    server_version: &'static str,
    min_app_version: String,
}

/// Unauthenticated connection and compatibility check for the app. Carries no
/// household data, and skips the version check so an outdated app can still
/// learn the minimum it needs.
async fn health() -> Json<Health> {
    Json(Health {
        status: "ok",
        server_version: SERVER_VERSION,
        min_app_version: MIN_APP_VERSION.to_string(),
    })
}

#[derive(Serialize)]
struct Me {
    device: String,
    /// The profile named by `X-Chotu-Member`, or null when none was sent.
    member: Option<Profile>,
    /// Profiles the app's picker may offer on this device.
    profiles: Vec<Profile>,
}

/// Who this device is and which profiles it may use. Works without
/// `X-Chotu-Member` so the app can fill its profile picker.
async fn me(Extension(session): Extension<Session>) -> Json<Me> {
    let member = session.member_id.as_ref().and_then(|id| {
        session
            .device
            .profiles
            .iter()
            .find(|p| &p.id == id)
            .cloned()
    });
    Json(Me {
        device: session.device.name.clone(),
        member,
        profiles: session.device.profiles.clone(),
    })
}

/// One logfmt line per request. Never logs tokens, bodies, or raw paths.
async fn log_request(request: Request, next: Next) -> Response {
    let started = Instant::now();
    let method = request.method().clone();
    let route = request
        .extensions()
        .get::<MatchedPath>()
        .map_or("<unmatched>", MatchedPath::as_str)
        .to_string();
    let tailscale_user = request
        .headers()
        .get(TAILSCALE_USER_LOGIN)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);

    let response = next.run(request).await;

    let caller = response.extensions().get::<LoggedCaller>();
    println!(
        "api_request method={} route={} status={} duration_ms={} device={} member_id={} tailscale_user={:?}",
        method,
        route,
        response.status().as_u16(),
        started.elapsed().as_millis(),
        caller.map_or("-", |c| c.device.as_str()),
        caller.and_then(|c| c.member_id.as_deref()).unwrap_or("-"),
        tailscale_user.as_deref().unwrap_or("-"),
    );
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const ALEX_PHONE: &str = "alex_phone.alex-secret";
    const KITCHEN_IPAD: &str = "kitchen_ipad.kitchen-secret";

    fn member(id: &str, name: &str) -> FamilyMember {
        let mut member = AppConfig::default().family.members.remove(0);
        member.id = id.to_string();
        member.name = name.to_string();
        member
    }

    fn test_auth() -> Arc<DeviceAuth> {
        let members = [member("alex", "Alex"), member("jordan", "Jordan")];
        let hash = |token: &str| parse_token_hash(&hash_api_token(token).unwrap()).unwrap();
        Arc::new(
            DeviceAuth::new(
                vec![
                    (
                        "alex_phone".to_string(),
                        hash(ALEX_PHONE),
                        vec!["alex".to_string()],
                    ),
                    (
                        "kitchen_ipad".to_string(),
                        hash(KITCHEN_IPAD),
                        vec!["alex".to_string(), "jordan".to_string()],
                    ),
                ],
                &members,
            )
            .unwrap(),
        )
    }

    /// Stand-in for a member-data route: echoes the scope the handler received.
    async fn member_data(Caller(scope): Caller) -> String {
        format!("{scope:?}")
    }

    async fn serve_test_app() -> (ApiServer, String) {
        let protected = Router::new()
            .route("/v1/me", get(me))
            .route("/v1/data", get(member_data).post(member_data));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = ApiServer::spawn(listener, with_auth(test_auth(), protected));
        (server, format!("http://{addr}"))
    }

    fn request(
        client: &reqwest::Client,
        method: reqwest::Method,
        url: String,
        token: Option<&str>,
        member: Option<&str>,
    ) -> reqwest::RequestBuilder {
        let mut builder = client.request(method, url);
        if let Some(token) = token {
            builder = builder.bearer_auth(token);
        }
        if let Some(member) = member {
            builder = builder.header(CHOTU_MEMBER, member);
        }
        builder
    }

    #[tokio::test]
    async fn rejects_non_loopback_bind_before_listening() {
        let api = ApiConfig {
            bind: "0.0.0.0:0".parse().unwrap(),
            devices: vec![],
        };
        let err = ApiServer::start(&api, &AppConfig::default())
            .await
            .err()
            .expect("must refuse");
        assert!(err.to_string().contains("not a loopback address"), "{err}");
    }

    #[tokio::test]
    async fn serves_health_and_shuts_down_gracefully() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = ApiServer::spawn(listener, router(test_auth()));

        let client = reqwest::Client::new();
        let response = client
            .get(format!("http://{addr}/v1/health"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let body: serde_json::Value = response.json().await.unwrap();
        assert_eq!(
            body,
            serde_json::json!({
                "status": "ok",
                "server_version": SERVER_VERSION,
                "min_app_version": "0.1.0",
            })
        );
        assert!(
            SERVER_VERSION.starts_with(concat!(env!("CARGO_PKG_VERSION"), "+")),
            "{SERVER_VERSION}"
        );

        let missing = client
            .get(format!("http://{addr}/v1/nope"))
            .send()
            .await
            .unwrap();
        assert_eq!(missing.status(), 404);

        tokio::time::timeout(SHUTDOWN_GRACE, server.shutdown())
            .await
            .expect("shutdown finishes");
        assert!(
            client
                .get(format!("http://{addr}/v1/health"))
                .send()
                .await
                .is_err(),
            "listener must be closed after shutdown"
        );
    }

    #[tokio::test]
    async fn missing_or_invalid_token_is_401() {
        let (server, base) = serve_test_app().await;
        let client = reqwest::Client::new();

        for token in [
            None,
            Some(""),
            Some("wrong-token"),
            Some("alex_phone.wrong-secret"),
            Some("ghost_device.alex-secret"),
            Some(KITCHEN_IPAD.replace("kitchen_ipad", "alex_phone").as_str()),
        ] {
            let response = request(
                &client,
                reqwest::Method::GET,
                format!("{base}/v1/me"),
                token,
                None,
            )
            .send()
            .await
            .unwrap();
            assert_eq!(response.status(), 401, "{token:?}");
            assert_eq!(response.headers()[WWW_AUTHENTICATE], "Bearer");
        }
        let basic = client
            .get(format!("{base}/v1/me"))
            .header(AUTHORIZATION, format!("Basic {ALEX_PHONE}"))
            .send()
            .await
            .unwrap();
        assert_eq!(basic.status(), 401);

        server.shutdown().await;
    }

    #[tokio::test]
    async fn member_not_allowed_for_token_is_403() {
        let (server, base) = serve_test_app().await;
        let client = reqwest::Client::new();

        for member in ["jordan", "sam", ""] {
            let response = request(
                &client,
                reqwest::Method::GET,
                format!("{base}/v1/me"),
                Some(ALEX_PHONE),
                Some(member),
            )
            .send()
            .await
            .unwrap();
            assert_eq!(response.status(), 403, "{member:?}");
        }

        server.shutdown().await;
    }

    #[tokio::test]
    async fn token_for_one_member_cannot_read_or_write_anothers_data() {
        let (server, base) = serve_test_app().await;
        let client = reqwest::Client::new();

        for method in [reqwest::Method::GET, reqwest::Method::POST] {
            let own = request(
                &client,
                method.clone(),
                format!("{base}/v1/data"),
                Some(ALEX_PHONE),
                Some("ALEX"),
            )
            .send()
            .await
            .unwrap();
            assert_eq!(own.status(), 200);
            assert_eq!(
                own.text().await.unwrap(),
                r#"LinkedDm { member_id: "alex" }"#
            );

            let other = request(
                &client,
                method.clone(),
                format!("{base}/v1/data"),
                Some(ALEX_PHONE),
                Some("jordan"),
            )
            .send()
            .await
            .unwrap();
            assert_eq!(other.status(), 403, "{method}");

            let no_profile = request(
                &client,
                method.clone(),
                format!("{base}/v1/data"),
                Some(ALEX_PHONE),
                None,
            )
            .send()
            .await
            .unwrap();
            assert_eq!(no_profile.status(), 400, "{method}");
        }

        // A shared device may act as either of its profiles.
        let jordan = request(
            &client,
            reqwest::Method::POST,
            format!("{base}/v1/data"),
            Some(KITCHEN_IPAD),
            Some("jordan"),
        )
        .send()
        .await
        .unwrap();
        assert_eq!(
            jordan.text().await.unwrap(),
            r#"LinkedDm { member_id: "jordan" }"#
        );

        server.shutdown().await;
    }

    #[tokio::test]
    async fn me_returns_resolved_member_and_allowed_profiles() {
        let (server, base) = serve_test_app().await;
        let client = reqwest::Client::new();
        let profiles = json!([
            { "id": "alex", "name": "Alex" },
            { "id": "jordan", "name": "Jordan" },
        ]);

        let picker: serde_json::Value = request(
            &client,
            reqwest::Method::GET,
            format!("{base}/v1/me"),
            Some(KITCHEN_IPAD),
            None,
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
        assert_eq!(
            picker,
            json!({ "device": "kitchen_ipad", "member": null, "profiles": profiles })
        );

        let chosen: serde_json::Value = request(
            &client,
            reqwest::Method::GET,
            format!("{base}/v1/me"),
            Some(KITCHEN_IPAD),
            Some("Jordan"),
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
        assert_eq!(
            chosen,
            json!({
                "device": "kitchen_ipad",
                "member": { "id": "jordan", "name": "Jordan" },
                "profiles": profiles,
            })
        );

        server.shutdown().await;
    }

    #[test]
    fn parses_and_orders_app_versions() {
        assert_eq!(AppVersion::parse("1.4.2"), Some(AppVersion(1, 4, 2)));
        assert_eq!(
            AppVersion::parse(" 1.4.2-beta.1 "),
            Some(AppVersion(1, 4, 2))
        );
        assert_eq!(AppVersion::parse("1.4.2+7"), Some(AppVersion(1, 4, 2)));
        for bad in [
            "", "1", "1.4", "1.4.2.0", "v1.4.2", "1.x.2", "1..2", "-1.0.0",
        ] {
            assert_eq!(AppVersion::parse(bad), None, "{bad:?}");
        }
        assert!(AppVersion(0, 10, 0) > AppVersion(0, 9, 9));
        assert!(AppVersion(1, 0, 0) > AppVersion(0, 99, 99));
        assert_eq!(MIN_APP_VERSION.to_string(), "0.1.0");
    }

    #[tokio::test]
    async fn app_below_min_version_gets_426() {
        let (server, base) = serve_test_app().await;
        let client = reqwest::Client::new();
        let me = |version: Option<&str>, token: Option<&str>| {
            let mut builder = request(
                &client,
                reqwest::Method::GET,
                format!("{base}/v1/me"),
                token,
                None,
            );
            if let Some(version) = version {
                builder = builder.header(CHOTU_APP_VERSION, version);
            }
            builder.send()
        };

        // Checked before the token, so an outdated app is told to update
        // whatever state its credentials are in.
        for token in [Some(ALEX_PHONE), None] {
            let response = me(Some("0.0.9"), token).await.unwrap();
            assert_eq!(response.status(), 426, "{token:?}");
            let body: serde_json::Value = response.json().await.unwrap();
            assert_eq!(
                body,
                json!({ "error": "app update required", "min_app_version": "0.1.0" })
            );
        }

        for version in [Some("0.1.0"), Some("0.1.1-beta"), Some("2.0.0"), None] {
            let response = me(version, Some(ALEX_PHONE)).await.unwrap();
            assert_eq!(response.status(), 200, "{version:?}");
        }

        for version in ["", "latest", "1.2"] {
            let response = me(Some(version), Some(ALEX_PHONE)).await.unwrap();
            assert_eq!(response.status(), 400, "{version:?}");
        }

        // Health stays reachable so an outdated app can read the minimum.
        let health = client
            .get(format!("{base}/v1/health"))
            .header(CHOTU_APP_VERSION, "0.0.1")
            .send()
            .await
            .unwrap();
        assert_eq!(health.status(), 200);

        server.shutdown().await;
    }
}
