//! Gym workouts logged set by set from Signal (`/log`) and double-progression targets.
//!
//! `/log` opens a durable workout session for the member's local day. While it is
//! open, short plain messages (`180x12`, `x11`, `next`, `undo`, `done`) log sets
//! against today's planned exercises. Anything outside that grammar is left for the
//! normal message handlers.

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, FixedOffset, NaiveDate, TimeZone, Utc};
use sqlx::SqlitePool;
use std::collections::{BTreeSet, HashMap};

use crate::fitness_plan::{
    load_weekly_plan, session_for_date_from_stored, week_start_monday, PlanDay, PlanDayKind,
    PlanExercise,
};

/// Idle time after which an open workout is closed automatically.
const SESSION_IDLE_HOURS: i64 = 3;
/// A set within this many minutes of server time is live: it opens or continues
/// the current workout exactly like a Signal message.
const LIVE_SET_MINUTES: i64 = 5;
/// Client timestamps may run this far ahead of the server (clock skew).
const MAX_PERFORMED_AT_FUTURE_MINUTES: i64 = 5;
/// Queued sets older than this are refused rather than rewriting old history.
const MAX_PERFORMED_AT_AGE_DAYS: i64 = 7;
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

/// Load for a set as entered.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Load {
    Weight(f64),
    Bodyweight,
    /// `x8` or bare `8`: same load as the previous set of this exercise.
    Same,
}

/// `count` identical sets of `reps` at `load`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SetInput {
    pub load: Load,
    pub reps: u32,
    pub count: u32,
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

// ─── Typed core ─────────────────────────────────────────────────────────────

/// Failures from the workout core. Everything before `Database` is a caller
/// mistake a client can explain; `Database` and `Plan` are storage faults.
#[derive(Debug, thiserror::Error)]
pub enum WorkoutError {
    #[error("no workout in progress")]
    NoOpenWorkout,
    #[error("no lifts planned today")]
    NoPlannedLifts,
    #[error("exercise index {index} is outside today's {count} planned lifts")]
    ExerciseOutOfRange { index: usize, count: usize },
    #[error("no exercise selected; name the lift with the set")]
    NoCurrentExercise,
    #[error("no weight yet for {exercise}")]
    UnknownLoad { exercise: String },
    #[error("invalid set: {0}")]
    InvalidSet(&'static str),
    #[error("set not found in the open workout")]
    SetNotFound,
    #[error("{action}: {source}")]
    Database {
        action: &'static str,
        #[source]
        source: sqlx::Error,
    },
    #[error("failed to load today's plan: {0}")]
    Plan(#[source] Box<dyn std::error::Error + Send + Sync>),
}

type WorkoutResult<T> = std::result::Result<T, WorkoutError>;

/// A client-supplied set timestamp that cannot be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PerformedAtError {
    #[error("performed_at must be an RFC 3339 timestamp with an offset")]
    Invalid,
    #[error("performed_at is more than {MAX_PERFORMED_AT_FUTURE_MINUTES} minutes in the future")]
    InFuture,
    #[error("performed_at is more than {MAX_PERFORMED_AT_AGE_DAYS} days old")]
    TooOld,
}

/// When a set was done: the instant, and its civil day in the household timezone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PerformedAt {
    pub at: DateTime<Utc>,
    pub date: NaiveDate,
}

impl PerformedAt {
    /// `at` on its civil day in `at`'s own timezone (pass a household-tz time).
    pub fn new<Z: TimeZone>(at: DateTime<Z>) -> Self {
        Self {
            date: at.date_naive(),
            at: at.with_timezone(&Utc),
        }
    }
}

/// Parse an RFC 3339 timestamp; the offset (`Z` or `±hh:mm`) is required.
pub fn parse_performed_at(raw: &str) -> Result<DateTime<FixedOffset>, PerformedAtError> {
    DateTime::parse_from_rfc3339(raw.trim()).map_err(|_| PerformedAtError::Invalid)
}

/// A client's `performed_at` checked against the server clock `now` (household tz).
/// `None` means server time. Refuses more than 5 minutes ahead or 7 days old.
pub fn resolve_performed_at<Z: TimeZone>(
    performed_at: Option<DateTime<FixedOffset>>,
    now: DateTime<Z>,
) -> Result<PerformedAt, PerformedAtError> {
    let Some(at) = performed_at else {
        return Ok(PerformedAt::new(now));
    };
    let at = at.with_timezone(&now.timezone());
    if at.clone() - now.clone() > Duration::minutes(MAX_PERFORMED_AT_FUTURE_MINUTES) {
        return Err(PerformedAtError::InFuture);
    }
    if now - at.clone() > Duration::days(MAX_PERFORMED_AT_AGE_DAYS) {
        return Err(PerformedAtError::TooOld);
    }
    Ok(PerformedAt::new(at))
}

fn db(action: &'static str) -> impl FnOnce(sqlx::Error) -> WorkoutError {
    move |source| WorkoutError::Database { action, source }
}

/// One stored set in a workout.
#[derive(Debug, Clone, PartialEq)]
pub struct LoggedSet {
    pub id: String,
    pub exercise_key: String,
    pub exercise_name: String,
    /// `None` is bodyweight.
    pub weight_lb: Option<f64>,
    pub reps: u32,
}

type SetRow = (String, String, String, Option<f64>, i64);

impl LoggedSet {
    fn from_row((id, exercise_key, exercise_name, weight_lb, reps): SetRow) -> Self {
        Self {
            id,
            exercise_key,
            exercise_name,
            weight_lb,
            reps: reps as u32,
        }
    }

    fn record(&self) -> SetRecord {
        (self.weight_lb, self.reps)
    }
}

/// The member's open workout for today.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkoutState {
    pub session_id: String,
    pub current_exercise: Option<String>,
    /// Today's plan day when a weekly plan covers it (rest days included).
    pub plan_day: Option<PlanDay>,
    /// Today's planned lifts; empty on rest days or without a plan.
    pub exercises: Vec<PlanExercise>,
    /// Next target per planned lift from earlier days, aligned with `exercises`.
    pub targets: Vec<Option<Target>>,
    /// Sets logged in this workout, oldest first.
    pub sets: Vec<LoggedSet>,
}

impl WorkoutState {
    fn current_index(&self) -> Option<usize> {
        self.current_exercise
            .as_deref()
            .and_then(|name| match_plan_exercise(&self.exercises, name))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct WorkoutStart {
    pub state: WorkoutState,
    /// `true` when today's workout was already open.
    pub resumed: bool,
}

/// Where the workout moved after logging sets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Advance {
    /// Still on the same exercise (planned sets left, or not a planned lift).
    Stay,
    /// Planned sets done; moved to this planned lift index.
    Next(usize),
    /// Every planned lift has its planned sets.
    AllPlannedDone,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SetsLogged {
    pub state: WorkoutState,
    pub exercise: String,
    /// The sets just stored, in order.
    pub logged: Vec<LoggedSet>,
    /// 1-based number of the first new set within this exercise today.
    pub first_number: usize,
    pub advance: Advance,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DeletedSet {
    pub set: LoggedSet,
    pub state: WorkoutState,
}

/// One exercise in a closed workout, with the target for its next session.
#[derive(Debug, Clone, PartialEq)]
pub struct ExerciseSummary {
    pub name: String,
    pub sets: Vec<SetRecord>,
    /// Only for planned lifts with a rep range.
    pub next: Option<Target>,
}

impl SetInput {
    fn validate(&self) -> WorkoutResult<()> {
        if !(1..=MAX_REPS).contains(&self.reps) {
            return Err(WorkoutError::InvalidSet("reps out of range"));
        }
        if !(1..=MAX_SETS_PER_TOKEN).contains(&self.count) {
            return Err(WorkoutError::InvalidSet("set count out of range"));
        }
        if let Load::Weight(w) = self.load {
            if !(w > 0.0 && w <= MAX_WEIGHT_LB) {
                return Err(WorkoutError::InvalidSet("weight out of range"));
            }
        }
        Ok(())
    }
}

/// Today's open workout, or `None` (a stale one is closed first).
pub async fn workout_state(
    pool: &SqlitePool,
    member_id: &str,
    now: DateTime<Utc>,
    today: NaiveDate,
) -> WorkoutResult<Option<WorkoutState>> {
    let Some(session) = active_session(pool, member_id, now, today).await? else {
        return Ok(None);
    };
    let (day, exercises) = todays_plan_exercises(pool, member_id, today).await?;
    build_state(
        pool,
        member_id,
        today,
        session.id,
        session.current_exercise,
        day,
        exercises,
    )
    .await
    .map(Some)
}

/// Resume today's workout, or start one on the first planned lift.
pub async fn start_workout(
    pool: &SqlitePool,
    member_id: &str,
    now: DateTime<Utc>,
    today: NaiveDate,
) -> WorkoutResult<WorkoutStart> {
    let (day, exercises) = todays_plan_exercises(pool, member_id, today).await?;
    let existing = active_session(pool, member_id, now, today).await?;
    let resumed = existing.is_some();
    let session = match existing {
        Some(s) => s,
        None => open_or_start(pool, member_id, now, today, &exercises).await?,
    };
    let state = build_state(
        pool,
        member_id,
        today,
        session.id,
        session.current_exercise,
        day,
        exercises,
    )
    .await?;
    Ok(WorkoutStart { state, resumed })
}

/// Make the planned lift at `index` (0-based) current, starting a workout if needed.
pub async fn select_exercise(
    pool: &SqlitePool,
    member_id: &str,
    now: DateTime<Utc>,
    today: NaiveDate,
    index: usize,
) -> WorkoutResult<WorkoutState> {
    let (day, exercises) = todays_plan_exercises(pool, member_id, today).await?;
    if exercises.is_empty() {
        return Err(WorkoutError::NoPlannedLifts);
    }
    if index >= exercises.len() {
        return Err(WorkoutError::ExerciseOutOfRange {
            index,
            count: exercises.len(),
        });
    }
    let session = open_or_start(pool, member_id, now, today, &exercises).await?;
    let name = exercises[index].name.clone();
    touch_session(pool, &session.id, now, Some(&name)).await?;
    build_state(
        pool,
        member_id,
        today,
        session.id,
        Some(name),
        day,
        exercises,
    )
    .await
}

/// Log sets to `exercise` (a planned lift by name, or any lift), else the current
/// one. Moves on once a planned lift's sets are done.
///
/// `now`/`today` are the server clock; `performed` is when the sets were done.
/// A live set (within 5 minutes of `now`, today) opens or continues the current
/// workout. An earlier one joins the latest workout on its own day, even a closed
/// one, or a new closed workout; it never moves the live workout's clock back.
pub async fn log_sets(
    pool: &SqlitePool,
    member_id: &str,
    now: DateTime<Utc>,
    today: NaiveDate,
    performed: PerformedAt,
    exercise: Option<&str>,
    inputs: &[SetInput],
) -> WorkoutResult<SetsLogged> {
    if inputs.is_empty() {
        return Err(WorkoutError::InvalidSet("no sets"));
    }
    if exercise.is_some_and(|q| exercise_key(q).is_empty()) {
        return Err(WorkoutError::InvalidSet("empty exercise name"));
    }
    for input in inputs {
        input.validate()?;
    }
    let date = performed.date;
    let live = date == today && (now - performed.at).abs() <= Duration::minutes(LIVE_SET_MINUTES);
    let (day, exercises) = todays_plan_exercises(pool, member_id, date).await?;
    let session = if live {
        open_or_start(pool, member_id, now, today, &exercises).await?
    } else {
        session_for_day(pool, member_id, now, today, performed, &exercises).await?
    };
    let (name, plan_index) = match exercise {
        Some(query) => match match_plan_exercise(&exercises, query) {
            Some(i) => (exercises[i].name.clone(), Some(i)),
            None => (capitalize(query), None),
        },
        None => match session.current_exercise.clone() {
            Some(name) => {
                let index = match_plan_exercise(&exercises, &name);
                (name, index)
            }
            None => return Err(WorkoutError::NoCurrentExercise),
        },
    };
    let key = exercise_key(&name);
    let mut sets = session_sets(pool, &session.id).await?;
    let earlier = sets.iter().filter(|s| s.exercise_key == key).count();

    // Resolve `x8` against the previous set; fall back to the last session's load.
    let mut previous = match sets.iter().rev().find(|s| s.exercise_key == key) {
        Some(set) => Some(set.weight_lb),
        None => exercise_history(pool, member_id, &key, date, 1)
            .await?
            .first()
            .map(|sets| working_sets(sets).0),
    };
    let mut records: Vec<SetRecord> = Vec::new();
    for input in inputs {
        let weight = match input.load {
            Load::Weight(w) => Some(w),
            Load::Bodyweight => None,
            Load::Same => previous.ok_or_else(|| WorkoutError::UnknownLoad {
                exercise: name.clone(),
            })?,
        };
        previous = Some(weight);
        records.extend(std::iter::repeat_n(
            (weight, input.reps),
            input.count as usize,
        ));
    }

    // Same UTC text format as the column default, so old and new rows sort together.
    let created_at = performed.at.format("%Y-%m-%d %H:%M:%S").to_string();
    let mut logged = Vec::with_capacity(records.len());
    let mut tx = pool.begin().await.map_err(db("Failed to begin set log"))?;
    for (weight, reps) in records {
        let set = LoggedSet {
            id: uuid::Uuid::new_v4().to_string(),
            exercise_key: key.clone(),
            exercise_name: name.clone(),
            weight_lb: weight,
            reps,
        };
        sqlx::query(
            "INSERT INTO fitness_set_log \
             (id, session_id, family_member_id, date, exercise_key, exercise_name, weight_lb, reps, \
              created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&set.id)
        .bind(&session.id)
        .bind(member_id)
        .bind(date.format("%Y-%m-%d").to_string())
        .bind(&set.exercise_key)
        .bind(&set.exercise_name)
        .bind(set.weight_lb)
        .bind(set.reps as i64)
        .bind(&created_at)
        .execute(&mut *tx)
        .await
        .map_err(db("Failed to save set"))?;
        logged.push(set);
    }
    tx.commit().await.map_err(db("Failed to commit set log"))?;
    sets.extend(logged.iter().cloned());

    // Planned sets done: move to the next plan lift that still needs sets.
    let planned_sets = |i: usize| exercises[i].sets.max(1) as usize;
    let (current, advance) = match plan_index {
        Some(index) if earlier + logged.len() >= planned_sets(index) => {
            let incomplete = |i: usize| {
                let key = exercise_key(&exercises[i].name);
                sets.iter().filter(|s| s.exercise_key == key).count() < planned_sets(i)
            };
            match (index + 1..exercises.len())
                .chain(0..index)
                .find(|i| incomplete(*i))
            {
                Some(i) => (exercises[i].name.clone(), Advance::Next(i)),
                None => (name.clone(), Advance::AllPlannedDone),
            }
        }
        _ => (name.clone(), Advance::Stay),
    };
    if live {
        touch_session(pool, &session.id, now, Some(&current)).await?;
    } else {
        touch_session_no_earlier(pool, &session.id, performed.at, Some(&current)).await?;
    }
    let state = build_state(
        pool,
        member_id,
        date,
        session.id,
        Some(current),
        day,
        exercises,
    )
    .await?;
    Ok(SetsLogged {
        state,
        exercise: name,
        logged,
        first_number: earlier + 1,
        advance,
    })
}

/// Delete one set from the member's own open workout; it becomes the current lift.
pub async fn delete_set(
    pool: &SqlitePool,
    member_id: &str,
    now: DateTime<Utc>,
    today: NaiveDate,
    set_id: &str,
) -> WorkoutResult<DeletedSet> {
    let session = active_session(pool, member_id, now, today)
        .await?
        .ok_or(WorkoutError::NoOpenWorkout)?;
    let row: Option<SetRow> = sqlx::query_as(
        "DELETE FROM fitness_set_log WHERE id = ? AND session_id = ? AND family_member_id = ? \
         RETURNING id, exercise_key, exercise_name, weight_lb, reps",
    )
    .bind(set_id)
    .bind(&session.id)
    .bind(member_id)
    .fetch_optional(pool)
    .await
    .map_err(db("Failed to delete set"))?;
    let set = row
        .map(LoggedSet::from_row)
        .ok_or(WorkoutError::SetNotFound)?;
    touch_session(pool, &session.id, now, Some(&set.exercise_name)).await?;
    let (day, exercises) = todays_plan_exercises(pool, member_id, today).await?;
    let state = build_state(
        pool,
        member_id,
        today,
        session.id,
        Some(set.exercise_name.clone()),
        day,
        exercises,
    )
    .await?;
    Ok(DeletedSet { set, state })
}

/// Close today's workout; per exercise, what was done and the next target.
pub async fn close_workout(
    pool: &SqlitePool,
    member_id: &str,
    now: DateTime<Utc>,
    today: NaiveDate,
) -> WorkoutResult<Vec<ExerciseSummary>> {
    let session = active_session(pool, member_id, now, today)
        .await?
        .ok_or(WorkoutError::NoOpenWorkout)?;
    let sets = session_sets(pool, &session.id).await?;
    close_session(pool, &session.id, now).await?;
    let (_, exercises) = todays_plan_exercises(pool, member_id, today).await?;
    let tomorrow = today + Duration::days(1);
    let mut summary = Vec::new();
    for (name, done) in group_by_exercise(&sets) {
        let next = match match_plan_exercise(&exercises, &name) {
            Some(i) => plan_target(pool, member_id, tomorrow, &exercises[i]).await?,
            None => None,
        };
        summary.push(ExerciseSummary {
            name,
            sets: done,
            next,
        });
    }
    Ok(summary)
}

// ─── Storage ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, sqlx::FromRow)]
struct WorkoutSession {
    id: String,
    date: String,
    current_exercise: Option<String>,
    last_activity_at: String,
}

/// Open session for today, closing it first when stale (other day or idle too long).
async fn active_session(
    pool: &SqlitePool,
    member_id: &str,
    now: DateTime<Utc>,
    today: NaiveDate,
) -> WorkoutResult<Option<WorkoutSession>> {
    let Some(session) = sqlx::query_as::<_, WorkoutSession>(
        "SELECT id, date, current_exercise, last_activity_at FROM fitness_workout_sessions \
         WHERE family_member_id = ? AND closed_at IS NULL",
    )
    .bind(member_id)
    .fetch_optional(pool)
    .await
    .map_err(db("Failed to load open workout"))?
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

/// Today's open session, or a new one on the first planned lift.
async fn open_or_start(
    pool: &SqlitePool,
    member_id: &str,
    now: DateTime<Utc>,
    today: NaiveDate,
    exercises: &[PlanExercise],
) -> WorkoutResult<WorkoutSession> {
    if let Some(session) = active_session(pool, member_id, now, today).await? {
        return Ok(session);
    }
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
    .bind(exercises.first().map(|e| e.name.as_str()))
    .bind(&now_s)
    .bind(&now_s)
    .execute(pool)
    .await
    .map_err(db("Failed to start workout"))?;
    active_session(pool, member_id, now, today)
        .await?
        .ok_or(WorkoutError::NoOpenWorkout)
}

/// The latest workout on `performed.date` (open or closed), else a new closed one.
/// A stale open workout is closed first, as for live logging.
async fn session_for_day(
    pool: &SqlitePool,
    member_id: &str,
    now: DateTime<Utc>,
    today: NaiveDate,
    performed: PerformedAt,
    exercises: &[PlanExercise],
) -> WorkoutResult<WorkoutSession> {
    active_session(pool, member_id, now, today).await?;
    let date = performed.date.format("%Y-%m-%d").to_string();
    if let Some(session) = latest_session_on(pool, member_id, &date).await? {
        return Ok(session);
    }
    let at = performed.at.to_rfc3339();
    sqlx::query(
        "INSERT INTO fitness_workout_sessions \
         (id, family_member_id, date, current_exercise, started_at, last_activity_at, closed_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(uuid::Uuid::new_v4().to_string())
    .bind(member_id)
    .bind(&date)
    .bind(exercises.first().map(|e| e.name.as_str()))
    .bind(&at)
    .bind(&at)
    .bind(&at)
    .execute(pool)
    .await
    .map_err(db("Failed to record past workout"))?;
    latest_session_on(pool, member_id, &date)
        .await?
        .ok_or(WorkoutError::NoOpenWorkout)
}

async fn latest_session_on(
    pool: &SqlitePool,
    member_id: &str,
    date: &str,
) -> WorkoutResult<Option<WorkoutSession>> {
    sqlx::query_as::<_, WorkoutSession>(
        "SELECT id, date, current_exercise, last_activity_at FROM fitness_workout_sessions \
         WHERE family_member_id = ? AND date = ? ORDER BY started_at DESC, rowid DESC LIMIT 1",
    )
    .bind(member_id)
    .bind(date)
    .fetch_optional(pool)
    .await
    .map_err(db("Failed to load workout for day"))
}

async fn close_session(
    pool: &SqlitePool,
    session_id: &str,
    now: DateTime<Utc>,
) -> WorkoutResult<()> {
    sqlx::query("UPDATE fitness_workout_sessions SET closed_at = ? WHERE id = ?")
        .bind(now.to_rfc3339())
        .bind(session_id)
        .execute(pool)
        .await
        .map_err(db("Failed to close workout"))?;
    Ok(())
}

async fn touch_session(
    pool: &SqlitePool,
    session_id: &str,
    now: DateTime<Utc>,
    current_exercise: Option<&str>,
) -> WorkoutResult<()> {
    sqlx::query(
        "UPDATE fitness_workout_sessions SET last_activity_at = ?, current_exercise = ? WHERE id = ?",
    )
    .bind(now.to_rfc3339())
    .bind(current_exercise)
    .bind(session_id)
    .execute(pool)
    .await
    .map_err(db("Failed to update workout"))?;
    Ok(())
}

/// Like `touch_session`, but `last_activity_at` only moves forward, so a
/// backdated set never makes a live workout go idle sooner.
async fn touch_session_no_earlier(
    pool: &SqlitePool,
    session_id: &str,
    at: DateTime<Utc>,
    current_exercise: Option<&str>,
) -> WorkoutResult<()> {
    sqlx::query(
        "UPDATE fitness_workout_sessions \
         SET last_activity_at = MAX(last_activity_at, ?), current_exercise = ? WHERE id = ?",
    )
    .bind(at.to_rfc3339())
    .bind(current_exercise)
    .bind(session_id)
    .execute(pool)
    .await
    .map_err(db("Failed to update workout"))?;
    Ok(())
}

async fn session_sets(pool: &SqlitePool, session_id: &str) -> WorkoutResult<Vec<LoggedSet>> {
    let rows: Vec<SetRow> = sqlx::query_as(
        "SELECT id, exercise_key, exercise_name, weight_lb, reps FROM fitness_set_log \
         WHERE session_id = ? ORDER BY rowid",
    )
    .bind(session_id)
    .fetch_all(pool)
    .await
    .map_err(db("Failed to load workout sets"))?;
    Ok(rows.into_iter().map(LoggedSet::from_row).collect())
}

/// Up to `limit` most recent days of sets for an exercise before `before` (newest first).
async fn exercise_history(
    pool: &SqlitePool,
    member_id: &str,
    key: &str,
    before: NaiveDate,
    limit: usize,
) -> WorkoutResult<Vec<Vec<SetRecord>>> {
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
    .map_err(db("Failed to load exercise history"))?;
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
) -> WorkoutResult<Option<Target>> {
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
) -> WorkoutResult<(Option<PlanDay>, Vec<PlanExercise>)> {
    let week_start = week_start_monday(today).format("%Y-%m-%d").to_string();
    let day = load_weekly_plan(pool, member_id, &week_start)
        .await
        .map_err(|e| WorkoutError::Plan(e.into()))?
        .and_then(|stored| session_for_date_from_stored(&stored, today));
    let exercises = day
        .as_ref()
        .filter(|d| d.kind != PlanDayKind::Rest)
        .map(|d| d.exercises.clone())
        .unwrap_or_default();
    Ok((day, exercises))
}

async fn build_state(
    pool: &SqlitePool,
    member_id: &str,
    today: NaiveDate,
    session_id: String,
    current_exercise: Option<String>,
    plan_day: Option<PlanDay>,
    exercises: Vec<PlanExercise>,
) -> WorkoutResult<WorkoutState> {
    let mut targets = Vec::with_capacity(exercises.len());
    for exercise in &exercises {
        targets.push(plan_target(pool, member_id, today, exercise).await?);
    }
    let sets = session_sets(pool, &session_id).await?;
    Ok(WorkoutState {
        session_id,
        current_exercise,
        plan_day,
        exercises,
        targets,
        sets,
    })
}

/// Sets grouped by exercise in first-logged order.
fn group_by_exercise(sets: &[LoggedSet]) -> Vec<(String, Vec<SetRecord>)> {
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

fn capitalize(raw: &str) -> String {
    let mut chars = raw.trim().chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

// ─── Signal replies ─────────────────────────────────────────────────────────

const NO_PLANNED_LIFTS: &str = "No lifts planned today — log any lift by name, e.g. `bench 135x8`.";

/// `/log [input]` in a linked DM: start or resume today's workout, optionally applying input.
pub async fn log_command_reply(
    pool: &SqlitePool,
    member_id: &str,
    now: DateTime<Utc>,
    today: NaiveDate,
    args: &str,
) -> Result<String> {
    if args.trim().is_empty() {
        let start = start_workout(pool, member_id, now, today).await?;
        return Ok(status_text(&start.state, start.resumed));
    }
    let Some(input) = parse_workout_input(args) else {
        return Ok(LOG_USAGE.into());
    };
    let state = match (workout_state(pool, member_id, now, today).await?, &input) {
        (Some(state), _) => state,
        (None, WorkoutInput::Sets { .. } | WorkoutInput::Jump(_)) => {
            start_workout(pool, member_id, now, today).await?.state
        }
        (None, _) => return Ok("No workout in progress. Send /log to start one.".into()),
    };
    apply_input(pool, member_id, now, today, &state, input).await
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
    let Some(state) = workout_state(pool, member_id, now, today).await? else {
        return Ok(None);
    };
    apply_input(pool, member_id, now, today, &state, input)
        .await
        .map(Some)
}

async fn apply_input(
    pool: &SqlitePool,
    member_id: &str,
    now: DateTime<Utc>,
    today: NaiveDate,
    state: &WorkoutState,
    input: WorkoutInput,
) -> Result<String> {
    match input {
        WorkoutInput::Done => Ok(finish_text(
            &close_workout(pool, member_id, now, today).await?,
        )),
        WorkoutInput::Undo => {
            let Some(last) = state.sets.last() else {
                touch_session(
                    pool,
                    &state.session_id,
                    now,
                    state.current_exercise.as_deref(),
                )
                .await?;
                return Ok("Nothing to undo in this workout.".into());
            };
            let deleted = delete_set(pool, member_id, now, today, &last.id).await?;
            Ok(format!(
                "↩️ Removed {} {}",
                deleted.set.exercise_name,
                format_sets(&[deleted.set.record()])
            ))
        }
        WorkoutInput::Next | WorkoutInput::Jump(_) => {
            let target = match input {
                WorkoutInput::Jump(n) => n - 1,
                _ => state.current_index().map_or(0, |i| i + 1),
            };
            if state.exercises.is_empty() {
                return Ok(NO_PLANNED_LIFTS.into());
            }
            if target >= state.exercises.len() {
                touch_session(
                    pool,
                    &state.session_id,
                    now,
                    state.current_exercise.as_deref(),
                )
                .await?;
                return Ok(match input {
                    WorkoutInput::Jump(_) => {
                        format!("Pick an exercise from #1 to #{}.", state.exercises.len())
                    }
                    _ => "That was the last planned lift. Log extras like `curl 30x12`, or send `done`."
                        .into(),
                });
            }
            let state = select_exercise(pool, member_id, now, today, target).await?;
            Ok(format!("Now: {}", exercise_line(&state, target)))
        }
        WorkoutInput::Sets { exercise, sets } => {
            let performed = PerformedAt {
                at: now,
                date: today,
            };
            match log_sets(pool, member_id, now, today, performed, exercise.as_deref(), &sets).await {
                Ok(logged) => Ok(logged_text(&logged)),
                Err(WorkoutError::NoCurrentExercise) => {
                    Ok("Which lift? Send it with the set, e.g. `bench 135x8`.".into())
                }
                Err(WorkoutError::UnknownLoad { exercise }) => Ok(format!(
                    "No weight yet for {exercise} — send it once like `135x8` (or `bwx8` for bodyweight)."
                )),
                Err(error) => Err(error.into()),
            }
        }
    }
}

fn exercise_line(state: &WorkoutState, index: usize) -> String {
    let suffix = state.targets[index]
        .as_ref()
        .map(target_suffix)
        .unwrap_or_default();
    format!(
        "{}. {}{}",
        index + 1,
        state.exercises[index].render(),
        suffix
    )
}

fn status_text(state: &WorkoutState, resumed: bool) -> String {
    let mut out = if resumed {
        "▶️ Workout resumed".to_string()
    } else {
        "▶️ Workout started".to_string()
    };
    if let Some(day) = &state.plan_day {
        let notes = day.notes.trim();
        out.push_str(&format!(" — {} {}", day.weekday, day.kind.as_str()));
        if !notes.is_empty() {
            out.push_str(&format!(": {notes}"));
        }
    }
    out.push('\n');

    for (i, exercise) in state.exercises.iter().enumerate() {
        let key = exercise_key(&exercise.name);
        let done: Vec<SetRecord> = state
            .sets
            .iter()
            .filter(|s| s.exercise_key == key)
            .map(LoggedSet::record)
            .collect();
        out.push_str(&format!(" {}", exercise_line(state, i)));
        if !done.is_empty() {
            out.push_str(&format!("  ✓ {}", format_sets(&done)));
        }
        out.push('\n');
    }
    let plan_keys: BTreeSet<String> = state
        .exercises
        .iter()
        .map(|e| exercise_key(&e.name))
        .collect();
    for (name, done) in group_by_exercise(&state.sets) {
        if !plan_keys.contains(&exercise_key(&name)) {
            out.push_str(&format!(" + {name}  ✓ {}\n", format_sets(&done)));
        }
    }
    if state.exercises.is_empty() {
        out.push_str(NO_PLANNED_LIFTS);
        out.push('\n');
    } else if let Some(current) = state.current_exercise.as_deref() {
        out.push_str(&format!("\nNow: {current}\n"));
    }
    out.push_str(SHORT_HELP);
    out
}

fn logged_text(logged: &SetsLogged) -> String {
    let records: Vec<SetRecord> = logged.logged.iter().map(LoggedSet::record).collect();
    let first = logged.first_number;
    let label = if records.len() == 1 {
        format!("set {first}")
    } else {
        format!("sets {first}–{}", first + records.len() - 1)
    };
    let mut out = format!("✓ {} {label}: {}", logged.exercise, format_sets(&records));
    match logged.advance {
        Advance::Stay => {}
        Advance::Next(i) => {
            out.push_str(&format!("\n→ Next: {}", exercise_line(&logged.state, i)));
        }
        Advance::AllPlannedDone => {
            out.push_str("\nAll planned lifts logged 💪 Send `done` to finish.");
        }
    }
    out
}

fn finish_text(summary: &[ExerciseSummary]) -> String {
    if summary.is_empty() {
        return "Workout closed — nothing was logged.".into();
    }
    let total: usize = summary.iter().map(|e| e.sets.len()).sum();
    let mut out = format!(
        "🏁 Workout logged: {total} sets across {} exercises.\n",
        summary.len()
    );
    for exercise in summary {
        out.push_str(&format!(
            "• {}: {}",
            exercise.name,
            format_sets(&exercise.sets)
        ));
        if let Some(target) = exercise.next.as_ref().filter(|t| !t.reps.is_empty()) {
            match target.weight_lb {
                Some(w) => out.push_str(&format!(
                    " → next {} lb, aim {}",
                    format_weight(w),
                    format_reps(&target.reps)
                )),
                None => out.push_str(&format!(" → next aim {}", format_reps(&target.reps))),
            }
        }
        out.push('\n');
    }
    out.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fitness_plan::{save_weekly_plan, WeeklyFitnessPlan};
    use chrono::{Datelike, TimeZone};

    fn set(load: Load, reps: u32, count: u32) -> SetInput {
        SetInput { load, reps, count }
    }

    /// A Signal-style set: performed at server time.
    fn live(now: DateTime<Utc>, today: NaiveDate) -> PerformedAt {
        PerformedAt {
            at: now,
            date: today,
        }
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
    async fn typed_core_start_log_jump_delete_close() {
        let pool = pool_with_plan().await;
        let monday = NaiveDate::from_ymd_opt(2026, 10, 19).unwrap();
        assert_eq!(
            workout_state(&pool, "alex", at(17, 0), monday)
                .await
                .unwrap(),
            None
        );

        let start = start_workout(&pool, "alex", at(17, 0), monday)
            .await
            .unwrap();
        assert!(!start.resumed);
        let state = start.state;
        assert_eq!(state.current_exercise.as_deref(), Some("Leg press"));
        assert_eq!(state.exercises.len(), 2);
        assert_eq!(state.targets.len(), 2);
        assert!(
            state.targets[0].as_ref().unwrap().reps.is_empty(),
            "first time"
        );
        assert!(state.sets.is_empty());
        let again = start_workout(&pool, "alex", at(17, 1), monday)
            .await
            .unwrap();
        assert!(again.resumed);
        assert_eq!(again.state.session_id, state.session_id);

        let logged = log_sets(
            &pool,
            "alex",
            at(17, 2),
            monday,
            live(at(17, 2), monday),
            None,
            &[set(Load::Weight(180.0), 12, 2)],
        )
        .await
        .unwrap();
        assert_eq!(logged.exercise, "Leg press");
        assert_eq!(logged.first_number, 1);
        assert_eq!(logged.logged.len(), 2);
        assert_eq!(logged.advance, Advance::Next(1));
        assert_eq!(
            logged.state.current_exercise.as_deref(),
            Some("Chest press")
        );
        assert_eq!(logged.state.sets, logged.logged);

        let jumped = select_exercise(&pool, "alex", at(17, 3), monday, 0)
            .await
            .unwrap();
        assert_eq!(jumped.current_exercise.as_deref(), Some("Leg press"));
        assert!(matches!(
            select_exercise(&pool, "alex", at(17, 3), monday, 2).await,
            Err(WorkoutError::ExerciseOutOfRange { index: 2, count: 2 })
        ));

        let same = log_sets(
            &pool,
            "alex",
            at(17, 4),
            monday,
            live(at(17, 4), monday),
            None,
            &[set(Load::Same, 10, 1)],
        )
        .await
        .unwrap();
        assert_eq!(same.logged[0].weight_lb, Some(180.0));
        assert_eq!(same.first_number, 3);

        let deleted = delete_set(&pool, "alex", at(17, 5), monday, &same.logged[0].id)
            .await
            .unwrap();
        assert_eq!(deleted.set, same.logged[0]);
        assert_eq!(deleted.state.sets, logged.logged);
        assert_eq!(deleted.state.current_exercise.as_deref(), Some("Leg press"));
        assert!(matches!(
            delete_set(&pool, "alex", at(17, 5), monday, &same.logged[0].id).await,
            Err(WorkoutError::SetNotFound)
        ));

        let summary = close_workout(&pool, "alex", at(17, 6), monday)
            .await
            .unwrap();
        assert_eq!(summary.len(), 1);
        assert_eq!(summary[0].name, "Leg press");
        assert_eq!(summary[0].sets, vec![(Some(180.0), 12), (Some(180.0), 12)]);
        assert_eq!(summary[0].next.as_ref().unwrap().weight_lb, Some(190.0));
        assert_eq!(
            workout_state(&pool, "alex", at(17, 7), monday)
                .await
                .unwrap(),
            None
        );
        assert!(matches!(
            close_workout(&pool, "alex", at(17, 7), monday).await,
            Err(WorkoutError::NoOpenWorkout)
        ));
        assert!(matches!(
            delete_set(&pool, "alex", at(17, 7), monday, &logged.logged[0].id).await,
            Err(WorkoutError::NoOpenWorkout)
        ));
    }

    #[tokio::test]
    async fn delete_set_refuses_another_members_set() {
        let pool = pool_with_plan().await;
        let monday = NaiveDate::from_ymd_opt(2026, 10, 19).unwrap();
        let theirs = log_sets(
            &pool,
            "sam",
            at(9, 0),
            monday,
            live(at(9, 0), monday),
            Some("bench"),
            &[set(Load::Weight(95.0), 8, 1)],
        )
        .await
        .unwrap();
        start_workout(&pool, "alex", at(9, 1), monday)
            .await
            .unwrap();
        assert!(matches!(
            delete_set(&pool, "alex", at(9, 2), monday, &theirs.logged[0].id).await,
            Err(WorkoutError::SetNotFound)
        ));
        let still = workout_state(&pool, "sam", at(9, 3), monday)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(still.sets, theirs.logged);
    }

    #[tokio::test]
    async fn log_sets_rejects_bad_input() {
        let pool = pool_with_plan().await;
        let monday = NaiveDate::from_ymd_opt(2026, 10, 19).unwrap();
        for (exercise, sets) in [
            (None, vec![]),
            (None, vec![set(Load::Weight(100.0), 0, 1)]),
            (None, vec![set(Load::Weight(100.0), 8, 11)]),
            (None, vec![set(Load::Weight(-5.0), 8, 1)]),
            (None, vec![set(Load::Weight(f64::NAN), 8, 1)]),
            (Some(" - "), vec![set(Load::Weight(100.0), 8, 1)]),
        ] {
            assert!(
                matches!(
                    log_sets(
                        &pool,
                        "alex",
                        at(9, 0),
                        monday,
                        live(at(9, 0), monday),
                        exercise,
                        &sets
                    )
                    .await,
                    Err(WorkoutError::InvalidSet(_))
                ),
                "{exercise:?} {sets:?}"
            );
        }
        assert_eq!(
            workout_state(&pool, "alex", at(9, 0), monday)
                .await
                .unwrap(),
            None,
            "invalid input never starts a workout"
        );
        assert!(matches!(
            log_sets(
                &pool,
                "alex",
                at(9, 1),
                monday,
                live(at(9, 1), monday),
                Some("curl"),
                &[set(Load::Same, 8, 1)]
            )
            .await,
            Err(WorkoutError::UnknownLoad { exercise }) if exercise == "Curl"
        ));
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

    // ─── App timestamps (performed_at) ──────────────────────────────────────

    /// Household clock for app-timestamp tests (EDT, UTC-4, in October).
    fn toronto(day: u32, hour: u32, minute: u32) -> DateTime<chrono_tz::Tz> {
        chrono_tz::America::Toronto
            .with_ymd_and_hms(2026, 10, day, hour, minute, 0)
            .unwrap()
    }

    /// Log one app set: `performed_at` resolved against household `now`.
    async fn app_set(
        pool: &SqlitePool,
        now: DateTime<chrono_tz::Tz>,
        performed_at: &str,
    ) -> SetsLogged {
        let performed =
            resolve_performed_at(Some(parse_performed_at(performed_at).unwrap()), now).unwrap();
        log_sets(
            pool,
            "alex",
            now.with_timezone(&Utc),
            now.date_naive(),
            performed,
            Some("leg press"),
            &[set(Load::Weight(180.0), 10, 1)],
        )
        .await
        .unwrap()
    }

    async fn stored_set(pool: &SqlitePool, id: &str) -> (String, String, String) {
        sqlx::query_as("SELECT date, created_at, session_id FROM fitness_set_log WHERE id = ?")
            .bind(id)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    async fn live_set(pool: &SqlitePool, now: DateTime<chrono_tz::Tz>) -> SetsLogged {
        let performed = resolve_performed_at(None, now).unwrap();
        log_sets(
            pool,
            "alex",
            now.with_timezone(&Utc),
            now.date_naive(),
            performed,
            Some("leg press"),
            &[set(Load::Weight(180.0), 10, 1)],
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn set_from_two_hours_ago_joins_that_days_idle_closed_workout() {
        let pool = pool_with_plan().await;
        let monday = NaiveDate::from_ymd_opt(2026, 10, 19).unwrap();
        let first = live_set(&pool, toronto(19, 15, 0)).await;
        let session = first.state.session_id.clone();

        // 18:30: idle > 3h, so the workout is closed.
        let now = toronto(19, 18, 30);
        assert_eq!(
            workout_state(&pool, "alex", now.with_timezone(&Utc), monday)
                .await
                .unwrap(),
            None
        );

        let late = app_set(&pool, now, "2026-10-19T16:30:00-04:00").await;
        assert_eq!(late.state.session_id, session, "joins the closed workout");
        assert_eq!(late.first_number, 2);
        assert_eq!(
            stored_set(&pool, &late.logged[0].id).await,
            (
                "2026-10-19".into(),
                "2026-10-19 20:30:00".into(),
                session.clone()
            )
        );
        assert_eq!(
            workout_state(&pool, "alex", now.with_timezone(&Utc), monday)
                .await
                .unwrap(),
            None,
            "a backdated set does not reopen the workout"
        );

        // A live set still starts a new workout, as Signal always has.
        let fresh = live_set(&pool, toronto(19, 18, 31)).await;
        assert_ne!(fresh.state.session_id, session);
        assert_eq!(fresh.first_number, 1);
    }

    #[tokio::test]
    async fn backdated_sets_without_a_workout_share_a_new_closed_one() {
        let pool = pool_with_plan().await;
        let monday = NaiveDate::from_ymd_opt(2026, 10, 19).unwrap();
        let now = toronto(19, 20, 0);
        let a = app_set(&pool, now, "2026-10-19T18:00:00-04:00").await;
        let b = app_set(&pool, now, "2026-10-19T18:03:00-04:00").await;
        assert_eq!(a.state.session_id, b.state.session_id);
        assert_eq!(b.first_number, 2);
        assert_eq!(
            workout_state(&pool, "alex", now.with_timezone(&Utc), monday)
                .await
                .unwrap(),
            None,
            "the past workout is created closed"
        );
    }

    #[tokio::test]
    async fn backdated_set_joins_open_workout_without_rewinding_its_clock() {
        let pool = pool_with_plan().await;
        let monday = NaiveDate::from_ymd_opt(2026, 10, 19).unwrap();
        let first = live_set(&pool, toronto(19, 17, 0)).await;
        let late = app_set(&pool, toronto(19, 17, 0), "2026-10-19T16:50:00-04:00").await;
        assert_eq!(late.state.session_id, first.state.session_id);
        // Idle is still measured from 17:00, not 16:50.
        let open = workout_state(
            &pool,
            "alex",
            toronto(19, 19, 59).with_timezone(&Utc),
            monday,
        )
        .await
        .unwrap();
        assert_eq!(
            open.map(|s| s.session_id),
            Some(first.state.session_id.clone())
        );
    }

    #[tokio::test]
    async fn set_sent_after_midnight_lands_on_the_earlier_day() {
        let pool = pool_with_plan().await;
        let first = live_set(&pool, toronto(19, 23, 30)).await;
        // 23:50 EDT on Monday, sent as UTC (already Tuesday there) at 00:10 Tuesday.
        let late = app_set(&pool, toronto(20, 0, 10), "2026-10-20T03:50:00Z").await;
        assert_eq!(late.state.session_id, first.state.session_id);
        assert_eq!(late.first_number, 2);
        let (date, created_at, _) = stored_set(&pool, &late.logged[0].id).await;
        assert_eq!(date, "2026-10-19");
        assert_eq!(created_at, "2026-10-20 03:50:00");
    }

    #[test]
    fn performed_at_validation() {
        let now = toronto(19, 12, 0);
        let parse = |raw: &str| parse_performed_at(raw).unwrap();
        assert_eq!(
            resolve_performed_at(None, now),
            Ok(PerformedAt::new(now)),
            "missing means server time"
        );
        assert_eq!(
            resolve_performed_at(Some(parse("2026-10-19T12:05:00-04:00")), now),
            Ok(PerformedAt::new(toronto(19, 12, 5))),
            "5 minutes ahead is clock skew"
        );
        assert_eq!(
            resolve_performed_at(Some(parse("2026-10-19T12:05:01-04:00")), now),
            Err(PerformedAtError::InFuture)
        );
        assert!(resolve_performed_at(Some(parse("2026-10-12T12:00:00-04:00")), now).is_ok());
        assert_eq!(
            resolve_performed_at(Some(parse("2026-10-12T11:59:00-04:00")), now),
            Err(PerformedAtError::TooOld)
        );
        for raw in ["2026-10-19T12:00:00", "2026-10-19", "yesterday", ""] {
            assert_eq!(
                parse_performed_at(raw),
                Err(PerformedAtError::Invalid),
                "{raw}"
            );
        }
    }
}
