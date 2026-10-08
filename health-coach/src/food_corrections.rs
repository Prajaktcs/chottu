//! In-place meal revisions and durable delete/recreate of immutable anonymous Google logs.

use anyhow::{bail, Context, Result};
use chotu_common::{AppConfig, FoodLog, GoogleHealthClient, NutritionEstimation};
use sha2::{Digest, Sha256};
use sqlx::{Sqlite, SqlitePool, Transaction};
use std::fmt::Write;
use std::future::Future;
use tokio::sync::Mutex;

use crate::sync::{
    fetch_summary_nutrition, food_log_to_nutrition_write, google_health_client_for_member,
    sum_food_log_for_day_filtered, write_summary_nutrition_on, DayNutritionTotals,
    FoodLogSyncFilter,
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

// The same state machine is exercised with a controlled remote in race tests.
trait NutritionRemote: Sync {
    fn create(&self, log: &FoodLog, name: &str) -> impl Future<Output = Result<String>> + Send;
    fn delete(&self, name: &str) -> impl Future<Output = Result<()>> + Send;
}

impl NutritionRemote for GoogleHealthClient {
    async fn create(&self, log: &FoodLog, name: &str) -> Result<String> {
        self.create_nutrition_log_named(&food_log_to_nutrition_write(log), name)
            .await
    }

    async fn delete(&self, name: &str) -> Result<()> {
        self.batch_delete_nutrition_logs(&[name.to_owned()]).await
    }
}

// The supervisor owns all nutrition writers in one process. Serialize only
// remote mutation windows; SQLite transactions remain short and never span HTTP.
static NUTRITION_MUTATIONS: Mutex<()> = Mutex::const_new(());

#[derive(Debug, sqlx::FromRow)]
struct RemoteResource {
    remote_name: String,
    revision: i64,
    snapshot_json: Option<String>,
    create_pending: bool,
}

async fn retain_resource(
    tx: &mut Transaction<'_, Sqlite>,
    log_id: &str,
    name: &str,
    revision: i64,
    snapshot: Option<&str>,
    pending: bool,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO food_log_remote_resources \
         (food_log_id, remote_name, revision, snapshot_json, create_pending) VALUES (?, ?, ?, ?, ?) \
         ON CONFLICT(food_log_id, remote_name) DO UPDATE SET \
         snapshot_json = COALESCE(food_log_remote_resources.snapshot_json, excluded.snapshot_json), \
         create_pending = MAX(food_log_remote_resources.create_pending, excluded.create_pending)",
    )
    .bind(log_id)
    .bind(name)
    .bind(revision)
    .bind(snapshot)
    .bind(pending)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Delete only the selected meals on their original civil day.
///
/// Intent is durable before HTTP cleanup. Failures keep the meals and their
/// cleanup snapshots, block uploads/rollup refreshes, and can be retried with the
/// same IDs after restart. The returned totals preserve external nutrition and
/// activity; callers must not perform an additional stale summary rebuild.
pub async fn delete_food_logs_for_day(
    pool: &SqlitePool,
    config: &AppConfig,
    member_id: &str,
    date: &str,
    log_ids: &[String],
) -> Result<DayNutritionTotals> {
    mark_deletion_intents(pool, config, member_id, date, log_ids).await?;
    let mut has_resources = false;
    for id in log_ids {
        has_resources |= sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM food_log_remote_resources WHERE food_log_id = ?)",
        )
        .bind(id)
        .fetch_one(pool)
        .await?;
    }
    // Unuploaded local meals do not require configured Google credentials.
    let client = if has_resources {
        Some(google_health_client_for_member(member_id, config)?)
    } else {
        None
    };
    delete_marked_food_logs(pool, config, member_id, date, log_ids, client.as_ref()).await
}

/// Resume authorized deletions on their recorded days, including before today.
pub(crate) async fn resume_pending_food_deletions(
    pool: &SqlitePool,
    config: &AppConfig,
    member_id: &str,
) -> Result<()> {
    let pending: Vec<(String, String)> = sqlx::query_as(
        "SELECT civil_date, food_log_id FROM food_log_deletion_intents \
         WHERE family_member_id = ? AND completed = 0 ORDER BY civil_date, food_log_id",
    )
    .bind(member_id)
    .fetch_all(pool)
    .await?;
    let mut pending = pending.into_iter().peekable();
    while let Some((date, id)) = pending.next() {
        let mut ids = vec![id];
        while pending
            .peek()
            .is_some_and(|(next_date, _)| next_date == &date)
        {
            ids.push(pending.next().expect("peeked pending deletion").1);
        }
        delete_food_logs_for_day(pool, config, member_id, &date, &ids)
            .await
            .with_context(|| {
                format!("Food deletion for {member_id} on {date} remains pending; retry /sync")
            })?;
    }
    Ok(())
}

/// Apply requested day macros as a signed, local-only adjustment audit row.
/// Call after deleting the explicitly selected meals. Remaining concurrent meals,
/// micronutrients and activity are preserved, and undo restores the prior base.
pub async fn adjust_food_totals(
    pool: &SqlitePool,
    config: &AppConfig,
    member_id: &str,
    date: &str,
    calories: i32,
    protein: f64,
    carbs: f64,
    fats: f64,
) -> Result<()> {
    let (start, end) = chotu_common::civil_day_bounds_utc(date, config.resolved_tz())?;
    let mut tx = pool.begin().await?;
    sqlx::query(
        "UPDATE health_family_summary SET date = date WHERE date = ? AND family_member_id = ?",
    )
    .bind(date)
    .bind(member_id)
    .execute(&mut *tx)
    .await?;
    let pending: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM food_log_deletion_intents \
         WHERE family_member_id = ? AND civil_date = ? AND completed = 0)",
    )
    .bind(member_id)
    .bind(date)
    .fetch_one(&mut *tx)
    .await?;
    if pending {
        bail!("Food deletion remains pending; adjustment was not applied");
    }
    let summary = fetch_summary_nutrition(&mut *tx, member_id, date).await?;
    let remaining = sum_food_log_for_day_filtered(
        &mut *tx,
        member_id,
        date,
        config.resolved_tz(),
        FoodLogSyncFilter::All,
    )
    .await?;
    let external = summary.saturating_sub(&remaining);
    let base = external.add(&remaining);
    let mut desired = base.clone();
    desired.calories = i64::from(calories);
    desired.protein = protein;
    desired.carbs = carbs;
    desired.fats = fats;
    let now = chrono::Utc::now();
    let timestamp = if now >= start && now < end {
        now
    } else {
        start
    };
    sqlx::query(
        "INSERT INTO food_log (id, timestamp, family_member_id, raw_text_description, \
         estimated_calories, estimated_protein, estimated_carbs, estimated_fats) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(uuid::Uuid::new_v4().to_string()).bind(timestamp).bind(member_id)
    .bind(format!("Manual adjustment: {calories} kcal, {protein:.1}g protein, {carbs:.1}g carbs, {fats:.1}g fats"))
    .bind(desired.calories - base.calories).bind(desired.protein - base.protein)
    .bind(desired.carbs - base.carbs).bind(desired.fats - base.fats)
    .execute(&mut *tx).await?;
    // Audit rows have no ingredient tags. All micros are zero on the new row;
    // write_summary_nutrition_on changes nutrition only, not activity/sleep.
    write_summary_nutrition_on(&mut *tx, member_id, date, &desired).await?;
    tx.commit()
        .await
        .context("Commit food adjustment audit row and summary")?;
    Ok(())
}

async fn mark_deletion_intents(
    pool: &SqlitePool,
    config: &AppConfig,
    member_id: &str,
    date: &str,
    log_ids: &[String],
) -> Result<()> {
    let (start, end) = chotu_common::civil_day_bounds_utc(date, config.resolved_tz())?;
    let mut tx = pool.begin().await?;
    // Acquire the write lock before reading any upload state, including unenrolled meals.
    sqlx::query("UPDATE food_log_deletion_intents SET completed = completed WHERE 0")
        .execute(&mut *tx)
        .await?;
    for id in log_ids {
        let intent: Option<(String, String, bool)> = sqlx::query_as(
            "SELECT family_member_id, civil_date, completed \
             FROM food_log_deletion_intents WHERE food_log_id = ?",
        )
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some((member, day, completed)) = intent {
            if member != member_id || day != date {
                bail!("Food deletion does not match the original member and civil day");
            }
            if completed {
                let exists: bool =
                    sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM food_log WHERE id = ?)")
                        .bind(id)
                        .fetch_one(&mut *tx)
                        .await?;
                if exists {
                    bail!("Deleted food identity was reused");
                }
            }
            // References were captured atomically on the first attempt. Do not
            // recreate already-confirmed resources from stale correction fields.
            continue;
        }
        let log: FoodLog = sqlx::query_as("SELECT * FROM food_log WHERE id = ?")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?
            .context("Selected food entry was deleted or does not exist")?;
        if log.family_member_id != member_id || log.timestamp < start || log.timestamp >= end {
            bail!("Selected food entry does not belong to this member and original civil day");
        }
        let state: Option<CorrectionState> = sqlx::query_as(
            "SELECT revision, sync_state, remote_name, remote_snapshot_json, remote_create_pending, replacement_name \
             FROM food_log_corrections WHERE food_log_id = ?",
        ).bind(id).fetch_optional(&mut *tx).await?;
        let revision = state.as_ref().map_or(0, |s| s.revision);
        if let Some(s) = &state {
            if let Some(name) = s.remote_name.as_deref().filter(|name| !name.is_empty()) {
                retain_resource(
                    &mut tx,
                    id,
                    name,
                    revision,
                    s.remote_snapshot_json.as_deref(),
                    s.remote_create_pending,
                )
                .await?;
            }
        }
        if let Some(name) = log
            .google_data_point_id
            .as_deref()
            .filter(|name| !name.is_empty())
        {
            retain_resource(
                &mut tx,
                id,
                name,
                revision,
                Some(&serde_json::to_string(&log)?),
                false,
            )
            .await?;
        }
        sqlx::query(
            "INSERT INTO food_log_deletion_intents \
             (food_log_id, family_member_id, civil_date, original_timestamp, revision, google_name, \
              correction_remote_name, correction_replacement_name, correction_sync_state) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(id).bind(member_id).bind(date).bind(log.timestamp).bind(revision)
        .bind(&log.google_data_point_id)
        .bind(state.as_ref().and_then(|s| s.remote_name.as_deref()))
        .bind(state.as_ref().map(|s| s.replacement_name.as_str()))
        .bind(state.as_ref().map(|s| s.sync_state.as_str()))
        .execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(())
}

async fn delete_marked_food_logs<R: NutritionRemote>(
    pool: &SqlitePool,
    config: &AppConfig,
    member_id: &str,
    date: &str,
    log_ids: &[String],
    remote: Option<&R>,
) -> Result<DayNutritionTotals> {
    let _remote_guard = NUTRITION_MUTATIONS.lock().await;
    for id in log_ids {
        cleanup_deletion_resources(pool, id, remote).await?;
    }
    let mut tx = pool.begin().await?;
    sqlx::query("UPDATE food_log_deletion_intents SET completed = completed WHERE 0")
        .execute(&mut *tx)
        .await?;
    let summary = fetch_summary_nutrition(&mut *tx, member_id, date).await?;
    let before = sum_food_log_for_day_filtered(
        &mut *tx,
        member_id,
        date,
        config.resolved_tz(),
        FoodLogSyncFilter::All,
    )
    .await?;
    let external = summary.saturating_sub(&before);
    for id in log_ids {
        let completed: bool = sqlx::query_scalar(
            "SELECT completed FROM food_log_deletion_intents \
             WHERE food_log_id = ? AND family_member_id = ? AND civil_date = ?",
        )
        .bind(id)
        .bind(member_id)
        .bind(date)
        .fetch_optional(&mut *tx)
        .await?
        .context("Food deletion intent changed")?;
        if completed {
            continue;
        }
        let changed = sqlx::query(
            "UPDATE food_log SET id = id WHERE id = ? AND EXISTS ( \
             SELECT 1 FROM food_log_deletion_intents d \
             LEFT JOIN food_log_corrections c ON c.food_log_id = d.food_log_id \
             WHERE d.food_log_id = food_log.id AND d.completed = 0 \
             AND d.family_member_id = food_log.family_member_id \
             AND julianday(d.original_timestamp) = julianday(food_log.timestamp) \
             AND d.revision = COALESCE(c.revision, 0) \
             AND d.google_name IS food_log.google_data_point_id \
             AND d.correction_remote_name IS c.remote_name \
             AND d.correction_replacement_name IS c.replacement_name \
             AND d.correction_sync_state IS c.sync_state) \
             AND NOT EXISTS (SELECT 1 FROM food_log_remote_resources r WHERE r.food_log_id = food_log.id)",
        ).bind(id).execute(&mut *tx).await?;
        if changed.rows_affected() != 1 {
            bail!(
                "Food entry or remote resources changed during deletion; local entry was retained"
            );
        }
        chotu_common::delete_food_log_tags(&mut tx, id).await?;
        sqlx::query("DELETE FROM food_log WHERE id = ?")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE food_log_deletion_intents SET completed = 1 WHERE food_log_id = ? AND completed = 0")
            .bind(id).execute(&mut *tx).await?;
    }
    let after = sum_food_log_for_day_filtered(
        &mut *tx,
        member_id,
        date,
        config.resolved_tz(),
        FoodLogSyncFilter::All,
    )
    .await?;
    let totals = external.add(&after);
    write_summary_nutrition_on(&mut *tx, member_id, date, &totals).await?;
    tx.commit()
        .await
        .context("Commit food deletion and original-day totals")?;
    Ok(totals)
}

async fn cleanup_deletion_resources<R: NutritionRemote>(
    pool: &SqlitePool,
    log_id: &str,
    remote: Option<&R>,
) -> Result<()> {
    loop {
        let mut tx = pool.begin().await?;
        let resource: Option<RemoteResource> = sqlx::query_as(
            "SELECT r.remote_name, r.revision, r.snapshot_json, r.create_pending \
             FROM food_log_remote_resources r JOIN food_log_deletion_intents d ON d.food_log_id = r.food_log_id \
             WHERE r.food_log_id = ? AND d.completed = 0 ORDER BY r.remote_name LIMIT 1",
        ).bind(log_id).fetch_optional(&mut *tx).await?;
        tx.commit().await?;
        let Some(resource) = resource else {
            return Ok(());
        };
        let remote =
            remote.context("Google cleanup is pending; member credentials are required")?;
        if resource.create_pending {
            resolve_pending_create(
                remote,
                &resource.remote_name,
                resource.snapshot_json.as_deref(),
            )
            .await?;
        }
        remote
            .delete(&resource.remote_name)
            .await
            .context("Google meal deletion remains pending; local entry was retained")?;
        let mut tx = pool.begin().await?;
        let changed = sqlx::query(
            "DELETE FROM food_log_remote_resources WHERE food_log_id = ? AND remote_name = ? \
             AND revision = ? AND snapshot_json IS ? AND create_pending = ?",
        )
        .bind(log_id)
        .bind(&resource.remote_name)
        .bind(resource.revision)
        .bind(&resource.snapshot_json)
        .bind(resource.create_pending)
        .execute(&mut *tx)
        .await?;
        if changed.rows_affected() != 1 {
            bail!("Remote cleanup snapshot changed; food deletion remains pending");
        }
        tx.commit().await?;
    }
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
    let locked = sqlx::query(
        "UPDATE food_log SET id = id WHERE id = ? AND family_member_id = ? \
         AND NOT EXISTS (SELECT 1 FROM food_log_deletion_intents d WHERE d.food_log_id = food_log.id)",
    )
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

/// Atomically replace nutrition, tags and supplied facts, retaining the meal's member and instant.
/// The summary uses the original civil day, preserving its external nutrition and activity.
pub async fn revise_food_log(
    pool: &SqlitePool,
    config: &AppConfig,
    original: &FoodLog,
    expected_revision: i64,
    description: &str,
    estimation: &NutritionEstimation,
    tag_context: &str,
    signal_context: Option<&chotu_common::FoodSignalContext<'_>>,
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
         AND COALESCE((SELECT revision FROM food_log_corrections WHERE food_log_id = food_log.id), 0) = ? \
         AND NOT EXISTS (SELECT 1 FROM food_log_deletion_intents d WHERE d.food_log_id = food_log.id)",
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
    let current_snapshot = serde_json::to_string(&current)?;
    if let Some(s) = &state {
        if let Some(name) = s.remote_name.as_deref().filter(|name| !name.is_empty()) {
            retain_resource(
                &mut tx,
                &current.id,
                name,
                s.revision,
                s.remote_snapshot_json.as_deref(),
                s.remote_create_pending,
            )
            .await?;
        }
    }
    if let Some(name) = current
        .google_data_point_id
        .as_deref()
        .filter(|name| !name.is_empty())
    {
        retain_resource(
            &mut tx,
            &current.id,
            name,
            expected_revision,
            Some(&current_snapshot),
            false,
        )
        .await?;
    }
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
    if let Some(context) = signal_context {
        chotu_common::write_food_signal_context(&mut *tx, &current.id, context).await?;
    } else if sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM food_signal_context WHERE food_log_id = ?)",
    )
    .bind(&current.id)
    .fetch_one(&mut *tx)
    .await?
    {
        bail!("Signal meal revision requires updated user facts");
    }
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
         FROM food_log_corrections c JOIN food_log f ON f.id = c.food_log_id WHERE c.food_log_id = ? \
         AND NOT EXISTS (SELECT 1 FROM food_log_deletion_intents d WHERE d.food_log_id = f.id)",
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
         AND remote_name IS ? AND remote_snapshot_json IS ? AND remote_create_pending = ? AND replacement_name = ? \
         AND EXISTS (SELECT 1 FROM food_log f WHERE f.id = food_log_id) \
         AND NOT EXISTS (SELECT 1 FROM food_log_deletion_intents d WHERE d.food_log_id = food_log_corrections.food_log_id)",
    ).bind(log_id).bind(state.revision).bind(&state.sync_state)
        .bind(&state.remote_name).bind(&state.remote_snapshot_json).bind(state.remote_create_pending)
        .bind(&state.replacement_name).execute(&mut *tx).await?;
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
    let snapshot = serde_json::to_string(&log)?;
    sqlx::query("UPDATE food_log_corrections SET remote_snapshot_json = ?, remote_create_pending = 1 WHERE food_log_id = ?")
        .bind(&snapshot).bind(log_id).execute(&mut *tx).await?;
    retain_resource(
        &mut tx,
        log_id,
        &state.replacement_name,
        state.revision,
        Some(&snapshot),
        true,
    )
    .await?;
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
    // Even a stale/intent-blocked completion must retain the confirmed resource.
    // Never compensate with an untracked DELETE: cleanup belongs to the ledger.
    sqlx::query("UPDATE food_log SET id = id WHERE id = ?")
        .bind(&log.id)
        .execute(&mut *tx)
        .await?;
    let exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM food_log WHERE id = ?)")
        .bind(&log.id)
        .fetch_one(&mut *tx)
        .await?;
    if !exists {
        bail!("Food entry was deleted during Google sync");
    }
    let snapshot = serde_json::to_string(log)?;
    retain_resource(
        &mut tx,
        &log.id,
        name,
        state.revision,
        Some(&snapshot),
        false,
    )
    .await?;
    sqlx::query(
        "UPDATE food_log_remote_resources SET create_pending = 0 \
         WHERE food_log_id = ? AND remote_name IN (?, ?) AND revision = ?",
    )
    .bind(&log.id)
    .bind(name)
    .bind(&state.replacement_name)
    .bind(state.revision)
    .execute(&mut *tx)
    .await?;
    if name != state.replacement_name
        && name.rsplit('/').next() == state.replacement_name.rsplit('/').next()
    {
        sqlx::query("DELETE FROM food_log_remote_resources WHERE food_log_id = ? AND remote_name = ? AND revision = ?")
            .bind(&log.id).bind(&state.replacement_name).bind(state.revision).execute(&mut *tx).await?;
    }
    let changed = sqlx::query(
        "UPDATE food_log_corrections SET sync_state = 'synced', remote_name = ?, remote_snapshot_json = ?, remote_create_pending = 0 \
         WHERE food_log_id = ? AND revision = ? AND sync_state = 'create_pending' \
         AND remote_name = ? AND replacement_name = ? AND remote_snapshot_json = ? AND remote_create_pending = 1 \
         AND NOT EXISTS (SELECT 1 FROM food_log_deletion_intents d WHERE d.food_log_id = food_log_corrections.food_log_id)",
    ).bind(name).bind(&snapshot).bind(&log.id).bind(state.revision)
        .bind(&state.replacement_name).bind(&state.replacement_name).bind(&snapshot).execute(&mut *tx).await?;
    if changed.rows_affected() != 1 {
        let synced: Option<String> = sqlx::query_scalar(
            "SELECT f.google_data_point_id FROM food_log f JOIN food_log_corrections c ON c.food_log_id = f.id \
             WHERE f.id = ? AND c.revision = ? AND c.sync_state = 'synced' \
             AND NOT EXISTS (SELECT 1 FROM food_log_deletion_intents d WHERE d.food_log_id = f.id)",
        ).bind(&log.id).bind(state.revision).fetch_optional(&mut *tx).await?.flatten();
        let already_synced = synced
            .as_deref()
            .is_some_and(|saved| saved.rsplit('/').next() == name.rsplit('/').next());
        tx.commit().await?;
        if already_synced {
            return Ok(());
        }
        bail!("Food entry changed or deletion was requested during Google sync; remote cleanup was retained");
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
    sync_food_log_with_remote(pool, client, log_id).await?;
    Ok(())
}

async fn sync_food_log_with_remote<R: NutritionRemote>(
    pool: &SqlitePool,
    remote: &R,
    log_id: &str,
) -> Result<()> {
    let _remote_guard = NUTRITION_MUTATIONS.lock().await;
    // A queued worker must re-read intent after acquiring the remote guard.
    let state = correction_state(pool, log_id).await?;
    if state.sync_state == "synced" {
        cleanup_correction_resources(pool, remote, log_id, &state).await?;
        return Ok(());
    }
    cleanup_correction_resources(pool, remote, log_id, &state).await?;
    let log = prepare_create(pool, log_id, &state).await?;
    // prepare_create durably authorizes this named snapshot before HTTP. Intent
    // may now be marked, but the deleter waits for this mutation window to end
    // and then settles/removes the retained snapshot, even on ambiguous failure.
    let name = remote
        .create(&log, &state.replacement_name)
        .await
        .context("Saved locally; Google nutrition upload is still pending")?;
    finish_create(pool, &log, &state, &name).await
}

async fn cleanup_correction_resources<R: NutritionRemote>(
    pool: &SqlitePool,
    remote: &R,
    log_id: &str,
    state: &CorrectionState,
) -> Result<()> {
    let resources: Vec<RemoteResource> = sqlx::query_as(
        "SELECT remote_name, revision, snapshot_json, create_pending \
         FROM food_log_remote_resources WHERE food_log_id = ? ORDER BY remote_name",
    )
    .bind(log_id)
    .fetch_all(pool)
    .await?;
    for resource in resources {
        if state.sync_state == "synced"
            && state.remote_name.as_deref() == Some(resource.remote_name.as_str())
        {
            continue;
        }
        // A current create retry must read back the same name rather than delete it.
        if state.sync_state == "create_pending"
            && resource.remote_name == state.replacement_name
            && resource.revision == state.revision
        {
            continue;
        }
        if resource.create_pending {
            resolve_pending_create(
                remote,
                &resource.remote_name,
                resource.snapshot_json.as_deref(),
            )
            .await?;
        }
        remote
            .delete(&resource.remote_name)
            .await
            .context("Corrected locally; Google deletion is still pending")?;
        sqlx::query(
            "DELETE FROM food_log_remote_resources WHERE food_log_id = ? AND remote_name = ? \
             AND revision = ? AND snapshot_json IS ? AND create_pending = ?",
        )
        .bind(log_id)
        .bind(&resource.remote_name)
        .bind(resource.revision)
        .bind(&resource.snapshot_json)
        .bind(resource.create_pending)
        .execute(pool)
        .await?;
    }
    Ok(())
}

async fn resolve_pending_create<R: NutritionRemote>(
    client: &R,
    name: &str,
    snapshot: Option<&str>,
) -> Result<()> {
    let baseline: FoodLog =
        serde_json::from_str(snapshot.context("Pending Google create is missing its snapshot")?)?;
    // A missing GET alone cannot rule out an in-flight POST. Complete/read back the
    // same persisted name and payload before removing it, including on undo/clear.
    client.create(&baseline, name).await.context(
        "Earlier Google nutrition creation is still unresolved; deletion remains pending",
    )?;
    Ok(())
}

pub(crate) async fn ensure_corrections_synced_for_member(
    pool: &SqlitePool,
    member_id: &str,
    timezone: chrono_tz::Tz,
) -> Result<()> {
    let pending: Option<chrono::DateTime<chrono::Utc>> = sqlx::query_scalar(
        "SELECT f.timestamp FROM food_log f \
         LEFT JOIN food_log_corrections c ON c.food_log_id = f.id \
         LEFT JOIN food_log_deletion_intents d ON d.food_log_id = f.id \
         WHERE f.family_member_id = ? AND (c.sync_state != 'synced' OR d.completed = 0 \
         OR EXISTS (SELECT 1 FROM food_log_remote_resources r WHERE r.food_log_id = f.id \
         AND (r.remote_name IS NOT c.remote_name OR r.create_pending = 1))) \
         ORDER BY f.timestamp LIMIT 1",
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
        "SELECT COUNT(*) FROM food_log f \
         LEFT JOIN food_log_corrections c ON c.food_log_id = f.id \
         LEFT JOIN food_log_deletion_intents d ON d.food_log_id = f.id \
         WHERE f.family_member_id = ? AND julianday(f.timestamp) >= julianday(?) \
         AND julianday(f.timestamp) < julianday(?) AND (c.sync_state != 'synced' OR d.completed = 0 \
         OR EXISTS (SELECT 1 FROM food_log_remote_resources r WHERE r.food_log_id = f.id \
         AND (r.remote_name IS NOT c.remote_name OR r.create_pending = 1)))",
    )
    .bind(member_id)
    .bind(start)
    .bind(end)
    .fetch_one(pool)
    .await?;
    if pending != 0 {
        bail!("{pending} food update/deletion(s) for {date} remain pending; local totals were preserved. Retry the food deletion command or /sync");
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

    #[derive(Default)]
    struct ControlledRemote {
        meals: Mutex<std::collections::BTreeMap<String, FoodLog>>,
        creates: std::sync::atomic::AtomicUsize,
        deletes: std::sync::atomic::AtomicUsize,
        fail_create: std::sync::atomic::AtomicBool,
        fail_delete: std::sync::atomic::AtomicBool,
        block_create: std::sync::atomic::AtomicBool,
        create_entered: tokio::sync::Notify,
        create_release: tokio::sync::Notify,
    }

    impl NutritionRemote for ControlledRemote {
        async fn create(&self, log: &FoodLog, name: &str) -> Result<String> {
            use std::sync::atomic::Ordering::SeqCst;
            self.creates.fetch_add(1, SeqCst);
            if self.block_create.swap(false, SeqCst) {
                self.create_entered.notify_one();
                self.create_release.notified().await;
            }
            self.meals.lock().await.insert(name.to_owned(), log.clone());
            if self.fail_create.load(SeqCst) {
                bail!("Ambiguous create: the remote may have accepted the request");
            }
            Ok(name.to_owned())
        }

        async fn delete(&self, name: &str) -> Result<()> {
            use std::sync::atomic::Ordering::SeqCst;
            self.deletes.fetch_add(1, SeqCst);
            // Match the production reader: an Operation is not a DataPoint,
            // and its absence from this fixture's meals never confirms deletion.
            let mut parts = name.rsplit('/');
            if parts.next().is_some() && parts.next() == Some("operations") {
                bail!("Legacy Operation has no confirmed DataPoint name");
            }
            if self.fail_delete.load(SeqCst) {
                bail!("Remote deletion failed");
            }
            self.meals.lock().await.remove(name);
            Ok(())
        }
    }

    async fn baseline_summary(pool: &SqlitePool) {
        write_summary_nutrition(
            pool,
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
            .execute(pool).await.unwrap();
    }

    #[tokio::test]
    async fn deletion_before_enrollment_blocks_queued_upload_and_preserves_original_day() {
        use std::sync::{atomic::Ordering::SeqCst, Arc};
        let pool = chotu_common::init_db(":memory:").await.unwrap();
        let config = config();
        let original = original(&pool, None).await;
        baseline_summary(&pool).await;
        let remote = Arc::new(ControlledRemote::default());
        let ids = vec![original.id.clone()];
        let guard = NUTRITION_MUTATIONS.lock().await;
        mark_deletion_intents(&pool, &config, "alex", "2026-09-29", &ids)
            .await
            .unwrap();
        assert!(ensure_food_log_upload_state(&pool, &original)
            .await
            .is_err());
        assert!(revise_food_log(
            &pool,
            &config,
            &original,
            0,
            "stale",
            &estimation(),
            "stale",
            None,
        )
        .await
        .is_err());
        let worker_pool = pool.clone();
        let worker_remote = remote.clone();
        let queued = tokio::spawn(async move {
            sync_food_log_with_remote(&worker_pool, worker_remote.as_ref(), "meal").await
        });
        drop(guard);
        assert!(queued.await.unwrap().is_err());
        let totals = delete_marked_food_logs(
            &pool,
            &config,
            "alex",
            "2026-09-29",
            &ids,
            Some(remote.as_ref()),
        )
        .await
        .unwrap();
        assert_eq!(
            (totals.calories, totals.protein, totals.sodium_mg),
            (100, 10.0, 50.0)
        );
        assert_eq!(remote.creates.load(SeqCst), 0);
        let activity: (i32, i32, f64) = sqlx::query_as("SELECT step_count, active_calories_burned, sleep_hours FROM health_family_summary WHERE family_member_id = 'alex' AND date = '2026-09-29'")
            .fetch_one(&pool).await.unwrap();
        assert_eq!(activity, (1234, 80, 7.0));
        let leftovers: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM food_log_tags WHERE food_log_id = 'meal'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(leftovers, 0);
        let retry = delete_food_logs_for_day(&pool, &config, "alex", "2026-09-29", &ids)
            .await
            .unwrap();
        assert_eq!(retry.calories, 100);
        assert!(
            delete_food_logs_for_day(&pool, &config, "alex", "2026-09-30", &ids)
                .await
                .is_err()
        );
        assert!(
            delete_food_logs_for_day(&pool, &config, "other", "2026-09-29", &ids)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn replacement_finishing_after_intent_is_cleaned_without_deleting_later_meal() {
        use std::sync::{atomic::Ordering::SeqCst, Arc};
        let pool = chotu_common::init_db(":memory:").await.unwrap();
        let config = config();
        let old = "users/me/dataTypes/nutrition-log/dataPoints/old-meal";
        let original = original(&pool, Some(old)).await;
        baseline_summary(&pool).await;
        revise_food_log(
            &pool,
            &config,
            &original,
            0,
            "lentils",
            &estimation(),
            "lentils",
            None,
        )
        .await
        .unwrap();
        let remote = Arc::new(ControlledRemote::default());
        remote.meals.lock().await.insert(old.to_owned(), original);
        remote.block_create.store(true, SeqCst);
        let worker_pool = pool.clone();
        let worker_remote = remote.clone();
        let upload = tokio::spawn(async move {
            sync_food_log_with_remote(&worker_pool, worker_remote.as_ref(), "meal").await
        });
        remote.create_entered.notified().await;
        let ids = vec!["meal".to_owned()];
        mark_deletion_intents(&pool, &config, "alex", "2026-09-29", &ids)
            .await
            .unwrap();
        // This write completes while HTTP is paused: the remote guard never
        // holds SQLite's write lock, and the new meal is outside the selected IDs.
        let mut tx = pool.begin().await.unwrap();
        sqlx::query("INSERT INTO food_log (id,timestamp,family_member_id,raw_text_description,estimated_calories) VALUES ('later','2026-09-30T02:30:00Z','alex','later snack',50)")
            .execute(&mut *tx).await.unwrap();
        sqlx::query("UPDATE health_family_summary SET total_calories_ingested = total_calories_ingested + 50 WHERE family_member_id = 'alex' AND date = '2026-09-29'")
            .execute(&mut *tx).await.unwrap();
        tx.commit().await.unwrap();
        let retry_id: String = sqlx::query_scalar(
            "SELECT f.id FROM food_log f LEFT JOIN food_log_deletion_intents d \
             ON d.food_log_id = f.id AND d.completed = 0 AND d.civil_date = ? \
             WHERE f.family_member_id = ? ORDER BY (d.food_log_id IS NOT NULL) DESC, f.timestamp DESC LIMIT 1",
        ).bind("2026-09-29").bind("alex").fetch_one(&pool).await.unwrap();
        assert_eq!(retry_id, "meal");
        let delete_pool = pool.clone();
        let delete_remote = remote.clone();
        let delete_config = config.clone();
        let deletion = tokio::spawn(async move {
            delete_marked_food_logs(
                &delete_pool,
                &delete_config,
                "alex",
                "2026-09-29",
                &ids,
                Some(delete_remote.as_ref()),
            )
            .await
        });
        remote.create_release.notify_one();
        assert!(upload.await.unwrap().is_err());
        let totals = deletion.await.unwrap().unwrap();
        assert_eq!(totals.calories, 150);
        assert_eq!(remote.creates.load(SeqCst), 1);
        assert!(remote.meals.lock().await.is_empty());
        let remaining: Vec<String> = sqlx::query_scalar("SELECT id FROM food_log ORDER BY id")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(remaining, vec!["later"]);
    }

    #[tokio::test]
    async fn ambiguous_create_and_failed_cleanup_survive_restart_and_retry() {
        use std::sync::atomic::Ordering::SeqCst;
        let path =
            std::env::temp_dir().join(format!("chotu-food-deletion-{}.db", uuid::Uuid::new_v4()));
        let pool = chotu_common::init_db(path.to_str().unwrap()).await.unwrap();
        let config = config();
        let original = original(&pool, None).await;
        baseline_summary(&pool).await;
        ensure_food_log_upload_state(&pool, &original)
            .await
            .unwrap();
        let remote = ControlledRemote::default();
        remote.fail_create.store(true, SeqCst);
        assert!(sync_food_log_with_remote(&pool, &remote, "meal")
            .await
            .is_err());
        let ids = vec!["meal".to_owned()];
        mark_deletion_intents(&pool, &config, "alex", "2026-09-29", &ids)
            .await
            .unwrap();
        assert!(
            delete_marked_food_logs(&pool, &config, "alex", "2026-09-29", &ids, Some(&remote))
                .await
                .is_err()
        );
        let snapshot: (String, bool) = sqlx::query_as("SELECT snapshot_json, create_pending FROM food_log_remote_resources WHERE food_log_id = 'meal'")
            .fetch_one(&pool).await.unwrap();
        assert!(snapshot.1);
        assert_eq!(
            serde_json::from_str::<FoodLog>(&snapshot.0)
                .unwrap()
                .estimated_calories,
            500
        );
        assert_eq!(
            fetch_summary_nutrition(&pool, "alex", "2026-09-29")
                .await
                .unwrap()
                .calories,
            600
        );
        assert!(
            ensure_corrections_synced_for_member(&pool, "alex", config.resolved_tz())
                .await
                .is_err()
        );
        let mut tx = pool.begin().await.unwrap();
        chotu_common::delete_food_log_tags(&mut tx, "meal")
            .await
            .unwrap();
        assert!(sqlx::query("DELETE FROM food_log WHERE id = 'meal'")
            .execute(&mut *tx)
            .await
            .is_err());
        tx.rollback().await.unwrap();
        pool.close().await;
        let pool = chotu_common::init_db(path.to_str().unwrap()).await.unwrap();
        remote.fail_create.store(false, SeqCst);
        remote.fail_delete.store(true, SeqCst);
        mark_deletion_intents(&pool, &config, "alex", "2026-09-29", &ids)
            .await
            .unwrap();
        assert!(
            delete_marked_food_logs(&pool, &config, "alex", "2026-09-29", &ids, Some(&remote))
                .await
                .is_err()
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM food_log_remote_resources")
                .fetch_one(&pool)
                .await
                .unwrap(),
            1
        );
        remote.fail_delete.store(false, SeqCst);
        let totals =
            delete_marked_food_logs(&pool, &config, "alex", "2026-09-29", &ids, Some(&remote))
                .await
                .unwrap();
        assert_eq!(totals.calories, 100);
        assert!(remote.meals.lock().await.is_empty());
        let creates = remote.creates.load(SeqCst);
        mark_deletion_intents(&pool, &config, "alex", "2026-09-29", &ids)
            .await
            .unwrap();
        delete_marked_food_logs(&pool, &config, "alex", "2026-09-29", &ids, Some(&remote))
            .await
            .unwrap();
        assert_eq!(remote.creates.load(SeqCst), creates);
        pool.close().await;
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn legacy_operation_correction_and_deletion_retain_identity_after_restart() {
        use std::sync::atomic::Ordering::SeqCst;
        let path = std::env::temp_dir().join(format!(
            "chotu-legacy-operation-{}.db",
            uuid::Uuid::new_v4()
        ));
        let pool = chotu_common::init_db(path.to_str().unwrap()).await.unwrap();
        let config = config();
        let operation = "users/me/dataTypes/nutrition-log/operations/legacy-create";
        let original = original(&pool, Some(operation)).await;
        baseline_summary(&pool).await;
        // Reproduce the migration's legacy row: an anonymous create's Operation
        // name was copied verbatim, with no completed DataPoint or payload.
        sqlx::query(
            "INSERT INTO food_log_remote_resources \
             (food_log_id, remote_name, revision, create_pending) VALUES ('meal', ?, 0, 0)",
        )
        .bind(operation)
        .execute(&pool)
        .await
        .unwrap();
        ensure_food_log_upload_state(&pool, &original)
            .await
            .unwrap();
        revise_food_log(
            &pool,
            &config,
            &original,
            0,
            "lentils",
            &estimation(),
            "lentils",
            None,
        )
        .await
        .unwrap();
        let remote = ControlledRemote::default();
        assert!(sync_food_log_with_remote(&pool, &remote, "meal")
            .await
            .is_err());
        let state = correction_state(&pool, "meal").await.unwrap();
        assert_eq!(state.revision, 1);
        assert_eq!(state.sync_state, "delete_pending");
        assert_eq!(state.remote_name.as_deref(), Some(operation));
        let resource: (String, i64, String, bool) = sqlx::query_as(
            "SELECT remote_name, revision, snapshot_json, create_pending \
             FROM food_log_remote_resources WHERE food_log_id = 'meal'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let baseline: FoodLog = serde_json::from_str(&resource.2).unwrap();
        assert_eq!(baseline.google_data_point_id.as_deref(), Some(operation));
        assert_eq!(baseline.estimated_calories, 500);
        assert_eq!(
            state.remote_snapshot_json.as_deref(),
            Some(resource.2.as_str())
        );
        let ids = vec!["meal".to_owned()];
        mark_deletion_intents(&pool, &config, "alex", "2026-09-29", &ids)
            .await
            .unwrap();
        assert!(
            delete_marked_food_logs(&pool, &config, "alex", "2026-09-29", &ids, Some(&remote))
                .await
                .is_err()
        );
        pool.close().await;
        let pool = chotu_common::init_db(path.to_str().unwrap()).await.unwrap();
        mark_deletion_intents(&pool, &config, "alex", "2026-09-29", &ids)
            .await
            .unwrap();
        assert!(
            delete_marked_food_logs(&pool, &config, "alex", "2026-09-29", &ids, Some(&remote))
                .await
                .is_err()
        );
        let retained: (String, i64, String, bool) = sqlx::query_as(
            "SELECT remote_name, revision, snapshot_json, create_pending \
             FROM food_log_remote_resources WHERE food_log_id = 'meal'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(retained, resource);
        let intent: (i64, Option<String>, Option<String>, String, bool) = sqlx::query_as(
            "SELECT revision, google_name, correction_remote_name, correction_sync_state, completed \
             FROM food_log_deletion_intents WHERE food_log_id = 'meal'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            intent,
            (
                1,
                Some(operation.to_owned()),
                Some(operation.to_owned()),
                "delete_pending".into(),
                false
            )
        );
        let retained: FoodLog = sqlx::query_as("SELECT * FROM food_log WHERE id = 'meal'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(retained.timestamp, original.timestamp);
        assert_eq!(retained.google_data_point_id.as_deref(), Some(operation));
        assert_eq!(retained.estimated_calories, 240);
        assert_eq!(
            fetch_summary_nutrition(&pool, "alex", "2026-09-29")
                .await
                .unwrap()
                .calories,
            340
        );
        let activity: (i32, i32, f64) = sqlx::query_as(
            "SELECT step_count, active_calories_burned, sleep_hours FROM health_family_summary \
             WHERE family_member_id = 'alex' AND date = '2026-09-29'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(activity, (1234, 80, 7.0));
        assert_eq!(remote.creates.load(SeqCst), 0);
        assert!(remote.meals.lock().await.is_empty());
        pool.close().await;
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn stale_completion_retains_every_resource_until_confirmed_cleanup() {
        let pool = chotu_common::init_db(":memory:").await.unwrap();
        let config = config();
        let original = original(&pool, None).await;
        ensure_food_log_upload_state(&pool, &original)
            .await
            .unwrap();
        let first = correction_state(&pool, "meal").await.unwrap();
        let in_flight = prepare_create(&pool, "meal", &first).await.unwrap();
        revise_food_log(
            &pool,
            &config,
            &original,
            0,
            "lentils",
            &estimation(),
            "lentils",
            None,
        )
        .await
        .unwrap();
        assert!(
            finish_create(&pool, &in_flight, &first, &first.replacement_name)
                .await
                .is_err()
        );
        let next = correction_state(&pool, "meal").await.unwrap();
        let replacement = prepare_create(&pool, "meal", &next).await.unwrap();
        finish_create(&pool, &replacement, &next, &next.replacement_name)
            .await
            .unwrap();
        let remote = ControlledRemote::default();
        remote
            .meals
            .lock()
            .await
            .insert(first.replacement_name.clone(), in_flight);
        remote
            .meals
            .lock()
            .await
            .insert(next.replacement_name.clone(), replacement);
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM food_log_remote_resources")
                .fetch_one(&pool)
                .await
                .unwrap(),
            2
        );
        assert!(
            ensure_corrections_synced_for_member(&pool, "alex", config.resolved_tz())
                .await
                .is_err()
        );
        sync_food_log_with_remote(&pool, &remote, "meal")
            .await
            .unwrap();
        let names: Vec<String> =
            sqlx::query_scalar("SELECT remote_name FROM food_log_remote_resources")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(names, vec![next.replacement_name.clone()]);
        assert_eq!(remote.meals.lock().await.len(), 1);
        let ids = vec!["meal".to_owned()];
        mark_deletion_intents(&pool, &config, "alex", "2026-09-29", &ids)
            .await
            .unwrap();
        delete_marked_food_logs(&pool, &config, "alex", "2026-09-29", &ids, Some(&remote))
            .await
            .unwrap();
        assert!(remote.meals.lock().await.is_empty());
    }

    #[tokio::test]
    async fn deletion_validates_entire_selection_and_final_resource_revision_cas() {
        let pool = chotu_common::init_db(":memory:").await.unwrap();
        let config = config();
        let original = original(&pool, None).await;
        let ids = vec![original.id.clone(), "missing".to_owned()];
        assert!(
            mark_deletion_intents(&pool, &config, "alex", "2026-09-29", &ids)
                .await
                .is_err()
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM food_log_deletion_intents")
                .fetch_one(&pool)
                .await
                .unwrap(),
            0
        );
        assert!(mark_deletion_intents(
            &pool,
            &config,
            "other",
            "2026-09-29",
            std::slice::from_ref(&original.id)
        )
        .await
        .is_err());
        assert!(mark_deletion_intents(
            &pool,
            &config,
            "alex",
            "2026-09-30",
            std::slice::from_ref(&original.id)
        )
        .await
        .is_err());
        ensure_food_log_upload_state(&pool, &original)
            .await
            .unwrap();
        mark_deletion_intents(
            &pool,
            &config,
            "alex",
            "2026-09-29",
            std::slice::from_ref(&original.id),
        )
        .await
        .unwrap();
        // An unexpected writer cannot make a stale cleanup commit erase changed data.
        sqlx::query(
            "UPDATE food_log_corrections SET revision = revision + 1 WHERE food_log_id = 'meal'",
        )
        .execute(&pool)
        .await
        .unwrap();
        assert!(delete_marked_food_logs::<ControlledRemote>(
            &pool,
            &config,
            "alex",
            "2026-09-29",
            std::slice::from_ref(&original.id),
            None
        )
        .await
        .is_err());
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM food_log WHERE id = 'meal'")
                .fetch_one(&pool)
                .await
                .unwrap(),
            1
        );
    }

    #[tokio::test]
    async fn confirmed_cleanup_is_not_recreated_when_local_delete_commit_fails() {
        use std::sync::atomic::Ordering::SeqCst;
        let pool = chotu_common::init_db(":memory:").await.unwrap();
        let config = config();
        let name = "users/me/dataTypes/nutrition-log/dataPoints/legacy-meal";
        let original = original(&pool, Some(name)).await;
        baseline_summary(&pool).await;
        let remote = ControlledRemote::default();
        remote.meals.lock().await.insert(name.to_owned(), original);
        let ids = vec!["meal".to_owned()];
        mark_deletion_intents(&pool, &config, "alex", "2026-09-29", &ids)
            .await
            .unwrap();
        sqlx::query("CREATE TRIGGER reject_local_delete BEFORE DELETE ON food_log BEGIN SELECT RAISE(ABORT, 'local delete failed'); END")
            .execute(&pool).await.unwrap();
        assert!(
            delete_marked_food_logs(&pool, &config, "alex", "2026-09-29", &ids, Some(&remote))
                .await
                .is_err()
        );
        assert!(remote.meals.lock().await.is_empty());
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM food_log_remote_resources")
                .fetch_one(&pool)
                .await
                .unwrap(),
            0
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM food_log_tags WHERE food_log_id = 'meal'"
            )
            .fetch_one(&pool)
            .await
            .unwrap(),
            1
        );
        assert_eq!(
            fetch_summary_nutrition(&pool, "alex", "2026-09-29")
                .await
                .unwrap()
                .calories,
            600
        );
        assert!(
            ensure_corrections_synced_for_member(&pool, "alex", config.resolved_tz())
                .await
                .is_err()
        );
        sqlx::query("DROP TRIGGER reject_local_delete")
            .execute(&pool)
            .await
            .unwrap();
        mark_deletion_intents(&pool, &config, "alex", "2026-09-29", &ids)
            .await
            .unwrap();
        let totals =
            delete_marked_food_logs(&pool, &config, "alex", "2026-09-29", &ids, Some(&remote))
                .await
                .unwrap();
        assert_eq!(totals.calories, 100);
        assert_eq!(remote.creates.load(SeqCst), 0);
        assert_eq!(remote.deletes.load(SeqCst), 1);
    }

    #[tokio::test]
    async fn signed_adjustment_preserves_remaining_meal_and_undo_restores_base() {
        let pool = chotu_common::init_db(":memory:").await.unwrap();
        let config = config();
        original(&pool, None).await;
        baseline_summary(&pool).await;
        adjust_food_totals(&pool, &config, "alex", "2026-09-29", 50, 5.0, 1.0, 2.0)
            .await
            .unwrap();
        let audit: FoodLog = sqlx::query_as(
            "SELECT * FROM food_log WHERE raw_text_description LIKE 'Manual adjustment:%'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(audit.estimated_calories, -550);
        assert_eq!(audit.estimated_protein, -35.0);
        let local = sum_food_log_for_day(&pool, "alex", "2026-09-29", config.resolved_tz())
            .await
            .unwrap();
        assert_eq!(local.calories, -50);
        let adjusted = fetch_summary_nutrition(&pool, "alex", "2026-09-29")
            .await
            .unwrap();
        assert_eq!(
            (adjusted.calories, adjusted.protein, adjusted.sodium_mg),
            (50, 5.0, 50.0)
        );
        let totals = delete_food_logs_for_day(
            &pool,
            &config,
            "alex",
            "2026-09-29",
            std::slice::from_ref(&audit.id),
        )
        .await
        .unwrap();
        assert_eq!(
            (totals.calories, totals.protein, totals.sodium_mg),
            (600, 40.0, 50.0)
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM food_log WHERE id = 'meal'")
                .fetch_one(&pool)
                .await
                .unwrap(),
            1
        );
    }

    #[tokio::test]
    async fn adjustment_rolls_back_audit_and_summary_together() {
        let pool = chotu_common::init_db(":memory:").await.unwrap();
        baseline_summary(&pool).await;
        sqlx::query("CREATE TRIGGER reject_adjustment_summary BEFORE UPDATE ON health_family_summary WHEN NEW.total_calories_ingested != OLD.total_calories_ingested BEGIN SELECT RAISE(ABORT, 'summary rejected'); END")
            .execute(&pool).await.unwrap();
        assert!(
            adjust_food_totals(&pool, &config(), "alex", "2026-09-29", 50, 5.0, 1.0, 2.0)
                .await
                .is_err()
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM food_log")
                .fetch_one(&pool)
                .await
                .unwrap(),
            0
        );
        assert_eq!(
            fetch_summary_nutrition(&pool, "alex", "2026-09-29")
                .await
                .unwrap()
                .calories,
            600
        );
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
    async fn meal_revision_and_signal_facts_commit_or_roll_back_together() {
        let pool = chotu_common::init_db(":memory:").await.unwrap();
        let config = config();
        let original = original(&pool, None).await;
        baseline_summary(&pool).await;
        let old_context = chotu_common::FoodSignalContext {
            recipient_kind: "group",
            recipient_id: "household",
            sender_aci: "aci-alex",
            user_facts: "milk curry, full bowl",
        };
        chotu_common::write_food_signal_context(&pool, "meal", &old_context)
            .await
            .unwrap();
        let next_context = chotu_common::FoodSignalContext {
            user_facts: "No dairy. Lentil curry, half bowl",
            ..old_context
        };
        let mut estimate = estimation();
        estimate.tags = vec!["dairy".into()];
        assert!(revise_food_log(
            &pool,
            &config,
            &original,
            0,
            "lentil curry",
            &estimate,
            "lentil curry; no dairy",
            None
        )
        .await
        .is_err());
        sqlx::query("CREATE TRIGGER fail_meal_facts BEFORE UPDATE ON food_signal_context BEGIN SELECT RAISE(ABORT, 'facts unavailable'); END")
            .execute(&pool).await.unwrap();
        assert!(revise_food_log(
            &pool,
            &config,
            &original,
            0,
            "lentil curry",
            &estimate,
            "lentil curry; no dairy",
            Some(&next_context)
        )
        .await
        .is_err());
        let retained: (String, i32) = sqlx::query_as(
            "SELECT raw_text_description, estimated_calories FROM food_log WHERE id = 'meal'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(retained, ("milk curry".into(), 500));
        let facts: String = sqlx::query_scalar(
            "SELECT user_facts FROM food_signal_context WHERE food_log_id = 'meal'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(facts, "milk curry, full bowl");
        let tags: Vec<String> = sqlx::query_scalar(
            "SELECT tag FROM food_log_tags WHERE food_log_id = 'meal' ORDER BY tag",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(tags, ["dairy"]);
        assert_eq!(food_log_revision(&pool, "meal").await.unwrap(), 0);
        assert_eq!(
            fetch_summary_nutrition(&pool, "alex", "2026-09-29")
                .await
                .unwrap()
                .calories,
            600
        );
        sqlx::query("DROP TRIGGER fail_meal_facts")
            .execute(&pool)
            .await
            .unwrap();
        let foreign_context = chotu_common::FoodSignalContext {
            sender_aci: "aci-jordan",
            ..next_context
        };
        assert!(revise_food_log(
            &pool,
            &config,
            &original,
            0,
            "lentil curry",
            &estimate,
            "lentil curry; no dairy",
            Some(&foreign_context),
        )
        .await
        .is_err());
        assert_eq!(
            revise_food_log(
                &pool,
                &config,
                &original,
                0,
                "lentil curry",
                &estimate,
                "lentil curry; no dairy",
                Some(&next_context)
            )
            .await
            .unwrap(),
            1
        );
        let committed: (String, i32, String) = sqlx::query_as(
            "SELECT f.raw_text_description, f.estimated_calories, c.user_facts \
             FROM food_log f JOIN food_signal_context c ON c.food_log_id = f.id WHERE f.id = 'meal'",
        ).fetch_one(&pool).await.unwrap();
        assert_eq!(
            committed,
            ("lentil curry".into(), 240, next_context.user_facts.into())
        );
        let tags: Vec<String> = sqlx::query_scalar(
            "SELECT tag FROM food_log_tags WHERE food_log_id = 'meal' ORDER BY tag",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert!(tags.is_empty());
        assert_eq!(
            fetch_summary_nutrition(&pool, "alex", "2026-09-29")
                .await
                .unwrap()
                .calories,
            340
        );
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
            None,
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
            None,
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
        // Fixture-only hard deletion models an already-confirmed remote cleanup.
        sqlx::query("DELETE FROM food_log_remote_resources WHERE food_log_id = 'meal'")
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
                "lentil curry",
                None,
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
            None,
        )
        .await
        .unwrap();
        let before: String =
            sqlx::query_scalar("SELECT raw_text_description FROM food_log WHERE id = 'meal'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(revise_food_log(
            &pool,
            &config,
            &original,
            0,
            "stale",
            &estimation(),
            "milk",
            None
        )
        .await
        .is_err());
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
        sqlx::query("DELETE FROM food_log_remote_resources WHERE food_log_id = 'meal'")
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
            "milk",
            None,
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
            "lentils",
            None,
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
            None,
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
        // Simulate the confirmed HTTP delete before the committed create snapshot.
        sqlx::query(
            "DELETE FROM food_log_remote_resources WHERE food_log_id = 'meal' AND remote_name = ?",
        )
        .bind(old_name)
        .execute(&pool)
        .await
        .unwrap();
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
            None,
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
            None,
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
            None,
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
            None,
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
