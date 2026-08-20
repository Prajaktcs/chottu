//! Family calendar agenda: day/week windows, fetch, formatting, conflict detection.

use chrono::{Datelike, DateTime, Duration, Local, NaiveDate, TimeZone, Timelike, Utc};

use crate::calendar::{build_calendar_client, CalendarError, CalendarEvent};
use crate::family::AppConfig;

/// Agenda window for `/cal` and CALENDAR intent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CalendarWindow {
    Today,
    Tomorrow,
    Week,
}

impl CalendarWindow {
    /// Parse slash/intent args: `today` (default), `tomorrow`, `week` / `this week`.
    pub fn parse(args: &str) -> Self {
        let t = args.trim().to_lowercase();
        if t.is_empty() || t == "today" || t == "day" {
            CalendarWindow::Today
        } else if t == "tomorrow" || t == "tmr" || t == "tmrw" {
            CalendarWindow::Tomorrow
        } else if t == "week" || t == "this week" || t == "thisweek" {
            CalendarWindow::Week
        } else {
            // Unknown token → today (handler may still show usage on free-form)
            CalendarWindow::Today
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            CalendarWindow::Today => "today",
            CalendarWindow::Tomorrow => "tomorrow",
            CalendarWindow::Week => "week",
        }
    }

    pub fn title(self) -> String {
        let now = Local::now();
        match self {
            CalendarWindow::Today => {
                format!("Today — {}", now.format("%A, %b %d"))
            }
            CalendarWindow::Tomorrow => {
                let d = now.date_naive() + Duration::days(1);
                format!("Tomorrow — {}", d.format("%A, %b %d"))
            }
            CalendarWindow::Week => {
                let (mon, sun) = local_week_monday_sunday(now.date_naive());
                format!(
                    "This week — {} → {}",
                    mon.format("%b %d"),
                    sun.format("%b %d")
                )
            }
        }
    }
}

/// Result of fetching events across linked family calendars.
#[derive(Debug)]
pub struct FamilyEventsFetch {
    pub events: Vec<CalendarEvent>,
    /// True if at least one member had a usable calendar client.
    pub any_client: bool,
    /// Typed per-member failures, while preserving successful members' events.
    pub errors: Vec<FamilyCalendarError>,
}

#[derive(Debug)]
pub struct FamilyCalendarError {
    pub member_id: String,
    pub member_name: String,
    pub source: CalendarError,
}

/// Overlapping timed event pair (same or cross-member).
#[derive(Debug, Clone)]
pub struct CalendarConflict {
    pub a: CalendarEvent,
    pub b: CalendarEvent,
}

/// Local civil day `[start, end)` in UTC.
pub fn local_day_bounds_utc(
    date_yyyy_mm_dd: &str,
) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
    let naive = NaiveDate::parse_from_str(date_yyyy_mm_dd, "%Y-%m-%d").ok()?;
    local_naive_day_bounds_utc(naive)
}

fn local_naive_day_bounds_utc(naive: NaiveDate) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
    naive_day_bounds_utc_in(&Local, naive)
}

fn naive_day_bounds_utc_in<Tz: TimeZone>(
    timezone: &Tz,
    naive: NaiveDate,
) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
    let next_day = naive.succ_opt()?;
    let start_local = timezone
        .from_local_datetime(&naive.and_hms_opt(0, 0, 0)?)
        .single()?;
    let end_local = timezone
        .from_local_datetime(&next_day.and_hms_opt(0, 0, 0)?)
        .single()?;
    Some((start_local.with_timezone(&Utc), end_local.with_timezone(&Utc)))
}

/// Full local calendar week Mon 00:00 → next Mon 00:00 (UTC).
pub fn week_bounds_utc(anchor: NaiveDate) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
    week_bounds_utc_in(&Local, anchor)
}

fn week_bounds_utc_in<Tz: TimeZone>(
    timezone: &Tz,
    anchor: NaiveDate,
) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
    let (monday, _) = local_week_monday_sunday(anchor);
    let next_monday = monday.checked_add_signed(Duration::days(7))?;
    let start_local = timezone
        .from_local_datetime(&monday.and_hms_opt(0, 0, 0)?)
        .single()?;
    let end_local = timezone
        .from_local_datetime(&next_monday.and_hms_opt(0, 0, 0)?)
        .single()?;
    Some((start_local.with_timezone(&Utc), end_local.with_timezone(&Utc)))
}

fn local_week_monday_sunday(anchor: NaiveDate) -> (NaiveDate, NaiveDate) {
    let days_from_monday = anchor.weekday().num_days_from_monday() as i64;
    let monday = anchor - Duration::days(days_from_monday);
    let sunday = monday + Duration::days(6);
    (monday, sunday)
}

/// UTC range for a calendar window relative to local now.
pub fn window_bounds_utc(window: CalendarWindow) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
    let today = Local::now().date_naive();
    match window {
        CalendarWindow::Today => local_naive_day_bounds_utc(today),
        CalendarWindow::Tomorrow => local_naive_day_bounds_utc(today + Duration::days(1)),
        CalendarWindow::Week => week_bounds_utc(today),
    }
}

pub fn is_all_day(ev: &CalendarEvent) -> bool {
    // Date-only Google events are stored as midnight UTC (see calendar::parse_datetime).
    // Checking local midnight would misclassify them in non-UTC zones (e.g. America/Toronto).
    let utc_midnight_span = ev.start.time().num_seconds_from_midnight() == 0
        && ev.end.time().num_seconds_from_midnight() == 0
        && (ev.end - ev.start) >= Duration::hours(23);
    if utc_midnight_span {
        return true;
    }

    // Fallback: local midnight spans (defensive if a timed all-day arrives differently).
    let start_local = ev.start.with_timezone(&Local);
    let end_local = ev.end.with_timezone(&Local);
    start_local.time().num_seconds_from_midnight() == 0
        && end_local.time().num_seconds_from_midnight() == 0
        && (end_local - start_local) >= Duration::hours(23)
}

/// Civil date used for agenda grouping/display.
/// All-day events keep the UTC date from Google's date-only field; timed events use local.
pub fn event_civil_day(ev: &CalendarEvent) -> String {
    if is_all_day(ev) {
        ev.start.format("%Y-%m-%d").to_string()
    } else {
        ev.start.with_timezone(&Local).format("%Y-%m-%d").to_string()
    }
}

pub fn is_declined(ev: &CalendarEvent) -> bool {
    ev.response_status
        .as_deref()
        .map(|s| s.eq_ignore_ascii_case("declined"))
        .unwrap_or(false)
}

/// Whether a family member's calendar should be fetched for this agenda scope.
///
/// `None` = household / unlinked chat (all members). `Some(id)` = linked personal
/// DM — only that member's calendar.
pub fn member_in_calendar_scope(member_id: &str, for_member_id: Option<&str>) -> bool {
    match for_member_id {
        Some(only) => member_id.eq_ignore_ascii_case(only),
        None => true,
    }
}

/// Fetch + merge events for linked family calendars in `[from, to)`.
///
/// When `for_member_id` is set (linked personal DM), only that member's calendar
/// is fetched so private chats do not see the household agenda.
pub async fn fetch_family_events(
    config: &AppConfig,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    for_member_id: Option<&str>,
) -> FamilyEventsFetch {
    let mut events: Vec<CalendarEvent> = Vec::new();
    let mut any_client = false;
    let mut errors: Vec<FamilyCalendarError> = Vec::new();

    for member in &config.family.members {
        if !member_in_calendar_scope(&member.id, for_member_id) {
            continue;
        }
        let Some(client) = build_calendar_client(member) else {
            continue;
        };
        any_client = true;
        match client.fetch_events(&member.name, from, to).await {
            Ok(mut member_events) => events.append(&mut member_events),
            Err(e) => {
                eprintln!(
                    "Calendar: fetch failed for {}: {:?}",
                    member.id, e
                );
                errors.push(FamilyCalendarError {
                    member_id: member.id.clone(),
                    member_name: member.name.clone(),
                    source: e,
                });
            }
        }
    }

    events.retain(|e| !is_declined(e));
    events.sort_by_key(|e| e.start);

    FamilyEventsFetch {
        events,
        any_client,
        errors,
    }
}

/// Timed-event overlaps (same or cross-member). All-day events are excluded.
pub fn find_conflicts(events: &[CalendarEvent]) -> Vec<CalendarConflict> {
    let mut timed: Vec<&CalendarEvent> = events.iter().filter(|e| !is_all_day(e)).collect();
    timed.sort_by_key(|e| e.start);
    let mut out = Vec::new();

    for i in 0..timed.len() {
        let a = timed[i];
        for b in timed.iter().skip(i + 1) {
            // Sorted by start: once b starts at/after a ends, later events can't overlap a.
            if b.start >= a.end {
                break;
            }
            // Half-open: [start, end) — touching endpoints are not conflicts.
            if a.start < b.end {
                // Skip identical Google event mirrored on two calendars (same id).
                if a.id == b.id {
                    continue;
                }
                out.push(CalendarConflict {
                    a: (*a).clone(),
                    b: (*b).clone(),
                });
            }
        }
    }

    out
}

pub fn format_event_when(ev: &CalendarEvent, reference_day: &str) -> String {
    if is_all_day(ev) {
        // Use UTC civil dates — matches how date-only Google events are stored.
        let start_day = ev.start.format("%Y-%m-%d").to_string();
        let end_day = (ev.end - Duration::seconds(1))
            .format("%Y-%m-%d")
            .to_string();
        if start_day == reference_day && end_day == reference_day {
            "all day".to_string()
        } else {
            format!("all day ({}→{})", start_day, end_day)
        }
    } else {
        format!("{}", ev.start.with_timezone(&Local).format("%H:%M"))
    }
}

fn format_event_line(ev: &CalendarEvent, reference_day: &str) -> String {
    let when = format_event_when(ev, reference_day);
    let title = escape_md(&truncate(&ev.title, 60));
    let who = escape_md(&ev.member_name);
    let loc = ev
        .location
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .map(|s| format!(" · {}", escape_md(&truncate(s, 40))))
        .unwrap_or_default();
    format!("• {} — {} ({}){}\n", when, title, who, loc)
}

fn format_conflict_line(c: &CalendarConflict) -> String {
    let overlap_start_dt = if c.a.start > c.b.start {
        c.a.start
    } else {
        c.b.start
    };
    let overlap_end_dt = if c.a.end < c.b.end { c.a.end } else { c.b.end };
    let day = overlap_start_dt
        .with_timezone(&Local)
        .format("%a %b %e")
        .to_string()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let overlap_start = overlap_start_dt.with_timezone(&Local).format("%H:%M");
    let overlap_end = overlap_end_dt.with_timezone(&Local).format("%H:%M");
    format!(
        "⚠ {} {}–{} — {} ({}) ∩ {} ({})\n",
        day,
        overlap_start,
        overlap_end,
        escape_md(&truncate(&c.a.title, 40)),
        escape_md(&c.a.member_name),
        escape_md(&truncate(&c.b.title, 40)),
        escape_md(&c.b.member_name),
    )
}

/// Format a compact timeline for one civil day (used by morning brief).
pub fn format_day_timeline(
    events: &[CalendarEvent],
    day_yyyy_mm_dd: &str,
    empty_msg: &str,
    cap: usize,
) -> String {
    let mut lines = String::new();
    if events.is_empty() {
        lines.push_str(empty_msg);
        if !empty_msg.ends_with('\n') {
            lines.push('\n');
        }
        return lines;
    }

    for ev in events.iter().take(cap) {
        lines.push_str(&format_event_line(ev, day_yyyy_mm_dd));
    }
    if events.len() > cap {
        lines.push_str(&format!("_…and {} more_\n", events.len() - cap));
    }
    lines
}

fn format_week_timeline(events: &[CalendarEvent], cap: usize) -> String {
    if events.is_empty() {
        return "_No events this week._\n".to_string();
    }

    let mut lines = String::new();
    let mut shown = 0usize;
    let mut last_day = String::new();

    for ev in events {
        if shown >= cap {
            break;
        }
        let day = event_civil_day(ev);
        if day != last_day {
            // %e pads day-of-month with a space; collapse for Telegram.
            // All-day: UTC civil date from Google date-only; timed: local wall clock.
            let heading = if is_all_day(ev) {
                format!("{}", ev.start.format("%a %b %e"))
            } else {
                format!("{}", ev.start.with_timezone(&Local).format("%a %b %e"))
            }
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
            lines.push_str(&format!("\n*{}*\n", escape_md(&heading)));
            last_day = day.clone();
        }
        lines.push_str(&format_event_line(ev, &day));
        shown += 1;
    }

    if events.len() > cap {
        lines.push_str(&format!("_…and {} more_\n", events.len() - cap));
    }
    lines
}

fn format_errors_footer(errors: &[FamilyCalendarError]) -> String {
    if errors.is_empty() {
        return String::new();
    }
    let member_names = errors
        .iter()
        .map(|error| error.member_name.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "_Calendar unavailable for: {}_\n",
        escape_md(&member_names)
    )
}

/// Full `/cal` Markdown reply for a window (timeline + conflicts).
///
/// When `for_member_id` is set (linked personal DM), only that member's calendar
/// is included.
pub async fn compose_calendar_agenda(
    config: &AppConfig,
    window: CalendarWindow,
    for_member_id: Option<&str>,
) -> String {
    let Some((from, to)) = window_bounds_utc(window) else {
        return "_Could not resolve calendar date bounds._".to_string();
    };

    let fetch = fetch_family_events(config, from, to, for_member_id).await;

    if !fetch.any_client {
        return format!(
            "📅 *{}*\n\n_No calendars linked — `/login calendar <member>`._",
            escape_md(&window.title())
        );
    }

    let mut out = format!("📅 *{}*\n", escape_md(&window.title()));

    match window {
        CalendarWindow::Today => {
            let today = Local::now().format("%Y-%m-%d").to_string();
            out.push('\n');
            out.push_str(&format_day_timeline(
                &fetch.events,
                &today,
                "_No events today._\n",
                30,
            ));
        }
        CalendarWindow::Tomorrow => {
            let day = (Local::now().date_naive() + Duration::days(1))
                .format("%Y-%m-%d")
                .to_string();
            out.push('\n');
            out.push_str(&format_day_timeline(
                &fetch.events,
                &day,
                "_No events tomorrow._\n",
                30,
            ));
        }
        CalendarWindow::Week => {
            out.push_str(&format_week_timeline(&fetch.events, 30));
        }
    }

    let conflicts = find_conflicts(&fetch.events);
    if !conflicts.is_empty() {
        out.push_str("\n*Conflicts*\n");
        for c in conflicts.iter().take(15) {
            out.push_str(&format_conflict_line(c));
        }
        if conflicts.len() > 15 {
            out.push_str(&format!("_…and {} more_\n", conflicts.len() - 15));
        }
    }

    out.push_str(&format_errors_footer(&fetch.errors));
    out
}

/// Morning-brief calendar section (today only, shorter cap, no conflicts).
///
/// When `for_member_id` is set (linked personal DM), only that member's calendar
/// is included.
pub async fn format_brief_calendar_section(
    config: &AppConfig,
    today: &str,
    for_member_id: Option<&str>,
) -> String {
    let Some((day_start_utc, day_end_utc)) = local_day_bounds_utc(today) else {
        return "_Could not resolve today's date bounds._\n".to_string();
    };

    let fetch = fetch_family_events(config, day_start_utc, day_end_utc, for_member_id).await;

    if !fetch.any_client {
        return "_No calendars linked — `/login calendar <member>`._\n".to_string();
    }

    let mut lines = format_day_timeline(&fetch.events, today, "_No events today._\n", 12);
    lines.push_str(&format_errors_footer(&fetch.errors));
    lines
}

pub fn truncate(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if count <= max {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{}…", truncated)
    }
}

pub fn escape_md(s: &str) -> String {
    s.replace('_', "\\_")
        .replace('*', "\\*")
        .replace('`', "\\`")
        .replace('[', "\\[")
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Weekday;

    fn timed_event(
        id: &str,
        title: &str,
        member: &str,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> CalendarEvent {
        CalendarEvent {
            id: id.to_string(),
            title: title.to_string(),
            start,
            end,
            location: None,
            description: None,
            member_name: member.to_string(),
            response_status: Some("accepted".to_string()),
        }
    }

    #[test]
    fn test_parse_window() {
        assert_eq!(CalendarWindow::parse(""), CalendarWindow::Today);
        assert_eq!(CalendarWindow::parse("tomorrow"), CalendarWindow::Tomorrow);
        assert_eq!(CalendarWindow::parse("this week"), CalendarWindow::Week);
        assert_eq!(CalendarWindow::parse("week"), CalendarWindow::Week);
    }

    #[test]
    fn test_find_conflicts_overlap_and_touch() {
        let t0 = Utc::now();
        let a = timed_event("1", "A", "Praj", t0, t0 + Duration::hours(1));
        let b = timed_event(
            "2",
            "B",
            "Sam",
            t0 + Duration::minutes(30),
            t0 + Duration::hours(2),
        );
        // Starts exactly when A ends — no overlap with A.
        let touch = timed_event(
            "3",
            "C",
            "Praj",
            t0 + Duration::hours(1),
            t0 + Duration::hours(2),
        );

        let conflicts = find_conflicts(&[a.clone(), b.clone()]);
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].a.id, "1");
        assert_eq!(conflicts[0].b.id, "2");

        // Touching endpoints are not conflicts.
        assert!(find_conflicts(&[a, touch]).is_empty());
    }

    #[test]
    fn test_is_all_day_utc_midnight_date_only() {
        // Mirrors calendar::parse_datetime for Google date-only events.
        let start = NaiveDate::from_ymd_opt(2026, 8, 3)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc();
        let end = NaiveDate::from_ymd_opt(2026, 8, 4)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc();
        let ev = CalendarEvent {
            id: "allday".to_string(),
            title: "Holiday".to_string(),
            start,
            end,
            location: None,
            description: None,
            member_name: "Praj".to_string(),
            response_status: None,
        };
        assert!(is_all_day(&ev));
        assert_eq!(event_civil_day(&ev), "2026-08-03");
        assert!(find_conflicts(&[ev]).is_empty());
    }

    #[test]
    fn test_declined_filtered_in_fetch_retain() {
        let mut ev = timed_event(
            "1",
            "Skip",
            "Praj",
            Utc::now(),
            Utc::now() + Duration::hours(1),
        );
        ev.response_status = Some("declined".to_string());
        assert!(is_declined(&ev));
    }

    #[test]
    fn test_week_monday() {
        // 2026-08-05 is Wednesday
        let wed = NaiveDate::from_ymd_opt(2026, 8, 5).unwrap();
        let (mon, sun) = local_week_monday_sunday(wed);
        assert_eq!(mon.weekday(), Weekday::Mon);
        assert_eq!(sun.weekday(), Weekday::Sun);
        assert_eq!(mon, NaiveDate::from_ymd_opt(2026, 8, 3).unwrap());
        assert_eq!(sun, NaiveDate::from_ymd_opt(2026, 8, 9).unwrap());
    }

    #[test]
    fn test_bounds_follow_civil_midnights_across_dst() {
        let timezone = chrono_tz::America::Toronto;

        let spring_forward = NaiveDate::from_ymd_opt(2026, 3, 8).unwrap();
        let (spring_start, spring_end) =
            naive_day_bounds_utc_in(&timezone, spring_forward).unwrap();
        assert_eq!(spring_end - spring_start, Duration::hours(23));

        let fall_back = NaiveDate::from_ymd_opt(2026, 11, 1).unwrap();
        let (fall_start, fall_end) = naive_day_bounds_utc_in(&timezone, fall_back).unwrap();
        assert_eq!(fall_end - fall_start, Duration::hours(25));

        let week_anchor = NaiveDate::from_ymd_opt(2026, 3, 4).unwrap();
        let (week_start, week_end) = week_bounds_utc_in(&timezone, week_anchor).unwrap();
        let start_local = week_start.with_timezone(&timezone);
        let end_local = week_end.with_timezone(&timezone);
        assert_eq!(start_local.weekday(), Weekday::Mon);
        assert_eq!(end_local.weekday(), Weekday::Mon);
        assert_eq!(start_local.time().num_seconds_from_midnight(), 0);
        assert_eq!(end_local.time().num_seconds_from_midnight(), 0);
        assert_eq!(week_end - week_start, Duration::hours(167));
    }

    #[test]
    fn test_same_id_not_conflict() {
        let t0 = Utc::now();
        let a = timed_event("same", "Meet", "Praj", t0, t0 + Duration::hours(1));
        let mut b = a.clone();
        b.member_name = "Sam".to_string();
        assert!(find_conflicts(&[a, b]).is_empty());
    }

    #[test]
    fn test_member_in_calendar_scope() {
        assert!(member_in_calendar_scope("alex", None));
        assert!(member_in_calendar_scope("jordan", None));
        assert!(member_in_calendar_scope("alex", Some("alex")));
        assert!(member_in_calendar_scope("Alex", Some("alex")));
        assert!(!member_in_calendar_scope("jordan", Some("alex")));
        assert!(!member_in_calendar_scope("alex", Some("jordan")));
    }
}
