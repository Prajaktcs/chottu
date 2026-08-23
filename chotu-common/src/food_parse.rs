//! Resolve food-log date/time into a local civil day + UTC instant.
//!
//! Slash `/food` and photo captions use [`parse_food_log_utterance`] (no LLM).
//! Relative phrases ("yesterday", "last Friday") become YYYY-MM-DD; meal-of-day
//! words map onto household time windows. Natural-language chat still uses the
//! intent classifier, which may emit the same structured fields.

use chrono::{Datelike, Duration, Local, NaiveDate, NaiveTime, TimeZone, Utc, Weekday};

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

/// Parsed meal text plus optional civil date/time (no LLM).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedFoodUtterance {
    /// Food/meal text with date/meal/clock framing words removed when possible.
    pub food_description: String,
    /// YYYY-MM-DD when the user named a day; `None` means today / unspecified.
    pub food_date: Option<String>,
    /// HH:MM 24h local when a meal-of-day or clock time was named.
    pub food_time: Option<String>,
}

/// Local parse of `/food` args or a photo caption (after member id is stripped).
pub fn parse_food_log_utterance(text: &str) -> ParsedFoodUtterance {
    let trimmed = text.trim();
    let today = Local::now().date_naive();
    let food_date = detect_food_date(trimmed, today).map(|d| d.format("%Y-%m-%d").to_string());
    let clock = extract_clock_hhmm(trimmed);
    let mut food_time = effective_food_time(trimmed, clock.as_deref());
    if food_time.is_none() && utterance_has_last_night(trimmed) {
        food_time = Some(MEAL_TIME_DINNER.to_string());
    }
    let food_description = strip_food_framing(trimmed);
    ParsedFoodUtterance {
        food_description: if food_description.is_empty() {
            trimmed.to_string()
        } else {
            food_description
        },
        food_date,
        food_time,
    }
}

fn utterance_has_last_night(utterance: &str) -> bool {
    let lower = utterance.to_lowercase();
    lower.contains("last night")
}

fn detect_food_date(utterance: &str, today: NaiveDate) -> Option<NaiveDate> {
    let lower = utterance.to_lowercase();
    if lower.contains("last night") || token_eq(&lower, "yesterday") {
        return Some(today - Duration::days(1));
    }
    if token_eq(&lower, "tomorrow") || token_eq(&lower, "tmr") || token_eq(&lower, "tmrw") {
        return Some(today + Duration::days(1));
    }
    if token_eq(&lower, "today") || token_eq(&lower, "tonight") {
        return None;
    }

    let tokens: Vec<&str> = lower
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '-')
        .filter(|t| !t.is_empty())
        .collect();

    for (i, token) in tokens.iter().enumerate() {
        if let Ok(d) = NaiveDate::parse_from_str(token, "%Y-%m-%d") {
            return Some(d);
        }
        let last = i > 0 && tokens[i - 1] == "last";
        if let Some(wd) = parse_weekday_token(token) {
            return Some(if last {
                previous_weekday_exclusive(today, wd)
            } else {
                previous_or_today_weekday(today, wd)
            });
        }
    }
    None
}

fn token_eq(lower_utterance: &str, word: &str) -> bool {
    lower_utterance
        .split(|c: char| !c.is_ascii_alphabetic())
        .any(|t| t == word)
}

/// Full weekday names only — 3-letter tokens collide with food ("sun chips").
fn parse_weekday_token(token: &str) -> Option<Weekday> {
    match token {
        "monday" => Some(Weekday::Mon),
        "tuesday" => Some(Weekday::Tue),
        "wednesday" => Some(Weekday::Wed),
        "thursday" => Some(Weekday::Thu),
        "friday" => Some(Weekday::Fri),
        "saturday" => Some(Weekday::Sat),
        "sunday" => Some(Weekday::Sun),
        _ => None,
    }
}

fn previous_or_today_weekday(today: NaiveDate, target: Weekday) -> NaiveDate {
    let mut d = today;
    for _ in 0..7 {
        if d.weekday() == target {
            return d;
        }
        d -= Duration::days(1);
    }
    today
}

fn previous_weekday_exclusive(today: NaiveDate, target: Weekday) -> NaiveDate {
    let mut d = today - Duration::days(1);
    for _ in 0..7 {
        if d.weekday() == target {
            return d;
        }
        d -= Duration::days(1);
    }
    today - Duration::days(7)
}

fn extract_clock_hhmm(utterance: &str) -> Option<String> {
    let lower = utterance.to_lowercase();
    let tokens: Vec<&str> = lower
        .split_whitespace()
        .map(|raw| raw.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != ':'))
        .filter(|t| !t.is_empty())
        .collect();

    for (i, token) in tokens.iter().enumerate() {
        if let Some(t) = parse_clock_to_naive(token) {
            return Some(t.format("%H:%M").to_string());
        }
        if i + 1 < tokens.len() && is_ampm_token(tokens[i + 1]) {
            let joined = format!("{}{}", token, tokens[i + 1]);
            if let Some(t) = parse_clock_to_naive(&joined) {
                return Some(t.format("%H:%M").to_string());
            }
        }
    }
    None
}

fn parse_clock_to_naive(token: &str) -> Option<NaiveTime> {
    let s = token
        .trim()
        .to_lowercase()
        .replace(' ', "")
        .replace('.', "");
    if NaiveTime::parse_from_str(&s, "%H:%M").is_ok()
        || NaiveTime::parse_from_str(&s, "%H:%M:%S").is_ok()
    {
        return NaiveTime::parse_from_str(&s, "%H:%M")
            .or_else(|_| NaiveTime::parse_from_str(&s, "%H:%M:%S"))
            .ok();
    }
    let (body, pm) = if let Some(rest) = s.strip_suffix("am") {
        (rest, false)
    } else if let Some(rest) = s.strip_suffix("pm") {
        (rest, true)
    } else {
        return None;
    };
    let body = body.trim_end_matches(':');
    let (hour_s, min_s) = match body.split_once(':') {
        Some((h, m)) => (h, m),
        None => (body, "0"),
    };
    let hour = hour_s.parse::<u32>().ok()?;
    let min = min_s.parse::<u32>().ok()?;
    if min >= 60 {
        return None;
    }
    let hour24 = match (hour, pm) {
        (12, false) => 0,
        (12, true) => 12,
        (h, true) if (1..=11).contains(&h) => h + 12,
        (h, false) if (1..=11).contains(&h) => h,
        _ => return None,
    };
    NaiveTime::from_hms_opt(hour24, min, 0)
}

fn strip_food_framing(utterance: &str) -> String {
    let tokens: Vec<&str> = utterance.split_whitespace().collect();
    let mut out: Vec<&str> = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        let raw = tokens[i];
        let cleaned = raw
            .trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != ':' && c != '/' && c != '.')
            .to_lowercase();
        if cleaned.is_empty() {
            i += 1;
            continue;
        }
        if cleaned == "last" && i + 1 < tokens.len() {
            // Possessives: "night's", "friday's" — take the alphabetic prefix.
            let next = alpha_key(tokens[i + 1]);
            if next == "night" || parse_weekday_token(&next).is_some() {
                i += 2;
                continue;
            }
        }
        let key = alpha_key(&cleaned);
        if key == "at" || key == "for" {
            if next_is_timing(&tokens, i + 1) {
                i += 1;
                continue;
            }
        } else if is_framing_word(&key) || parse_clock_to_naive(&cleaned).is_some() {
            i += 1;
            continue;
        }
        if i + 1 < tokens.len() && is_ampm_token(&tokens[i + 1].to_lowercase()) {
            let joined = format!("{}{}", cleaned, tokens[i + 1].to_lowercase());
            if parse_clock_to_naive(&joined).is_some() {
                i += 2;
                continue;
            }
        }
        if NaiveDate::parse_from_str(&cleaned, "%Y-%m-%d").is_ok() {
            i += 1;
            continue;
        }
        out.push(raw);
        i += 1;
    }
    out.join(" ")
}

fn alpha_key(token: &str) -> String {
    token
        .chars()
        .take_while(|c| c.is_ascii_alphabetic())
        .collect::<String>()
        .to_lowercase()
}

fn is_framing_word(token: &str) -> bool {
    matches!(
        token,
        "yesterday"
            | "today"
            | "tonight"
            | "tomorrow"
            | "night"
            | "breakfast"
            | "lunch"
            | "dinner"
            | "supper"
            | "snack"
            | "snacks"
            | "was"
    ) || parse_weekday_token(token).is_some()
}

fn next_is_timing(tokens: &[&str], j: usize) -> bool {
    if j >= tokens.len() {
        return false;
    }
    let cleaned = tokens[j]
        .trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != ':')
        .to_lowercase();
    if parse_clock_to_naive(&cleaned).is_some() {
        return true;
    }
    if j + 1 < tokens.len() && is_ampm_token(&tokens[j + 1].to_lowercase()) {
        let joined = format!("{}{}", cleaned, tokens[j + 1].to_lowercase());
        if parse_clock_to_naive(&joined).is_some() {
            return true;
        }
    }
    let key = alpha_key(&cleaned);
    matches!(
        key.as_str(),
        "breakfast"
            | "lunch"
            | "dinner"
            | "supper"
            | "snack"
            | "snacks"
            | "night"
            | "yesterday"
            | "today"
            | "tonight"
            | "tomorrow"
    ) || parse_weekday_token(&key).is_some()
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
    let tokens: Vec<&str> = lower
        .split_whitespace()
        .map(|raw| raw.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != ':'))
        .filter(|t| !t.is_empty())
        .collect();

    for (i, token) in tokens.iter().enumerate() {
        if looks_like_clock_token(token) {
            return true;
        }
        // Spaced forms: "7 pm", "7:30 am"
        if i + 1 < tokens.len() && is_ampm_token(tokens[i + 1]) && looks_like_hour_minute(token)
        {
            return true;
        }
    }
    false
}

fn is_ampm_token(token: &str) -> bool {
    matches!(token, "am" | "pm" | "a.m" | "p.m" | "a.m." | "p.m.")
}

fn looks_like_hour_minute(token: &str) -> bool {
    let body = token.trim_end_matches(':');
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

fn looks_like_clock_token(token: &str) -> bool {
    let s = token
        .trim()
        .to_lowercase()
        .replace(' ', "")
        .replace('.', "");
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
    looks_like_hour_minute(body)
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
            effective_food_time("dinner at 7 pm pasta", Some("19:00")).as_deref(),
            Some("19:00")
        );
        assert_eq!(
            effective_food_time("snacks at 4:30 pm almonds", Some("16:30")).as_deref(),
            Some("16:30")
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

    #[test]
    fn plain_food_has_no_date_or_time() {
        let p = parse_food_log_utterance(
            "1 1/2 cups of milk with coffee and 1/2 tsp. of sugar",
        );
        assert_eq!(
            p.food_description,
            "1 1/2 cups of milk with coffee and 1/2 tsp. of sugar"
        );
        assert_eq!(p.food_date, None);
        assert_eq!(p.food_time, None);
    }

    #[test]
    fn yesterdays_dinner_maps_date_and_window() {
        let p = parse_food_log_utterance("yesterday's dinner pasta");
        let yesterday = (Local::now().date_naive() - Duration::days(1))
            .format("%Y-%m-%d")
            .to_string();
        assert_eq!(p.food_date.as_deref(), Some(yesterday.as_str()));
        assert_eq!(p.food_time.as_deref(), Some(MEAL_TIME_DINNER));
        assert_eq!(p.food_description, "pasta");
    }

    #[test]
    fn last_night_is_yesterday_dinner() {
        let p = parse_food_log_utterance("last night pizza");
        let yesterday = (Local::now().date_naive() - Duration::days(1))
            .format("%Y-%m-%d")
            .to_string();
        assert_eq!(p.food_date.as_deref(), Some(yesterday.as_str()));
        assert_eq!(p.food_time.as_deref(), Some(MEAL_TIME_DINNER));
        assert_eq!(p.food_description, "pizza");
    }

    #[test]
    fn last_nights_possessive_does_not_leave_last() {
        let p = parse_food_log_utterance("last night's pizza");
        let yesterday = (Local::now().date_naive() - Duration::days(1))
            .format("%Y-%m-%d")
            .to_string();
        assert_eq!(p.food_date.as_deref(), Some(yesterday.as_str()));
        assert_eq!(p.food_time.as_deref(), Some(MEAL_TIME_DINNER));
        assert_eq!(p.food_description, "pizza");
    }

    #[test]
    fn tomorrow_lunch_maps_next_day() {
        let p = parse_food_log_utterance("tomorrow lunch sandwich");
        let tomorrow = (Local::now().date_naive() + Duration::days(1))
            .format("%Y-%m-%d")
            .to_string();
        assert_eq!(p.food_date.as_deref(), Some(tomorrow.as_str()));
        assert_eq!(p.food_time.as_deref(), Some(MEAL_TIME_LUNCH));
        assert_eq!(p.food_description, "sandwich");
    }

    #[test]
    fn lunch_and_explicit_clock() {
        let lunch = parse_food_log_utterance("lunch salad");
        assert_eq!(lunch.food_time.as_deref(), Some(MEAL_TIME_LUNCH));
        assert_eq!(lunch.food_description, "salad");

        let clocked = parse_food_log_utterance("pasta at 7pm");
        assert_eq!(clocked.food_time.as_deref(), Some("19:00"));
        assert_eq!(clocked.food_description, "pasta");

        let dotted = parse_food_log_utterance("pasta at 7 a.m.");
        assert_eq!(dotted.food_time.as_deref(), Some("07:00"));
        assert_eq!(dotted.food_description, "pasta");

        let dotted_pm = parse_food_log_utterance("pasta at 7 p.m.");
        assert_eq!(dotted_pm.food_time.as_deref(), Some("19:00"));
        assert_eq!(dotted_pm.food_description, "pasta");
    }

    #[test]
    fn at_for_kept_unless_timing() {
        let place = parse_food_log_utterance("milk at Starbucks");
        assert_eq!(place.food_description, "milk at Starbucks");
        assert_eq!(place.food_time, None);

        let meal = parse_food_log_utterance("eggs for breakfast");
        assert_eq!(meal.food_description, "eggs");
        assert_eq!(meal.food_time.as_deref(), Some(MEAL_TIME_BREAKFAST));

        let for_someone = parse_food_log_utterance("coffee for praj");
        assert_eq!(for_someone.food_description, "coffee for praj");

        let spaced = parse_food_log_utterance("pasta at 7 pm");
        assert_eq!(spaced.food_description, "pasta");
        assert_eq!(spaced.food_time.as_deref(), Some("19:00"));
    }

    #[test]
    fn sun_chips_is_food_not_sunday() {
        let p = parse_food_log_utterance("sun chips");
        assert_eq!(p.food_date, None);
        assert_eq!(p.food_description, "sun chips");
    }

    #[test]
    fn last_friday_maps_previous_weekday() {
        let today = Local::now().date_naive();
        let p = parse_food_log_utterance("last friday pasta");
        let expected = {
            let mut d = today - Duration::days(1);
            while d.weekday() != Weekday::Fri {
                d -= Duration::days(1);
            }
            d.format("%Y-%m-%d").to_string()
        };
        assert_eq!(p.food_date.as_deref(), Some(expected.as_str()));
        assert_eq!(p.food_description, "pasta");

        let possessive = parse_food_log_utterance("last friday's pasta");
        assert_eq!(possessive.food_date.as_deref(), Some(expected.as_str()));
        assert_eq!(possessive.food_description, "pasta");
    }
}
