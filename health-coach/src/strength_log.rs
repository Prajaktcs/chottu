//! Gym workouts logged set by set from Signal (`/log`) and double-progression targets.
//!
//! `/log` opens a durable workout session for the member's local day. While it is
//! open, short plain messages (`180x12`, `x11`, `next`, `undo`, `done`) log sets
//! against today's planned exercises. Anything outside that grammar is left for the
//! normal message handlers.

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, NaiveDate, Utc};
use sqlx::SqlitePool;
use std::collections::{BTreeSet, HashMap};

use crate::fitness_plan::{
    load_weekly_plan, session_for_date_from_stored, week_start_monday, PlanDay, PlanDayKind,
    PlanExercise,
};

/// Idle time after which an open workout is closed automatically.
const SESSION_IDLE_HOURS: i64 = 3;
const MAX_REPS: u32 = 100;
const MAX_WEIGHT_LB: f64 = 1500.0;
const MAX_SETS_PER_TOKEN: u32 = 10;
const MAX_NAME_WORDS: usize = 5;
/// Weeks of history summarized for the plan prompt.
const PROMPT_HISTORY_DAYS: i64 = 28;
const PROMPT_MAX_LIFTS: usize = 15;

const SHORT_HELP: &str =
    "Send `180x12` per set · `x11` same weight · `next` or `#3` to switch · `undo` · `done`";

const LOG_USAGE: &str = "Usage: `/log` starts today's workout. Then send sets like `135x8` \
(lb × reps), `135x8x3` (3 sets), `x8` (same weight), `bwx12` (bodyweight). `#2` or `next` \
switches exercise, `bench 135x8` logs a lift by name, `undo` removes the last set, `done` finishes.";

// ─── Input grammar ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq)]
enum Load {
    Weight(f64),
    Bodyweight,
    /// `x8` or bare `8`: same load as the previous set of this exercise.
    Same,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct SetInput {
    load: Load,
    reps: u32,
    count: u32,
}

#[derive(Debug, Clone, PartialEq)]
enum WorkoutInput {
    Sets {
        exercise: Option<String>,
        sets: Vec<SetInput>,
    },
    /// 1-based plan exercise number.
    Jump(usize),
    Next,
    Undo,
    Done,
}

/// `135x8`, `135x8x3`, `x8`, `bwx12`, `8` (lb; `lb`/`lbs` suffix on weight allowed).
fn parse_set_token(token: &str) -> Option<SetInput> {
    let parse_reps = |raw: &str| {
        raw.parse::<u32>()
            .ok()
            .filter(|r| (1..=MAX_REPS).contains(r))
    };
    if token.bytes().all(|b| b.is_ascii_digit()) {
        return Some(SetInput {
            load: Load::Same,
            reps: parse_reps(token)?,
            count: 1,
        });
    }
    let parts: Vec<&str> = token.split('x').collect();
    let (load_raw, reps_raw, count_raw) = match parts.as_slice() {
        [load, reps] => (*load, *reps, None),
        [load, reps, count] => (*load, *reps, Some(*count)),
        _ => return None,
    };
    let load = match load_raw {
        "" => Load::Same,
        "bw" => Load::Bodyweight,
        raw => {
            let number = raw
                .strip_suffix("lbs")
                .or_else(|| raw.strip_suffix("lb"))
                .unwrap_or(raw);
            let weight = number.parse::<f64>().ok()?;
            if !(weight > 0.0 && weight <= MAX_WEIGHT_LB) {
                return None;
            }
            Load::Weight(weight)
        }
    };
    let count = match count_raw {
        None => 1,
        Some(raw) => raw
            .parse::<u32>()
            .ok()
            .filter(|c| (1..=MAX_SETS_PER_TOKEN).contains(c))?,
    };
    Some(SetInput {
        load,
        reps: parse_reps(reps_raw)?,
        count,
    })
}

/// Lowercase, unify `×`/`*`, drop commas, and join `135 x 8 x 3` into `135x8x3`.
/// `135x8 x8` stays two sets: a glued `x8` only joins a bare load.
fn normalize_tokens(text: &str) -> Vec<String> {
    let lowered = text
        .trim()
        .to_lowercase()
        .replace(['×', '*'], "x")
        .replace(',', " ");
    let mut tokens: Vec<String> = Vec::new();
    let mut words = lowered.split_whitespace();
    while let Some(word) = words.next() {
        let glued_reps = word.len() > 1
            && word.starts_with('x')
            && word[1..].bytes().all(|b| b.is_ascii_digit());
        match tokens.last_mut() {
            Some(prev) if prev.ends_with(|c: char| c.is_ascii_digit()) || prev == "bw" => {
                let separators = prev.matches('x').count();
                if word == "x" && separators < 2 {
                    prev.push('x');
                    prev.push_str(words.next().unwrap_or_default());
                } else if glued_reps && separators == 0 {
                    prev.push_str(word);
                } else {
                    tokens.push(word.to_string());
                }
            }
            _ => tokens.push(word.to_string()),
        }
    }
    tokens
}

/// Strict grammar; `None` means the message is not workout input.
fn parse_workout_input(text: &str) -> Option<WorkoutInput> {
    let tokens = normalize_tokens(text);
    match tokens
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        [] => return None,
        ["done" | "finish" | "finished" | "end"] => return Some(WorkoutInput::Done),
        ["undo"] => return Some(WorkoutInput::Undo),
        ["next" | "skip"] => return Some(WorkoutInput::Next),
        [single] if single.starts_with('#') => {
            return single[1..]
                .parse::<usize>()
                .ok()
                .filter(|n| *n >= 1)
                .map(WorkoutInput::Jump);
        }
        _ => {}
    }

    let first_set = tokens
        .iter()
        .rposition(|t| parse_set_token(t).is_none())
        .map_or(0, |i| i + 1);
    if first_set == tokens.len() {
        return None;
    }
    let sets: Vec<SetInput> = tokens[first_set..]
        .iter()
        .filter_map(|t| parse_set_token(t))
        .collect();
    let name_words = &tokens[..first_set];
    if name_words.is_empty() {
        return Some(WorkoutInput::Sets {
            exercise: None,
            sets,
        });
    }
    // A named lift needs an explicit `x` set so prose like "ate 2" is never captured.
    let explicit = tokens[first_set..].iter().any(|t| t.contains('x'));
    let starts_with_letter = name_words[0].starts_with(|c: char| c.is_alphabetic());
    if !explicit || !starts_with_letter || name_words.len() > MAX_NAME_WORDS {
        return None;
    }
    Some(WorkoutInput::Sets {
        exercise: Some(name_words.join(" ")),
        sets,
    })
}

/// Stable identity across weeks: lowercase alphanumerics only ("Pull-ups" == "pullups").
pub fn exercise_key(name: &str) -> String {
    name.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// Index of the plan exercise `query` names: exact key, else a unique word-prefix match.
fn match_plan_exercise(exercises: &[PlanExercise], query: &str) -> Option<usize> {
    let key = exercise_key(query);
    if let Some(i) = exercises.iter().position(|e| exercise_key(&e.name) == key) {
        return Some(i);
    }
    let query_words: Vec<String> = query.split_whitespace().map(exercise_key).collect();
    let mut matches = exercises.iter().enumerate().filter(|(_, e)| {
        let words: Vec<String> = e.name.split_whitespace().map(exercise_key).collect();
        query_words
            .iter()
            .all(|q| !q.is_empty() && words.iter().any(|w| w.starts_with(q.as_str())))
    });
    let (index, _) = matches.next()?;
    matches.next().is_none().then_some(index)
}

// ─── Progression ────────────────────────────────────────────────────────────

/// One logged set: `(weight_lb, reps)`; `None` weight is bodyweight.
pub type SetRecord = (Option<f64>, u32);

/// Next-session prescription for one exercise.
#[derive(Debug, Clone, PartialEq)]
pub struct Target {
    pub weight_lb: Option<f64>,
    /// Reps to aim for per set; empty when there is no history yet.
    pub reps: Vec<u32>,
    pub note: Option<String>,
}

/// Rep range from a plan string ("8-10", "8 to 12", "12 each side"). Time or distance → `None`.
pub fn rep_range(reps: &str) -> Option<(u32, u32)> {
    let s = reps.trim().to_lowercase();
    let leading_number = |s: &str| -> Option<(u32, usize)> {
        let end = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
        s[..end].parse().ok().map(|n| (n, end))
    };
    let (low, end) = leading_number(&s)?;
    let mut rest = s[end..].trim_start();
    let mut high = low;
    for separator in ["-", "–", "to "] {
        if let Some(after) = rest.strip_prefix(separator) {
            let after = after.trim_start();
            let (n, end) = leading_number(after)?;
            high = n;
            rest = after[end..].trim_start();
            break;
        }
    }
    let timed = ["s", "sec", "min", "m", "km", "mi", "'", "\""]
        .iter()
        .any(|unit| rest.starts_with(unit) && !rest.starts_with("each"));
    if timed || low == 0 {
        return None;
    }
    Some((low.min(high), low.max(high)))
}

/// Load step: heavier lower-body lifts move in 10 lb, everything else in 5 lb.
fn increment_lb(exercise_name: &str) -> f64 {
    let key = exercise_key(exercise_name);
    let lower = [
        "squat",
        "deadlift",
        "legpress",
        "hipthrust",
        "rdl",
        "romanian",
        "hack",
    ];
    if lower.iter().any(|k| key.contains(k)) {
        10.0
    } else {
        5.0
    }
}

fn round_to_5(weight: f64) -> f64 {
    ((weight / 5.0).round() * 5.0).max(5.0)
}

/// Heaviest load in a session and the reps done at it, in logged order.
fn working_sets(session: &[SetRecord]) -> (Option<f64>, Vec<u32>) {
    let top = session
        .iter()
        .filter_map(|(w, _)| *w)
        .fold(None, |acc: Option<f64>, w| {
            Some(acc.map_or(w, |a| a.max(w)))
        });
    let reps = session
        .iter()
        .filter(|(w, _)| *w == top)
        .map(|(_, r)| *r)
        .collect();
    (top, reps)
}

/// Double progression from recent sessions (newest first).
///
/// All working sets at the top of the range → add load and restart at the bottom.
/// Two sessions in a row short of the bottom at the same load → deload 10%.
/// Otherwise hold the load and add a rep per set.
pub fn next_target(
    exercise_name: &str,
    range: Option<(u32, u32)>,
    planned_sets: u32,
    history: &[Vec<SetRecord>],
) -> Option<Target> {
    let (low, high) = range?;
    let sets = planned_sets.max(1) as usize;
    let Some(last) = history.first().filter(|s| !s.is_empty()) else {
        return Some(Target {
            weight_lb: None,
            reps: Vec::new(),
            note: Some(format!(
                "first time: pick a weight you can lift {high} times at RPE 7"
            )),
        });
    };
    let (weight, reps) = working_sets(last);
    if reps.len() >= sets && reps.iter().all(|r| *r >= high) {
        return Some(match weight {
            Some(w) => {
                let step = increment_lb(exercise_name);
                Target {
                    weight_lb: Some(w + step),
                    reps: vec![low; sets],
                    note: Some(format!("+{} lb", format_weight(step))),
                }
            }
            None => Target {
                weight_lb: None,
                reps: vec![high; sets],
                note: Some("all reps hit — try a harder variation or add weight".into()),
            },
        });
    }
    if let (Some(w), Some(previous)) = (weight, history.get(1)) {
        let (prev_weight, prev_reps) = working_sets(previous);
        let short = |reps: &[u32]| reps.iter().max().is_some_and(|best| *best < low);
        if prev_weight == Some(w) && short(&reps) && short(&prev_reps) {
            return Some(Target {
                weight_lb: Some(round_to_5(w * 0.9)),
                reps: vec![low; sets],
                note: Some("deload after two short sessions".into()),
            });
        }
    }
    let aim = (0..sets)
        .map(|i| {
            let done = reps.get(i).or(reps.last()).copied().unwrap_or(low);
            (done + 1).clamp(low, high)
        })
        .collect();
    Some(Target {
        weight_lb: weight,
        reps: aim,
        note: None,
    })
}

fn format_weight(weight: f64) -> String {
    if weight.fract() == 0.0 {
        format!("{weight:.0}")
    } else {
        format!("{weight:.1}")
    }
}

fn format_reps(reps: &[u32]) -> String {
    reps.iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join("/")
}

/// "180 lb × 12/11/10, bw × 8"
fn format_sets(sets: &[SetRecord]) -> String {
    let mut groups: Vec<(Option<f64>, Vec<u32>)> = Vec::new();
    for (weight, reps) in sets {
        match groups.last_mut() {
            Some((w, rs)) if w == weight => rs.push(*reps),
            _ => groups.push((*weight, vec![*reps])),
        }
    }
    groups
        .iter()
        .map(|(weight, reps)| {
            let load = weight.map_or("bw".to_string(), |w| format!("{} lb", format_weight(w)));
            format!("{load} × {}", format_reps(reps))
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Suffix appended after a plan exercise line, e.g. " @ 180 lb, aim 12/12/11 (+10 lb)".
fn target_suffix(target: &Target) -> String {
    let note = target.note.as_deref().unwrap_or_default();
    if target.reps.is_empty() {
        return format!(" · {note}");
    }
    let mut out = match target.weight_lb {
        Some(w) => format!(
            " @ {} lb, aim {}",
            format_weight(w),
            format_reps(&target.reps)
        ),
        None => format!(" · aim {}", format_reps(&target.reps)),
    };
    if !note.is_empty() {
        out.push_str(&format!(" ({note})"));
    }
    out
}

// ─── Training phase ─────────────────────────────────────────────────────────

/// Program stage from completed weeks with logged sets: basics first, then expand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrainingPhase {
    Foundation,
    Build,
    Expand,
}

impl TrainingPhase {
    pub fn for_weeks_logged(weeks: usize) -> Self {
        match weeks {
            0..=3 => TrainingPhase::Foundation,
            4..=11 => TrainingPhase::Build,
            _ => TrainingPhase::Expand,
        }
    }

    pub fn prompt_guidance(self) -> &'static str {
        match self {
            TrainingPhase::Foundation => {
                "Foundation: full-body strength days built on selectorized machines and basic \
                 compounds (leg press, goblet squat, chest press, lat pulldown, seated row, \
                 dumbbell Romanian deadlift, plank); keep the same lifts every week; 2-3 sets of 8-12."
            }
            TrainingPhase::Build => {
                "Build: keep last weeks' main lifts, add free-weight variations (barbell squat, \
                 bench press, Romanian deadlift, dumbbell shoulder press) and 1-2 accessories; 3 sets of 6-12."
            }
            TrainingPhase::Expand => {
                "Expand: upper/lower split; barbell main lifts in 5-8 reps first, then accessories in 8-15."
            }
        }
    }
}

/// Logged-training context for the weekly plan prompt.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TrainingHistory {
    /// Distinct weeks before the plan week with at least one logged set.
    pub weeks_logged: usize,
    /// "Leg press (2026-10-17): 180 lb × 12/11/10", newest first.
    pub recent_lifts: Vec<String>,
}

impl TrainingHistory {
    pub fn phase(&self) -> TrainingPhase {
        TrainingPhase::for_weeks_logged(self.weeks_logged)
    }
}

pub async fn training_history(
    pool: &SqlitePool,
    member_id: &str,
    week_start: NaiveDate,
) -> Result<TrainingHistory> {
    let before = week_start.format("%Y-%m-%d").to_string();
    let dates: Vec<(String,)> = sqlx::query_as(
        "SELECT DISTINCT date FROM fitness_set_log WHERE family_member_id = ? AND date < ?",
    )
    .bind(member_id)
    .bind(&before)
    .fetch_all(pool)
    .await
    .context("Failed to load logged workout dates")?;
    let weeks_logged = dates
        .iter()
        .filter_map(|(d,)| NaiveDate::parse_from_str(d, "%Y-%m-%d").ok())
        .map(week_start_monday)
        .collect::<BTreeSet<_>>()
        .len();

    let since = (week_start - Duration::days(PROMPT_HISTORY_DAYS))
        .format("%Y-%m-%d")
        .to_string();
    let rows: Vec<(String, String, String, Option<f64>, i64)> = sqlx::query_as(
        "SELECT exercise_key, exercise_name, date, weight_lb, reps FROM fitness_set_log \
         WHERE family_member_id = ? AND date >= ? AND date < ? \
         ORDER BY date DESC, rowid ASC",
    )
    .bind(member_id)
    .bind(&since)
    .bind(&before)
    .fetch_all(pool)
    .await
    .context("Failed to load recent logged lifts")?;

    // Latest session per exercise, in first-seen (newest) order.
    let mut order: Vec<String> = Vec::new();
    let mut latest: HashMap<String, (String, String, Vec<SetRecord>)> = HashMap::new();
    for (key, name, date, weight, reps) in rows {
        let entry = latest.entry(key.clone()).or_insert_with(|| {
            order.push(key.clone());
            (name, date.clone(), Vec::new())
        });
        if entry.1 == date {
            entry.2.push((weight, reps as u32));
        }
    }
    let recent_lifts = order
        .iter()
        .take(PROMPT_MAX_LIFTS)
        .filter_map(|key| latest.get(key))
        .map(|(name, date, sets)| format!("{name} ({date}): {}", format_sets(sets)))
        .collect();
    Ok(TrainingHistory {
        weeks_logged,
        recent_lifts,
    })
}

/// Distinct days in `[start, end]` with logged sets (counted as strength sessions).
pub async fn set_log_dates(
    pool: &SqlitePool,
    member_id: &str,
    start: &str,
    end: &str,
) -> Result<Vec<String>> {
    let rows: Vec<(String,)> = sqlx::query_as(
        "SELECT DISTINCT date FROM fitness_set_log \
         WHERE family_member_id = ? AND date >= ? AND date <= ? ORDER BY date",
    )
    .bind(member_id)
    .bind(start)
    .bind(end)
    .fetch_all(pool)
    .await
    .context("Failed to load logged workout dates")?;
    Ok(rows.into_iter().map(|(d,)| d).collect())
}

// ─── Storage ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, sqlx::FromRow)]
struct WorkoutSession {
    id: String,
    date: String,
    current_exercise: Option<String>,
    last_activity_at: String,
}

#[derive(Debug, Clone, sqlx::FromRow)]
struct SessionSet {
    exercise_key: String,
    exercise_name: String,
    weight_lb: Option<f64>,
    reps: i64,
}

impl SessionSet {
    fn record(&self) -> SetRecord {
        (self.weight_lb, self.reps as u32)
    }
}

/// Open session for today, closing it first when stale (other day or idle too long).
async fn active_session(
    pool: &SqlitePool,
    member_id: &str,
    now: DateTime<Utc>,
    today: NaiveDate,
) -> Result<Option<WorkoutSession>> {
    let Some(session) = sqlx::query_as::<_, WorkoutSession>(
        "SELECT id, date, current_exercise, last_activity_at FROM fitness_workout_sessions \
         WHERE family_member_id = ? AND closed_at IS NULL",
    )
    .bind(member_id)
    .fetch_optional(pool)
    .await
    .context("Failed to load open workout")?
    else {
        return Ok(None);
    };
    let idle = DateTime::parse_from_rfc3339(&session.last_activity_at)
        .map(|t| now - t.with_timezone(&Utc) > Duration::hours(SESSION_IDLE_HOURS))
        .unwrap_or(true);
    if idle || session.date != today.format("%Y-%m-%d").to_string() {
        close_session(pool, &session.id, now).await?;
        return Ok(None);
    }
    Ok(Some(session))
}

async fn start_session(
    pool: &SqlitePool,
    member_id: &str,
    now: DateTime<Utc>,
    today: NaiveDate,
    first_exercise: Option<&str>,
) -> Result<WorkoutSession> {
    let now_s = now.to_rfc3339();
    // The partial unique index keeps one open session per member.
    sqlx::query(
        "INSERT INTO fitness_workout_sessions \
         (id, family_member_id, date, current_exercise, started_at, last_activity_at) \
         VALUES (?, ?, ?, ?, ?, ?) ON CONFLICT DO NOTHING",
    )
    .bind(uuid::Uuid::new_v4().to_string())
    .bind(member_id)
    .bind(today.format("%Y-%m-%d").to_string())
    .bind(first_exercise)
    .bind(&now_s)
    .bind(&now_s)
    .execute(pool)
    .await
    .context("Failed to start workout")?;
    active_session(pool, member_id, now, today)
        .await?
        .context("Workout session missing after start")
}

async fn close_session(pool: &SqlitePool, session_id: &str, now: DateTime<Utc>) -> Result<()> {
    sqlx::query("UPDATE fitness_workout_sessions SET closed_at = ? WHERE id = ?")
        .bind(now.to_rfc3339())
        .bind(session_id)
        .execute(pool)
        .await
        .context("Failed to close workout")?;
    Ok(())
}

async fn touch_session(
    pool: &SqlitePool,
    session_id: &str,
    now: DateTime<Utc>,
    current_exercise: Option<&str>,
) -> Result<()> {
    sqlx::query(
        "UPDATE fitness_workout_sessions SET last_activity_at = ?, current_exercise = ? WHERE id = ?",
    )
    .bind(now.to_rfc3339())
    .bind(current_exercise)
    .bind(session_id)
    .execute(pool)
    .await
    .context("Failed to update workout")?;
    Ok(())
}

async fn session_sets(pool: &SqlitePool, session_id: &str) -> Result<Vec<SessionSet>> {
    sqlx::query_as(
        "SELECT exercise_key, exercise_name, weight_lb, reps FROM fitness_set_log \
         WHERE session_id = ? ORDER BY rowid",
    )
    .bind(session_id)
    .fetch_all(pool)
    .await
    .context("Failed to load workout sets")
}

/// Up to `limit` most recent days of sets for an exercise before `before` (newest first).
async fn exercise_history(
    pool: &SqlitePool,
    member_id: &str,
    key: &str,
    before: NaiveDate,
    limit: usize,
) -> Result<Vec<Vec<SetRecord>>> {
    let rows: Vec<(String, Option<f64>, i64)> = sqlx::query_as(
        "SELECT date, weight_lb, reps FROM fitness_set_log \
         WHERE family_member_id = ? AND exercise_key = ? AND date < ? \
         ORDER BY date DESC, rowid ASC",
    )
    .bind(member_id)
    .bind(key)
    .bind(before.format("%Y-%m-%d").to_string())
    .fetch_all(pool)
    .await
    .context("Failed to load exercise history")?;
    let mut sessions: Vec<(String, Vec<SetRecord>)> = Vec::new();
    for (date, weight, reps) in rows {
        if let Some((_, sets)) = sessions.last_mut().filter(|(d, _)| *d == date) {
            sets.push((weight, reps as u32));
        } else if sessions.len() == limit {
            break;
        } else {
            sessions.push((date, vec![(weight, reps as u32)]));
        }
    }
    Ok(sessions.into_iter().map(|(_, sets)| sets).collect())
}

async fn plan_target(
    pool: &SqlitePool,
    member_id: &str,
    before: NaiveDate,
    exercise: &PlanExercise,
) -> Result<Option<Target>> {
    let history =
        exercise_history(pool, member_id, &exercise_key(&exercise.name), before, 2).await?;
    Ok(next_target(
        &exercise.name,
        rep_range(&exercise.reps),
        exercise.sets,
        &history,
    ))
}

/// Per-exercise target suffixes for `day`, aligned with `day.exercises` (empty when none).
pub async fn exercise_target_suffixes(
    pool: &SqlitePool,
    member_id: &str,
    today: NaiveDate,
    day: &PlanDay,
) -> Result<Vec<String>> {
    let mut out = Vec::with_capacity(day.exercises.len());
    for exercise in &day.exercises {
        let target = plan_target(pool, member_id, today, exercise).await?;
        out.push(target.as_ref().map(target_suffix).unwrap_or_default());
    }
    Ok(out)
}

async fn todays_plan_exercises(
    pool: &SqlitePool,
    member_id: &str,
    today: NaiveDate,
) -> Result<(Option<PlanDay>, Vec<PlanExercise>)> {
    let week_start = week_start_monday(today).format("%Y-%m-%d").to_string();
    let day = load_weekly_plan(pool, member_id, &week_start)
        .await?
        .and_then(|stored| session_for_date_from_stored(&stored, today));
    let exercises = day
        .as_ref()
        .filter(|d| d.kind != PlanDayKind::Rest)
        .map(|d| d.exercises.clone())
        .unwrap_or_default();
    Ok((day, exercises))
}

// ─── Replies ────────────────────────────────────────────────────────────────

/// `/log [input]` in a linked DM: start or resume today's workout, optionally applying input.
pub async fn log_command_reply(
    pool: &SqlitePool,
    member_id: &str,
    now: DateTime<Utc>,
    today: NaiveDate,
    args: &str,
) -> Result<String> {
    let (day, exercises) = todays_plan_exercises(pool, member_id, today).await?;
    let existing = active_session(pool, member_id, now, today).await?;
    if args.trim().is_empty() {
        let resumed = existing.is_some();
        let session = match existing {
            Some(s) => s,
            None => {
                let first = exercises.first().map(|e| e.name.as_str());
                start_session(pool, member_id, now, today, first).await?
            }
        };
        return status_reply(
            pool,
            member_id,
            today,
            &session,
            day.as_ref(),
            &exercises,
            resumed,
        )
        .await;
    }
    let Some(input) = parse_workout_input(args) else {
        return Ok(LOG_USAGE.into());
    };
    let session = match (existing, &input) {
        (Some(s), _) => s,
        (None, WorkoutInput::Sets { .. } | WorkoutInput::Jump(_)) => {
            let first = exercises.first().map(|e| e.name.as_str());
            start_session(pool, member_id, now, today, first).await?
        }
        (None, _) => return Ok("No workout in progress. Send /log to start one.".into()),
    };
    apply_input(pool, member_id, now, today, &session, &exercises, input).await
}

/// Plain DM text during an open workout. `None` when no workout is open or the
/// text is not workout input, so other handlers can take it.
pub async fn workout_message_reply(
    pool: &SqlitePool,
    member_id: &str,
    now: DateTime<Utc>,
    today: NaiveDate,
    text: &str,
) -> Result<Option<String>> {
    if text.trim_start().starts_with('/') {
        return Ok(None);
    }
    let Some(input) = parse_workout_input(text) else {
        return Ok(None);
    };
    let Some(session) = active_session(pool, member_id, now, today).await? else {
        return Ok(None);
    };
    let (_, exercises) = todays_plan_exercises(pool, member_id, today).await?;
    apply_input(pool, member_id, now, today, &session, &exercises, input)
        .await
        .map(Some)
}

async fn exercise_line(
    pool: &SqlitePool,
    member_id: &str,
    today: NaiveDate,
    exercises: &[PlanExercise],
    index: usize,
) -> Result<String> {
    let exercise = &exercises[index];
    let suffix = plan_target(pool, member_id, today, exercise)
        .await?
        .as_ref()
        .map(target_suffix)
        .unwrap_or_default();
    Ok(format!("{}. {}{}", index + 1, exercise.render(), suffix))
}

async fn status_reply(
    pool: &SqlitePool,
    member_id: &str,
    today: NaiveDate,
    session: &WorkoutSession,
    day: Option<&PlanDay>,
    exercises: &[PlanExercise],
    resumed: bool,
) -> Result<String> {
    let mut out = if resumed {
        "▶️ Workout resumed".to_string()
    } else {
        "▶️ Workout started".to_string()
    };
    if let Some(day) = day {
        let notes = day.notes.trim();
        out.push_str(&format!(" — {} {}", day.weekday, day.kind.as_str()));
        if !notes.is_empty() {
            out.push_str(&format!(": {notes}"));
        }
    }
    out.push('\n');

    let sets = session_sets(pool, &session.id).await?;
    for (i, exercise) in exercises.iter().enumerate() {
        let key = exercise_key(&exercise.name);
        let done: Vec<SetRecord> = sets
            .iter()
            .filter(|s| s.exercise_key == key)
            .map(SessionSet::record)
            .collect();
        out.push_str(&format!(
            " {}",
            exercise_line(pool, member_id, today, exercises, i).await?
        ));
        if !done.is_empty() {
            out.push_str(&format!("  ✓ {}", format_sets(&done)));
        }
        out.push('\n');
    }
    let plan_keys: BTreeSet<String> = exercises.iter().map(|e| exercise_key(&e.name)).collect();
    for (name, done) in group_by_exercise(&sets) {
        if !plan_keys.contains(&exercise_key(&name)) {
            out.push_str(&format!(" + {name}  ✓ {}\n", format_sets(&done)));
        }
    }
    if exercises.is_empty() {
        out.push_str("No lifts planned today — log any lift by name, e.g. `bench 135x8`.\n");
    } else if let Some(current) = session.current_exercise.as_deref() {
        out.push_str(&format!("\nNow: {current}\n"));
    }
    out.push_str(SHORT_HELP);
    Ok(out)
}

/// Sets grouped by exercise in first-logged order.
fn group_by_exercise(sets: &[SessionSet]) -> Vec<(String, Vec<SetRecord>)> {
    let mut groups: Vec<(String, Vec<SetRecord>)> = Vec::new();
    for set in sets {
        match groups
            .iter_mut()
            .find(|(name, _)| exercise_key(name) == set.exercise_key)
        {
            Some((_, records)) => records.push(set.record()),
            None => groups.push((set.exercise_name.clone(), vec![set.record()])),
        }
    }
    groups
}

async fn apply_input(
    pool: &SqlitePool,
    member_id: &str,
    now: DateTime<Utc>,
    today: NaiveDate,
    session: &WorkoutSession,
    exercises: &[PlanExercise],
    input: WorkoutInput,
) -> Result<String> {
    let current_index = session
        .current_exercise
        .as_deref()
        .and_then(|name| match_plan_exercise(exercises, name));
    match input {
        WorkoutInput::Done => finish_reply(pool, member_id, now, today, session, exercises).await,
        WorkoutInput::Undo => {
            let last: Option<(String, String, Option<f64>, i64)> = sqlx::query_as(
                "DELETE FROM fitness_set_log WHERE rowid = \
                 (SELECT rowid FROM fitness_set_log WHERE session_id = ? ORDER BY rowid DESC LIMIT 1) \
                 RETURNING id, exercise_name, weight_lb, reps",
            )
            .bind(&session.id)
            .fetch_optional(pool)
            .await
            .context("Failed to undo set")?;
            let Some((_, name, weight, reps)) = last else {
                touch_session(pool, &session.id, now, session.current_exercise.as_deref()).await?;
                return Ok("Nothing to undo in this workout.".into());
            };
            touch_session(pool, &session.id, now, Some(&name)).await?;
            Ok(format!(
                "↩️ Removed {name} {}",
                format_sets(&[(weight, reps as u32)])
            ))
        }
        WorkoutInput::Next | WorkoutInput::Jump(_) => {
            let target = match input {
                WorkoutInput::Jump(n) => n - 1,
                _ => current_index.map_or(0, |i| i + 1),
            };
            if exercises.is_empty() {
                return Ok(
                    "No lifts planned today — log any lift by name, e.g. `bench 135x8`.".into(),
                );
            }
            if target >= exercises.len() {
                touch_session(pool, &session.id, now, session.current_exercise.as_deref()).await?;
                return Ok(match input {
                    WorkoutInput::Jump(_) => format!("Pick an exercise from #1 to #{}.", exercises.len()),
                    _ => "That was the last planned lift. Log extras like `curl 30x12`, or send `done`."
                        .into(),
                });
            }
            touch_session(pool, &session.id, now, Some(&exercises[target].name)).await?;
            Ok(format!(
                "Now: {}",
                exercise_line(pool, member_id, today, exercises, target).await?
            ))
        }
        WorkoutInput::Sets { exercise, sets } => {
            let (name, plan_index) = match exercise.as_deref() {
                Some(query) => match match_plan_exercise(exercises, query) {
                    Some(i) => (exercises[i].name.clone(), Some(i)),
                    None => (capitalize(query), None),
                },
                None => match session.current_exercise.clone() {
                    Some(name) => (name, current_index),
                    None => {
                        return Ok("Which lift? Send it with the set, e.g. `bench 135x8`.".into())
                    }
                },
            };
            log_sets_reply(
                pool, member_id, now, today, session, exercises, &name, plan_index, &sets,
            )
            .await
        }
    }
}

fn capitalize(raw: &str) -> String {
    let mut chars = raw.trim().chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

#[allow(clippy::too_many_arguments)]
async fn log_sets_reply(
    pool: &SqlitePool,
    member_id: &str,
    now: DateTime<Utc>,
    today: NaiveDate,
    session: &WorkoutSession,
    exercises: &[PlanExercise],
    name: &str,
    plan_index: Option<usize>,
    inputs: &[SetInput],
) -> Result<String> {
    let key = exercise_key(name);
    let earlier: Vec<SetRecord> = session_sets(pool, &session.id)
        .await?
        .iter()
        .filter(|s| s.exercise_key == key)
        .map(SessionSet::record)
        .collect();

    // Resolve `x8` against the previous set; fall back to the last session's load.
    let mut previous = match earlier.last() {
        Some((weight, _)) => Some(*weight),
        None => exercise_history(pool, member_id, &key, today, 1)
            .await?
            .first()
            .map(|sets| working_sets(sets).0),
    };
    let mut records: Vec<SetRecord> = Vec::new();
    for input in inputs {
        let weight = match input.load {
            Load::Weight(w) => Some(w),
            Load::Bodyweight => None,
            Load::Same => match previous {
                Some(w) => w,
                None => {
                    return Ok(format!(
                        "No weight yet for {name} — send it once like `135x8` (or `bwx8` for bodyweight)."
                    ))
                }
            },
        };
        previous = Some(weight);
        records.extend(std::iter::repeat_n(
            (weight, input.reps),
            input.count as usize,
        ));
    }

    let mut tx = pool.begin().await.context("Failed to begin set log")?;
    for (weight, reps) in &records {
        sqlx::query(
            "INSERT INTO fitness_set_log \
             (id, session_id, family_member_id, date, exercise_key, exercise_name, weight_lb, reps) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(uuid::Uuid::new_v4().to_string())
        .bind(&session.id)
        .bind(member_id)
        .bind(today.format("%Y-%m-%d").to_string())
        .bind(&key)
        .bind(name)
        .bind(weight)
        .bind(*reps as i64)
        .execute(&mut *tx)
        .await
        .context("Failed to save set")?;
    }
    tx.commit().await.context("Failed to commit set log")?;

    let first_number = earlier.len() + 1;
    let last_number = earlier.len() + records.len();
    let label = if records.len() == 1 {
        format!("set {first_number}")
    } else {
        format!("sets {first_number}–{last_number}")
    };
    let mut out = format!("✓ {name} {label}: {}", format_sets(&records));

    let Some(index) = plan_index else {
        touch_session(pool, &session.id, now, Some(name)).await?;
        return Ok(out);
    };
    let planned = exercises[index].sets.max(1) as usize;
    if last_number < planned {
        touch_session(pool, &session.id, now, Some(name)).await?;
        return Ok(out);
    }

    // Planned sets done: move to the next plan lift that still needs sets.
    let all_sets = session_sets(pool, &session.id).await?;
    let incomplete = |i: usize| {
        let key = exercise_key(&exercises[i].name);
        all_sets.iter().filter(|s| s.exercise_key == key).count()
            < exercises[i].sets.max(1) as usize
    };
    let next = (index + 1..exercises.len())
        .chain(0..index)
        .find(|i| incomplete(*i));
    match next {
        Some(i) => {
            touch_session(pool, &session.id, now, Some(&exercises[i].name)).await?;
            out.push_str(&format!(
                "\n→ Next: {}",
                exercise_line(pool, member_id, today, exercises, i).await?
            ));
        }
        None => {
            touch_session(pool, &session.id, now, Some(name)).await?;
            out.push_str("\nAll planned lifts logged 💪 Send `done` to finish.");
        }
    }
    Ok(out)
}

async fn finish_reply(
    pool: &SqlitePool,
    member_id: &str,
    now: DateTime<Utc>,
    today: NaiveDate,
    session: &WorkoutSession,
    exercises: &[PlanExercise],
) -> Result<String> {
    let sets = session_sets(pool, &session.id).await?;
    close_session(pool, &session.id, now).await?;
    if sets.is_empty() {
        return Ok("Workout closed — nothing was logged.".into());
    }
    let groups = group_by_exercise(&sets);
    let mut out = format!(
        "🏁 Workout logged: {} sets across {} exercises.\n",
        sets.len(),
        groups.len()
    );
    let tomorrow = today + Duration::days(1);
    for (name, done) in &groups {
        out.push_str(&format!("• {name}: {}", format_sets(done)));
        let planned = match_plan_exercise(exercises, name).map(|i| &exercises[i]);
        if let Some(exercise) = planned {
            let target = plan_target(pool, member_id, tomorrow, exercise).await?;
            if let Some(target) = target.filter(|t| !t.reps.is_empty()) {
                if let Some(w) = target.weight_lb {
                    out.push_str(&format!(
                        " → next {} lb, aim {}",
                        format_weight(w),
                        format_reps(&target.reps)
                    ));
                } else {
                    out.push_str(&format!(" → next aim {}", format_reps(&target.reps)));
                }
            }
        }
        out.push('\n');
    }
    Ok(out.trim_end().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fitness_plan::{save_weekly_plan, WeeklyFitnessPlan};
    use chrono::{Datelike, TimeZone};

    fn set(load: Load, reps: u32, count: u32) -> SetInput {
        SetInput { load, reps, count }
    }

    #[test]
    fn set_tokens_parse() {
        assert_eq!(
            parse_set_token("135x8"),
            Some(set(Load::Weight(135.0), 8, 1))
        );
        assert_eq!(
            parse_set_token("22.5x10x3"),
            Some(set(Load::Weight(22.5), 10, 3))
        );
        assert_eq!(
            parse_set_token("135lbx8"),
            Some(set(Load::Weight(135.0), 8, 1))
        );
        assert_eq!(parse_set_token("x8"), Some(set(Load::Same, 8, 1)));
        assert_eq!(parse_set_token("bwx12"), Some(set(Load::Bodyweight, 12, 1)));
        assert_eq!(parse_set_token("12"), Some(set(Load::Same, 12, 1)));
        for bad in [
            "0",
            "135x0",
            "135x8x0",
            "135x8x20",
            "9999x5",
            "eggs",
            "2x",
            "x",
            "135x8x3x2",
        ] {
            assert_eq!(parse_set_token(bad), None, "{bad}");
        }
    }

    #[test]
    fn workout_grammar_claims_only_workout_text() {
        assert_eq!(
            parse_workout_input("180x12"),
            Some(WorkoutInput::Sets {
                exercise: None,
                sets: vec![set(Load::Weight(180.0), 12, 1)]
            })
        );
        assert_eq!(
            parse_workout_input("180 x 12, x11"),
            Some(WorkoutInput::Sets {
                exercise: None,
                sets: vec![set(Load::Weight(180.0), 12, 1), set(Load::Same, 11, 1)]
            })
        );
        assert_eq!(
            parse_workout_input("Bench press 135×8×3"),
            Some(WorkoutInput::Sets {
                exercise: Some("bench press".into()),
                sets: vec![set(Load::Weight(135.0), 8, 3)]
            })
        );
        assert_eq!(parse_workout_input("Done"), Some(WorkoutInput::Done));
        assert_eq!(parse_workout_input("undo"), Some(WorkoutInput::Undo));
        assert_eq!(parse_workout_input("next"), Some(WorkoutInput::Next));
        assert_eq!(parse_workout_input("#3"), Some(WorkoutInput::Jump(3)));
        for prose in [
            "2 eggs and toast",
            "ate 2",
            "pullups 8",
            "thanks!",
            "I'm done for today",
            "#0",
            "",
        ] {
            assert_eq!(parse_workout_input(prose), None, "{prose}");
        }
    }

    #[test]
    fn plan_exercise_matching() {
        let exercises: Vec<PlanExercise> = ["Bench press", "Incline dumbbell press", "Pull-ups"]
            .into_iter()
            .map(|name| PlanExercise {
                name: name.into(),
                sets: 3,
                reps: "8-10".into(),
            })
            .collect();
        assert_eq!(match_plan_exercise(&exercises, "bench"), Some(0));
        assert_eq!(match_plan_exercise(&exercises, "pullups"), Some(2));
        assert_eq!(match_plan_exercise(&exercises, "incline db"), None);
        assert_eq!(match_plan_exercise(&exercises, "incline"), Some(1));
        assert_eq!(match_plan_exercise(&exercises, "press"), None, "ambiguous");
        assert_eq!(match_plan_exercise(&exercises, "curl"), None);
    }

    #[test]
    fn rep_ranges_from_plan_strings() {
        assert_eq!(rep_range("8-10"), Some((8, 10)));
        assert_eq!(rep_range("8 – 12"), Some((8, 12)));
        assert_eq!(rep_range("10 to 15"), Some((10, 15)));
        assert_eq!(rep_range("12 each side"), Some((12, 12)));
        assert_eq!(rep_range("5"), Some((5, 5)));
        assert_eq!(rep_range("45s"), None);
        assert_eq!(rep_range("30 min easy"), None);
        assert_eq!(rep_range("AMRAP"), None);
    }

    #[test]
    fn double_progression() {
        let range = Some((8, 12));
        let first = next_target("Chest press", range, 3, &[]).unwrap();
        assert!(first.reps.is_empty());
        assert!(first.note.unwrap().contains("12 times"));

        let topped = vec![vec![(Some(90.0), 12), (Some(90.0), 12), (Some(90.0), 12)]];
        let up = next_target("Chest press", range, 3, &topped).unwrap();
        assert_eq!(up.weight_lb, Some(95.0));
        assert_eq!(up.reps, vec![8, 8, 8]);

        let lower = next_target("Leg press", range, 3, &topped).unwrap();
        assert_eq!(lower.weight_lb, Some(100.0));

        let partial = vec![vec![(Some(90.0), 12), (Some(90.0), 10), (Some(90.0), 9)]];
        let hold = next_target("Chest press", range, 3, &partial).unwrap();
        assert_eq!(hold.weight_lb, Some(90.0));
        assert_eq!(hold.reps, vec![12, 11, 10]);

        let short = vec![(Some(100.0), 6), (Some(100.0), 5)];
        let deload = next_target("Chest press", range, 3, &[short.clone(), short.clone()]).unwrap();
        assert_eq!(deload.weight_lb, Some(90.0));
        let once = next_target("Chest press", range, 3, &[short]).unwrap();
        assert_eq!(once.weight_lb, Some(100.0));
        assert_eq!(once.reps, vec![8, 8, 8]);

        let bodyweight = vec![vec![(None, 12), (None, 12), (None, 12)]];
        let harder = next_target("Pull-ups", range, 3, &bodyweight).unwrap();
        assert_eq!(harder.weight_lb, None);
        assert!(harder.note.unwrap().contains("harder"));

        assert_eq!(next_target("Plank", None, 3, &topped), None);
    }

    #[test]
    fn phases_follow_logged_weeks() {
        assert_eq!(
            TrainingPhase::for_weeks_logged(0),
            TrainingPhase::Foundation
        );
        assert_eq!(
            TrainingPhase::for_weeks_logged(3),
            TrainingPhase::Foundation
        );
        assert_eq!(TrainingPhase::for_weeks_logged(4), TrainingPhase::Build);
        assert_eq!(TrainingPhase::for_weeks_logged(12), TrainingPhase::Expand);
    }

    fn at(hour: u32, minute: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 19, hour, minute, 0).unwrap()
    }

    fn test_plan() -> WeeklyFitnessPlan {
        let plan = WeeklyFitnessPlan {
            theme: "Base".into(),
            days: vec![crate::fitness_plan::PlanDay {
                weekday: "Monday".into(),
                kind: PlanDayKind::Strength,
                notes: "full body".into(),
                exercises: vec![
                    PlanExercise {
                        name: "Leg press".into(),
                        sets: 2,
                        reps: "8-12".into(),
                    },
                    PlanExercise {
                        name: "Chest press".into(),
                        sets: 2,
                        reps: "8-12".into(),
                    },
                ],
            }],
        };
        crate::fitness_plan::normalize_weekly_plan(plan).unwrap()
    }

    async fn pool_with_plan() -> SqlitePool {
        let pool = chotu_common::init_db(":memory:").await.unwrap();
        save_weekly_plan(&pool, "alex", "2026-10-19", &test_plan())
            .await
            .unwrap();
        pool
    }

    #[tokio::test]
    async fn workout_session_flow() {
        let pool = pool_with_plan().await;
        let monday = NaiveDate::from_ymd_opt(2026, 10, 19).unwrap();

        // Plain text is ignored until /log opens a workout.
        assert_eq!(
            workout_message_reply(&pool, "alex", at(17, 0), monday, "180x12")
                .await
                .unwrap(),
            None
        );

        let start = log_command_reply(&pool, "alex", at(17, 0), monday, "")
            .await
            .unwrap();
        assert!(start.contains("Workout started"), "{start}");
        assert!(
            start.contains("1. Leg press — 2×8-12 · first time"),
            "{start}"
        );
        assert!(start.contains("Now: Leg press"), "{start}");

        let reply = |text: &'static str, minute: u32| {
            let pool = pool.clone();
            async move {
                workout_message_reply(&pool, "alex", at(17, minute), monday, text)
                    .await
                    .unwrap()
            }
        };
        assert_eq!(reply("2 eggs and toast", 1).await, None);
        assert_eq!(
            reply("180x12", 2).await.unwrap(),
            "✓ Leg press set 1: 180 lb × 12"
        );
        let advanced = reply("x12", 3).await.unwrap();
        assert!(advanced.contains("set 2: 180 lb × 12"), "{advanced}");
        assert!(advanced.contains("→ Next: 2. Chest press"), "{advanced}");

        assert!(reply("90x10", 4)
            .await
            .unwrap()
            .contains("Chest press set 1"));
        assert!(reply("undo", 5)
            .await
            .unwrap()
            .contains("Removed Chest press"));
        let both = reply("90x10x2", 6).await.unwrap();
        assert!(both.contains("sets 1–2: 90 lb × 10/10"), "{both}");
        assert!(both.contains("All planned lifts logged"), "{both}");

        assert!(reply("curl 30x12", 7)
            .await
            .unwrap()
            .contains("Curl set 1: 30 lb × 12"));

        let done = reply("done", 8).await.unwrap();
        assert!(done.contains("5 sets across 3 exercises"), "{done}");
        assert!(
            done.contains("Leg press: 180 lb × 12/12 → next 190 lb, aim 8/8"),
            "{done}"
        );
        assert!(
            done.contains("Chest press: 90 lb × 10/10 → next 90 lb, aim 11/11"),
            "{done}"
        );
        assert_eq!(reply("180x12", 9).await, None, "closed");

        // Next Monday's targets come from this log.
        let next_monday = monday + Duration::days(7);
        let history = training_history(&pool, "alex", next_monday).await.unwrap();
        assert_eq!(history.weeks_logged, 1);
        assert!(history
            .recent_lifts
            .contains(&"Leg press (2026-10-19): 180 lb × 12/12".to_string()));
        assert_eq!(
            set_log_dates(&pool, "alex", "2026-10-19", "2026-10-25")
                .await
                .unwrap(),
            vec!["2026-10-19".to_string()]
        );
    }

    #[tokio::test]
    async fn idle_or_next_day_sessions_close() {
        let pool = pool_with_plan().await;
        let monday = NaiveDate::from_ymd_opt(2026, 10, 19).unwrap();
        log_command_reply(&pool, "alex", at(10, 0), monday, "")
            .await
            .unwrap();
        assert!(
            workout_message_reply(&pool, "alex", at(12, 0), monday, "100x10")
                .await
                .unwrap()
                .is_some()
        );
        assert_eq!(
            workout_message_reply(&pool, "alex", at(15, 1), monday, "x10")
                .await
                .unwrap(),
            None,
            "idle > 3h closes the workout"
        );

        log_command_reply(&pool, "alex", at(16, 0), monday, "")
            .await
            .unwrap();
        let tuesday = monday + Duration::days(1);
        assert_eq!(
            workout_message_reply(&pool, "alex", at(16, 5), tuesday, "100x10")
                .await
                .unwrap(),
            None,
            "a new day closes yesterday's workout"
        );
        assert!(log_command_reply(&pool, "alex", at(16, 6), tuesday, "done")
            .await
            .unwrap()
            .contains("No workout in progress"));
    }

    /// Exact Signal reply text for a scripted conversation. `/`-prefixed steps go
    /// through `/log`, the rest through plain DM handling. Captured before the
    /// typed-core split so formatter changes show up as diffs.
    async fn transcript(pool: &SqlitePool, day: NaiveDate, hour: u32, steps: &[&str]) -> String {
        let mut out = String::new();
        for (minute, step) in steps.iter().enumerate() {
            let now = Utc
                .with_ymd_and_hms(day.year(), day.month(), day.day(), hour, minute as u32, 0)
                .unwrap();
            let reply = match step.strip_prefix("/log") {
                Some(args) => Some(
                    log_command_reply(pool, "alex", now, day, args.trim())
                        .await
                        .unwrap(),
                ),
                None => workout_message_reply(pool, "alex", now, day, step)
                    .await
                    .unwrap(),
            };
            out.push_str(&format!(
                "> {step}\n{}\n\n",
                reply.as_deref().unwrap_or("<not claimed>")
            ));
        }
        out
    }

    #[tokio::test]
    async fn log_reply_snapshots() {
        let pool = pool_with_plan().await;
        let monday = NaiveDate::from_ymd_opt(2026, 10, 19).unwrap();
        let first = transcript(
            &pool,
            monday,
            17,
            &[
                "180x12",
                "/log done",
                "/log hello there",
                "/log x10",
                "/log",
                "180x12",
                "x12",
                "#5",
                "#1",
                "next",
                "next",
                "90x10",
                "undo",
                "90x10x2",
                "curl 30x12",
                "/log",
                "done",
                "undo",
                "/log undo",
            ],
        )
        .await;
        check_snapshot("log_monday", &first).await;

        // Next week: targets come from Monday's log; Tuesday has no lifts planned.
        let next_monday = monday + Duration::days(7);
        save_weekly_plan(&pool, "alex", "2026-10-26", &test_plan())
            .await
            .unwrap();
        let second = transcript(&pool, next_monday, 17, &["/log", "x8", "#2", "done"]).await;
        check_snapshot("log_next_monday", &second).await;

        let tuesday = next_monday + Duration::days(1);
        let third = transcript(
            &pool,
            tuesday,
            9,
            &[
                "/log",
                "x8",
                "next",
                "bench 135x8",
                "x8",
                "undo",
                "undo",
                "undo",
                "done",
            ],
        )
        .await;
        check_snapshot("log_tuesday", &third).await;
    }

    /// Compares against `snapshots/<name>.txt`; `UPDATE_SNAPSHOTS=1` rewrites it.
    async fn check_snapshot(name: &str, actual: &str) {
        let path = format!("{}/src/snapshots/{name}.txt", env!("CARGO_MANIFEST_DIR"));
        if std::env::var_os("UPDATE_SNAPSHOTS").is_some() {
            tokio::fs::write(&path, actual).await.unwrap();
            return;
        }
        let expected = tokio::fs::read_to_string(&path)
            .await
            .unwrap_or_else(|e| panic!("{path}: {e} (run with UPDATE_SNAPSHOTS=1)"));
        assert_eq!(actual, expected, "snapshot {name} changed");
    }

    #[tokio::test]
    async fn same_weight_needs_a_known_load() {
        let pool = pool_with_plan().await;
        let monday = NaiveDate::from_ymd_opt(2026, 10, 19).unwrap();
        let reply = log_command_reply(&pool, "alex", at(10, 0), monday, "x10")
            .await
            .unwrap();
        assert!(reply.contains("No weight yet for Leg press"), "{reply}");
    }
}
