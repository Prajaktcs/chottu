//! Idempotent writes. Every `POST`/`DELETE` carries a client-generated request id
//! in the `Idempotency-Key` header. A repeat from the same member returns the
//! stored response instead of applying the write again (see `docs/app-api.md`).

use super::{ApiError, Caller};
use axum::body::{to_bytes, Body};
use axum::extract::{FromRef, FromRequestParts};
use axum::http::header::CONTENT_TYPE;
use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, SecondsFormat, TimeDelta, Utc};
use sqlx::SqlitePool;
use std::future::Future;

pub(crate) const IDEMPOTENCY_KEY: &str = "Idempotency-Key";

const MAX_REQUEST_ID_LEN: usize = 128;

/// Stored responses older than this are pruned; a retry after that applies again.
const RETENTION_DAYS: i64 = 7;

/// A write request's idempotency key. Extract it in every write handler and
/// wrap the write in [`Idempotency::run`]. Rejects with 400 without a profile
/// or a valid `Idempotency-Key`.
pub(crate) struct Idempotency {
    pool: SqlitePool,
    member_id: String,
    request_id: String,
    /// `"<METHOD> <path>"`, so a reused id on a different request is caught.
    request: String,
}

impl<S> FromRequestParts<S> for Idempotency
where
    SqlitePool: FromRef<S>,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let Caller(scope) = Caller::from_request_parts(parts, state).await?;
        let member_id = scope
            .member_id()
            .ok_or(ApiError::MemberRequired)?
            .to_string();
        let request_id = parts
            .headers
            .get(IDEMPOTENCY_KEY)
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
            .filter(|id| valid_request_id(id))
            .ok_or(ApiError::RequestIdRequired)?
            .to_string();
        Ok(Self {
            pool: SqlitePool::from_ref(state),
            member_id,
            request_id,
            request: format!("{} {}", parts.method, parts.uri.path()),
        })
    }
}

/// `(request, status, content_type, body)`; `status` is NULL while in flight.
type StoredRow = (String, Option<i64>, Option<String>, Option<Vec<u8>>);

fn valid_request_id(id: &str) -> bool {
    (1..=MAX_REQUEST_ID_LEN).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_graphic())
}

impl Idempotency {
    /// Applies `write` once per `(member, request id)` and stores its response;
    /// a repeat gets the stored status, `Content-Type` and body back.
    ///
    /// A 5xx is not stored, so the client may retry: `write` must only return
    /// one when nothing was applied (keep each write in one transaction).
    /// A repeat while the first attempt is still running gets 409.
    pub(crate) async fn run<F>(self, write: F) -> Response
    where
        F: Future + Send + 'static,
        F::Output: IntoResponse,
    {
        // Spawned so a client that disconnects mid-write cannot cancel it
        // between applying the write and storing its response.
        match tokio::spawn(self.claim_and_apply(write)).await {
            Ok(Ok(response)) => response,
            Ok(Err(err)) => err.into_response(),
            Err(err) => {
                // The id stays claimed (409) until restart: the write may have applied.
                eprintln!("api_idempotency write task failed: {err}");
                ApiError::Internal.into_response()
            }
        }
    }

    async fn claim_and_apply<F>(self, write: F) -> Result<Response, ApiError>
    where
        F: Future,
        F::Output: IntoResponse,
    {
        let now = Utc::now();
        if let Err(err) = prune(&self.pool, now).await {
            eprintln!("api_idempotency prune failed: {err}");
        }
        let claimed = sqlx::query(
            "INSERT INTO api_write_requests (member_id, request_id, request, created_at) \
             VALUES (?, ?, ?, ?) ON CONFLICT DO NOTHING",
        )
        .bind(&self.member_id)
        .bind(&self.request_id)
        .bind(&self.request)
        .bind(timestamp(now))
        .execute(&self.pool)
        .await
        .map_err(internal("claim"))?
        .rows_affected()
            == 1;
        if !claimed {
            return self.replay().await;
        }

        let (parts, body) = write.await.into_response().into_parts();
        if parts.status.is_server_error() {
            self.release().await;
            return Ok(Response::from_parts(parts, body));
        }
        let body = to_bytes(body, usize::MAX)
            .await
            .map_err(internal("read response"))?;
        let content_type = parts
            .headers
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok());
        let stored = sqlx::query(
            "UPDATE api_write_requests SET status = ?, content_type = ?, body = ? \
             WHERE member_id = ? AND request_id = ?",
        )
        .bind(i64::from(parts.status.as_u16()))
        .bind(content_type)
        .bind(body.as_ref())
        .bind(&self.member_id)
        .bind(&self.request_id)
        .execute(&self.pool)
        .await;
        if let Err(err) = stored {
            // The write applied, so still answer with its response; a retry gets 409.
            eprintln!("api_idempotency store failed: {err}");
        }
        Ok(Response::from_parts(parts, Body::from(body)))
    }

    async fn replay(&self) -> Result<Response, ApiError> {
        let row: Option<StoredRow> = sqlx::query_as(
            "SELECT request, status, content_type, body FROM api_write_requests \
             WHERE member_id = ? AND request_id = ?",
        )
        .bind(&self.member_id)
        .bind(&self.request_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(internal("replay"))?;
        // A missing row was just released after a 5xx; the client should retry.
        let Some((request, status, content_type, body)) = row else {
            return Err(ApiError::RequestInProgress);
        };
        if request != self.request {
            return Err(ApiError::RequestIdReused);
        }
        let status = status
            .and_then(|s| u16::try_from(s).ok())
            .and_then(|s| StatusCode::from_u16(s).ok())
            .ok_or(ApiError::RequestInProgress)?;
        let mut response = (status, body.unwrap_or_default()).into_response();
        response.headers_mut().remove(CONTENT_TYPE);
        if let Some(value) = content_type.and_then(|ct| ct.parse().ok()) {
            response.headers_mut().insert(CONTENT_TYPE, value);
        }
        Ok(response)
    }

    /// Frees the id after a 5xx so the client's retry applies the write.
    async fn release(&self) {
        let released =
            sqlx::query("DELETE FROM api_write_requests WHERE member_id = ? AND request_id = ?")
                .bind(&self.member_id)
                .bind(&self.request_id)
                .execute(&self.pool)
                .await;
        if let Err(err) = released {
            eprintln!("api_idempotency release failed: {err}");
        }
    }
}

/// Drops ids still in flight when the previous process stopped, so their
/// retries are not stuck on 409. Call before serving.
pub(crate) async fn release_abandoned(pool: &SqlitePool) -> Result<u64, sqlx::Error> {
    sqlx::query("DELETE FROM api_write_requests WHERE status IS NULL")
        .execute(pool)
        .await
        .map(|done| done.rows_affected())
}

async fn prune(pool: &SqlitePool, now: DateTime<Utc>) -> Result<u64, sqlx::Error> {
    sqlx::query("DELETE FROM api_write_requests WHERE created_at < ?")
        .bind(timestamp(now - TimeDelta::days(RETENTION_DAYS)))
        .execute(pool)
        .await
        .map(|done| done.rows_affected())
}

/// Fixed-width UTC so `created_at` compares as text.
fn timestamp(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn internal<E: std::fmt::Display>(step: &'static str) -> impl FnOnce(E) -> ApiError {
    move |err| {
        eprintln!("api_idempotency {step} failed: {err}");
        ApiError::Internal
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_ids_are_bounded_visible_ascii() {
        assert!(valid_request_id("3f2b9c4e-8a1d-4c55-9a8e-2b7f0c1d9e6a"));
        assert!(valid_request_id(&"a".repeat(MAX_REQUEST_ID_LEN)));
        assert!(!valid_request_id(""));
        assert!(!valid_request_id(&"a".repeat(MAX_REQUEST_ID_LEN + 1)));
        assert!(!valid_request_id("has space"));
    }

    #[tokio::test]
    async fn prunes_old_responses_and_releases_abandoned_claims() {
        let dir = tempfile::tempdir().unwrap();
        let pool = chotu_common::init_db(dir.path().join("api.sqlite").to_str().unwrap())
            .await
            .unwrap();
        let now = Utc::now();
        for (request_id, status, age_days) in [
            ("old", Some(201), RETENTION_DAYS + 1),
            ("recent", Some(201), RETENTION_DAYS - 1),
            ("pending", None, 0),
        ] {
            sqlx::query(
                "INSERT INTO api_write_requests (member_id, request_id, request, status, created_at) \
                 VALUES ('alex', ?, 'POST /v1/x', ?, ?)",
            )
            .bind(request_id)
            .bind(status)
            .bind(timestamp(now - TimeDelta::days(age_days)))
            .execute(&pool)
            .await
            .unwrap();
        }

        assert_eq!(prune(&pool, now).await.unwrap(), 1);
        assert_eq!(release_abandoned(&pool).await.unwrap(), 1);
        let left: Vec<(String,)> = sqlx::query_as("SELECT request_id FROM api_write_requests")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(left, vec![("recent".to_string(),)]);
    }
}
