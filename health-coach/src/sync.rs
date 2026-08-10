use anyhow::{bail, Context, Result};
use chotu_common::{
    health_refresh_token_env_key, resolve_health_refresh_token, AppConfig, FoodLog, GeminiClient,
    GoogleHealthClient, GoogleHealthFoodSummary, MissingSyncNutrition, NutritionLogWrite,
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
    pub exercises: Vec<String>,
    /// Number of Telegram `/food` rows merged on top of Google Health totals.
    pub manual_food_entries: i64,
}

impl HealthSyncReport {
    pub fn telegram_markdown(&self) -> String {
        let sleep_str = match self.sleep_hours {
            Some(h) => format!("{:.1} hours", h),
            None => "No sleep log".to_string(),
        };

        let exercise_str = if self.exercises.is_empty() {
            "None logged".to_string()
        } else {
            self.exercises
                .iter()
                .map(|e| format!("• {}", e))
                .collect::<Vec<_>>()
                .join("\n")
        };

        let manual_note = if self.manual_food_entries > 0 {
            format!(
                "\n_Includes {} Telegram `/food` entr{}_",
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
            "✅ *Google Health Sync Complete!*\n\n\
             Logged metrics for *{}* on *{}*:{}\n\n\
             *Activity & Sleep:*\n\
             • Steps: {} steps\n\
             • Active Energy: {} kcal\n\
             • Sleep Duration: {}\n\n\
             *Exercises:*\n\
             {}\n\n\
             *Nutrition:*\n\
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
    Ok(GoogleHealthClient::new(client_id, client_secret, refresh_token))
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
    Ok(GoogleHealthClient::new(client_id, client_secret, refresh_token))
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

fn food_log_to_nutrition_write(log: &FoodLog) -> NutritionLogWrite {
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
    let write = food_log_to_nutrition_write(log);
    let name = client.create_nutrition_log(&write).await?;
    sqlx::query("UPDATE food_log SET google_data_point_id = ? WHERE id = ?")
        .bind(&name)
        .bind(&log.id)
        .execute(pool)
        .await
        .context("Failed to store google_data_point_id on food_log")?;
    Ok(name)
}

/// Best-effort push of all unsynced food_log rows for a member/day.
pub async fn push_pending_food_logs(
    pool: &SqlitePool,
    client: &GoogleHealthClient,
    member_id: &str,
    date: &str,
) -> Result<usize> {
    let pending: Vec<FoodLog> = sqlx::query_as(
        "SELECT * FROM food_log \
         WHERE family_member_id = ? AND date(timestamp, 'localtime') = ? \
           AND (google_data_point_id IS NULL OR google_data_point_id = '') \
         ORDER BY timestamp ASC",
    )
    .bind(member_id)
    .bind(date)
    .fetch_all(pool)
    .await
    .context("Failed to fetch pending food_log rows")?;

    let mut pushed = 0;
    for log in pending {
        // Skip pure local adjustment audit rows — they are not real meals.
        if log
            .raw_text_description
            .starts_with("Manual adjustment:")
        {
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

/// Delete Google Health nutrition-log data points by stored resource names,
/// using the given member's OAuth token.
pub async fn delete_google_nutrition_logs(
    member_id: &str,
    config: &AppConfig,
    names: &[String],
) -> Result<()> {
    if names.is_empty() {
        return Ok(());
    }
    let client = google_health_client_for_member(member_id, config)?;
    client.batch_delete_nutrition_logs(names).await
}

/// Collect google_data_point_id values for a member's food_log on a local day.
pub async fn google_data_point_ids_for_day(
    pool: &SqlitePool,
    member_id: &str,
    date: &str,
) -> Result<Vec<String>> {
    let rows: Vec<(Option<String>,)> = sqlx::query_as(
        "SELECT google_data_point_id FROM food_log \
         WHERE family_member_id = ? AND date(timestamp, 'localtime') = ? \
           AND google_data_point_id IS NOT NULL AND google_data_point_id != ''",
    )
    .bind(member_id)
    .bind(date)
    .fetch_all(pool)
    .await
    .context("Failed to fetch google_data_point_id values")?;

    Ok(rows.into_iter().filter_map(|(id,)| id).collect())
}

/// Syncs Google Health metrics for `member_id` on `date` (YYYY-MM-DD) into SQLite.
///
/// Nutrition is Google Health's daily rollup (which already includes Telegram meals
/// that were pushed upstream) **plus** any still-unsynced local `/food` rows.
pub async fn sync_member_for_date(
    pool: &SqlitePool,
    gemini_client: Option<&GeminiClient>,
    config: &AppConfig,
    member_id: &str,
    date: &str,
) -> Result<HealthSyncReport> {
    let client = google_health_client_for_member(member_id, config)?;

    // Best-effort: push any pending local meals so Google becomes the shared store.
    if let Err(e) = push_pending_food_logs(pool, &client, member_id, date).await {
        eprintln!(
            "Health Coach: Failed to push pending food logs to Google Health: {:?}",
            e
        );
    }

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
    let exercises = client.fetch_exercise_summary(date).await.unwrap_or_default();

    // Local Telegram meals that have not been pushed to Google Health yet.
    let manual = sum_unsynced_food_log_for_day(pool, member_id, date)
        .await
        .unwrap_or_default();

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
    .execute(pool)
    .await
    .context("Failed to update health_family_summary in database")?;

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

/// Replace Google Health exercise rows for one member/day (atomic delete+insert).
pub async fn replace_exercise_log_for_day(
    pool: &SqlitePool,
    member_id: &str,
    date: &str,
    exercises: &[String],
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

    for desc in exercises {
        let trimmed = desc.trim();
        if trimmed.is_empty() {
            continue;
        }
        let id = uuid::Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO exercise_log (id, date, family_member_id, description, source) VALUES (?, ?, ?, ?, 'google_health')",
        )
        .bind(&id)
        .bind(date)
        .bind(member_id)
        .bind(trimmed)
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
    let rows: Vec<(String,)> = sqlx::query_as(
        "SELECT description FROM exercise_log \
         WHERE family_member_id = ? AND date = ? \
         ORDER BY created_at ASC, id ASC",
    )
    .bind(member_id)
    .bind(date)
    .fetch_all(pool)
    .await
    .context("Failed to query exercise_log")?;
    Ok(rows.into_iter().map(|(d,)| d).collect())
}

/// Exercise descriptions for a member between `start_date` and `end_date` inclusive.
pub async fn exercises_for_range(
    pool: &SqlitePool,
    member_id: &str,
    start_date: &str,
    end_date: &str,
) -> Result<Vec<(String, String)>> {
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT date, description FROM exercise_log \
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

    fn add(&self, other: &Self) -> Self {
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

/// Sum Telegram `/food` (and adjustment) rows for a local calendar day.
pub async fn sum_food_log_for_day(
    pool: &SqlitePool,
    member_id: &str,
    date: &str,
) -> Result<DayNutritionTotals> {
    sum_food_log_for_day_filtered(pool, member_id, date, FoodLogSyncFilter::All).await
}

/// Sum only local `/food` rows that have not been pushed to Google Health yet.
pub async fn sum_unsynced_food_log_for_day(
    pool: &SqlitePool,
    member_id: &str,
    date: &str,
) -> Result<DayNutritionTotals> {
    sum_food_log_for_day_filtered(pool, member_id, date, FoodLogSyncFilter::UnsyncedOnly).await
}

#[derive(Clone, Copy)]
enum FoodLogSyncFilter {
    All,
    UnsyncedOnly,
}

async fn sum_food_log_for_day_filtered(
    pool: &SqlitePool,
    member_id: &str,
    date: &str,
    filter: FoodLogSyncFilter,
) -> Result<DayNutritionTotals> {
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
        WHERE family_member_id = ? AND date(timestamp, 'localtime') = ?{sync_clause}
        "#
    );
    let totals = sqlx::query_as::<_, DayNutritionTotals>(sqlx::AssertSqlSafe(sql))
        .bind(member_id)
        .bind(date)
        .fetch_one(pool)
        .await
        .context("Failed to sum food_log for day")?;
    Ok(totals)
}

async fn fetch_summary_nutrition(
    pool: &SqlitePool,
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
    .fetch_optional(pool)
    .await
    .context("Failed to fetch health_family_summary nutrition")?;

    Ok(row.unwrap_or_default())
}

/// Non-`food_log` portion of today's summary (usually Google Health), inferred as
/// `summary − sum(food_log)` so Telegram edits can rebuild without another API call.
pub async fn external_nutrition_base(
    pool: &SqlitePool,
    member_id: &str,
    date: &str,
) -> Result<DayNutritionTotals> {
    let summary = fetch_summary_nutrition(pool, member_id, date).await?;
    let manual = sum_food_log_for_day(pool, member_id, date).await?;
    Ok(summary.saturating_sub(&manual))
}

/// Write nutrition columns on `health_family_summary` (activity/sleep untouched).
pub async fn write_summary_nutrition(
    pool: &SqlitePool,
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
    .execute(pool)
    .await
    .context("Failed to write health_family_summary nutrition")?;
    Ok(())
}

/// After mutating `food_log`, set summary nutrition to `external + sum(food_log)`.
pub async fn rebuild_summary_from_food_log(
    pool: &SqlitePool,
    member_id: &str,
    date: &str,
    external: &DayNutritionTotals,
) -> Result<DayNutritionTotals> {
    let manual = sum_food_log_for_day(pool, member_id, date).await?;
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
    let date = chrono::Local::now().format("%Y-%m-%d").to_string();
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
    let date = chrono::Local::now().format("%Y-%m-%d").to_string();
    let mut reports = Vec::new();
    let mut errors = Vec::new();

    for member in &config.family.members {
        if !member_health_credentials_configured(&member.id, config) {
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
        bail!("Google Health sync failed for all members: {}", errors.join("; "));
    }

    Ok(reports)
}

/// Returns true when the Google Health OAuth app credentials and at least one
/// refresh token (per-member `HEALTH_REFRESH_TOKEN_*` or legacy `FITBIT_REFRESH_TOKEN`)
/// appear to be configured.
pub fn credentials_configured() -> bool {
    oauth_app_configured() && any_health_refresh_token_present()
}
