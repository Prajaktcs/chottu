//! Resolve LLM-provided food log date/time into a local civil day + UTC instant.
//!
//! Relative phrases ("yesterday", "last Friday") are understood by the intent/LLM
//! layer, which emits YYYY-MM-DD and optional HH:MM. This module validates and
//! converts those structured fields, and maps meal-of-day words (lunch / snacks /
//! dinner) onto household time windows when no explicit clock time was spoken.

use chrono::{Local, NaiveDate, NaiveTime, TimeZone, Utc};

/// Midpoints of household meal windows (local 24h clock).
///
/// - Breakfast ≈ morning (kept for prompt parity)
/// - Lunch: 12:00–13:00 → 12:30
/// - Snacks: 16:00–18:00 → 17:00
/// - Dinner: 20:00–21:30 → 20:45
pub const MEAL_TIME_BREAKFAST: &str = "08:00";
pub const MEAL_TIME_LUNCH: &str = "12:30";
pub const MEAL_TIME_SNACK: &str = "17:00";
pub const MEAL_TIME_DINNER: &str = "20:45";

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

/// Prefer household meal windows when the utterance names a meal-of-day and does
/// not also name an explicit clock time ("3pm", "19:00"). Otherwise keep the
/// LLM-provided `HH:MM` (or `None`).
///
/// Call this with the *original* user text (before meal words are stripped from
/// `food_description`).
pub fn effective_food_time(utterance: &str, llm_food_time: Option<&str>) -> Option<String> {
    let llm = llm_food_time
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());

    if utterance_has_explicit_clock(utterance) {
        return llm;
    }

    if let Some(meal_hhmm) = meal_of_day_clock_time(utterance) {
        return Some(meal_hhmm.to_string());
    }

    llm
}

/// Midpoint clock time for the first meal-of-day word in `utterance`, if any.
pub fn meal_of_day_clock_time(utterance: &str) -> Option<&'static str> {
    let lower = utterance.to_lowercase();
    // Word-ish boundaries: avoid matching inside longer tokens.
    let has = |word: &str| {
        lower
            .split(|c: char| !c.is_ascii_alphabetic())
            .any(|t| t == word)
    };

    if has("breakfast") {
        Some(MEAL_TIME_BREAKFAST)
    } else if has("lunch") {
        Some(MEAL_TIME_LUNCH)
    } else if has("dinner") || has("supper") {
        Some(MEAL_TIME_DINNER)
    } else if has("snack") || has("snacks") {
        Some(MEAL_TIME_SNACK)
    } else {
        None
    }
}

fn utterance_has_explicit_clock(utterance: &str) -> bool {
    let lower = utterance.to_lowercase();
    for raw in lower.split_whitespace() {
        let token = raw.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != ':');
        if token.is_empty() {
            continue;
        }
        if looks_like_clock_token(token) {
            return true;
        }
    }
    false
}

fn looks_like_clock_token(token: &str) -> bool {
    let s = token.trim().to_lowercase().replace(' ', "");
    if NaiveTime::parse_from_str(&s, "%H:%M").is_ok()
        || NaiveTime::parse_from_str(&s, "%H:%M:%S").is_ok()
    {
        return true;
    }

    let (body, ampm) = if let Some(rest) = s.strip_suffix("am") {
        (rest, true)
    } else if let Some(rest) = s.strip_suffix("pm") {
        (rest, true)
    } else {
        (s.as_str(), false)
    };
    if !ampm {
        return false;
    }
    let body = body.trim_end_matches(':');
    let (hour_s, min_s) = match body.split_once(':') {
        Some((h, m)) => (h, m),
        None => (body, "0"),
    };
    let Ok(hour) = hour_s.parse::<u32>() else {
        return false;
    };
    let Ok(min) = min_s.parse::<u32>() else {
        return false;
    };
    hour >= 1 && hour <= 12 && min < 60
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

    #[test]
    fn meal_of_day_maps_to_window_midpoints() {
        assert_eq!(
            meal_of_day_clock_time("had pasta for lunch"),
            Some(MEAL_TIME_LUNCH)
        );
        assert_eq!(
            meal_of_day_clock_time("snacks: almonds"),
            Some(MEAL_TIME_SNACK)
        );
        assert_eq!(
            meal_of_day_clock_time("yesterday's dinner was curry"),
            Some(MEAL_TIME_DINNER)
        );
        assert_eq!(
            meal_of_day_clock_time("supper leftovers"),
            Some(MEAL_TIME_DINNER)
        );
        assert_eq!(meal_of_day_clock_time("just some pasta"), None);
    }

    #[test]
    fn effective_food_time_prefers_meal_window_over_llm() {
        assert_eq!(
            effective_food_time("lunch was pasta", Some("12:00")).as_deref(),
            Some(MEAL_TIME_LUNCH)
        );
        assert_eq!(
            effective_food_time("snacks almonds", None).as_deref(),
            Some(MEAL_TIME_SNACK)
        );
        assert_eq!(
            effective_food_time("dinner curry", Some("19:00")).as_deref(),
            Some(MEAL_TIME_DINNER)
        );
    }

    #[test]
    fn explicit_clock_keeps_llm_time() {
        assert_eq!(
            effective_food_time("dinner at 7pm pasta", Some("19:00")).as_deref(),
            Some("19:00")
        );
        assert_eq!(
            effective_food_time("lunch at 13:15 rice", Some("13:15")).as_deref(),
            Some("13:15")
        );
        assert_eq!(
            effective_food_time("just pasta", Some("14:00")).as_deref(),
            Some("14:00")
        );
        assert_eq!(effective_food_time("just pasta", None), None);
    }
}
