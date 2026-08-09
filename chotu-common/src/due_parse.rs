//! Parse relative/absolute due phrases into local civil date + optional time → UTC.

use chrono::{Datelike, Duration, Local, NaiveDate, NaiveTime, TimeZone, Utc, Weekday};

/// Resolved due: civil day for lists/brief, and UTC instant for timed reminders.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedDue {
    /// YYYY-MM-DD (local civil day).
    pub due_date: String,
    /// RFC3339 UTC. Date-only inputs use local 09:00 that day.
    pub due_at: String,
}

/// Parse a due phrase such as `tomorrow 15:00`, `friday 3pm`, `2026-08-10`.
/// Returns `None` if empty or unparseable.
pub fn parse_due_phrase(raw: &str) -> Option<ParsedDue> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }

    let tokens: Vec<&str> = raw.split_whitespace().collect();
    if tokens.is_empty() {
        return None;
    }

    let today = Local::now().date_naive();
    let (date, time_tokens) = parse_date_prefix(&tokens, today)?;
    let time = if time_tokens.is_empty() {
        NaiveTime::from_hms_opt(9, 0, 0)?
    } else {
        parse_time_tokens(&time_tokens)?
    };

    let local_naive = date.and_time(time);
    let local_dt = match Local.from_local_datetime(&local_naive) {
        chrono::LocalResult::Single(dt) | chrono::LocalResult::Ambiguous(dt, _) => dt,
        chrono::LocalResult::None => return None,
    };

    Some(ParsedDue {
        due_date: date.format("%Y-%m-%d").to_string(),
        due_at: local_dt.with_timezone(&Utc).to_rfc3339(),
    })
}

/// Split `/tasks add …` args into (optional member, title, optional due phrase).
///
/// Forms:
/// - `buy milk`
/// - `praj buy milk`
/// - `buy milk due tomorrow 3pm`
/// - `change battery for fob by today 3 pm`
/// - `praj call dentist due friday 15:00`
pub fn split_task_add_args(
    args: &str,
    member_ids: &[String],
) -> Option<(Option<String>, String, Option<String>)> {
    let args = args.trim();
    if args.is_empty() {
        return None;
    }

    let (before_due, due_raw) = match split_due_marker(args) {
        Some((b, d)) => (b.trim(), Some(d.trim().to_string())),
        None => (args, None),
    };

    if before_due.is_empty() {
        return None;
    }

    let mut parts: Vec<&str> = before_due.split_whitespace().collect();
    let mut member_id: Option<String> = None;
    if let Some(first) = parts.first() {
        if let Some(m) = member_ids
            .iter()
            .find(|id| id.eq_ignore_ascii_case(first))
        {
            member_id = Some(m.clone());
            parts.remove(0);
        }
    }

    if parts.is_empty() {
        return None;
    }

    let title = parts.join(" ");
    Some((member_id, title, due_raw))
}

fn split_due_marker(args: &str) -> Option<(&str, &str)> {
    // Case-insensitive due separator; rightmost of due/by/before wins.
    let lower = args.to_lowercase();
    let mut best: Option<(usize, usize)> = None; // (index, separator length)
    for sep in [" due ", " by ", " before "] {
        if let Some(idx) = lower.rfind(sep) {
            if best.map(|(bi, _)| idx > bi).unwrap_or(true) {
                best = Some((idx, sep.len()));
            }
        }
    }
    let (idx, sep_len) = best?;
    let before = &args[..idx];
    let after = &args[idx + sep_len..];
    if before.trim().is_empty() || after.trim().is_empty() {
        return None;
    }
    Some((before, after))
}

fn parse_date_prefix<'a>(
    tokens: &[&'a str],
    today: NaiveDate,
) -> Option<(NaiveDate, Vec<&'a str>)> {
    let first = tokens.first()?.to_lowercase();
    match first.as_str() {
        "today" => Some((today, tokens[1..].to_vec())),
        "tomorrow" | "tmr" | "tmrw" => {
            Some((today + Duration::days(1), tokens[1..].to_vec()))
        }
        "monday" | "mon" => Some((next_weekday(today, Weekday::Mon), tokens[1..].to_vec())),
        "tuesday" | "tue" | "tues" => {
            Some((next_weekday(today, Weekday::Tue), tokens[1..].to_vec()))
        }
        "wednesday" | "wed" => Some((next_weekday(today, Weekday::Wed), tokens[1..].to_vec())),
        "thursday" | "thu" | "thur" | "thurs" => {
            Some((next_weekday(today, Weekday::Thu), tokens[1..].to_vec()))
        }
        "friday" | "fri" => Some((next_weekday(today, Weekday::Fri), tokens[1..].to_vec())),
        "saturday" | "sat" => Some((next_weekday(today, Weekday::Sat), tokens[1..].to_vec())),
        "sunday" | "sun" => Some((next_weekday(today, Weekday::Sun), tokens[1..].to_vec())),
        _ => {
            // YYYY-MM-DD
            if let Ok(d) = NaiveDate::parse_from_str(tokens[0], "%Y-%m-%d") {
                return Some((d, tokens[1..].to_vec()));
            }
            None
        }
    }
}

/// Next occurrence of weekday (today if it matches and no time-in-past preference —
/// for simplicity always next including today when weekday == today).
fn next_weekday(today: NaiveDate, target: Weekday) -> NaiveDate {
    let mut d = today;
    for _ in 0..7 {
        if d.weekday() == target {
            return d;
        }
        d += Duration::days(1);
    }
    today + Duration::days(7)
}

fn parse_time_tokens(tokens: &[&str]) -> Option<NaiveTime> {
    let joined = tokens.join("").to_lowercase();
    let single = tokens.join(" ").to_lowercase();

    // 3pm / 3:30pm / 15:00 / 9am
    if let Some(t) = parse_clock(&joined).or_else(|| parse_clock(&single)) {
        return Some(t);
    }
    if tokens.len() == 1 {
        return parse_clock(tokens[0]);
    }
    None
}

fn parse_clock(s: &str) -> Option<NaiveTime> {
    let s = s.trim().to_lowercase().replace(' ', "");
    if s.is_empty() {
        return None;
    }

    // 15:00 / 9:30
    if let Ok(t) = NaiveTime::parse_from_str(&s, "%H:%M") {
        return Some(t);
    }
    if let Ok(t) = NaiveTime::parse_from_str(&s, "%H:%M:%S") {
        return Some(t);
    }

    let (body, ampm) = if let Some(rest) = s.strip_suffix("am") {
        (rest, Some(false))
    } else if let Some(rest) = s.strip_suffix("pm") {
        (rest, Some(true))
    } else {
        (s.as_str(), None)
    };

    let body = body.trim_end_matches(':');
    let (hour_s, min_s) = if let Some((h, m)) = body.split_once(':') {
        (h, m)
    } else {
        (body, "0")
    };

    let mut hour: u32 = hour_s.parse().ok()?;
    let min: u32 = min_s.parse().ok()?;
    if min >= 60 {
        return None;
    }

    match ampm {
        Some(false) => {
            if hour == 12 {
                hour = 0;
            }
            if hour > 12 {
                return None;
            }
        }
        Some(true) => {
            if hour == 12 {
                // 12pm stays 12
            } else if hour < 12 {
                hour += 12;
            } else {
                return None;
            }
        }
        None => {
            if hour > 23 {
                return None;
            }
        }
    }

    NaiveTime::from_hms_opt(hour, min, 0)
}

/// Whether a stored due_at is due for a one-shot reminder ping.
pub fn is_due_for_reminder(due_at: &str, reminded_at: Option<&str>, now: chrono::DateTime<Utc>) -> bool {
    if reminded_at.is_some() {
        return false;
    }
    let Ok(due) = chrono::DateTime::parse_from_rfc3339(due_at) else {
        return false;
    };
    due.with_timezone(&Utc) <= now
}

/// Known `/tasks` list status filter tokens (not task titles).
pub fn is_known_task_status_filter(tok: &str) -> bool {
    matches!(
        tok.to_lowercase().as_str(),
        "all" | "done" | "completed" | "ignored" | "snoozed" | "open"
    )
}

/// True when `/tasks …` / `/task …` args look like creating a task rather than listing.
///
/// List shapes: empty (handled by caller), single status/member, or status↔member pair.
pub fn looks_like_task_add_query(tokens: &[&str], member_ids: &[String]) -> bool {
    if tokens.is_empty() {
        return false;
    }
    let is_member = |tok: &str| {
        member_ids
            .iter()
            .any(|id| id.eq_ignore_ascii_case(tok))
    };
    match tokens {
        [a] => !is_known_task_status_filter(a) && !is_member(a),
        [a, b] => {
            let list_shape = (is_known_task_status_filter(a) && is_member(b))
                || (is_member(a) && is_known_task_status_filter(b))
                || (is_known_task_status_filter(a) && is_known_task_status_filter(b));
            !list_shape
        }
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Timelike;

    #[test]
    fn test_split_task_add_args() {
        let members = vec!["praj".to_string(), "alex".to_string()];
        let (m, title, due) = split_task_add_args("buy milk", &members).unwrap();
        assert!(m.is_none());
        assert_eq!(title, "buy milk");
        assert!(due.is_none());

        let (m, title, due) =
            split_task_add_args("praj call dentist due tomorrow 3pm", &members).unwrap();
        assert_eq!(m.as_deref(), Some("praj"));
        assert_eq!(title, "call dentist");
        assert_eq!(due.as_deref(), Some("tomorrow 3pm"));

        let (m, title, due) =
            split_task_add_args("change battery for fob by today 3 pm", &members).unwrap();
        assert!(m.is_none());
        assert_eq!(title, "change battery for fob");
        assert_eq!(due.as_deref(), Some("today 3 pm"));

        let (m, title, due) =
            split_task_add_args("submit form before friday 9am", &members).unwrap();
        assert!(m.is_none());
        assert_eq!(title, "submit form");
        assert_eq!(due.as_deref(), Some("friday 9am"));

        // Rightmost separator wins when both appear.
        let (_, title, due) =
            split_task_add_args("stand by door due tomorrow 10am", &members).unwrap();
        assert_eq!(title, "stand by door");
        assert_eq!(due.as_deref(), Some("tomorrow 10am"));
    }

    #[test]
    fn test_looks_like_task_add_query() {
        let members = vec!["praj".to_string(), "alex".to_string()];
        assert!(!looks_like_task_add_query(&[], &members));
        assert!(!looks_like_task_add_query(&["open"], &members));
        assert!(!looks_like_task_add_query(&["praj"], &members));
        assert!(!looks_like_task_add_query(&["open", "praj"], &members));
        assert!(!looks_like_task_add_query(&["praj", "snoozed"], &members));
        assert!(!looks_like_task_add_query(&["all", "completed"], &members));

        assert!(looks_like_task_add_query(&["buy"], &members));
        assert!(looks_like_task_add_query(&["buy", "milk"], &members));
        assert!(looks_like_task_add_query(
            &["change", "battery", "for", "fob", "by", "today", "3", "pm"],
            &members
        ));
        // Member + title (not a list filter pair) → add.
        assert!(looks_like_task_add_query(&["praj", "milk"], &members));
    }

    #[test]
    fn test_parse_due_today_3_pm_spaced() {
        let parsed = parse_due_phrase("today 3 pm").unwrap();
        let due = chrono::DateTime::parse_from_rfc3339(&parsed.due_at).unwrap();
        assert_eq!(due.with_timezone(&Local).hour(), 15);
        assert_eq!(parsed.due_date, Local::now().date_naive().format("%Y-%m-%d").to_string());
    }

    #[test]
    fn test_parse_due_before_friday_9am() {
        let parsed = parse_due_phrase("friday 9am").unwrap();
        let due = chrono::DateTime::parse_from_rfc3339(&parsed.due_at).unwrap();
        let local = due.with_timezone(&Local);
        assert_eq!(local.hour(), 9);
        assert_eq!(local.minute(), 0);
    }

    #[test]
    fn test_parse_due_tomorrow_with_time() {
        let parsed = parse_due_phrase("tomorrow 15:00").unwrap();
        let today = Local::now().date_naive();
        let expected_day = (today + Duration::days(1)).format("%Y-%m-%d").to_string();
        assert_eq!(parsed.due_date, expected_day);
        let due = chrono::DateTime::parse_from_rfc3339(&parsed.due_at).unwrap();
        let local = due.with_timezone(&Local);
        assert_eq!(local.hour(), 15);
        assert_eq!(local.minute(), 0);
    }

    #[test]
    fn test_parse_due_date_only_defaults_nine_am() {
        let parsed = parse_due_phrase("2026-08-10").unwrap();
        assert_eq!(parsed.due_date, "2026-08-10");
        let due = chrono::DateTime::parse_from_rfc3339(&parsed.due_at).unwrap();
        let local = due.with_timezone(&Local);
        assert_eq!(local.hour(), 9);
        assert_eq!(local.date_naive().to_string(), "2026-08-10");
    }

    #[test]
    fn test_parse_3pm() {
        let parsed = parse_due_phrase("today 3pm").unwrap();
        let due = chrono::DateTime::parse_from_rfc3339(&parsed.due_at).unwrap();
        assert_eq!(due.with_timezone(&Local).hour(), 15);
    }

    #[test]
    fn test_is_due_for_reminder() {
        let now = Utc::now();
        let past = (now - Duration::minutes(5)).to_rfc3339();
        let future = (now + Duration::hours(1)).to_rfc3339();
        assert!(is_due_for_reminder(&past, None, now));
        assert!(!is_due_for_reminder(&future, None, now));
        assert!(!is_due_for_reminder(&past, Some(&now.to_rfc3339()), now));
    }

    #[test]
    fn test_next_weekday_includes_today() {
        let wed = NaiveDate::from_ymd_opt(2026, 8, 5).unwrap(); // Wednesday
        assert_eq!(next_weekday(wed, Weekday::Wed), wed);
        assert_eq!(
            next_weekday(wed, Weekday::Fri),
            NaiveDate::from_ymd_opt(2026, 8, 7).unwrap()
        );
    }
}
