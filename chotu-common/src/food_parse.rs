//! Resolve LLM-provided food log date/time into a local civil day + UTC instant.
//!
//! Relative phrases ("yesterday", "last Friday") are understood by the intent/LLM
//! layer, which emits YYYY-MM-DD and optional HH:MM. This module only validates
//! and converts those structured fields — it does not hardcode natural-language
//! date vocabularies.

use chrono::{Local, NaiveDate, NaiveTime, TimeZone, Utc};

/// When a food log should be attributed (local civil day + UTC instant).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FoodLogTiming {
    /// YYYY-MM-DD (local civil day).
    pub date: String,
    /// UTC instant used for `food_log.timestamp` and Google Health meal type.
    pub timestamp: chrono::DateTime<Utc>,
    /// True when the LLM supplied an explicit `food_date`.
    pub date_was_explicit: bool,
}

/// Build timing from optional LLM fields.
///
/// - Missing/invalid `food_date` → today (local).
/// - Missing/invalid `food_time` → now if the day is today, else local noon.
/// - `food_time` is `HH:MM` or `HH:MM:SS` (24-hour).
pub fn resolve_food_log_timing(
    food_date: Option<&str>,
    food_time: Option<&str>,
) -> FoodLogTiming {
    let today = Local::now().date_naive();
    let (date, date_was_explicit) = match food_date.map(str::trim).filter(|s| !s.is_empty()) {
        Some(raw) => match NaiveDate::parse_from_str(raw, "%Y-%m-%d") {
            Ok(d) => (d, true),
            Err(_) => (today, false),
        },
        None => (today, false),
    };

    let time = food_time
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .and_then(parse_hhmm);

    let timestamp = match time {
        Some(t) => local_datetime_to_utc(date, t),
        None if date == today => Local::now().with_timezone(&Utc),
        None => {
            let noon = NaiveTime::from_hms_opt(12, 0, 0).expect("noon is valid");
            local_datetime_to_utc(date, noon)
        }
    };

    FoodLogTiming {
        date: date.format("%Y-%m-%d").to_string(),
        timestamp,
        date_was_explicit,
    }
}

fn parse_hhmm(raw: &str) -> Option<NaiveTime> {
    let s = raw.trim();
    NaiveTime::parse_from_str(s, "%H:%M")
        .or_else(|_| NaiveTime::parse_from_str(s, "%H:%M:%S"))
        .ok()
}

fn local_datetime_to_utc(date: NaiveDate, time: NaiveTime) -> chrono::DateTime<Utc> {
    let local_naive = date.and_time(time);
    match Local.from_local_datetime(&local_naive) {
        chrono::LocalResult::Single(dt) | chrono::LocalResult::Ambiguous(dt, _) => {
            dt.with_timezone(&Utc)
        }
        chrono::LocalResult::None => {
            // DST gap — try nearby hours, then UTC now.
            for h in [time.hour().saturating_add(1), 12, 15, 18] {
                if let Some(t) = NaiveTime::from_hms_opt(h, 0, 0) {
                    if let chrono::LocalResult::Single(dt)
                    | chrono::LocalResult::Ambiguous(dt, _) =
                        Local.from_local_datetime(&date.and_time(t))
                    {
                        return dt.with_timezone(&Utc);
                    }
                }
            }
            Utc::now()
        }
    }
}

// `Timelike` for `.hour()` in DST fallback.
use chrono::Timelike;

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    #[test]
    fn missing_fields_default_to_today_now() {
        let before = Utc::now();
        let timing = resolve_food_log_timing(None, None);
        let after = Utc::now();
        let today = Local::now().format("%Y-%m-%d").to_string();
        assert_eq!(timing.date, today);
        assert!(!timing.date_was_explicit);
        assert!(timing.timestamp >= before - Duration::seconds(1));
        assert!(timing.timestamp <= after + Duration::seconds(1));
    }

    #[test]
    fn explicit_date_and_time() {
        let timing = resolve_food_log_timing(Some("2026-08-07"), Some("19:00"));
        assert_eq!(timing.date, "2026-08-07");
        assert!(timing.date_was_explicit);
        let local = timing.timestamp.with_timezone(&Local);
        assert_eq!(local.hour(), 19);
        assert_eq!(local.minute(), 0);
        assert_eq!(local.date_naive().to_string(), "2026-08-07");
    }

    #[test]
    fn past_date_without_time_uses_noon() {
        let today = Local::now().date_naive();
        let past = (today - Duration::days(1)).format("%Y-%m-%d").to_string();
        let timing = resolve_food_log_timing(Some(&past), None);
        assert_eq!(timing.date, past);
        let local = timing.timestamp.with_timezone(&Local);
        assert_eq!(local.hour(), 12);
        assert_eq!(local.minute(), 0);
    }

    #[test]
    fn invalid_date_falls_back_to_today() {
        let timing = resolve_food_log_timing(Some("not-a-date"), Some("19:00"));
        let today = Local::now().format("%Y-%m-%d").to_string();
        assert_eq!(timing.date, today);
        assert!(!timing.date_was_explicit);
    }

    #[test]
    fn explicit_time_with_seconds() {
        let timing = resolve_food_log_timing(Some("2026-08-07"), Some("19:00:30"));
        assert_eq!(timing.date, "2026-08-07");
        let local = timing.timestamp.with_timezone(&Local);
        assert_eq!(local.hour(), 19);
        assert_eq!(local.minute(), 0);
        assert_eq!(local.second(), 30);
    }

    #[test]
    fn today_with_explicit_time() {
        let today = Local::now().format("%Y-%m-%d").to_string();
        let timing = resolve_food_log_timing(Some(&today), Some("08:00"));
        assert_eq!(timing.date, today);
        assert!(timing.date_was_explicit);
        let local = timing.timestamp.with_timezone(&Local);
        assert_eq!(local.hour(), 8);
        assert_eq!(local.minute(), 0);
    }
}
