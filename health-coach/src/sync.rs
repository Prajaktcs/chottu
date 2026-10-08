use anyhow::{bail, Context, Result};
use chotu_common::{
    health_refresh_token_env_key, resolve_health_refresh_token, AppConfig, ExerciseSession,
    FoodLog, GeminiClient, GoogleHealthClient, GoogleHealthFoodSummary, MissingSyncNutrition,
    NutritionLogWrite,
};
use sqlx::SqlitePool;

/// Result of a successful Google Health sync for one family member / day.
#[derive(Debug, Clone)]
pub struct HealthSyncReport {
    pub member_id: String,
    pub date: String,
    pub calories: i32,
    pub protein: f64,
    pub carbs: f64,
    pub fats: f64,
    pub saturated_fat: f64,
    pub unsaturated_fat: f64,
    pub cholesterol: f64,
    pub iron: f64,
    pub vitamin_b: f64,
    pub vitamin_c: f64,
    pub fiber: f64,
    pub sugar: f64,
    pub sodium: f64,
    pub omega_3_dha_mg: f64,
    pub triglycerides_mg: f64,
    pub steps: i32,
    pub active_calories: i32,
    pub sleep_hours: Option<f64>,
    pub exercises: Vec<ExerciseSession>,
    /// Number of Signal `/food` rows merged on top of Google Health totals.
    pub manual_food_entries: i64,
}

impl HealthSyncReport {
    /// Plain-text summary for Signal DMs (no `*`/`_` emphasis; backticks OK).
    pub fn signal_text(&self) -> String {
        let sleep_str = match self.sleep_hours {
            Some(h) => format!("{:.1} hours", h),
            None => "No sleep log".to_string(),
        };

        let exercise_str = if self.exercises.is_empty() {
            "None logged".to_string()
        } else {
            self.exercises
                .iter()
                .map(|e| format!("• {}", e.display()))
                .collect::<Vec<_>>()
                .join("\n")
        };

        let manual_note = if self.manual_food_entries > 0 {
            format!(
                "\nIncludes {} `/food` entr{}",
                self.manual_food_entries,
                if self.manual_food_entries == 1 {
                    "y"
                } else {
                    "ies"
                }
            )
        } else {
            String::new()
        };

        format!(
            "✅ Google Health Sync Complete!\n\n\
             Logged metrics for {} on {}:{}\n\n\
             Activity & Sleep:\n\
             • Steps: {} steps\n\
             • Active Energy: {} kcal\n\
             • Sleep Duration: {}\n\n\
             Exercises:\n\
             {}\n\n\
             Nutrition:\n\
             • Calories: {} kcal\n\
             • Protein: {:.1}g | Carbs: {:.1}g | Fats: {:.1}g\n\
             • Fiber: {:.1}g | Sugar: {:.1}g | Sodium: {:.0}mg",
            self.member_id,
            self.date,
            manual_note,
            self.steps,
            self.active_calories,
            sleep_str,
            exercise_str,
            self.calories,
            self.protein,
            self.carbs,
            self.fats,
            self.fiber,
            self.sugar,
            self.sodium
        )
    }
}

/// Build a Google Health client from the legacy shared `FITBIT_REFRESH_TOKEN`.
/// Prefer [`google_health_client_for_member`] for per-account sync.
pub fn google_health_client_from_env() -> Result<GoogleHealthClient> {
    let client_id = std::env::var("FITBIT_CLIENT_ID")
        .context("FITBIT_CLIENT_ID not found in environment. Please add it to your .env file.")?;
    let client_secret = std::env::var("FITBIT_CLIENT_SECRET").context(
        "FITBIT_CLIENT_SECRET not found in environment. Please add it to your .env file.",
    )?;
    let refresh_token = std::env::var("FITBIT_REFRESH_TOKEN").context(
        "FITBIT_REFRESH_TOKEN not found in environment. Please add it to your .env file.",
    )?;
    Ok(GoogleHealthClient::new(
        client_id,
        client_secret,
        refresh_token,
    ))
}

/// Build a Google Health client for a specific family member.
pub fn google_health_client_for_member(
    member_id: &str,
    config: &AppConfig,
) -> Result<GoogleHealthClient> {
    let client_id = std::env::var("FITBIT_CLIENT_ID")
        .context("FITBIT_CLIENT_ID not found in environment. Please add it to your .env file.")?;
    let client_secret = std::env::var("FITBIT_CLIENT_SECRET").context(
        "FITBIT_CLIENT_SECRET not found in environment. Please add it to your .env file.",
    )?;
    let refresh_token = resolve_health_refresh_token(member_id, config).with_context(|| {
        format!(
            "No Google Health refresh token for member `{}`. \
             Run `/login health {}` (saves `{}`, with legacy `FITBIT_REFRESH_TOKEN` for the primary).",
            member_id,
            member_id,
            health_refresh_token_env_key(member_id)
        )
    })?;
    Ok(GoogleHealthClient::new(
        client_id,
        client_secret,
        refresh_token,
    ))
}

/// True when this member has a usable Google Health refresh token.
pub fn member_health_credentials_configured(member_id: &str, config: &AppConfig) -> bool {
    oauth_app_configured() && resolve_health_refresh_token(member_id, config).is_some()
}

fn oauth_app_configured() -> bool {
    std::env::var("FITBIT_CLIENT_ID").is_ok() && std::env::var("FITBIT_CLIENT_SECRET").is_ok()
}

fn any_health_refresh_token_present() -> bool {
    if std::env::var("FITBIT_REFRESH_TOKEN")
        .ok()
        .is_some_and(|t| !t.is_empty())
    {
        return true;
    }
    std::env::vars().any(|(k, v)| k.starts_with("HEALTH_REFRESH_TOKEN_") && !v.is_empty())
}

pub(crate) fn food_log_to_nutrition_write(log: &FoodLog) -> NutritionLogWrite {
    let start = log.timestamp;
    let end = start + chrono::Duration::minutes(1);
    NutritionLogWrite {
        food_display_name: log.raw_text_description.clone(),
        start_time: start,
        end_time: end,
        calories_kcal: log.estimated_calories as f64,
        carbs_g: log.estimated_carbs,
        fat_g: log.estimated_fats,
        protein_g: log.estimated_protein,
        cholesterol_mg: log.estimated_cholesterol_mg,
        saturated_fat_g: log.estimated_saturated_fat_g,
        unsaturated_fat_g: log.estimated_unsaturated_fat_g,
        iron_mg: log.estimated_iron_mg,
        vitamin_b_mg: log.estimated_vitamin_b_mg,
        vitamin_c_mg: log.estimated_vitamin_c_mg,
        sugar_g: log.estimated_sugar_g,
        fiber_g: log.estimated_fiber_g,
        sodium_mg: log.estimated_sodium_mg,
        potassium_mg: log.estimated_potassium_mg,
        calcium_mg: log.estimated_calcium_mg,
        magnesium_mg: log.estimated_magnesium_mg,
        zinc_mg: log.estimated_zinc_mg,
        vitamin_a_mcg: log.estimated_vitamin_a_mcg,
        vitamin_d_mcg: log.estimated_vitamin_d_mcg,
        vitamin_e_mg: log.estimated_vitamin_e_mg,
        vitamin_k_mcg: log.estimated_vitamin_k_mcg,
        caffeine_mg: log.estimated_caffeine_mg,
        trans_fat_g: log.estimated_trans_fat_g,
    }
}

/// Push a single local food_log row to Google Health and store the returned data-point name.
pub async fn push_food_log_to_google(
    pool: &SqlitePool,
    client: &GoogleHealthClient,
    log: &FoodLog,
) -> Result<String> {
    crate::food_corrections::ensure_food_log_upload_state(pool, log).await?;
    crate::food_corrections::sync_food_log_with_client(pool, client, &log.id).await?;
    sqlx::query_scalar("SELECT google_data_point_id FROM food_log WHERE id = ?")
        .bind(&log.id)
        .fetch_optional(pool)
        .await?
        .flatten()
        .context("Food entry was deleted during Google sync")
}

async fn pending_food_logs_for_sync(
    pool: &SqlitePool,
    member_id: &str,
    date: &str,
    timezone: chrono_tz::Tz,
) -> Result<Vec<FoodLog>> {
    let (start, end) = chotu_common::civil_day_bounds_utc(date, timezone)?;
    sqlx::query_as(
        "SELECT * FROM food_log \
         WHERE family_member_id = ? \
           AND NOT EXISTS (SELECT 1 FROM food_log_deletion_intents d WHERE d.food_log_id = food_log.id) \
           AND ((julianday(timestamp) >= julianday(?) AND julianday(timestamp) < julianday(?) \
                 AND (google_data_point_id IS NULL OR google_data_point_id = '')) \
                OR EXISTS (SELECT 1 FROM food_log_corrections c WHERE c.food_log_id = food_log.id AND c.sync_state != 'synced') \
                OR EXISTS (SELECT 1 FROM food_log_remote_resources r LEFT JOIN food_log_corrections c ON c.food_log_id = r.food_log_id \
                           WHERE r.food_log_id = food_log.id AND (r.remote_name IS NOT c.remote_name OR r.create_pending = 1))) \
         ORDER BY timestamp ASC",
    )
    .bind(member_id)
    .bind(start)
    .bind(end)
    .fetch_all(pool)
    .await
    .context("Failed to fetch pending food_log rows")
}

/// Best-effort push of unsynced meals on this day and outstanding corrections on any day.
pub async fn push_pending_food_logs(
    pool: &SqlitePool,
    client: &GoogleHealthClient,
    member_id: &str,
    date: &str,
    timezone: chrono_tz::Tz,
) -> Result<usize> {
    let pending = pending_food_logs_for_sync(pool, member_id, date, timezone).await?;

    let mut pushed = 0;
    for log in pending {
        // Skip pure local adjustment audit rows — they are not real meals.
        if log.raw_text_description.starts_with("Manual adjustment:") {
            continue;
        }
        match push_food_log_to_google(pool, client, &log).await {
            Ok(_) => pushed += 1,
            Err(e) => eprintln!(
                "Health Coach: Failed to push food_log {} to Google Health: {:?}",
                log.id, e
            ),
        }
    }
    Ok(pushed)
}

async fn correction_versions<'e>(
    executor: impl sqlx::Executor<'e, Database = sqlx::Sqlite>,
    member_id: &str,
) -> Result<Vec<(String, i64, String)>> {
    Ok(sqlx::query_as(
        "SELECT c.food_log_id, c.revision, c.sync_state FROM food_log_corrections c \
         JOIN food_log f ON f.id = c.food_log_id WHERE f.family_member_id = ? \
         UNION ALL SELECT 'deletion:' || food_log_id, revision, \
         CASE completed WHEN 1 THEN 'synced' ELSE 'deleting' END \
         FROM food_log_deletion_intents WHERE family_member_id = ? \
         UNION ALL SELECT 'resource:' || r.food_log_id || ':' || r.remote_name, r.revision, \
         CASE WHEN c.sync_state = 'synced' AND r.remote_name = c.remote_name AND r.create_pending = 0 \
              THEN 'synced' ELSE 'cleanup' END \
         FROM food_log_remote_resources r JOIN food_log f ON f.id = r.food_log_id \
         LEFT JOIN food_log_corrections c ON c.food_log_id = r.food_log_id \
         WHERE f.family_member_id = ? ORDER BY 1",
    ).bind(member_id).bind(member_id).bind(member_id).fetch_all(executor).await?)
}

async fn guard_summary_refresh(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    member_id: &str,
    date: &str,
    expected: &[(String, i64, String)],
) -> Result<()> {
    // Serialize the final revision check with correction/undo writes, not HTTP calls.
    sqlx::query(
        "UPDATE health_family_summary SET date = date WHERE date = ? AND family_member_id = ?",
    )
    .bind(date)
    .bind(member_id)
    .execute(&mut **tx)
    .await?;
    let current = correction_versions(&mut **tx, member_id).await?;
    if current != expected || current.iter().any(|(_, _, state)| state != "synced") {
        bail!("Food corrections/deletions changed while Google sync was running; local totals were preserved. Retry /sync");
    }
    Ok(())
}

/// Syncs Google Health metrics for `member_id` on `date` (YYYY-MM-DD) into SQLite.
///
/// Nutrition is Google Health's daily rollup (which already includes local meals
/// that were pushed upstream) **plus** any still-unsynced local `/food` rows.
pub async fn sync_member_for_date(
    pool: &SqlitePool,
    gemini_client: Option<&GeminiClient>,
    config: &AppConfig,
    member_id: &str,
    date: &str,
) -> Result<HealthSyncReport> {
    crate::food_corrections::resume_pending_food_deletions(pool, config, member_id).await?;
    let client = google_health_client_for_member(member_id, config)?;

    // Best-effort: push any pending local meals so Google becomes the shared store.
    if let Err(e) =
        push_pending_food_logs(pool, &client, member_id, date, config.resolved_tz()).await
    {
        eprintln!(
            "Health Coach: Failed to push pending food logs to Google Health: {:?}",
            e
        );
    }
    // A pending immutable-log replacement has an old or ambiguous upstream value.
    // Do not add the corrected local meal to that value or erase it with a stale rollup.
    crate::food_corrections::ensure_corrections_synced_for_member(
        pool,
        member_id,
        config.resolved_tz(),
    )
    .await?;
    let versions = correction_versions(pool, member_id).await?;

    let summary: GoogleHealthFoodSummary = client.fetch_nutrition_summary(date).await?;

    let gemini_est = match gemini_client {
        Some(g) => g
            .estimate_missing_sync_nutrients(
                summary.calories,
                summary.protein,
                summary.carbs,
                summary.fat,
            )
            .await
            .unwrap_or_else(|e| {
                eprintln!(
                    "Health Coach: Failed to estimate missing nutrients via Gemini: {:?}",
                    e
                );
                MissingSyncNutrition {
                    omega_3_dha_mg: 0.0,
                    triglycerides_mg: 0.0,
                }
            }),
        None => MissingSyncNutrition {
            omega_3_dha_mg: 0.0,
            triglycerides_mg: 0.0,
        },
    };

    let steps = client.fetch_steps_summary(date).await.unwrap_or(0);
    let active_calories = client.fetch_active_energy_summary(date).await.unwrap_or(0);
    let sleep_hours = client.fetch_sleep_summary(date).await.ok();
    let exercises = client
        .fetch_exercise_sessions(date)
        .await
        .unwrap_or_default();

    persist_sync_snapshot(
        pool,
        member_id,
        date,
        config.resolved_tz(),
        &versions,
        SyncSnapshot {
            summary,
            gemini_est,
            steps,
            active_calories,
            sleep_hours,
            exercises,
        },
    )
    .await
}

struct SyncSnapshot {
    summary: GoogleHealthFoodSummary,
    gemini_est: MissingSyncNutrition,
    steps: i32,
    active_calories: i32,
    sleep_hours: Option<f64>,
    exercises: Vec<ExerciseSession>,
}

async fn persist_sync_snapshot(
    pool: &SqlitePool,
    member_id: &str,
    date: &str,
    timezone: chrono_tz::Tz,
    versions: &[(String, i64, String)],
    snapshot: SyncSnapshot,
) -> Result<HealthSyncReport> {
    let SyncSnapshot {
        summary,
        gemini_est,
        steps,
        active_calories,
        sleep_hours,
        exercises,
    } = snapshot;
    let mut tx = pool.begin().await?;
    guard_summary_refresh(&mut tx, member_id, date, versions).await?;
    // Include adjustment audits committed during remote reads. The guard's write
    // lock keeps this local snapshot and the summary replacement atomic.
    let manual = sum_food_log_for_day_filtered(
        &mut *tx,
        member_id,
        date,
        timezone,
        FoodLogSyncFilter::UnsyncedOnly,
    )
    .await?;

    let calories = (summary.calories as i32).saturating_add(manual.calories as i32);
    let protein = summary.protein + manual.protein;
    let carbs = summary.carbs + manual.carbs;
    let fats = summary.fat + manual.fats;
    let omega_3 = gemini_est.omega_3_dha_mg + manual.omega_3_dha_mg;
    let cholesterol = summary.cholesterol + manual.cholesterol_mg;
    let saturated_fat = summary.saturated_fat + manual.saturated_fat_g;
    let unsaturated_fat = summary.unsaturated_fat + manual.unsaturated_fat_g;
    let triglycerides = gemini_est.triglycerides_mg + manual.triglycerides_mg;
    let iron = summary.iron + manual.iron_mg;
    let vitamin_b = summary.vitamin_b + manual.vitamin_b_mg;
    let vitamin_c = summary.vitamin_c + manual.vitamin_c_mg;
    let sugar = summary.sugar + manual.sugar_g;
    let fiber = summary.fiber + manual.fiber_g;
    let sodium = summary.sodium + manual.sodium_mg;
    let potassium = summary.potassium + manual.potassium_mg;
    let calcium = summary.calcium + manual.calcium_mg;
    let magnesium = summary.magnesium + manual.magnesium_mg;
    let zinc = summary.zinc + manual.zinc_mg;
    let vitamin_a = summary.vitamin_a + manual.vitamin_a_mcg;
    let vitamin_d = summary.vitamin_d + manual.vitamin_d_mcg;
    let vitamin_e = summary.vitamin_e + manual.vitamin_e_mg;
    let vitamin_k = summary.vitamin_k + manual.vitamin_k_mcg;
    let caffeine = summary.caffeine + manual.caffeine_mg;
    let trans_fat = summary.trans_fat + manual.trans_fat_g;

    sqlx::query(
        r#"
        INSERT INTO health_family_summary (
            date,
            family_member_id,
            total_calories_ingested,
            protein_grams,
            carbs_grams,
            fats_grams,
            omega_3_dha_mg,
            cholesterol_mg,
            saturated_fat_g,
            unsaturated_fat_g,
            triglycerides_mg,
            iron_mg,
            vitamin_b_mg,
            vitamin_c_mg,
            sugar_g,
            fiber_g,
            sodium_mg,
            potassium_mg,
            calcium_mg,
            magnesium_mg,
            zinc_mg,
            vitamin_a_mcg,
            vitamin_d_mcg,
            vitamin_e_mg,
            vitamin_k_mcg,
            caffeine_mg,
            trans_fat_g,
            step_count,
            active_calories_burned,
            sleep_hours
        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        ON CONFLICT(date, family_member_id) DO UPDATE SET
            total_calories_ingested = excluded.total_calories_ingested,
            protein_grams = excluded.protein_grams,
            carbs_grams = excluded.carbs_grams,
            fats_grams = excluded.fats_grams,
            omega_3_dha_mg = excluded.omega_3_dha_mg,
            cholesterol_mg = excluded.cholesterol_mg,
            saturated_fat_g = excluded.saturated_fat_g,
            unsaturated_fat_g = excluded.unsaturated_fat_g,
            triglycerides_mg = excluded.triglycerides_mg,
            iron_mg = excluded.iron_mg,
            vitamin_b_mg = excluded.vitamin_b_mg,
            vitamin_c_mg = excluded.vitamin_c_mg,
            sugar_g = excluded.sugar_g,
            fiber_g = excluded.fiber_g,
            sodium_mg = excluded.sodium_mg,
            potassium_mg = excluded.potassium_mg,
            calcium_mg = excluded.calcium_mg,
            magnesium_mg = excluded.magnesium_mg,
            zinc_mg = excluded.zinc_mg,
            vitamin_a_mcg = excluded.vitamin_a_mcg,
            vitamin_d_mcg = excluded.vitamin_d_mcg,
            vitamin_e_mg = excluded.vitamin_e_mg,
            vitamin_k_mcg = excluded.vitamin_k_mcg,
            caffeine_mg = excluded.caffeine_mg,
            trans_fat_g = excluded.trans_fat_g,
            step_count = excluded.step_count,
            active_calories_burned = excluded.active_calories_burned,
            sleep_hours = excluded.sleep_hours;
        "#,
    )
    .bind(date)
    .bind(member_id)
    .bind(calories)
    .bind(protein)
    .bind(carbs)
    .bind(fats)
    .bind(omega_3)
    .bind(cholesterol)
    .bind(saturated_fat)
    .bind(unsaturated_fat)
    .bind(triglycerides)
    .bind(iron)
    .bind(vitamin_b)
    .bind(vitamin_c)
    .bind(sugar)
    .bind(fiber)
    .bind(sodium)
    .bind(potassium)
    .bind(calcium)
    .bind(magnesium)
    .bind(zinc)
    .bind(vitamin_a)
    .bind(vitamin_d)
    .bind(vitamin_e)
    .bind(vitamin_k)
    .bind(caffeine)
    .bind(trans_fat)
    .bind(steps)
    .bind(active_calories)
    .bind(sleep_hours)
    .execute(&mut *tx)
    .await
    .context("Failed to update health_family_summary in database")?;
    tx.commit().await?;

    replace_exercise_log_for_day(pool, member_id, date, &exercises)
        .await
        .context("Failed to persist exercise_log for sync day")?;

    let report = HealthSyncReport {
        member_id: member_id.to_string(),
        date: date.to_string(),
        calories,
        protein,
        carbs,
        fats,
        saturated_fat,
        unsaturated_fat,
        cholesterol,
        iron,
        vitamin_b,
        vitamin_c,
        fiber,
        sugar,
        sodium,
        omega_3_dha_mg: omega_3,
        triglycerides_mg: triglycerides,
        steps,
        active_calories,
        sleep_hours,
        exercises,
        manual_food_entries: manual.entry_count,
    };

    Ok(report)
}

/// One persisted exercise_log row (structured fields optional for pre-migration rows).
#[derive(Debug, Clone, Default, sqlx::FromRow)]
pub struct ExerciseLogEntry {
    pub date: String,
    pub description: String,
    pub activity_type: Option<String>,
    pub duration_minutes: Option<i32>,
    pub active_calories: Option<f64>,
    pub start_at: Option<String>,
    pub end_at: Option<String>,
}

impl ExerciseLogEntry {
    /// Prefer stored activity_type; otherwise the label before ` (` in description.
    pub fn activity_label(&self) -> String {
        if let Some(t) = self
            .activity_type
            .as_ref()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
        {
            return t.to_string();
        }
        let desc = self.description.trim();
        match desc.find(" (") {
            Some(i) if i > 0 => desc[..i].to_string(),
            _ => desc.to_string(),
        }
    }

    pub fn duration_mins(&self) -> i32 {
        self.duration_minutes.unwrap_or(0).max(0)
    }
}

/// Replace Google Health exercise rows for one member/day (atomic delete+insert).
pub async fn replace_exercise_log_for_day(
    pool: &SqlitePool,
    member_id: &str,
    date: &str,
    exercises: &[ExerciseSession],
) -> Result<()> {
    let mut tx = pool
        .begin()
        .await
        .context("Failed to begin exercise_log transaction")?;

    sqlx::query("DELETE FROM exercise_log WHERE family_member_id = ? AND date = ? AND source = 'google_health'")
        .bind(member_id)
        .bind(date)
        .execute(&mut *tx)
        .await
        .context("Failed to clear exercise_log for day")?;

    for session in exercises {
        let desc = session.display();
        if desc.trim().is_empty() {
            continue;
        }
        let id = uuid::Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO exercise_log \
             (id, date, family_member_id, description, source, \
              activity_type, duration_minutes, active_calories, start_at, end_at) \
             VALUES (?, ?, ?, ?, 'google_health', ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(date)
        .bind(member_id)
        .bind(&desc)
        .bind(&session.activity_type)
        .bind(session.duration_minutes)
        .bind(session.active_calories)
        .bind(&session.start_at)
        .bind(&session.end_at)
        .execute(&mut *tx)
        .await
        .with_context(|| format!("Failed to insert exercise_log row for {}", member_id))?;
    }

    tx.commit()
        .await
        .context("Failed to commit exercise_log transaction")?;
    Ok(())
}

/// Exercise descriptions logged for a member on a civil day.
pub async fn exercises_for_day(
    pool: &SqlitePool,
    member_id: &str,
    date: &str,
) -> Result<Vec<String>> {
    Ok(exercise_entries_for_day(pool, member_id, date)
        .await?
        .into_iter()
        .map(|e| e.description)
        .collect())
}

/// Structured exercise rows for a member on a civil day.
pub async fn exercise_entries_for_day(
    pool: &SqlitePool,
    member_id: &str,
    date: &str,
) -> Result<Vec<ExerciseLogEntry>> {
    let rows: Vec<ExerciseLogEntry> = sqlx::query_as(
        "SELECT date, description, activity_type, duration_minutes, active_calories, start_at, end_at \
         FROM exercise_log \
         WHERE family_member_id = ? AND date = ? \
         ORDER BY created_at ASC, id ASC",
    )
    .bind(member_id)
    .bind(date)
    .fetch_all(pool)
    .await
    .context("Failed to query exercise_log")?;
    Ok(rows)
}

/// Exercise descriptions for a member between `start_date` and `end_date` inclusive.
pub async fn exercises_for_range(
    pool: &SqlitePool,
    member_id: &str,
    start_date: &str,
    end_date: &str,
) -> Result<Vec<(String, String)>> {
    Ok(
        exercise_entries_for_range(pool, member_id, start_date, end_date)
            .await?
            .into_iter()
            .map(|e| (e.date, e.description))
            .collect(),
    )
}

/// Structured exercise rows for a member between `start_date` and `end_date` inclusive.
pub async fn exercise_entries_for_range(
    pool: &SqlitePool,
    member_id: &str,
    start_date: &str,
    end_date: &str,
) -> Result<Vec<ExerciseLogEntry>> {
    let rows: Vec<ExerciseLogEntry> = sqlx::query_as(
        "SELECT date, description, activity_type, duration_minutes, active_calories, start_at, end_at \
         FROM exercise_log \
         WHERE family_member_id = ? AND date >= ? AND date <= ? \
         ORDER BY date ASC, created_at ASC",
    )
    .bind(member_id)
    .bind(start_date)
    .bind(end_date)
    .fetch_all(pool)
    .await
    .context("Failed to query exercise_log range")?;
    Ok(rows)
}

/// Aggregated nutrition from `food_log` (and the non-`food_log` "external" base
/// inferred as `summary − food_log`, typically Google Health).
#[derive(Debug, Clone, Default, sqlx::FromRow)]
pub struct DayNutritionTotals {
    pub calories: i64,
    pub protein: f64,
    pub carbs: f64,
    pub fats: f64,
    pub omega_3_dha_mg: f64,
    pub cholesterol_mg: f64,
    pub saturated_fat_g: f64,
    pub unsaturated_fat_g: f64,
    pub triglycerides_mg: f64,
    pub iron_mg: f64,
    pub vitamin_b_mg: f64,
    pub vitamin_c_mg: f64,
    pub sugar_g: f64,
    pub fiber_g: f64,
    pub sodium_mg: f64,
    pub potassium_mg: f64,
    pub calcium_mg: f64,
    pub magnesium_mg: f64,
    pub zinc_mg: f64,
    pub vitamin_a_mcg: f64,
    pub vitamin_d_mcg: f64,
    pub vitamin_e_mg: f64,
    pub vitamin_k_mcg: f64,
    pub caffeine_mg: f64,
    pub trans_fat_g: f64,
    pub entry_count: i64,
}

impl DayNutritionTotals {
    pub fn macros(calories: i64, protein: f64, carbs: f64, fats: f64) -> Self {
        Self {
            calories,
            protein,
            carbs,
            fats,
            ..Self::default()
        }
    }

    pub fn saturating_sub(&self, other: &Self) -> Self {
        Self {
            calories: (self.calories - other.calories).max(0),
            protein: (self.protein - other.protein).max(0.0),
            carbs: (self.carbs - other.carbs).max(0.0),
            fats: (self.fats - other.fats).max(0.0),
            omega_3_dha_mg: (self.omega_3_dha_mg - other.omega_3_dha_mg).max(0.0),
            cholesterol_mg: (self.cholesterol_mg - other.cholesterol_mg).max(0.0),
            saturated_fat_g: (self.saturated_fat_g - other.saturated_fat_g).max(0.0),
            unsaturated_fat_g: (self.unsaturated_fat_g - other.unsaturated_fat_g).max(0.0),
            triglycerides_mg: (self.triglycerides_mg - other.triglycerides_mg).max(0.0),
            iron_mg: (self.iron_mg - other.iron_mg).max(0.0),
            vitamin_b_mg: (self.vitamin_b_mg - other.vitamin_b_mg).max(0.0),
            vitamin_c_mg: (self.vitamin_c_mg - other.vitamin_c_mg).max(0.0),
            sugar_g: (self.sugar_g - other.sugar_g).max(0.0),
            fiber_g: (self.fiber_g - other.fiber_g).max(0.0),
            sodium_mg: (self.sodium_mg - other.sodium_mg).max(0.0),
            potassium_mg: (self.potassium_mg - other.potassium_mg).max(0.0),
            calcium_mg: (self.calcium_mg - other.calcium_mg).max(0.0),
            magnesium_mg: (self.magnesium_mg - other.magnesium_mg).max(0.0),
            zinc_mg: (self.zinc_mg - other.zinc_mg).max(0.0),
            vitamin_a_mcg: (self.vitamin_a_mcg - other.vitamin_a_mcg).max(0.0),
            vitamin_d_mcg: (self.vitamin_d_mcg - other.vitamin_d_mcg).max(0.0),
            vitamin_e_mg: (self.vitamin_e_mg - other.vitamin_e_mg).max(0.0),
            vitamin_k_mcg: (self.vitamin_k_mcg - other.vitamin_k_mcg).max(0.0),
            caffeine_mg: (self.caffeine_mg - other.caffeine_mg).max(0.0),
            trans_fat_g: (self.trans_fat_g - other.trans_fat_g).max(0.0),
            entry_count: 0,
        }
    }

    pub(crate) fn add(&self, other: &Self) -> Self {
        Self {
            calories: self.calories + other.calories,
            protein: self.protein + other.protein,
            carbs: self.carbs + other.carbs,
            fats: self.fats + other.fats,
            omega_3_dha_mg: self.omega_3_dha_mg + other.omega_3_dha_mg,
            cholesterol_mg: self.cholesterol_mg + other.cholesterol_mg,
            saturated_fat_g: self.saturated_fat_g + other.saturated_fat_g,
            unsaturated_fat_g: self.unsaturated_fat_g + other.unsaturated_fat_g,
            triglycerides_mg: self.triglycerides_mg + other.triglycerides_mg,
            iron_mg: self.iron_mg + other.iron_mg,
            vitamin_b_mg: self.vitamin_b_mg + other.vitamin_b_mg,
            vitamin_c_mg: self.vitamin_c_mg + other.vitamin_c_mg,
            sugar_g: self.sugar_g + other.sugar_g,
            fiber_g: self.fiber_g + other.fiber_g,
            sodium_mg: self.sodium_mg + other.sodium_mg,
            potassium_mg: self.potassium_mg + other.potassium_mg,
            calcium_mg: self.calcium_mg + other.calcium_mg,
            magnesium_mg: self.magnesium_mg + other.magnesium_mg,
            zinc_mg: self.zinc_mg + other.zinc_mg,
            vitamin_a_mcg: self.vitamin_a_mcg + other.vitamin_a_mcg,
            vitamin_d_mcg: self.vitamin_d_mcg + other.vitamin_d_mcg,
            vitamin_e_mg: self.vitamin_e_mg + other.vitamin_e_mg,
            vitamin_k_mcg: self.vitamin_k_mcg + other.vitamin_k_mcg,
            caffeine_mg: self.caffeine_mg + other.caffeine_mg,
            trans_fat_g: self.trans_fat_g + other.trans_fat_g,
            entry_count: self.entry_count + other.entry_count,
        }
    }
}

/// Sum local `/food` (and adjustment) rows for a local calendar day.
pub async fn sum_food_log_for_day(
    pool: &SqlitePool,
    member_id: &str,
    date: &str,
    timezone: chrono_tz::Tz,
) -> Result<DayNutritionTotals> {
    sum_food_log_for_day_filtered(pool, member_id, date, timezone, FoodLogSyncFilter::All).await
}

/// Sum only local `/food` rows that have not been pushed to Google Health yet.
pub async fn sum_unsynced_food_log_for_day(
    pool: &SqlitePool,
    member_id: &str,
    date: &str,
    timezone: chrono_tz::Tz,
) -> Result<DayNutritionTotals> {
    sum_food_log_for_day_filtered(
        pool,
        member_id,
        date,
        timezone,
        FoodLogSyncFilter::UnsyncedOnly,
    )
    .await
}

#[derive(Clone, Copy)]
pub(crate) enum FoodLogSyncFilter {
    All,
    UnsyncedOnly,
}

pub(crate) async fn sum_food_log_for_day_filtered<'e>(
    executor: impl sqlx::Executor<'e, Database = sqlx::Sqlite>,
    member_id: &str,
    date: &str,
    timezone: chrono_tz::Tz,
    filter: FoodLogSyncFilter,
) -> Result<DayNutritionTotals> {
    let (start, end) = chotu_common::civil_day_bounds_utc(date, timezone)?;
    // Filter only toggles a fixed clause; AssertSqlSafe is required for sqlx 0.9 SqlSafeStr.
    let sync_clause = match filter {
        FoodLogSyncFilter::All => "",
        FoodLogSyncFilter::UnsyncedOnly => {
            " AND (google_data_point_id IS NULL OR google_data_point_id = '')"
        }
    };
    let sql = format!(
        r#"
        SELECT
            CAST(COALESCE(SUM(estimated_calories), 0) AS INTEGER) as calories,
            COALESCE(SUM(estimated_protein), 0.0) as protein,
            COALESCE(SUM(estimated_carbs), 0.0) as carbs,
            COALESCE(SUM(estimated_fats), 0.0) as fats,
            COALESCE(SUM(estimated_omega_3_dha_mg), 0.0) as omega_3_dha_mg,
            COALESCE(SUM(estimated_cholesterol_mg), 0.0) as cholesterol_mg,
            COALESCE(SUM(estimated_saturated_fat_g), 0.0) as saturated_fat_g,
            COALESCE(SUM(estimated_unsaturated_fat_g), 0.0) as unsaturated_fat_g,
            COALESCE(SUM(estimated_triglycerides_mg), 0.0) as triglycerides_mg,
            COALESCE(SUM(estimated_iron_mg), 0.0) as iron_mg,
            COALESCE(SUM(estimated_vitamin_b_mg), 0.0) as vitamin_b_mg,
            COALESCE(SUM(estimated_vitamin_c_mg), 0.0) as vitamin_c_mg,
            COALESCE(SUM(estimated_sugar_g), 0.0) as sugar_g,
            COALESCE(SUM(estimated_fiber_g), 0.0) as fiber_g,
            COALESCE(SUM(estimated_sodium_mg), 0.0) as sodium_mg,
            COALESCE(SUM(estimated_potassium_mg), 0.0) as potassium_mg,
            COALESCE(SUM(estimated_calcium_mg), 0.0) as calcium_mg,
            COALESCE(SUM(estimated_magnesium_mg), 0.0) as magnesium_mg,
            COALESCE(SUM(estimated_zinc_mg), 0.0) as zinc_mg,
            COALESCE(SUM(estimated_vitamin_a_mcg), 0.0) as vitamin_a_mcg,
            COALESCE(SUM(estimated_vitamin_d_mcg), 0.0) as vitamin_d_mcg,
            COALESCE(SUM(estimated_vitamin_e_mg), 0.0) as vitamin_e_mg,
            COALESCE(SUM(estimated_vitamin_k_mcg), 0.0) as vitamin_k_mcg,
            COALESCE(SUM(estimated_caffeine_mg), 0.0) as caffeine_mg,
            COALESCE(SUM(estimated_trans_fat_g), 0.0) as trans_fat_g,
            COUNT(*) as entry_count
        FROM food_log
        WHERE family_member_id = ? AND julianday(timestamp) >= julianday(?) AND julianday(timestamp) < julianday(?){sync_clause}
        "#
    );
    let totals = sqlx::query_as::<_, DayNutritionTotals>(sqlx::AssertSqlSafe(sql))
        .bind(member_id)
        .bind(start)
        .bind(end)
        .fetch_one(executor)
        .await
        .context("Failed to sum food_log for day")?;
    Ok(totals)
}

pub(crate) async fn fetch_summary_nutrition<'e>(
    executor: impl sqlx::Executor<'e, Database = sqlx::Sqlite>,
    member_id: &str,
    date: &str,
) -> Result<DayNutritionTotals> {
    let row = sqlx::query_as::<_, DayNutritionTotals>(
        r#"
        SELECT
            CAST(total_calories_ingested AS INTEGER) as calories,
            protein_grams as protein,
            carbs_grams as carbs,
            fats_grams as fats,
            omega_3_dha_mg,
            cholesterol_mg,
            saturated_fat_g,
            unsaturated_fat_g,
            triglycerides_mg,
            iron_mg,
            vitamin_b_mg,
            vitamin_c_mg,
            sugar_g,
            fiber_g,
            sodium_mg,
            potassium_mg,
            calcium_mg,
            magnesium_mg,
            zinc_mg,
            vitamin_a_mcg,
            vitamin_d_mcg,
            vitamin_e_mg,
            vitamin_k_mcg,
            caffeine_mg,
            trans_fat_g,
            0 as entry_count
        FROM health_family_summary
        WHERE date = ? AND family_member_id = ?
        "#,
    )
    .bind(date)
    .bind(member_id)
    .fetch_optional(executor)
    .await
    .context("Failed to fetch health_family_summary nutrition")?;

    Ok(row.unwrap_or_default())
}

/// Non-`food_log` portion of today's summary (usually Google Health), inferred as
/// `summary − sum(food_log)` so chat edits can rebuild without another API call.
pub async fn external_nutrition_base(
    pool: &SqlitePool,
    member_id: &str,
    date: &str,
    timezone: chrono_tz::Tz,
) -> Result<DayNutritionTotals> {
    let summary = fetch_summary_nutrition(pool, member_id, date).await?;
    let manual = sum_food_log_for_day(pool, member_id, date, timezone).await?;
    Ok(summary.saturating_sub(&manual))
}

/// Write nutrition columns on `health_family_summary` (activity/sleep untouched).
pub async fn write_summary_nutrition(
    pool: &SqlitePool,
    member_id: &str,
    date: &str,
    totals: &DayNutritionTotals,
) -> Result<()> {
    write_summary_nutrition_on(pool, member_id, date, totals).await
}

pub(crate) async fn write_summary_nutrition_on<'e>(
    executor: impl sqlx::Executor<'e, Database = sqlx::Sqlite>,
    member_id: &str,
    date: &str,
    totals: &DayNutritionTotals,
) -> Result<()> {
    sqlx::query(
        r#"
        INSERT INTO health_family_summary (
            date,
            family_member_id,
            total_calories_ingested,
            protein_grams,
            carbs_grams,
            fats_grams,
            omega_3_dha_mg,
            cholesterol_mg,
            saturated_fat_g,
            unsaturated_fat_g,
            triglycerides_mg,
            iron_mg,
            vitamin_b_mg,
            vitamin_c_mg,
            sugar_g,
            fiber_g,
            sodium_mg,
            potassium_mg,
            calcium_mg,
            magnesium_mg,
            zinc_mg,
            vitamin_a_mcg,
            vitamin_d_mcg,
            vitamin_e_mg,
            vitamin_k_mcg,
            caffeine_mg,
            trans_fat_g,
            step_count,
            active_calories_burned
        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 0, 0)
        ON CONFLICT(date, family_member_id) DO UPDATE SET
            total_calories_ingested = excluded.total_calories_ingested,
            protein_grams = excluded.protein_grams,
            carbs_grams = excluded.carbs_grams,
            fats_grams = excluded.fats_grams,
            omega_3_dha_mg = excluded.omega_3_dha_mg,
            cholesterol_mg = excluded.cholesterol_mg,
            saturated_fat_g = excluded.saturated_fat_g,
            unsaturated_fat_g = excluded.unsaturated_fat_g,
            triglycerides_mg = excluded.triglycerides_mg,
            iron_mg = excluded.iron_mg,
            vitamin_b_mg = excluded.vitamin_b_mg,
            vitamin_c_mg = excluded.vitamin_c_mg,
            sugar_g = excluded.sugar_g,
            fiber_g = excluded.fiber_g,
            sodium_mg = excluded.sodium_mg,
            potassium_mg = excluded.potassium_mg,
            calcium_mg = excluded.calcium_mg,
            magnesium_mg = excluded.magnesium_mg,
            zinc_mg = excluded.zinc_mg,
            vitamin_a_mcg = excluded.vitamin_a_mcg,
            vitamin_d_mcg = excluded.vitamin_d_mcg,
            vitamin_e_mg = excluded.vitamin_e_mg,
            vitamin_k_mcg = excluded.vitamin_k_mcg,
            caffeine_mg = excluded.caffeine_mg,
            trans_fat_g = excluded.trans_fat_g;
        "#,
    )
    .bind(date)
    .bind(member_id)
    .bind(totals.calories as i32)
    .bind(totals.protein)
    .bind(totals.carbs)
    .bind(totals.fats)
    .bind(totals.omega_3_dha_mg)
    .bind(totals.cholesterol_mg)
    .bind(totals.saturated_fat_g)
    .bind(totals.unsaturated_fat_g)
    .bind(totals.triglycerides_mg)
    .bind(totals.iron_mg)
    .bind(totals.vitamin_b_mg)
    .bind(totals.vitamin_c_mg)
    .bind(totals.sugar_g)
    .bind(totals.fiber_g)
    .bind(totals.sodium_mg)
    .bind(totals.potassium_mg)
    .bind(totals.calcium_mg)
    .bind(totals.magnesium_mg)
    .bind(totals.zinc_mg)
    .bind(totals.vitamin_a_mcg)
    .bind(totals.vitamin_d_mcg)
    .bind(totals.vitamin_e_mg)
    .bind(totals.vitamin_k_mcg)
    .bind(totals.caffeine_mg)
    .bind(totals.trans_fat_g)
    .execute(executor)
    .await
    .context("Failed to write health_family_summary nutrition")?;
    Ok(())
}

/// After mutating `food_log`, set summary nutrition to `external + sum(food_log)`.
pub async fn rebuild_summary_from_food_log(
    pool: &SqlitePool,
    member_id: &str,
    date: &str,
    timezone: chrono_tz::Tz,
    external: &DayNutritionTotals,
) -> Result<DayNutritionTotals> {
    let manual = sum_food_log_for_day(pool, member_id, date, timezone).await?;
    let combined = external.add(&manual);
    write_summary_nutrition(pool, member_id, date, &combined).await?;
    Ok(combined)
}

/// Syncs today's Google Health data for the primary (first configured) family member.
pub async fn sync_primary_today(
    pool: &SqlitePool,
    gemini_client: Option<&GeminiClient>,
    config: &AppConfig,
) -> Result<HealthSyncReport> {
    let date = config.now_in_tz().format("%Y-%m-%d").to_string();
    let member_id = config
        .family
        .members
        .first()
        .map(|m| m.id.as_str())
        .unwrap_or("alex");
    sync_member_for_date(pool, gemini_client, config, member_id, &date).await
}

/// Syncs today for every family member that has a Google Health refresh token.
pub async fn sync_configured_members_today(
    pool: &SqlitePool,
    gemini_client: Option<&GeminiClient>,
    config: &AppConfig,
) -> Result<Vec<HealthSyncReport>> {
    let date = config.now_in_tz().format("%Y-%m-%d").to_string();
    let mut reports = Vec::new();
    let mut errors = Vec::new();

    for member in &config.family.members {
        if !member_health_credentials_configured(&member.id, config) {
            if let Err(error) =
                crate::food_corrections::resume_pending_food_deletions(pool, config, &member.id)
                    .await
            {
                errors.push(format!("{}: {}", member.id, error));
            }
            continue;
        }
        match sync_member_for_date(pool, gemini_client, config, &member.id, &date).await {
            Ok(report) => reports.push(report),
            Err(e) => {
                eprintln!(
                    "Health Coach: Google Health sync failed for {}: {:?}",
                    member.id, e
                );
                errors.push(format!("{}: {}", member.id, e));
            }
        }
    }

    if reports.is_empty() {
        if errors.is_empty() {
            bail!(
                "No family members have Google Health tokens. \
                 Run `/login health <member_id>` for each account."
            );
        }
        bail!(
            "Google Health sync failed for all members: {}",
            errors.join("; ")
        );
    }

    Ok(reports)
}

/// Returns true when the Google Health OAuth app credentials and at least one
/// refresh token (per-member `HEALTH_REFRESH_TOKEN_*` or legacy `FITBIT_REFRESH_TOKEN`)
/// appear to be configured.
pub fn credentials_configured() -> bool {
    oauth_app_configured() && any_health_refresh_token_present()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn sync_preserves_adjustfood_audit_committed_after_snapshot_with_zero_selected_meals() {
        for has_unselected_meal in [false, true] {
            let pool = chotu_common::init_db(":memory:").await.unwrap();
            let config = AppConfig {
                timezone: Some("UTC".into()),
                ..Default::default()
            };
            let date = "2026-10-07";
            if has_unselected_meal {
                sqlx::query(
                    "INSERT INTO food_log (id, timestamp, family_member_id, raw_text_description, \
                     estimated_calories, estimated_protein, estimated_carbs, estimated_fats, \
                     estimated_iron_mg, estimated_fiber_g) \
                     VALUES ('unselected', '2026-10-07T16:00:00Z', 'alex', 'unselected meal', \
                     120, 10, 15, 4, 2, 3)",
                )
                .execute(&pool)
                .await
                .unwrap();
            }
            let remote = GoogleHealthFoodSummary {
                calories: 800.0,
                protein: 40.0,
                carbs: 90.0,
                fat: 30.0,
                cholesterol: 10.0,
                saturated_fat: 11.0,
                unsaturated_fat: 12.0,
                iron: 13.0,
                vitamin_b: 14.0,
                vitamin_c: 15.0,
                sugar: 16.0,
                fiber: 17.0,
                sodium: 18.0,
                potassium: 19.0,
                calcium: 20.0,
                magnesium: 21.0,
                zinc: 22.0,
                vitamin_a: 23.0,
                vitamin_d: 24.0,
                vitamin_e: 25.0,
                vitamin_k: 26.0,
                caffeine: 27.0,
                trans_fat: 28.0,
            };
            let snapshot = || SyncSnapshot {
                summary: remote.clone(),
                gemini_est: MissingSyncNutrition {
                    omega_3_dha_mg: 29.0,
                    triglycerides_mg: 30.0,
                },
                steps: 1234,
                active_calories: 80,
                sleep_hours: Some(7.5),
                exercises: Vec::new(),
            };
            let versions = correction_versions(&pool, "alex").await.unwrap();
            persist_sync_snapshot(
                &pool,
                "alex",
                date,
                config.resolved_tz(),
                &versions,
                snapshot(),
            )
            .await
            .unwrap();
            let before = fetch_summary_nutrition(&pool, "alex", date).await.unwrap();

            // Hold the sync between its captured remote/local snapshot and the
            // final writer, while /adjustfood commits without selecting any meal.
            let (snapshot_ready, snapshot_wait) = tokio::sync::oneshot::channel();
            let (allow_write, write_wait) = tokio::sync::oneshot::channel();
            let sync_pool = pool.clone();
            let sync_versions = versions.clone();
            let pending_snapshot = snapshot();
            let timezone = config.resolved_tz();
            let sync = tokio::spawn(async move {
                let stale_manual =
                    sum_unsynced_food_log_for_day(&sync_pool, "alex", date, timezone)
                        .await
                        .unwrap();
                snapshot_ready.send(stale_manual).unwrap();
                write_wait.await.unwrap();
                persist_sync_snapshot(
                    &sync_pool,
                    "alex",
                    date,
                    timezone,
                    &sync_versions,
                    pending_snapshot,
                )
                .await
                .unwrap()
            });
            let stale_manual = snapshot_wait.await.unwrap();
            assert_eq!(stale_manual.entry_count, i64::from(has_unselected_meal));
            crate::food_corrections::adjust_food_totals(
                &pool, &config, "alex", date, 650, 32.0, 70.0, 25.0,
            )
            .await
            .unwrap();
            // This audit does not change correction/deletion versions; the
            // existing version guard alone cannot detect the stale local sum.
            assert_eq!(correction_versions(&pool, "alex").await.unwrap(), versions);
            let audit_count: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM food_log WHERE family_member_id = 'alex' \
                 AND raw_text_description LIKE 'Manual adjustment:%' \
                 AND google_data_point_id IS NULL",
            )
            .fetch_one(&pool)
            .await
            .unwrap();
            assert_eq!(audit_count, 1);
            allow_write.send(()).unwrap();
            let report = sync.await.unwrap();
            let after = fetch_summary_nutrition(&pool, "alex", date).await.unwrap();
            assert_eq!(
                (after.calories, after.protein, after.carbs, after.fats),
                (650, 32.0, 70.0, 25.0),
            );
            assert_eq!(
                (report.calories, report.protein, report.carbs, report.fats),
                (650, 32.0, 70.0, 25.0),
            );
            assert_eq!(report.manual_food_entries, stale_manual.entry_count + 1,);
            let micros = |totals: &DayNutritionTotals| {
                [
                    totals.omega_3_dha_mg,
                    totals.cholesterol_mg,
                    totals.saturated_fat_g,
                    totals.unsaturated_fat_g,
                    totals.triglycerides_mg,
                    totals.iron_mg,
                    totals.vitamin_b_mg,
                    totals.vitamin_c_mg,
                    totals.sugar_g,
                    totals.fiber_g,
                    totals.sodium_mg,
                    totals.potassium_mg,
                    totals.calcium_mg,
                    totals.magnesium_mg,
                    totals.zinc_mg,
                    totals.vitamin_a_mcg,
                    totals.vitamin_d_mcg,
                    totals.vitamin_e_mg,
                    totals.vitamin_k_mcg,
                    totals.caffeine_mg,
                    totals.trans_fat_g,
                ]
            };
            assert_eq!(micros(&after), micros(&before));
            let activity: (i32, i32, Option<f64>) = sqlx::query_as(
                "SELECT step_count, active_calories_burned, sleep_hours \
                 FROM health_family_summary WHERE date = ? AND family_member_id = 'alex'",
            )
            .bind(date)
            .fetch_one(&pool)
            .await
            .unwrap();
            assert_eq!(activity, (1234, 80, Some(7.5)));
        }
    }

    #[tokio::test]
    async fn manual_sync_resumes_prior_day_deletions_without_touching_other_meals() {
        let pool = chotu_common::init_db(":memory:").await.unwrap();
        let config = AppConfig::default();
        for (id, member, date, calories, pending) in [
            ("old-one", "alex", "2026-09-29", 400, true),
            ("unselected", "alex", "2026-09-29", 500, false),
            ("old-two", "alex", "2026-09-30", 200, true),
            ("new-meal", "alex", "2026-10-07", 300, false),
            ("other-member", "jordan", "2026-09-29", 250, true),
        ] {
            let (start, _) =
                chotu_common::civil_day_bounds_utc(date, config.resolved_tz()).unwrap();
            let timestamp = start + chrono::Duration::hours(1);
            sqlx::query("INSERT INTO food_log (id, timestamp, family_member_id, raw_text_description, estimated_calories) VALUES (?, ?, ?, ?, ?)")
                .bind(id).bind(timestamp).bind(member).bind(id).bind(calories)
                .execute(&pool).await.unwrap();
            if pending {
                sqlx::query("INSERT INTO food_log_deletion_intents (food_log_id, family_member_id, civil_date, original_timestamp, revision) VALUES (?, ?, ?, ?, 0)")
                    .bind(id).bind(member).bind(date).bind(timestamp)
                    .execute(&pool).await.unwrap();
            }
        }
        for (date, calories) in [("2026-09-29", 1000), ("2026-09-30", 250)] {
            write_summary_nutrition(
                &pool,
                "alex",
                date,
                &DayNutritionTotals {
                    calories,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        }
        // No account is configured: durable local cleanup still precedes login failure.
        assert!(
            sync_member_for_date(&pool, None, &config, "alex", "2026-10-07")
                .await
                .is_err()
        );
        let remaining: Vec<String> = sqlx::query_scalar("SELECT id FROM food_log ORDER BY id")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(remaining, ["new-meal", "other-member", "unselected"]);
        let states: Vec<(String, i64)> = sqlx::query_as(
            "SELECT food_log_id, completed FROM food_log_deletion_intents ORDER BY food_log_id",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            states,
            [
                ("old-one".into(), 1),
                ("old-two".into(), 1),
                ("other-member".into(), 0),
            ]
        );
        assert_eq!(
            fetch_summary_nutrition(&pool, "alex", "2026-09-29")
                .await
                .unwrap()
                .calories,
            600
        );
        assert_eq!(
            fetch_summary_nutrition(&pool, "alex", "2026-09-30")
                .await
                .unwrap()
                .calories,
            50
        );
    }

    #[tokio::test]
    async fn stale_rollup_guard_detects_pending_completed_and_deleted_revisions() {
        let pool = chotu_common::init_db(":memory:").await.unwrap();
        let before = correction_versions(&pool, "alex").await.unwrap();
        sqlx::query("INSERT INTO food_log (id, timestamp, family_member_id, raw_text_description, estimated_calories) VALUES ('meal', '2026-10-07T16:00:00Z', 'alex', 'corrected meal', 240)")
            .execute(&pool).await.unwrap();
        write_summary_nutrition(
            &pool,
            "alex",
            "2026-10-07",
            &DayNutritionTotals::macros(240, 0.0, 0.0, 0.0),
        )
        .await
        .unwrap();
        sqlx::query("INSERT INTO food_log_corrections (food_log_id, revision, sync_state, replacement_name) VALUES ('meal', 1, 'create_pending', 'users/me/dataTypes/nutrition-log/dataPoints/meal-r1')")
            .execute(&pool).await.unwrap();
        let pending = correction_versions(&pool, "alex").await.unwrap();
        let mut tx = pool.begin().await.unwrap();
        assert!(
            guard_summary_refresh(&mut tx, "alex", "2026-10-07", &pending)
                .await
                .is_err()
        );
        tx.rollback().await.unwrap();
        sqlx::query(
            "UPDATE food_log_corrections SET sync_state = 'synced' WHERE food_log_id = 'meal'",
        )
        .execute(&pool)
        .await
        .unwrap();
        let completed = correction_versions(&pool, "alex").await.unwrap();
        let mut tx = pool.begin().await.unwrap();
        assert!(
            guard_summary_refresh(&mut tx, "alex", "2026-10-07", &before)
                .await
                .is_err()
        );
        tx.rollback().await.unwrap();
        assert_eq!(
            fetch_summary_nutrition(&pool, "alex", "2026-10-07")
                .await
                .unwrap()
                .calories,
            240
        );
        let mut tx = pool.begin().await.unwrap();
        guard_summary_refresh(&mut tx, "alex", "2026-10-07", &completed)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        sqlx::query("DELETE FROM food_log WHERE id = 'meal'")
            .execute(&pool)
            .await
            .unwrap();
        let mut tx = pool.begin().await.unwrap();
        assert!(
            guard_summary_refresh(&mut tx, "alex", "2026-10-07", &completed)
                .await
                .is_err()
        );
        tx.rollback().await.unwrap();
    }

    #[tokio::test]
    async fn sync_retries_historical_corrections_without_pushing_other_historical_meals() {
        let pool = chotu_common::init_db(":memory:").await.unwrap();
        for (id, member, timestamp, google_id, correction_state) in [
            (
                "historical-correction",
                "alex",
                "2026-09-29T16:00:00Z",
                Some("old-id"),
                Some("delete_pending"),
            ),
            (
                "historical-unsynced",
                "alex",
                "2026-09-29T17:00:00Z",
                None,
                None,
            ),
            (
                "historical-synced",
                "alex",
                "2026-09-29T18:00:00Z",
                Some("synced-id"),
                Some("synced"),
            ),
            ("today-unsynced", "alex", "2026-10-07T16:00:00Z", None, None),
            (
                "other-member",
                "sam",
                "2026-09-29T16:00:00Z",
                Some("other-id"),
                Some("delete_pending"),
            ),
        ] {
            sqlx::query("INSERT INTO food_log (id, timestamp, family_member_id, raw_text_description, estimated_calories, google_data_point_id) VALUES (?, ?, ?, 'meal', 400, ?)")
                .bind(id).bind(timestamp).bind(member).bind(google_id).execute(&pool).await.unwrap();
            if let Some(state) = correction_state {
                sqlx::query("INSERT INTO food_log_corrections (food_log_id, revision, sync_state, remote_name, replacement_name) VALUES (?, 1, ?, ?, 'users/me/dataTypes/nutrition-log/dataPoints/replacement')")
                    .bind(id).bind(state).bind(google_id).execute(&pool).await.unwrap();
            }
        }
        let pending =
            pending_food_logs_for_sync(&pool, "alex", "2026-10-07", chrono_tz::America::Toronto)
                .await
                .unwrap();
        let ids: Vec<_> = pending.iter().map(|meal| meal.id.as_str()).collect();
        assert_eq!(ids, vec!["historical-correction", "today-unsynced"]);
    }

    #[tokio::test]
    async fn meal_totals_and_tag_deletion_share_the_configured_day() {
        let pool = chotu_common::init_db(":memory:").await.unwrap();
        let tz = chrono_tz::America::Toronto;
        // Mix timestamp encodings used by old rows and SQLx; compare actual instants.
        for (id, timestamp, calories, google_id) in [
            ("before", "2026-09-29T03:59:59Z", 1, Some("before-id")),
            ("first", "2026-09-29 04:00:00+00:00", 100, Some("first-id")),
            ("evening", "2026-09-30T02:00:00Z", 200, None),
            ("after", "2026-09-30T00:00:00-04:00", 2, Some("after-id")),
        ] {
            sqlx::query("INSERT INTO food_log (id, timestamp, family_member_id, raw_text_description, estimated_calories, google_data_point_id) VALUES (?, ?, 'alex', 'milk', ?, ?)")
                .bind(id).bind(timestamp).bind(calories).bind(google_id).execute(&pool).await.unwrap();
            sqlx::query("INSERT INTO food_log_tags (food_log_id, tag) VALUES (?, 'dairy')")
                .bind(id)
                .execute(&pool)
                .await
                .unwrap();
        }
        let all = sum_food_log_for_day(&pool, "alex", "2026-09-29", tz)
            .await
            .unwrap();
        assert_eq!(all.calories, 300);
        assert_eq!(all.entry_count, 2);
        let pending = sum_unsynced_food_log_for_day(&pool, "alex", "2026-09-29", tz)
            .await
            .unwrap();
        assert_eq!(pending.calories, 200);
        let external = DayNutritionTotals {
            calories: 50,
            ..Default::default()
        };
        rebuild_summary_from_food_log(&pool, "alex", "2026-09-29", tz, &external)
            .await
            .unwrap();
        assert_eq!(
            external_nutrition_base(&pool, "alex", "2026-09-29", tz)
                .await
                .unwrap()
                .calories,
            50
        );

        let mut tx = pool.begin().await.unwrap();
        chotu_common::delete_food_log_tags_for_member_day(&mut tx, "alex", "2026-09-29", tz)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        let remaining: Vec<(String,)> =
            sqlx::query_as("SELECT food_log_id FROM food_log_tags ORDER BY food_log_id")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(remaining, vec![("after".into(),), ("before".into(),)]);
    }

    #[tokio::test]
    async fn unenrolled_deletion_blocks_uploads_and_inflight_rollup_until_completed() {
        let pool = chotu_common::init_db(":memory:").await.unwrap();
        let config = AppConfig {
            timezone: Some("UTC".into()),
            ..Default::default()
        };
        for (id, calories, time) in [("selected", 250, "16:00:00"), ("later", 100, "17:00:00")] {
            sqlx::query("INSERT INTO food_log (id,timestamp,family_member_id,raw_text_description,estimated_calories) VALUES (?,?,'alex','meal',?)")
                .bind(id).bind(format!("2026-10-07T{time}Z")).bind(calories).execute(&pool).await.unwrap();
        }
        write_summary_nutrition(
            &pool,
            "alex",
            "2026-10-07",
            &DayNutritionTotals::macros(400, 0.0, 0.0, 0.0),
        )
        .await
        .unwrap();
        let before = correction_versions(&pool, "alex").await.unwrap();
        sqlx::query(
            "INSERT INTO food_log_deletion_intents (food_log_id,family_member_id,civil_date,original_timestamp,revision) \
             SELECT id,family_member_id,'2026-10-07',timestamp,0 FROM food_log WHERE id = 'selected'",
        ).execute(&pool).await.unwrap();
        let pending = pending_food_logs_for_sync(&pool, "alex", "2026-10-07", config.resolved_tz())
            .await
            .unwrap();
        assert_eq!(
            pending
                .iter()
                .map(|log| log.id.as_str())
                .collect::<Vec<_>>(),
            vec!["later"]
        );
        let during = correction_versions(&pool, "alex").await.unwrap();
        for versions in [&before, &during] {
            let mut tx = pool.begin().await.unwrap();
            assert!(
                guard_summary_refresh(&mut tx, "alex", "2026-10-07", versions)
                    .await
                    .is_err()
            );
            tx.rollback().await.unwrap();
        }
        crate::delete_food_logs_for_day(&pool, &config, "alex", "2026-10-07", &["selected".into()])
            .await
            .unwrap();
        assert_eq!(
            fetch_summary_nutrition(&pool, "alex", "2026-10-07")
                .await
                .unwrap()
                .calories,
            150
        );
        let mut tx = pool.begin().await.unwrap();
        assert!(
            guard_summary_refresh(&mut tx, "alex", "2026-10-07", &before)
                .await
                .is_err()
        );
        tx.rollback().await.unwrap();
        let completed = correction_versions(&pool, "alex").await.unwrap();
        let mut tx = pool.begin().await.unwrap();
        guard_summary_refresh(&mut tx, "alex", "2026-10-07", &completed)
            .await
            .unwrap();
        tx.rollback().await.unwrap();
    }
}
