//! Morning brief: calendar, open tasks, bills due, yesterday's nutrition vs goals, training.

use chrono::{Duration, Local};
use chotu_common::{
    escape_md, format_brief_calendar_section, truncate, AppConfig, HealthFamilySummary,
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
///
/// When `for_member_id` is set (linked personal DM), calendar, tasks, nutrition,
/// and training sections are scoped to that member so private chats do not see
/// the household surface. Shared bills stay household-wide by design.
/// Household shared chats (`None`) keep the family-wide view.
pub async fn compose_morning_brief(
    pool: &SqlitePool,
    config: &AppConfig,
    for_member_id: Option<&str>,
) -> String {
    let now = Local::now();
    let today = now.format("%Y-%m-%d").to_string();
    let yesterday = (now.date_naive() - Duration::days(1))
        .format("%Y-%m-%d")
        .to_string();
    let weekday = now.format("%A, %b %d, %Y");

    let mut out = format!("☀️ *Morning Brief* — {}\n", weekday);
    if let Some(mid) = for_member_id {
        if let Some(m) = config
            .family
            .members
            .iter()
            .find(|m| m.id.eq_ignore_ascii_case(mid))
        {
            out.push_str(&format!("_Private for {}_\n", escape_md(&m.name)));
        }
    }

    out.push_str("\n📅 *Today*\n");
    out.push_str(&format_brief_calendar_section(config, &today, for_member_id).await);

    out.push_str("\n✅ *Tasks*\n");
    out.push_str(&format_tasks_section(pool, &today, for_member_id).await);

    out.push_str("\n💳 *Bills*\n");
    out.push_str(&format_bills_section(pool, &today).await);

    out.push_str(&format!("\n🥗 *Nutrition* (yesterday {})\n", yesterday));
    out.push_str(&format_nutrition_section(pool, config, &yesterday, for_member_id).await);

    out.push_str("\n🏋️ *Training*\n");
    out.push_str(
        &format_training_section(pool, config, now.date_naive(), &yesterday, for_member_id).await,
    );

    out
}

/// Open tasks visible in a morning brief for the given chat scope.
///
/// Linked DMs (`Some`): assignee match or unassigned — same rule as
/// `/tasks complete all`. Household (`None`): all open tasks.
fn task_in_brief_scope(assigned_to: Option<&str>, for_member_id: Option<&str>) -> bool {
    match for_member_id {
        None => true,
        Some(mid) => match assigned_to {
            None => true,
            Some(a) => a.eq_ignore_ascii_case(mid),
        },
    }
}

async fn format_tasks_section(
    pool: &SqlitePool,
    today: &str,
    for_member_id: Option<&str>,
) -> String {
    let Some(today_date) = NaiveDateExt::parse(today) else {
        return "_Could not load tasks._\n".to_string();
    };
    let earliest = (today_date - Duration::days(31))
        .format("%Y-%m-%d")
        .to_string();
    let horizon = (today_date + Duration::days(3))
        .format("%Y-%m-%d")
        .to_string();

    let mut qb = sqlx::QueryBuilder::new(
        "SELECT id, title, due_date, assigned_to FROM tasks WHERE status = 'open'",
    );
    if let Some(member_id) = for_member_id {
        // COLLATE NOCASE matches task_in_brief_scope's eq_ignore_ascii_case so
        // casing differences don't get dropped before LIMIT 40.
        qb.push(" AND (assigned_to = ");
        qb.push_bind(member_id);
        qb.push(" COLLATE NOCASE OR assigned_to IS NULL)");
    }
    qb.push(" ORDER BY due_date IS NULL, due_date ASC, created_at DESC LIMIT 40");

    let rows: Vec<BriefTaskRow> = match qb.build_query_as::<BriefTaskRow>().fetch_all(pool).await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Morning brief: tasks query failed: {:?}", e);
            return "_Could not load tasks._\n".to_string();
        }
    };

    let mut prioritized: Vec<&BriefTaskRow> = rows
        .iter()
        .filter(|t| task_in_brief_scope(t.assigned_to.as_deref(), for_member_id))
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
    for_member_id: Option<&str>,
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
        if let Some(only) = for_member_id {
            if !member.id.eq_ignore_ascii_case(only) {
                continue;
            }
        }
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

async fn format_training_section(
    pool: &SqlitePool,
    config: &AppConfig,
    today: chrono::NaiveDate,
    yesterday: &str,
    for_member_id: Option<&str>,
) -> String {
    let week_start = health_coach::week_start_monday(today)
        .format("%Y-%m-%d")
        .to_string();
    let mut lines = String::new();
    let mut any = false;

    for member in &config.family.members {
        if let Some(only) = for_member_id {
            if !member.id.eq_ignore_ascii_case(only) {
                continue;
            }
        }
        let Some(fitness) = member.fitness_goals.as_ref().filter(|g| !g.is_empty()) else {
            continue;
        };
        any = true;

        let stored_plan = health_coach::load_weekly_plan(pool, &member.id, &week_start)
            .await
            .ok()
            .flatten();

        let planned_line = stored_plan.as_ref().and_then(|stored| {
            health_coach::session_for_date_from_stored(stored, today).map(|p| {
                let notes = p.notes.trim();
                if notes.is_empty() {
                    format!("{} ({})", p.weekday, p.kind.as_str())
                } else {
                    format!("{} ({}): {}", p.weekday, p.kind.as_str(), notes)
                }
            })
        });

        let yesterday_ex = health_coach::exercises_for_day(pool, &member.id, yesterday)
            .await
            .unwrap_or_default();

        lines.push_str(&format!("• *{}*\n", escape_md(&member.name)));
        lines.push_str(&health_coach::fitness_brief_lines(
            fitness,
            today,
            planned_line.as_deref(),
            &yesterday_ex,
        ));

        if let Some(stored) = stored_plan.as_ref() {
            if let Ok(plan) = health_coach::parse_plan_json(&stored.plan_json) {
                let week_end = (health_coach::week_start_monday(today) + chrono::Duration::days(6))
                    .format("%Y-%m-%d")
                    .to_string();
                if let Ok(week_ex) = health_coach::exercise_entries_for_range(
                    pool,
                    &member.id,
                    &week_start,
                    &week_end,
                )
                .await
                {
                    let labels: Vec<(String, String)> = week_ex
                        .iter()
                        .map(|e| (e.date.clone(), e.activity_label()))
                        .collect();
                    let (matched, planned) = health_coach::plan_session_adherence(
                        &week_start,
                        &plan,
                        today,
                        &labels,
                    );
                    let week_cardio = health_coach::sum_cardio_minutes(
                        week_ex
                            .iter()
                            .map(|e| (e.activity_label(), e.duration_mins())),
                    );
                    let duration_rows: Vec<(String, String, i32)> = week_ex
                        .iter()
                        .map(|e| (e.date.clone(), e.activity_label(), e.duration_mins()))
                        .collect();
                    let plan_cardio = health_coach::plan_cardio_minutes_on_cardio_days(
                        &week_start,
                        &plan,
                        today,
                        &duration_rows,
                    );
                    lines.push_str("  - ");
                    lines.push_str(&health_coach::format_plan_progress_line(
                        matched,
                        planned,
                        week_cardio,
                        plan_cardio,
                    ));
                    lines.push('\n');
                }
            }
        }
    }

    if !any {
        lines.push_str("_No fitness_goals configured — add them in config.yaml, then `/plan`._\n");
    }
    lines
}

/// Tiny helper so date math stays readable without sprinkling parse_from_str.
struct NaiveDateExt;
impl NaiveDateExt {
    fn parse(date: &str) -> Option<chrono::NaiveDate> {
        chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d").ok()
    }
}

#[cfg(test)]
mod tests {
    use super::task_in_brief_scope;

    #[test]
    fn household_brief_shows_all_assignees() {
        assert!(task_in_brief_scope(None, None));
        assert!(task_in_brief_scope(Some("alex"), None));
        assert!(task_in_brief_scope(Some("jordan"), None));
    }

    #[test]
    fn linked_dm_brief_keeps_mine_and_unassigned() {
        assert!(task_in_brief_scope(None, Some("alex")));
        assert!(task_in_brief_scope(Some("alex"), Some("alex")));
        assert!(task_in_brief_scope(Some("Alex"), Some("alex")));
        assert!(!task_in_brief_scope(Some("jordan"), Some("alex")));
        assert!(!task_in_brief_scope(Some("alex"), Some("jordan")));
    }
}
