//! Morning brief: calendar, open tasks, bills due, yesterday's nutrition vs goals.

use chrono::{Duration, Local, TimeZone, Timelike, Utc};
use chotu_common::{
    build_calendar_client, AppConfig, CalendarEvent, HealthFamilySummary,
};
use sqlx::SqlitePool;

#[derive(Debug, Clone, sqlx::FromRow)]
struct BriefTaskRow {
    id: String,
    title: String,
    due_date: Option<String>,
    assigned_to: Option<String>,
}

#[derive(Debug, Clone, sqlx::FromRow)]
struct BriefBillRow {
    biller: String,
    amount: Option<f64>,
    due_date: Option<String>,
}

/// Assemble a Markdown morning brief for Telegram.
pub async fn compose_morning_brief(pool: &SqlitePool, config: &AppConfig) -> String {
    let now = Local::now();
    let today = now.format("%Y-%m-%d").to_string();
    let yesterday = (now.date_naive() - Duration::days(1))
        .format("%Y-%m-%d")
        .to_string();
    let weekday = now.format("%A, %b %d, %Y");

    let mut out = format!("☀️ *Morning Brief* — {}\n", weekday);

    out.push_str("\n📅 *Today*\n");
    out.push_str(&format_calendar_section(config, &today).await);

    out.push_str("\n✅ *Tasks*\n");
    out.push_str(&format_tasks_section(pool, &today).await);

    out.push_str("\n💳 *Bills*\n");
    out.push_str(&format_bills_section(pool, &today).await);

    out.push_str(&format!("\n🥗 *Nutrition* (yesterday {})\n", yesterday));
    out.push_str(&format_nutrition_section(pool, config, &yesterday).await);

    out
}

async fn format_calendar_section(config: &AppConfig, today: &str) -> String {
    let (day_start_utc, day_end_utc) = match local_day_bounds_utc(today) {
        Some(bounds) => bounds,
        None => return "_Could not resolve today's date bounds._\n".to_string(),
    };

    let mut events: Vec<CalendarEvent> = Vec::new();
    let mut any_client = false;
    let mut errors: Vec<String> = Vec::new();

    for member in &config.family.members {
        let Some(client) = build_calendar_client(member) else {
            continue;
        };
        any_client = true;
        match client
            .fetch_events(&member.name, day_start_utc, day_end_utc)
            .await
        {
            Ok(mut member_events) => events.append(&mut member_events),
            Err(e) => {
                eprintln!(
                    "Morning brief: calendar fetch failed for {}: {:?}",
                    member.id, e
                );
                errors.push(member.name.clone());
            }
        }
    }

    if !any_client {
        return "_No calendars linked — `/login calendar <member>`._\n".to_string();
    }

    events.sort_by_key(|e| e.start);

    let mut lines = String::new();
    if events.is_empty() {
        lines.push_str("_No events today._\n");
    } else {
        for ev in events.iter().take(12) {
            let when = format_event_when(ev, today);
            let title = escape_md(&truncate(&ev.title, 60));
            let who = escape_md(&ev.member_name);
            let loc = ev
                .location
                .as_deref()
                .filter(|s| !s.trim().is_empty())
                .map(|s| format!(" · {}", escape_md(&truncate(s, 40))))
                .unwrap_or_default();
            lines.push_str(&format!("• {} — {} ({}){}\n", when, title, who, loc));
        }
        if events.len() > 12 {
            lines.push_str(&format!("_…and {} more_\n", events.len() - 12));
        }
    }

    if !errors.is_empty() {
        lines.push_str(&format!(
            "_Calendar unavailable for: {}_\n",
            escape_md(&errors.join(", "))
        ));
    }

    lines
}

async fn format_tasks_section(pool: &SqlitePool, today: &str) -> String {
    let Some(today_date) = NaiveDateExt::parse(today) else {
        return "_Could not load tasks._\n".to_string();
    };
    let earliest = (today_date - Duration::days(31))
        .format("%Y-%m-%d")
        .to_string();
    let horizon = (today_date + Duration::days(3))
        .format("%Y-%m-%d")
        .to_string();

    let rows: Vec<BriefTaskRow> = match sqlx::query_as::<_, BriefTaskRow>(
        "SELECT id, title, due_date, assigned_to FROM tasks \
         WHERE status = 'open' \
         ORDER BY due_date IS NULL, due_date ASC, created_at DESC LIMIT 40",
    )
    .fetch_all(pool)
    .await
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Morning brief: tasks query failed: {:?}", e);
            return "_Could not load tasks._\n".to_string();
        }
    };

    let mut prioritized: Vec<&BriefTaskRow> = rows
        .iter()
        .filter(|t| match t.due_date.as_deref() {
            // Undated open tasks still belong on the brief.
            None => true,
            // Skip ancient overdue; keep last ~month through next few days.
            Some(d) => d >= earliest.as_str() && d <= horizon.as_str(),
        })
        .collect();

    // Prefer due/overdue first: already ordered by SQL for dated rows.
    prioritized.truncate(10);

    if prioritized.is_empty() {
        return "_No open tasks due soon._\n".to_string();
    }

    let mut lines = String::new();
    for t in prioritized {
        let short_id: String = t.id.chars().take(8).collect();
        let due = match t.due_date.as_deref() {
            Some(d) if d < today => format!(" · *overdue* {}", d),
            Some(d) if d == today => " · *due today*".to_string(),
            Some(d) => format!(" · due {}", d),
            None => String::new(),
        };
        let assignee = t
            .assigned_to
            .as_deref()
            .map(|a| format!(" · @{}", a))
            .unwrap_or_default();
        lines.push_str(&format!(
            "• `{}` {}{}{}\n",
            short_id,
            escape_md(&truncate(&t.title, 70)),
            due,
            assignee
        ));
    }
    lines
}

async fn format_bills_section(pool: &SqlitePool, today: &str) -> String {
    let Some(today_date) = NaiveDateExt::parse(today) else {
        return "_Could not load bills._\n".to_string();
    };
    // Ignore ancient "unpaid" rows from email history — only recent overdue + near-term.
    let earliest = (today_date - Duration::days(31))
        .format("%Y-%m-%d")
        .to_string();
    let horizon = (today_date + Duration::days(7))
        .format("%Y-%m-%d")
        .to_string();

    let rows: Vec<BriefBillRow> = match sqlx::query_as::<_, BriefBillRow>(
        "SELECT biller, amount, due_date FROM upcoming_bills \
         WHERE status = 'unpaid' \
           AND due_date IS NOT NULL \
           AND due_date >= ? \
           AND due_date <= ? \
         ORDER BY due_date ASC LIMIT 15",
    )
    .bind(&earliest)
    .bind(&horizon)
    .fetch_all(pool)
    .await
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Morning brief: bills query failed: {:?}", e);
            return "_Could not load bills._\n".to_string();
        }
    };

    if rows.is_empty() {
        return "_No unpaid bills due in the past month or next 7 days._\n".to_string();
    }

    let mut lines = String::new();
    for b in rows {
        let amount = b
            .amount
            .map(|a| format!(" ${:.2}", a))
            .unwrap_or_default();
        let due = b.due_date.as_deref().unwrap_or("?");
        let urgency = if due < today {
            " *overdue*"
        } else if due == today {
            " *due today*"
        } else {
            ""
        };
        lines.push_str(&format!(
            "• {}{} — due {}{}\n",
            escape_md(&truncate(&b.biller, 40)),
            amount,
            due,
            urgency
        ));
    }
    lines
}

async fn format_nutrition_section(
    pool: &SqlitePool,
    config: &AppConfig,
    yesterday: &str,
) -> String {
    let healths: Vec<HealthFamilySummary> = match sqlx::query_as::<_, HealthFamilySummary>(
        "SELECT * FROM health_family_summary WHERE date = ?",
    )
    .bind(yesterday)
    .fetch_all(pool)
    .await
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Morning brief: nutrition query failed: {:?}", e);
            return "_Could not load nutrition._\n".to_string();
        }
    };

    let mut by_id: std::collections::HashMap<String, HealthFamilySummary> = healths
        .into_iter()
        .map(|h| (h.family_member_id.clone(), h))
        .collect();

    let mut lines = String::new();
    let mut any = false;

    for member in &config.family.members {
        let h = by_id.remove(&member.id);
        let goals = member.nutrition_goals.as_ref();

        let (cal, protein, carbs, fats, fiber, steps) = match &h {
            Some(h) => (
                h.total_calories_ingested,
                h.protein_grams,
                h.carbs_grams,
                h.fats_grams,
                h.fiber_g,
                h.step_count,
            ),
            None => (0, 0.0, 0.0, 0.0, 0.0, 0),
        };

        let has_data = cal > 0 || protein > 0.0 || carbs > 0.0 || fats > 0.0 || steps > 0;
        if !has_data && goals.map(|g| g.is_empty()).unwrap_or(true) {
            continue;
        }
        any = true;

        lines.push_str(&format!("• *{}*: ", escape_md(&member.name)));
        if let Some(goals) = goals.filter(|g| !g.is_empty()) {
            let mut bits = Vec::new();
            if let Some(g) = goals.calories {
                bits.push(format!("{}/{} kcal", cal, g));
            } else if cal > 0 {
                bits.push(format!("{} kcal", cal));
            }
            if let Some(g) = goals.protein_g {
                bits.push(format!("P {:.0}/{:.0}g", protein, g));
            }
            if let Some(g) = goals.carbs_g {
                bits.push(format!("C {:.0}/{:.0}g", carbs, g));
            }
            if let Some(g) = goals.fats_g {
                bits.push(format!("F {:.0}/{:.0}g", fats, g));
            }
            if let Some(g) = goals.fiber_g {
                bits.push(format!("fiber {:.0}/{:.0}g", fiber, g));
            }
            if let Some(g) = goals.steps {
                bits.push(format!("{}/{} steps", steps, g));
            }
            if bits.is_empty() {
                lines.push_str("_no intake logged_");
            } else {
                lines.push_str(&bits.join(" · "));
            }
        } else if has_data {
            lines.push_str(&format!(
                "{} kcal · P {:.0}g · C {:.0}g · F {:.0}g",
                cal, protein, carbs, fats
            ));
        } else {
            lines.push_str("_no intake logged_");
        }
        lines.push('\n');
    }

    if !any {
        lines.push_str("_No nutrition logged yesterday._\n");
    }
    lines
}

fn local_day_bounds_utc(
    date_yyyy_mm_dd: &str,
) -> Option<(chrono::DateTime<Utc>, chrono::DateTime<Utc>)> {
    let naive = chrono::NaiveDate::parse_from_str(date_yyyy_mm_dd, "%Y-%m-%d").ok()?;
    let start_local = Local
        .from_local_datetime(&naive.and_hms_opt(0, 0, 0)?)
        .single()?;
    let end_local = start_local + Duration::days(1);
    Some((start_local.with_timezone(&Utc), end_local.with_timezone(&Utc)))
}

fn format_event_when(ev: &CalendarEvent, today: &str) -> String {
    let start_local = ev.start.with_timezone(&Local);
    let end_local = ev.end.with_timezone(&Local);

    // All-day heuristic: midnight-to-midnight (or multi-day spanning midnight UTC from date-only)
    let all_day = start_local.time().num_seconds_from_midnight() == 0
        && end_local.time().num_seconds_from_midnight() == 0
        && (end_local - start_local) >= Duration::hours(23);

    if all_day {
        let start_day = start_local.format("%Y-%m-%d").to_string();
        let end_day = (end_local - Duration::seconds(1))
            .format("%Y-%m-%d")
            .to_string();
        if start_day == today && end_day == today {
            "all day".to_string()
        } else {
            format!("all day ({}→{})", start_day, end_day)
        }
    } else {
        format!("{}", start_local.format("%H:%M"))
    }
}

fn truncate(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if count <= max {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{}…", truncated)
    }
}

fn escape_md(s: &str) -> String {
    s.replace('_', "\\_")
        .replace('*', "\\*")
        .replace('`', "\\`")
        .replace('[', "\\[")
}

/// Tiny helper so date math stays readable without sprinkling parse_from_str.
struct NaiveDateExt;
impl NaiveDateExt {
    fn parse(date: &str) -> Option<chrono::NaiveDate> {
        chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d").ok()
    }
}
