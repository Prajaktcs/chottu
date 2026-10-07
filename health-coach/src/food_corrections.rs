//! In-place meal revisions and durable delete/recreate of immutable anonymous Google logs.

use anyhow::{bail, Context, Result};
use chotu_common::{AppConfig, FoodLog, GoogleHealthClient, NutritionEstimation};
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;
use std::fmt::Write;

use crate::sync::{
    fetch_summary_nutrition, food_log_to_nutrition_write, google_health_client_for_member,
    sum_food_log_for_day_filtered, write_summary_nutrition_on, FoodLogSyncFilter,
};

#[derive(Debug, sqlx::FromRow)]
struct CorrectionState {
    revision: i64,
    sync_state: String,
    remote_name: Option<String>,
    remote_snapshot_json: Option<String>,
    remote_create_pending: bool,
    replacement_name: String,
}

fn replacement_name(log_id: &str, revision: i64) -> String {
    let digest = Sha256::digest(log_id.as_bytes());
    let mut name = String::with_capacity(110);
    name.push_str("users/me/dataTypes/nutrition-log/dataPoints/chotu-");
    for byte in &digest[..16] {
        write!(name, "{byte:02x}").expect("writing to String cannot fail");
    }
    write!(name, "-r{revision}").expect("writing to String cannot fail");
    name
}

/// Zero for an existing meal that has not been revised; missing meals are errors.
pub async fn food_log_revision(pool: &SqlitePool, log_id: &str) -> Result<i64> {
    sqlx::query_scalar(
        "SELECT COALESCE(c.revision, 0) FROM food_log f \
         LEFT JOIN food_log_corrections c ON c.food_log_id = f.id WHERE f.id = ?",
    )
    .bind(log_id)
    .fetch_optional(pool)
    .await?
    .context("Food entry was deleted or does not exist")
}

/// Enroll initial uploads at revision zero under the same lock/CAS boundary as corrections.
/// Never replace an existing revision or trust the caller's potentially stale nutrition.
pub(crate) async fn ensure_food_log_upload_state(
    pool: &SqlitePool,
    original: &FoodLog,
) -> Result<()> {
    let mut tx = pool.begin().await?;
    let locked = sqlx::query("UPDATE food_log SET id = id WHERE id = ? AND family_member_id = ?")
        .bind(&original.id)
        .bind(&original.family_member_id)
        .execute(&mut *tx)
        .await?;
    if locked.rows_affected() != 1 {
        bail!("Food entry was deleted before Google upload");
    }
    let (timestamp, revision): (chrono::DateTime<chrono::Utc>, Option<i64>) = sqlx::query_as(
        "SELECT f.timestamp, c.revision FROM food_log f \
         LEFT JOIN food_log_corrections c ON c.food_log_id = f.id WHERE f.id = ?",
    )
    .bind(&original.id)
    .fetch_one(&mut *tx)
    .await?;
    if timestamp != original.timestamp {
        bail!("Food entry timing changed before Google upload");
    }
    if revision.is_some() {
        tx.commit().await?;
        return Ok(());
    }
    let current: FoodLog = sqlx::query_as("SELECT * FROM food_log WHERE id = ?")
        .bind(&original.id)
        .fetch_one(&mut *tx)
        .await?;
    let remote = current
        .google_data_point_id
        .as_deref()
        .filter(|name| !name.is_empty());
    let snapshot = if remote.is_some() {
        Some(serde_json::to_string(&current)?)
    } else {
        None
    };
    sqlx::query(
        "INSERT INTO food_log_corrections \
         (food_log_id, revision, sync_state, remote_name, remote_snapshot_json, replacement_name) \
         VALUES (?, 0, ?, ?, ?, ?)",
    )
    .bind(&current.id)
    .bind(if remote.is_some() {
        "synced"
    } else {
        "create_pending"
    })
    .bind(remote)
    .bind(snapshot)
    .bind(replacement_name(&current.id, 0))
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

/// Atomically replace nutrition and tags on the same meal, retaining its member and instant.
/// The summary uses the original civil day, preserving its external nutrition and activity.
pub async fn revise_food_log(
    pool: &SqlitePool,
    config: &AppConfig,
    original: &FoodLog,
    expected_revision: i64,
    description: &str,
    estimation: &NutritionEstimation,
    tag_context: &str,
) -> Result<i64> {
    let revision = expected_revision
        .checked_add(1)
        .context("Food revision overflow")?;
    if expected_revision < 0 {
        bail!("Invalid food revision");
    }
    let assigned = chotu_common::assign_food_tags(&estimation.tags, tag_context);
    let mut tx = pool.begin().await?;
    // Acquire SQLite's write lock and compare-and-swap before reading the summary.
    let locked = sqlx::query(
        "UPDATE food_log SET id = id WHERE id = ? AND family_member_id = ? \
         AND COALESCE((SELECT revision FROM food_log_corrections WHERE food_log_id = food_log.id), 0) = ?",
    )
    .bind(&original.id)
    .bind(&original.family_member_id)
    .bind(expected_revision)
    .execute(&mut *tx)
    .await?;
    if locked.rows_affected() != 1 {
        bail!("Food entry was deleted or changed; correction is stale");
    }
    let current: FoodLog = sqlx::query_as("SELECT * FROM food_log WHERE id = ?")
        .bind(&original.id)
        .fetch_one(&mut *tx)
        .await?;
    if current.timestamp != original.timestamp {
        bail!("Food entry timing changed; correction is stale");
    }
    let state: Option<CorrectionState> = sqlx::query_as(
        "SELECT revision, sync_state, remote_name, remote_snapshot_json, remote_create_pending, replacement_name \
         FROM food_log_corrections WHERE food_log_id = ?",
    )
    .bind(&original.id)
    .fetch_optional(&mut *tx)
    .await?;
    let date = current
        .timestamp
        .with_timezone(&config.resolved_tz())
        .format("%Y-%m-%d")
        .to_string();
    let summary = fetch_summary_nutrition(&mut *tx, &current.family_member_id, &date).await?;
    let before = sum_food_log_for_day_filtered(
        &mut *tx,
        &current.family_member_id,
        &date,
        config.resolved_tz(),
        FoodLogSyncFilter::All,
    )
    .await?;
    let external = summary.saturating_sub(&before);

    // Further edits before deletion retain the real upstream baseline, not an intermediate guess.
    let (remote_name, remote_snapshot_json, remote_create_pending) = match state {
        Some(state) if state.sync_state == "delete_pending" || state.remote_create_pending => (
            state.remote_name,
            state.remote_snapshot_json,
            state.remote_create_pending,
        ),
        _ => match current
            .google_data_point_id
            .as_deref()
            .filter(|name| !name.is_empty())
        {
            Some(name) => (
                Some(name.to_owned()),
                Some(serde_json::to_string(&current)?),
                false,
            ),
            None => (None, None, false),
        },
    };
    let sync_state = if remote_name.is_some() {
        "delete_pending"
    } else {
        "create_pending"
    };
    sqlx::query(
        "INSERT INTO food_log_corrections \
         (food_log_id, revision, sync_state, remote_name, remote_snapshot_json, remote_create_pending, replacement_name) \
         VALUES (?, ?, ?, ?, ?, ?, ?) ON CONFLICT(food_log_id) DO UPDATE SET \
         revision = excluded.revision, sync_state = excluded.sync_state, remote_name = excluded.remote_name, \
         remote_snapshot_json = excluded.remote_snapshot_json, remote_create_pending = excluded.remote_create_pending, \
         replacement_name = excluded.replacement_name",
    )
    .bind(&current.id).bind(revision).bind(sync_state).bind(remote_name)
    .bind(remote_snapshot_json).bind(remote_create_pending).bind(replacement_name(&current.id, revision))
    .execute(&mut *tx).await?;
    sqlx::query(
        "UPDATE food_log SET raw_text_description = ?, estimated_calories = ?, estimated_protein = ?, \
         estimated_carbs = ?, estimated_fats = ?, estimated_omega_3_dha_mg = ?, estimated_cholesterol_mg = ?, \
         estimated_saturated_fat_g = ?, estimated_unsaturated_fat_g = ?, estimated_triglycerides_mg = ?, \
         estimated_iron_mg = ?, estimated_vitamin_b_mg = ?, estimated_vitamin_c_mg = ?, estimated_sugar_g = ?, \
         estimated_fiber_g = ?, estimated_sodium_mg = ?, estimated_potassium_mg = ?, estimated_calcium_mg = ?, \
         estimated_magnesium_mg = ?, estimated_zinc_mg = ?, estimated_vitamin_a_mcg = ?, estimated_vitamin_d_mcg = ?, \
         estimated_vitamin_e_mg = ?, estimated_vitamin_k_mcg = ?, estimated_caffeine_mg = ?, estimated_trans_fat_g = ? \
         WHERE id = ?",
    )
    .bind(description).bind(estimation.total_calories).bind(estimation.protein_grams)
    .bind(estimation.carbs_grams).bind(estimation.fats_grams).bind(estimation.omega_3_dha_mg)
    .bind(estimation.cholesterol_mg).bind(estimation.saturated_fat_g).bind(estimation.unsaturated_fat_g)
    .bind(estimation.triglycerides_mg).bind(estimation.iron_mg).bind(estimation.vitamin_b_mg)
    .bind(estimation.vitamin_c_mg).bind(estimation.sugar_g).bind(estimation.fiber_g)
    .bind(estimation.sodium_mg).bind(estimation.potassium_mg).bind(estimation.calcium_mg)
    .bind(estimation.magnesium_mg).bind(estimation.zinc_mg).bind(estimation.vitamin_a_mcg)
    .bind(estimation.vitamin_d_mcg).bind(estimation.vitamin_e_mg).bind(estimation.vitamin_k_mcg)
    .bind(estimation.caffeine_mg).bind(estimation.trans_fat_g).bind(&current.id)
    .execute(&mut *tx).await?;
    chotu_common::delete_food_log_tags(&mut tx, &current.id).await?;
    chotu_common::insert_food_log_tags(&mut tx, &current.id, &assigned).await?;
    let after = sum_food_log_for_day_filtered(
        &mut *tx,
        &current.family_member_id,
        &date,
        config.resolved_tz(),
        FoodLogSyncFilter::All,
    )
    .await?;
    write_summary_nutrition_on(
        &mut *tx,
        &current.family_member_id,
        &date,
        &external.add(&after),
    )
    .await?;
    tx.commit().await.context("Commit food correction")?;
    Ok(revision)
}

/// Replace an immutable anonymous Google log. Local success does not imply Google success.
/// Callers without member credentials should keep the local revision and report pending sync.
pub async fn sync_corrected_food_log(
    pool: &SqlitePool,
    config: &AppConfig,
    log_id: &str,
) -> Result<()> {
    let member: String = sqlx::query_scalar("SELECT family_member_id FROM food_log WHERE id = ?")
        .bind(log_id)
        .fetch_optional(pool)
        .await?
        .context("Food entry was deleted")?;
    let client = google_health_client_for_member(&member, config)?;
    sync_food_log_with_client(pool, &client, log_id).await
}

async fn correction_state(pool: &SqlitePool, log_id: &str) -> Result<CorrectionState> {
    sqlx::query_as(
        "SELECT c.revision, c.sync_state, c.remote_name, c.remote_snapshot_json, c.remote_create_pending, c.replacement_name \
         FROM food_log_corrections c JOIN food_log f ON f.id = c.food_log_id WHERE c.food_log_id = ?",
    ).bind(log_id).fetch_optional(pool).await?.context("Food upload entry was deleted or does not exist")
}

// Persist the next known/possibly-created resource in the existing deletion path before POST.
// If the process exits after remote deletion, retry still has the old name until this commit.
async fn prepare_create(
    pool: &SqlitePool,
    log_id: &str,
    state: &CorrectionState,
) -> Result<FoodLog> {
    let mut tx = pool.begin().await?;
    let changed = sqlx::query(
        "UPDATE food_log_corrections SET sync_state = 'create_pending', remote_name = replacement_name \
         WHERE food_log_id = ? AND revision = ? AND sync_state = ? \
         AND EXISTS (SELECT 1 FROM food_log WHERE id = food_log_id)",
    ).bind(log_id).bind(state.revision).bind(&state.sync_state).execute(&mut *tx).await?;
    if changed.rows_affected() != 1 {
        bail!("Corrected food entry was deleted or changed during Google sync");
    }
    sqlx::query("UPDATE food_log SET google_data_point_id = ? WHERE id = ?")
        .bind(&state.replacement_name)
        .bind(log_id)
        .execute(&mut *tx)
        .await?;
    let log: FoodLog = sqlx::query_as("SELECT * FROM food_log WHERE id = ?")
        .bind(log_id)
        .fetch_one(&mut *tx)
        .await?;
    sqlx::query("UPDATE food_log_corrections SET remote_snapshot_json = ?, remote_create_pending = 1 WHERE food_log_id = ?")
        .bind(serde_json::to_string(&log)?).bind(log_id).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(log)
}

async fn finish_create(
    pool: &SqlitePool,
    log: &FoodLog,
    state: &CorrectionState,
    name: &str,
) -> Result<()> {
    let mut tx = pool.begin().await?;
    let changed = sqlx::query(
        "UPDATE food_log_corrections SET sync_state = 'synced', remote_name = ?, remote_snapshot_json = ?, remote_create_pending = 0 \
         WHERE food_log_id = ? AND revision = ? AND sync_state = 'create_pending' \
         AND EXISTS (SELECT 1 FROM food_log WHERE id = food_log_id)",
    ).bind(name).bind(serde_json::to_string(log)?).bind(&log.id).bind(state.revision)
    .execute(&mut *tx).await?;
    if changed.rows_affected() != 1 {
        let synced: Option<String> = sqlx::query_scalar(
            "SELECT f.google_data_point_id FROM food_log f JOIN food_log_corrections c ON c.food_log_id = f.id \
             WHERE f.id = ? AND c.revision = ? AND c.sync_state = 'synced'",
        ).bind(&log.id).bind(state.revision).fetch_optional(&mut *tx).await?.flatten();
        if synced
            .as_deref()
            .is_some_and(|saved| saved.rsplit('/').next() == name.rsplit('/').next())
        {
            tx.commit().await?;
            return Ok(());
        }
        bail!("Corrected food entry was deleted or changed during Google sync");
    }
    sqlx::query("UPDATE food_log SET google_data_point_id = ? WHERE id = ?")
        .bind(name)
        .bind(&log.id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

pub(crate) async fn sync_food_log_with_client(
    pool: &SqlitePool,
    client: &GoogleHealthClient,
    log_id: &str,
) -> Result<()> {
    let state = correction_state(pool, log_id).await?;
    if state.sync_state == "synced" {
        return Ok(());
    }
    if state.sync_state == "delete_pending" {
        let old = state
            .remote_name
            .as_ref()
            .context("Correction is missing its old Google resource")?;
        if state.remote_create_pending {
            resolve_pending_create(client, old, state.remote_snapshot_json.as_deref()).await?;
        }
        client
            .batch_delete_nutrition_logs(std::slice::from_ref(old))
            .await
            .context("Corrected locally; Google deletion is still pending")?;
    }
    let log = prepare_create(pool, log_id, &state).await?;
    let name = client
        .create_nutrition_log_named(&food_log_to_nutrition_write(&log), &state.replacement_name)
        .await
        .context("Saved locally; Google nutrition upload is still pending")?;
    if let Err(error) = finish_create(pool, &log, &state, &name).await {
        // Never INSERT a deleted local meal. Also remove this now-obsolete remote revision.
        client
            .batch_delete_nutrition_logs(std::slice::from_ref(&name))
            .await
            .context("Food changed during sync; obsolete Google replacement cleanup failed")?;
        return Err(error);
    }
    Ok(())
}

async fn resolve_pending_create(
    client: &GoogleHealthClient,
    name: &str,
    snapshot: Option<&str>,
) -> Result<()> {
    let baseline: FoodLog =
        serde_json::from_str(snapshot.context("Pending Google create is missing its snapshot")?)?;
    // A missing GET alone cannot rule out an in-flight POST. Complete/read back the
    // same persisted name and payload before removing it, including on undo/clear.
    client
        .create_nutrition_log_named(&food_log_to_nutrition_write(&baseline), name)
        .await
        .context(
            "Earlier Google nutrition creation is still unresolved; deletion remains pending",
        )?;
    Ok(())
}

pub(crate) async fn resolve_pending_nutrition_creates(
    pool: &SqlitePool,
    client: &GoogleHealthClient,
    names: &[String],
) -> Result<()> {
    for name in names {
        let snapshot: Option<String> = sqlx::query_scalar(
            "SELECT c.remote_snapshot_json FROM food_log_corrections c JOIN food_log f ON f.id = c.food_log_id \
             WHERE f.google_data_point_id = ? AND c.remote_create_pending = 1",
        ).bind(name).fetch_optional(pool).await?.flatten();
        if let Some(snapshot) = snapshot {
            resolve_pending_create(client, name, Some(&snapshot)).await?;
        }
    }
    Ok(())
}

pub(crate) async fn ensure_corrections_synced_for_member(
    pool: &SqlitePool,
    member_id: &str,
    timezone: chrono_tz::Tz,
) -> Result<()> {
    let pending: Option<chrono::DateTime<chrono::Utc>> = sqlx::query_scalar(
        "SELECT f.timestamp FROM food_log f JOIN food_log_corrections c ON c.food_log_id = f.id \
         WHERE f.family_member_id = ? AND c.sync_state != 'synced' ORDER BY f.timestamp LIMIT 1",
    )
    .bind(member_id)
    .fetch_optional(pool)
    .await?;
    if let Some(timestamp) = pending {
        let date = timestamp
            .with_timezone(&timezone)
            .format("%Y-%m-%d")
            .to_string();
        ensure_corrections_synced_for_day(pool, member_id, &date, timezone).await?;
    }
    Ok(())
}

pub(crate) async fn ensure_corrections_synced_for_day(
    pool: &SqlitePool,
    member_id: &str,
    date: &str,
    timezone: chrono_tz::Tz,
) -> Result<()> {
    let (start, end) = chotu_common::civil_day_bounds_utc(date, timezone)?;
    let pending: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM food_log f JOIN food_log_corrections c ON c.food_log_id = f.id \
         WHERE f.family_member_id = ? AND julianday(f.timestamp) >= julianday(?) \
         AND julianday(f.timestamp) < julianday(?) AND c.sync_state != 'synced'",
    )
    .bind(member_id)
    .bind(start)
    .bind(end)
    .fetch_one(pool)
    .await?;
    if pending != 0 {
        bail!("{pending} food update(s) for {date} are saved locally but pending Google upload; local totals were preserved. Retry /sync for {date}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::{sum_food_log_for_day, write_summary_nutrition, DayNutritionTotals};

    fn config() -> AppConfig {
        AppConfig {
            timezone: Some("America/Toronto".into()),
            ..AppConfig::default()
        }
    }

    fn estimation() -> NutritionEstimation {
        NutritionEstimation {
            total_calories: 240,
            protein_grams: 12.0,
            carbs_grams: 24.0,
            fats_grams: 8.0,
            dominant_macro: "carbs".into(),
            reasoning: "Explicit ingredients".into(),
            omega_3_dha_mg: 1.0,
            cholesterol_mg: 2.0,
            saturated_fat_g: 3.0,
            unsaturated_fat_g: 4.0,
            triglycerides_mg: 5.0,
            iron_mg: 6.0,
            vitamin_b_mg: 7.0,
            vitamin_c_mg: 8.0,
            sugar_g: 9.0,
            fiber_g: 10.0,
            sodium_mg: 11.0,
            potassium_mg: 12.0,
            calcium_mg: 13.0,
            magnesium_mg: 14.0,
            zinc_mg: 15.0,
            vitamin_a_mcg: 16.0,
            vitamin_d_mcg: 17.0,
            vitamin_e_mg: 18.0,
            vitamin_k_mcg: 19.0,
            caffeine_mg: 20.0,
            trans_fat_g: 21.0,
            tags: vec!["soy".into()],
        }
    }

    async fn original(pool: &SqlitePool, google_name: Option<&str>) -> FoodLog {
        sqlx::query(
            "INSERT INTO food_log (id, timestamp, family_member_id, raw_text_description, \
             estimated_calories, estimated_protein, google_data_point_id) \
             VALUES ('meal', '2026-09-30T02:00:00Z', 'alex', 'milk curry', 500, 30, ?)",
        )
        .bind(google_name)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO food_log_tags (food_log_id, tag) VALUES ('meal', 'dairy')")
            .execute(pool)
            .await
            .unwrap();
        sqlx::query_as("SELECT * FROM food_log WHERE id = 'meal'")
            .fetch_one(pool)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn initial_upload_can_be_corrected_while_its_named_create_is_in_flight() {
        let pool = chotu_common::init_db(":memory:").await.unwrap();
        let config = config();
        let original = original(&pool, None).await;
        ensure_food_log_upload_state(&pool, &original)
            .await
            .unwrap();
        let initial = correction_state(&pool, "meal").await.unwrap();
        assert_eq!(initial.revision, 0);
        assert_eq!(initial.sync_state, "create_pending");
        assert!(!initial.remote_create_pending);
        let in_flight = prepare_create(&pool, "meal", &initial).await.unwrap();
        assert_eq!(food_log_revision(&pool, "meal").await.unwrap(), 0);
        let uploaded = correction_state(&pool, "meal").await.unwrap();
        assert!(uploaded.remote_create_pending);
        assert_eq!(
            uploaded.remote_name.as_deref(),
            Some(initial.replacement_name.as_str())
        );

        revise_food_log(
            &pool,
            &config,
            &original,
            0,
            "lentils",
            &estimation(),
            "lentils",
        )
        .await
        .unwrap();
        let correction = correction_state(&pool, "meal").await.unwrap();
        assert_eq!(correction.revision, 1);
        assert_eq!(correction.sync_state, "delete_pending");
        assert!(correction.remote_create_pending);
        assert_eq!(
            correction.remote_name.as_deref(),
            Some(initial.replacement_name.as_str())
        );
        assert_eq!(
            correction.remote_snapshot_json,
            uploaded.remote_snapshot_json
        );
        let baseline: FoodLog =
            serde_json::from_str(correction.remote_snapshot_json.as_deref().unwrap()).unwrap();
        assert_eq!(baseline.raw_text_description, "milk curry");
        assert_eq!(baseline.estimated_calories, 500);
        assert_ne!(correction.replacement_name, initial.replacement_name);

        // A stale initial caller cannot reset the revision or overwrite its old remote reference.
        ensure_food_log_upload_state(&pool, &original)
            .await
            .unwrap();
        assert!(
            finish_create(&pool, &in_flight, &initial, &initial.replacement_name)
                .await
                .is_err()
        );
        assert!(prepare_create(&pool, "meal", &initial).await.is_err());
        let saved: FoodLog = sqlx::query_as("SELECT * FROM food_log WHERE id = 'meal'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(saved.raw_text_description, "lentils");
        assert_eq!(saved.estimated_calories, 240);
        assert_eq!(
            saved.google_data_point_id.as_deref(),
            Some(initial.replacement_name.as_str())
        );
        assert_eq!(food_log_revision(&pool, "meal").await.unwrap(), 1);
    }

    #[tokio::test]
    async fn initial_upload_enrollment_preserves_existing_uploads_and_corrections() {
        let pool = chotu_common::init_db(":memory:").await.unwrap();
        let original = original(
            &pool,
            Some("users/me/dataTypes/nutrition-log/dataPoints/legacy-meal"),
        )
        .await;
        ensure_food_log_upload_state(&pool, &original)
            .await
            .unwrap();
        let initial = correction_state(&pool, "meal").await.unwrap();
        assert_eq!(initial.revision, 0);
        assert_eq!(initial.sync_state, "synced");
        assert_eq!(initial.remote_name, original.google_data_point_id);
        assert!(!initial.remote_create_pending);
        revise_food_log(
            &pool,
            &config(),
            &original,
            0,
            "lentils",
            &estimation(),
            "lentils",
        )
        .await
        .unwrap();
        ensure_food_log_upload_state(&pool, &original)
            .await
            .unwrap();
        let correction = correction_state(&pool, "meal").await.unwrap();
        assert_eq!(correction.revision, 1);
        assert_eq!(correction.sync_state, "delete_pending");
        assert_eq!(correction.remote_name, original.google_data_point_id);
        assert!(!correction.remote_create_pending);
        sqlx::query("DELETE FROM food_log_tags WHERE food_log_id = 'meal'")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM food_log WHERE id = 'meal'")
            .execute(&pool)
            .await
            .unwrap();
        assert!(ensure_food_log_upload_state(&pool, &original)
            .await
            .is_err());
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM food_log_corrections")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn correction_preserves_identity_original_civil_day_external_base_and_activity() {
        let pool = chotu_common::init_db(":memory:").await.unwrap();
        let config = config();
        let original = original(
            &pool,
            Some("users/me/dataTypes/nutrition-log/dataPoints/old-meal"),
        )
        .await;
        assert_eq!(food_log_revision(&pool, "meal").await.unwrap(), 0);
        write_summary_nutrition(
            &pool,
            "alex",
            "2026-09-29",
            &DayNutritionTotals {
                calories: 600,
                protein: 40.0,
                sodium_mg: 50.0,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        sqlx::query("UPDATE health_family_summary SET step_count = 1234, active_calories_burned = 80, sleep_hours = 7 WHERE family_member_id = 'alex'")
            .execute(&pool).await.unwrap();
        let estimate = estimation();
        assert_eq!(
            revise_food_log(
                &pool,
                &config,
                &original,
                0,
                "lentil curry",
                &estimate,
                "lentil curry"
            )
            .await
            .unwrap(),
            1
        );
        let corrected: FoodLog = sqlx::query_as("SELECT * FROM food_log WHERE id = 'meal'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(corrected.id, original.id);
        assert_eq!(corrected.timestamp, original.timestamp);
        assert_eq!(corrected.family_member_id, original.family_member_id);
        assert_eq!(
            corrected.google_data_point_id,
            original.google_data_point_id
        );
        assert_eq!(corrected.raw_text_description, "lentil curry");
        assert_eq!(corrected.estimated_calories, estimate.total_calories);
        assert_eq!(corrected.estimated_protein, estimate.protein_grams);
        assert_eq!(corrected.estimated_carbs, estimate.carbs_grams);
        assert_eq!(corrected.estimated_fats, estimate.fats_grams);
        for (actual, expected) in [
            (corrected.estimated_omega_3_dha_mg, estimate.omega_3_dha_mg),
            (corrected.estimated_cholesterol_mg, estimate.cholesterol_mg),
            (
                corrected.estimated_saturated_fat_g,
                estimate.saturated_fat_g,
            ),
            (
                corrected.estimated_unsaturated_fat_g,
                estimate.unsaturated_fat_g,
            ),
            (
                corrected.estimated_triglycerides_mg,
                estimate.triglycerides_mg,
            ),
            (corrected.estimated_iron_mg, estimate.iron_mg),
            (corrected.estimated_vitamin_b_mg, estimate.vitamin_b_mg),
            (corrected.estimated_vitamin_c_mg, estimate.vitamin_c_mg),
            (corrected.estimated_sugar_g, estimate.sugar_g),
            (corrected.estimated_fiber_g, estimate.fiber_g),
            (corrected.estimated_sodium_mg, estimate.sodium_mg),
            (corrected.estimated_potassium_mg, estimate.potassium_mg),
            (corrected.estimated_calcium_mg, estimate.calcium_mg),
            (corrected.estimated_magnesium_mg, estimate.magnesium_mg),
            (corrected.estimated_zinc_mg, estimate.zinc_mg),
            (corrected.estimated_vitamin_a_mcg, estimate.vitamin_a_mcg),
            (corrected.estimated_vitamin_d_mcg, estimate.vitamin_d_mcg),
            (corrected.estimated_vitamin_e_mg, estimate.vitamin_e_mg),
            (corrected.estimated_vitamin_k_mcg, estimate.vitamin_k_mcg),
            (corrected.estimated_caffeine_mg, estimate.caffeine_mg),
            (corrected.estimated_trans_fat_g, estimate.trans_fat_g),
        ] {
            assert_eq!(actual, expected);
        }
        let totals = fetch_summary_nutrition(&pool, "alex", "2026-09-29")
            .await
            .unwrap();
        assert_eq!(totals.calories, 340);
        assert_eq!(totals.protein, 22.0);
        assert_eq!(totals.sodium_mg, 61.0);
        assert_eq!(totals.trans_fat_g, 21.0);
        let activity: (i32, i32, f64) = sqlx::query_as("SELECT step_count, active_calories_burned, sleep_hours FROM health_family_summary WHERE date = '2026-09-29' AND family_member_id = 'alex'")
            .fetch_one(&pool).await.unwrap();
        assert_eq!(activity, (1234, 80, 7.0));
        let tags: Vec<String> = sqlx::query_scalar(
            "SELECT tag FROM food_log_tags WHERE food_log_id = 'meal' ORDER BY tag",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(tags, vec!["soy"]);
        assert_eq!(
            sum_food_log_for_day(&pool, "alex", "2026-09-29", config.resolved_tz())
                .await
                .unwrap()
                .entry_count,
            1
        );
        let other_day: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM health_family_summary WHERE date != '2026-09-29'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(other_day, 0);
    }

    #[tokio::test]
    async fn stale_or_deleted_correction_never_mutates_or_resurrects() {
        let pool = chotu_common::init_db(":memory:").await.unwrap();
        let config = config();
        let original = original(&pool, None).await;
        revise_food_log(
            &pool,
            &config,
            &original,
            0,
            "lentils",
            &estimation(),
            "lentils",
        )
        .await
        .unwrap();
        let before: String =
            sqlx::query_scalar("SELECT raw_text_description FROM food_log WHERE id = 'meal'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(
            revise_food_log(&pool, &config, &original, 0, "stale", &estimation(), "milk")
                .await
                .is_err()
        );
        assert_eq!(food_log_revision(&pool, "meal").await.unwrap(), 1);
        let after: String =
            sqlx::query_scalar("SELECT raw_text_description FROM food_log WHERE id = 'meal'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(after, before);
        let state = correction_state(&pool, "meal").await.unwrap();
        sqlx::query("DELETE FROM food_log_tags WHERE food_log_id = 'meal'")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM food_log WHERE id = 'meal'")
            .execute(&pool)
            .await
            .unwrap();
        assert!(food_log_revision(&pool, "meal").await.is_err());
        assert!(revise_food_log(
            &pool,
            &config,
            &original,
            1,
            "deleted",
            &estimation(),
            "milk"
        )
        .await
        .is_err());
        assert!(prepare_create(&pool, "meal", &state).await.is_err());
        assert!(
            finish_create(&pool, &original, &state, &state.replacement_name)
                .await
                .is_err()
        );
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM food_log")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 0);
        let revisions: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM food_log_corrections")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(revisions, 0);
    }

    #[tokio::test]
    async fn correction_rolls_back_nutrients_revision_tags_and_totals_together() {
        let pool = chotu_common::init_db(":memory:").await.unwrap();
        let original = original(&pool, None).await;
        write_summary_nutrition(
            &pool,
            "alex",
            "2026-09-29",
            &DayNutritionTotals::macros(500, 30.0, 0.0, 0.0),
        )
        .await
        .unwrap();
        sqlx::query("CREATE TRIGGER reject_corrected_tags BEFORE INSERT ON food_log_tags BEGIN SELECT RAISE(ABORT, 'tag write failed'); END")
            .execute(&pool).await.unwrap();
        assert!(revise_food_log(
            &pool,
            &config(),
            &original,
            0,
            "lentils",
            &estimation(),
            "lentils"
        )
        .await
        .is_err());
        assert_eq!(food_log_revision(&pool, "meal").await.unwrap(), 0);
        let meal: (String, i32) = sqlx::query_as(
            "SELECT raw_text_description, estimated_calories FROM food_log WHERE id = 'meal'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(meal, ("milk curry".into(), 500));
        let tags: Vec<String> =
            sqlx::query_scalar("SELECT tag FROM food_log_tags WHERE food_log_id = 'meal'")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(tags, vec!["dairy"]);
        assert_eq!(
            fetch_summary_nutrition(&pool, "alex", "2026-09-29")
                .await
                .unwrap()
                .calories,
            500
        );
    }

    #[tokio::test]
    async fn replacement_state_survives_delete_create_gap_and_blocks_stale_rollup() {
        let pool = chotu_common::init_db(":memory:").await.unwrap();
        let config = config();
        let old_name = "users/me/dataTypes/nutrition-log/dataPoints/old-meal";
        let original = original(&pool, Some(old_name)).await;
        revise_food_log(
            &pool,
            &config,
            &original,
            0,
            "lentils",
            &estimation(),
            "lentils",
        )
        .await
        .unwrap();
        let deleting = correction_state(&pool, "meal").await.unwrap();
        assert_eq!(deleting.sync_state, "delete_pending");
        assert_eq!(deleting.remote_name.as_deref(), Some(old_name));
        assert!(!deleting.remote_create_pending);
        let baseline: FoodLog =
            serde_json::from_str(deleting.remote_snapshot_json.as_deref().unwrap()).unwrap();
        assert_eq!(baseline.estimated_calories, 500);
        assert_eq!(
            crate::google_data_point_ids_for_day(&pool, "alex", "2026-09-29", config.resolved_tz())
                .await
                .unwrap(),
            vec![old_name]
        );
        assert!(ensure_corrections_synced_for_day(
            &pool,
            "alex",
            "2026-09-29",
            config.resolved_tz()
        )
        .await
        .is_err());
        assert!(ensure_corrections_synced_for_day(
            &pool,
            "alex",
            "2026-09-30",
            config.resolved_tz()
        )
        .await
        .is_ok());
        // Today's /sync must surface and retry historical corrections as well.
        let historical = ensure_corrections_synced_for_member(&pool, "alex", config.resolved_tz())
            .await
            .unwrap_err();
        assert!(historical.to_string().contains("2026-09-29"));
        // Confirmed delete transitions commit before create. A failed create changes nothing else.
        let log = prepare_create(&pool, "meal", &deleting).await.unwrap();
        let restarted = correction_state(&pool, "meal").await.unwrap();
        assert_eq!(restarted.sync_state, "create_pending");
        assert_eq!(
            restarted.remote_name.as_deref(),
            Some(deleting.replacement_name.as_str())
        );
        assert!(restarted.remote_create_pending);
        let attempted: FoodLog =
            serde_json::from_str(restarted.remote_snapshot_json.as_deref().unwrap()).unwrap();
        assert_eq!(attempted.estimated_calories, 240);
        assert_eq!(restarted.replacement_name, deleting.replacement_name);
        assert_eq!(
            log.google_data_point_id.as_deref(),
            Some(deleting.replacement_name.as_str())
        );
        assert_eq!(
            crate::google_data_point_ids_for_day(&pool, "alex", "2026-09-29", config.resolved_tz())
                .await
                .unwrap(),
            vec![deleting.replacement_name.clone()]
        );
        assert!(ensure_corrections_synced_for_day(
            &pool,
            "alex",
            "2026-09-29",
            config.resolved_tz()
        )
        .await
        .is_err());
        assert_eq!(
            fetch_summary_nutrition(&pool, "alex", "2026-09-29")
                .await
                .unwrap()
                .calories,
            240
        );
        let retry = prepare_create(&pool, "meal", &restarted).await.unwrap();
        assert_eq!(retry.google_data_point_id, log.google_data_point_id);
        finish_create(&pool, &retry, &restarted, &restarted.replacement_name)
            .await
            .unwrap();
        // Concurrent same-revision retries must not delete the already-synced replacement.
        finish_create(&pool, &retry, &restarted, &restarted.replacement_name)
            .await
            .unwrap();
        assert!(
            !correction_state(&pool, "meal")
                .await
                .unwrap()
                .remote_create_pending
        );
        assert!(ensure_corrections_synced_for_day(
            &pool,
            "alex",
            "2026-09-29",
            config.resolved_tz()
        )
        .await
        .is_ok());
        assert_eq!(food_log_revision(&pool, "meal").await.unwrap(), 1);
    }

    #[tokio::test]
    async fn repeated_pending_edits_retain_real_remote_baseline_and_use_new_target() {
        let pool = chotu_common::init_db(":memory:").await.unwrap();
        let config = config();
        let original = original(
            &pool,
            Some("users/me/dataTypes/nutrition-log/dataPoints/old-meal"),
        )
        .await;
        revise_food_log(
            &pool,
            &config,
            &original,
            0,
            "lentils",
            &estimation(),
            "lentils",
        )
        .await
        .unwrap();
        let first = correction_state(&pool, "meal").await.unwrap();
        revise_food_log(
            &pool,
            &config,
            &original,
            1,
            "more lentils",
            &estimation(),
            "lentils",
        )
        .await
        .unwrap();
        let second = correction_state(&pool, "meal").await.unwrap();
        assert_eq!(second.remote_snapshot_json, first.remote_snapshot_json);
        assert_eq!(second.remote_name, first.remote_name);
        assert_ne!(second.replacement_name, first.replacement_name);
        assert!(prepare_create(&pool, "meal", &first).await.is_err());
        prepare_create(&pool, "meal", &second).await.unwrap();
        // If a previous create may have succeeded, the next revision must delete its target.
        revise_food_log(
            &pool,
            &config,
            &original,
            2,
            "final lentils",
            &estimation(),
            "lentils",
        )
        .await
        .unwrap();
        let third = correction_state(&pool, "meal").await.unwrap();
        assert_eq!(third.sync_state, "delete_pending");
        assert_eq!(
            third.remote_name.as_deref(),
            Some(second.replacement_name.as_str())
        );
        assert!(third.remote_create_pending);
        let ambiguous: FoodLog =
            serde_json::from_str(third.remote_snapshot_json.as_deref().unwrap()).unwrap();
        assert_eq!(ambiguous.raw_text_description, "more lentils");
    }

    #[tokio::test]
    async fn tag_assignment_uses_scoped_context_not_the_canonical_title() {
        let pool = chotu_common::init_db(":memory:").await.unwrap();
        let original = original(&pool, None).await;
        let mut estimate = estimation();
        estimate.tags = vec!["dairy".into()];
        revise_food_log(
            &pool,
            &config(),
            &original,
            0,
            "creamy curry",
            &estimate,
            "creamy curry; no dairy; lentils and coconut milk",
        )
        .await
        .unwrap();
        let tags: Vec<String> =
            sqlx::query_scalar("SELECT tag FROM food_log_tags WHERE food_log_id = 'meal'")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert!(!tags.iter().any(|tag| tag == "dairy"));
    }

    #[test]
    fn replacement_names_are_stable_revision_specific_and_within_google_limits() {
        let first = replacement_name("meal", 1);
        assert_eq!(first, replacement_name("meal", 1));
        assert_ne!(first, replacement_name("meal", 2));
        let maximum = replacement_name("any local id / with punctuation", i64::MAX);
        let leaf = maximum.rsplit('/').next().unwrap();
        assert!((4..=63).contains(&leaf.len()));
        assert!(leaf
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-'));
    }
}
