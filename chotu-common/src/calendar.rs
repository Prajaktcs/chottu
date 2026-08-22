use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::oauth::{refresh_oauth2_token, OAuthError};

#[derive(Error, Debug)]
pub enum CalendarError {
    #[error("failed to refresh Google Calendar access token")]
    TokenRefresh(#[source] OAuthError),
    #[error("Google Calendar {operation} request failed")]
    Request {
        operation: &'static str,
        #[source]
        source: reqwest::Error,
    },
    #[error("Google Calendar {operation} returned HTTP {status}: {body}")]
    Api {
        operation: &'static str,
        status: u16,
        body: String,
    },
    #[error("failed to decode Google Calendar {operation} response")]
    ResponseDecode {
        operation: &'static str,
        #[source]
        source: reqwest::Error,
    },
    #[error("Google Calendar create-event response did not contain an event id")]
    MissingEventId,
    #[error("invalid calendar date '{input}', expected YYYY-MM-DD")]
    InvalidDate {
        input: String,
        #[source]
        source: chrono::ParseError,
    },
    #[error("invalid local datetime for calendar scheduling: {0}")]
    InvalidLocalDateTime(chrono::NaiveDateTime),
}

// ─── Data Types ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CalendarEvent {
    /// Google Calendar event ID.
    pub id: String,
    /// Event title / summary.
    pub title: String,
    /// Start time (UTC).
    pub start: DateTime<Utc>,
    /// End time (UTC).
    pub end: DateTime<Utc>,
    /// Optional location string.
    pub location: Option<String>,
    /// Optional event description.
    pub description: Option<String>,
    /// Display name of the calendar owner (filled in by caller, not from API).
    pub member_name: String,
    /// The response status of the calendar owner (e.g. "needsAction", "tentative", "accepted", "declined").
    pub response_status: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct GoogleEventDateTime {
    #[serde(rename = "dateTime")]
    date_time: Option<String>,
    date: Option<String>,
    #[serde(rename = "timeZone")]
    time_zone: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct GoogleAttendee {
    email: Option<String>,
    #[serde(rename = "responseStatus")]
    response_status: Option<String>,
    #[serde(rename = "self", default)]
    is_self: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct GoogleEvent {
    id: String,
    summary: Option<String>,
    description: Option<String>,
    location: Option<String>,
    start: GoogleEventDateTime,
    end: GoogleEventDateTime,
    attendees: Option<Vec<GoogleAttendee>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct GoogleEventsListResponse {
    items: Option<Vec<GoogleEvent>>,
    #[serde(rename = "nextPageToken")]
    next_page_token: Option<String>,
}

// ─── Client ────────────────────────────────────────────────────────────────────

/// A thin async wrapper around the Google Calendar API v3.
/// Holds the OAuth credentials needed to refresh the access token on demand.
#[derive(Debug, Clone)]
pub struct GoogleCalendarClient {
    client_id: String,
    client_secret: String,
    refresh_token: String,
    http: reqwest::Client,
}

impl GoogleCalendarClient {
    pub fn new(client_id: impl Into<String>, client_secret: impl Into<String>, refresh_token: impl Into<String>) -> Self {
        Self {
            client_id: client_id.into(),
            client_secret: client_secret.into(),
            refresh_token: refresh_token.into(),
            http: reqwest::Client::new(),
        }
    }

    /// Obtains a fresh access token by exchanging the stored refresh token.
    async fn get_access_token(&self) -> Result<String, CalendarError> {
        let token = refresh_oauth2_token(&self.client_id, &self.client_secret, &self.refresh_token)
            .await
            .map_err(CalendarError::TokenRefresh)?;
        Ok(token.access_token)
    }

    /// Fetches all events for the primary calendar between `from` and `to` (UTC).
    /// Automatically handles pagination to return all events.
    pub async fn fetch_events(
        &self,
        member_name: &str,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<CalendarEvent>, CalendarError> {
        let access_token = self.get_access_token().await?;

        let time_min = from.to_rfc3339();
        let time_max = to.to_rfc3339();

        let mut all_events: Vec<CalendarEvent> = Vec::new();
        let mut page_token: Option<String> = None;

        loop {
            let mut req = self.http
                .get("https://www.googleapis.com/calendar/v3/calendars/primary/events")
                .bearer_auth(&access_token)
                .query(&[
                    ("timeMin", time_min.as_str()),
                    ("timeMax", time_max.as_str()),
                    ("singleEvents", "true"),
                    ("orderBy", "startTime"),
                    ("maxResults", "250"),
                ]);

            if let Some(ref pt) = page_token {
                req = req.query(&[("pageToken", pt.as_str())]);
            }

            let resp = req.send().await.map_err(|source| CalendarError::Request {
                operation: "list-events",
                source,
            })?;

            if !resp.status().is_success() {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                return Err(CalendarError::Api {
                    operation: "list-events",
                    status: status.as_u16(),
                    body,
                });
            }

            let list: GoogleEventsListResponse = resp.json().await.map_err(|source| {
                CalendarError::ResponseDecode {
                    operation: "list-events",
                    source,
                }
            })?;

            if let Some(items) = list.items {
                for item in items {
                    if let Some(event) = parse_google_event(item, member_name) {
                        all_events.push(event);
                    }
                }
            }

            match list.next_page_token {
                Some(pt) => page_token = Some(pt),
                None => break,
            }
        }

        Ok(all_events)
    }

    /// Creates a new event on the primary calendar. Returns the created event's ID.
    pub async fn create_event(
        &self,
        title: &str,
        description: Option<&str>,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        timezone: &str,
    ) -> Result<String, CalendarError> {
        let access_token = self.get_access_token().await?;

        let body = serde_json::json!({
            "summary": title,
            "description": description.unwrap_or(""),
            "start": {
                "dateTime": start.to_rfc3339(),
                "timeZone": timezone,
            },
            "end": {
                "dateTime": end.to_rfc3339(),
                "timeZone": timezone,
            },
        });

        let resp = self.http
            .post("https://www.googleapis.com/calendar/v3/calendars/primary/events")
            .bearer_auth(&access_token)
            .json(&body)
            .send()
            .await
            .map_err(|source| CalendarError::Request {
                operation: "create-event",
                source,
            })?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(CalendarError::Api {
                operation: "create-event",
                status: status.as_u16(),
                body: text,
            });
        }

        let created: serde_json::Value = resp.json().await.map_err(|source| {
            CalendarError::ResponseDecode {
                operation: "create-event",
                source,
            }
        })?;

        created["id"]
            .as_str()
            .map(|s| s.to_string())
            .ok_or(CalendarError::MissingEventId)
    }

    /// Updates an existing event's start/end on the primary calendar (PATCH).
    pub async fn update_event_times(
        &self,
        event_id: &str,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        timezone: &str,
    ) -> Result<(), CalendarError> {
        let access_token = self.get_access_token().await?;

        let body = serde_json::json!({
            "start": {
                "dateTime": start.to_rfc3339(),
                "timeZone": timezone,
            },
            "end": {
                "dateTime": end.to_rfc3339(),
                "timeZone": timezone,
            },
        });

        let url = format!(
            "https://www.googleapis.com/calendar/v3/calendars/primary/events/{}",
            event_id
        );

        let resp = self
            .http
            .patch(&url)
            .bearer_auth(&access_token)
            .json(&body)
            .send()
            .await
            .map_err(|source| CalendarError::Request {
                operation: "update-event",
                source,
            })?;

        // 404 = already gone — caller can decide whether to clear the stored id.
        if resp.status().is_success() {
            return Ok(());
        }

        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        Err(CalendarError::Api {
            operation: "update-event",
            status: status.as_u16(),
            body: text,
        })
    }

    /// Deletes an event by ID from the primary calendar.
    pub async fn delete_event(&self, event_id: &str) -> Result<(), CalendarError> {
        let access_token = self.get_access_token().await?;

        let url = format!(
            "https://www.googleapis.com/calendar/v3/calendars/primary/events/{}",
            event_id
        );

        let resp = self.http
            .delete(&url)
            .bearer_auth(&access_token)
            .send()
            .await
            .map_err(|source| CalendarError::Request {
                operation: "delete-event",
                source,
            })?;

        // 204 No Content = success; 404 = already gone (treat as OK)
        if resp.status().is_success() || resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(());
        }

        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        Err(CalendarError::Api {
            operation: "delete-event",
            status: status.as_u16(),
            body: text,
        })
    }
}

// ─── Helpers ───────────────────────────────────────────────────────────────────

fn parse_google_event(item: GoogleEvent, member_name: &str) -> Option<CalendarEvent> {
    // Parse start time — prefer dateTime, fall back to all-day date
    let start = parse_datetime(&item.start)?;
    let end = parse_datetime(&item.end)?;

    // Find the attendee that is "self" to extract their response status
    let mut response_status = None;
    if let Some(ref attendees) = item.attendees {
        for attendee in attendees {
            if attendee.is_self {
                response_status = attendee.response_status.clone();
                break;
            }
        }
    }

    Some(CalendarEvent {
        id: item.id,
        title: item.summary.unwrap_or_else(|| "(No Title)".to_string()),
        start,
        end,
        location: item.location,
        description: item.description,
        member_name: member_name.to_string(),
        response_status,
    })
}

fn parse_datetime(dt: &GoogleEventDateTime) -> Option<DateTime<Utc>> {
    if let Some(ref s) = dt.date_time {
        // RFC 3339 with offset
        DateTime::parse_from_rfc3339(s).ok().map(|d| d.with_timezone(&Utc))
    } else if let Some(ref d) = dt.date {
        // All-day event: "YYYY-MM-DD" — treat as midnight UTC
        let naive = chrono::NaiveDate::parse_from_str(d, "%Y-%m-%d").ok()?;
        Some(naive.and_hms_opt(0, 0, 0)?.and_utc())
    } else {
        None
    }
}

/// Builds a `GoogleCalendarClient` for a given family member, reading credentials
/// from environment variables. Returns `None` if the member has no calendar config
/// or if the refresh token env var is not set.
pub fn build_calendar_client(member: &crate::family::FamilyMember) -> Option<GoogleCalendarClient> {
    let _cal_cfg = member.calendar.as_ref()?;
    let client_id = std::env::var("CHOTU_OAUTH_CLIENT_ID").ok()?;
    let client_secret = std::env::var("CHOTU_OAUTH_CLIENT_SECRET").ok()?;
    let refresh_token = std::env::var(member.calendar_refresh_token_env_key()).ok()?;
    Some(GoogleCalendarClient::new(client_id, client_secret, refresh_token))
}

/// IANA timezone used when creating calendar events.
/// Prefers `CHOTU_TIMEZONE` (set from config `timezone` at boot), else `America/Toronto`.
pub fn default_calendar_timezone() -> String {
    crate::schedule::resolve_timezone_name(None)
}

/// Schedules a timed block starting at `start` UTC, lasting `duration_minutes`.
/// Returns the Google event ID.
pub async fn schedule_at(
    client: &GoogleCalendarClient,
    title: &str,
    description: Option<&str>,
    start: DateTime<Utc>,
    duration_minutes: i64,
) -> Result<String, CalendarError> {
    use chrono::Duration;

    let timezone = default_calendar_timezone();
    let end = start + Duration::minutes(clamp_event_duration_minutes(duration_minutes));
    client
        .create_event(title, description, start, end, &timezone)
        .await
}

/// Moves an existing timed block to a new `start` UTC, lasting `duration_minutes`.
pub async fn reschedule_at(
    client: &GoogleCalendarClient,
    event_id: &str,
    start: DateTime<Utc>,
    duration_minutes: i64,
) -> Result<(), CalendarError> {
    use chrono::Duration;

    let timezone = default_calendar_timezone();
    let end = start + Duration::minutes(clamp_event_duration_minutes(duration_minutes));
    client
        .update_event_times(event_id, start, end, &timezone)
        .await
}

fn clamp_event_duration_minutes(duration_minutes: i64) -> i64 {
    duration_minutes.max(15)
}

/// Default duration used when creating/rescheduling task calendar blocks.
pub const TASK_CALENDAR_DURATION_MINUTES: i64 = 30;

/// Schedules a timed block on a member's calendar starting at 09:00 in the agent
/// IANA timezone on `date_yyyy_mm_dd` (or tomorrow if `None`), lasting `duration_minutes`.
/// The Google API receives UTC instants plus the IANA zone label.
pub async fn schedule_timed_block(
    client: &GoogleCalendarClient,
    title: &str,
    description: Option<&str>,
    date_yyyy_mm_dd: Option<&str>,
    duration_minutes: i64,
) -> Result<String, CalendarError> {
    use chrono::{Duration, NaiveDate, NaiveTime, TimeZone};
    use chrono_tz::Tz;

    let tz: Tz = default_calendar_timezone()
        .parse()
        .unwrap_or(chrono_tz::America::Toronto);
    let today = Utc::now().with_timezone(&tz).date_naive();
    let target_date = match date_yyyy_mm_dd {
        Some(d) => NaiveDate::parse_from_str(d, "%Y-%m-%d").map_err(|source| {
            CalendarError::InvalidDate {
                input: d.to_string(),
                source,
            }
        })?,
        None => today + Duration::days(1),
    };

    let start_naive = target_date
        .and_time(NaiveTime::from_hms_opt(9, 0, 0).unwrap_or_default());
    let start = match tz.from_local_datetime(&start_naive) {
        chrono::LocalResult::Single(dt) | chrono::LocalResult::Ambiguous(dt, _) => {
            dt.with_timezone(&Utc)
        }
        chrono::LocalResult::None => {
            return Err(CalendarError::InvalidLocalDateTime(start_naive));
        }
    };

    schedule_at(client, title, description, start, duration_minutes).await
}

// ─── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_datetime_rfc3339() {
        use chrono::Timelike;
        let dt = GoogleEventDateTime {
            date_time: Some("2026-06-20T09:00:00-04:00".to_string()),
            date: None,
            time_zone: None,
        };
        let parsed = parse_datetime(&dt);
        assert!(parsed.is_some());
        assert_eq!(parsed.unwrap().hour(), 13); // 9 AM EDT = 13:00 UTC
    }

    #[test]
    fn test_parse_datetime_all_day() {
        let dt = GoogleEventDateTime {
            date_time: None,
            date: Some("2026-06-21".to_string()),
            time_zone: None,
        };
        let parsed = parse_datetime(&dt);
        assert!(parsed.is_some());
    }

    #[test]
    fn test_parse_google_event_missing_datetime() {
        let item = GoogleEvent {
            id: "evt1".to_string(),
            summary: Some("Test".to_string()),
            description: None,
            location: None,
            start: GoogleEventDateTime { date_time: None, date: None, time_zone: None },
            end: GoogleEventDateTime { date_time: None, date: None, time_zone: None },
            attendees: None,
        };
        assert!(parse_google_event(item, "Alex").is_none());
    }

    #[tokio::test]
    async fn test_schedule_timed_block_rejects_invalid_date_before_request() {
        let client = GoogleCalendarClient::new("client", "secret", "refresh");
        let error = schedule_timed_block(
            &client,
            "Test event",
            None,
            Some("not-a-date"),
            30,
        )
        .await
        .unwrap_err();

        assert!(matches!(
            error,
            CalendarError::InvalidDate { input, .. } if input == "not-a-date"
        ));
    }

    #[test]
    fn clamp_event_duration_minutes_enforces_minimum() {
        assert_eq!(clamp_event_duration_minutes(5), 15);
        assert_eq!(clamp_event_duration_minutes(15), 15);
        assert_eq!(clamp_event_duration_minutes(30), 30);
    }
}
