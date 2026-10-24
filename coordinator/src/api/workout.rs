//! Workout endpoints over the typed core in `health_coach::strength_log`.
//! Signal's `/log` uses the same core, so a workout started in one shows up in
//! the other (see `docs/app-api.md`).

use super::idempotency::Idempotency;
use super::{ApiError, Caller};
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use health_coach::{
    ExerciseSummary, Load, LoggedSet, PerformedAtError, SetInput, Target, WeightUnit, WorkoutError,
    WorkoutState,
};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

/// `POST /v1/workout/sets`: one set.
#[derive(Debug, Deserialize)]
pub(super) struct LogSetRequest {
    exercise: String,
    /// Required but nullable: `null` is bodyweight.
    #[serde(deserialize_with = "Option::deserialize")]
    weight: Option<f64>,
    unit: Unit,
    reps: u32,
    /// RFC 3339 with an offset; server time when absent.
    #[serde(default)]
    performed_at: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Unit {
    Lb,
    Kg,
}

impl From<Unit> for WeightUnit {
    fn from(unit: Unit) -> Self {
        match unit {
            Unit::Lb => Self::Lb,
            Unit::Kg => Self::Kg,
        }
    }
}

#[derive(Debug, Serialize)]
pub(super) struct WorkoutResponse {
    /// `null` when no workout is open today.
    workout: Option<WorkoutBody>,
}

#[derive(Debug, Serialize)]
pub(super) struct SetLoggedResponse {
    set: SetBody,
    /// The workout the set joined. For a set done earlier (`performed_at`)
    /// this can be an earlier workout of that day, already closed.
    workout: WorkoutBody,
}

#[derive(Debug, Serialize)]
pub(super) struct SetDeletedResponse {
    set: SetBody,
    workout: WorkoutBody,
}

#[derive(Debug, Serialize)]
pub(super) struct WorkoutDoneResponse {
    /// Per exercise, in first-logged order.
    exercises: Vec<SummaryBody>,
}

#[derive(Debug, Serialize)]
struct WorkoutBody {
    session_id: String,
    current_exercise: Option<String>,
    plan_day: Option<PlanDayBody>,
    exercises: Vec<PlannedExerciseBody>,
    /// Oldest first.
    sets: Vec<SetBody>,
}

#[derive(Debug, Serialize)]
struct PlanDayBody {
    weekday: String,
    kind: &'static str,
    notes: String,
}

#[derive(Debug, Serialize)]
struct PlannedExerciseBody {
    name: String,
    sets: u32,
    reps: String,
    target: Option<TargetBody>,
}

#[derive(Debug, Serialize)]
struct TargetBody {
    /// `null` is bodyweight.
    weight_lb: Option<f64>,
    reps: Vec<u32>,
    note: Option<String>,
}

#[derive(Debug, Serialize)]
struct SetBody {
    id: String,
    exercise: String,
    /// `null` is bodyweight.
    weight_lb: Option<f64>,
    reps: u32,
}

#[derive(Debug, Serialize)]
struct SummaryBody {
    name: String,
    sets: Vec<SetRecordBody>,
    next: Option<TargetBody>,
}

#[derive(Debug, Serialize)]
struct SetRecordBody {
    weight_lb: Option<f64>,
    reps: u32,
}

impl From<WorkoutState> for WorkoutBody {
    fn from(state: WorkoutState) -> Self {
        Self {
            session_id: state.session_id,
            current_exercise: state.current_exercise,
            plan_day: state.plan_day.map(|day| PlanDayBody {
                weekday: day.weekday,
                kind: day.kind.as_str(),
                notes: day.notes,
            }),
            exercises: state
                .exercises
                .into_iter()
                .zip(state.targets)
                .map(|(exercise, target)| PlannedExerciseBody {
                    name: exercise.name,
                    sets: exercise.sets,
                    reps: exercise.reps,
                    target: target.map(TargetBody::from),
                })
                .collect(),
            sets: state.sets.into_iter().map(SetBody::from).collect(),
        }
    }
}

impl From<Target> for TargetBody {
    fn from(target: Target) -> Self {
        Self {
            weight_lb: target.weight_lb,
            reps: target.reps,
            note: target.note,
        }
    }
}

impl From<LoggedSet> for SetBody {
    fn from(set: LoggedSet) -> Self {
        Self {
            id: set.id,
            exercise: set.exercise_name,
            weight_lb: set.weight_lb,
            reps: set.reps,
        }
    }
}

impl From<ExerciseSummary> for SummaryBody {
    fn from(summary: ExerciseSummary) -> Self {
        Self {
            name: summary.name,
            sets: summary
                .sets
                .into_iter()
                .map(|(weight_lb, reps)| SetRecordBody { weight_lb, reps })
                .collect(),
            next: summary.next.map(TargetBody::from),
        }
    }
}

/// Server time in the household timezone, as the core wants it: the instant
/// and today's civil date, the same pair Signal's `/log` passes.
fn clock(tz: Tz) -> (DateTime<Utc>, chrono::NaiveDate) {
    let now = Utc::now().with_timezone(&tz);
    (now.with_timezone(&Utc), now.date_naive())
}

/// The caller's member. `Caller` is always a linked DM, so this never fails.
fn member_id(Caller(scope): Caller) -> Result<String, ApiError> {
    scope
        .member_id()
        .map(str::to_string)
        .ok_or(ApiError::MemberRequired)
}

fn rejected(status: StatusCode, message: impl ToString) -> ApiError {
    ApiError::Rejected(status, message.to_string())
}

impl From<WorkoutError> for ApiError {
    fn from(err: WorkoutError) -> Self {
        match err {
            WorkoutError::NoOpenWorkout => rejected(StatusCode::CONFLICT, err),
            WorkoutError::SetNotFound => rejected(StatusCode::NOT_FOUND, err),
            WorkoutError::NoPlannedLifts
            | WorkoutError::ExerciseOutOfRange { .. }
            | WorkoutError::NoCurrentExercise
            | WorkoutError::UnknownLoad { .. }
            | WorkoutError::InvalidSet(_) => rejected(StatusCode::UNPROCESSABLE_ENTITY, err),
            WorkoutError::Database { .. } | WorkoutError::Plan(_) => {
                eprintln!("api_workout failed: {err:?}");
                ApiError::Internal
            }
        }
    }
}

impl From<PerformedAtError> for ApiError {
    fn from(err: PerformedAtError) -> Self {
        rejected(StatusCode::UNPROCESSABLE_ENTITY, err)
    }
}

impl From<JsonRejection> for ApiError {
    fn from(rejection: JsonRejection) -> Self {
        rejected(rejection.status(), rejection.body_text())
    }
}

/// `GET /v1/workout`: today's open workout, or `{"workout": null}`.
pub(super) async fn get_workout(
    caller: Caller,
    State(pool): State<SqlitePool>,
    State(tz): State<Tz>,
) -> Result<Json<WorkoutResponse>, ApiError> {
    let member_id = member_id(caller)?;
    let (now, today) = clock(tz);
    let state = health_coach::workout_state(&pool, &member_id, now, today).await?;
    Ok(Json(WorkoutResponse {
        workout: state.map(WorkoutBody::from),
    }))
}

/// `POST /v1/workout/sets`: log one set, starting today's workout if needed.
pub(super) async fn log_set(
    idempotency: Idempotency,
    caller: Caller,
    State(pool): State<SqlitePool>,
    State(tz): State<Tz>,
    body: Result<Json<LogSetRequest>, JsonRejection>,
) -> Result<Response, ApiError> {
    let member_id = member_id(caller)?;
    let Json(body) = body?;
    Ok(idempotency
        .run(async move {
            let result: Result<_, ApiError> = async {
                let (now, today) = clock(tz);
                let performed_at = body
                    .performed_at
                    .as_deref()
                    .map(health_coach::parse_performed_at)
                    .transpose()?;
                let performed =
                    health_coach::resolve_performed_at(performed_at, now.with_timezone(&tz))?;
                let load = match body.weight {
                    Some(weight) => Load::Weight(WeightUnit::from(body.unit).to_lb(weight)),
                    None => Load::Bodyweight,
                };
                let set = SetInput {
                    load,
                    reps: body.reps,
                    count: 1,
                };
                let logged = health_coach::log_sets(
                    &pool,
                    &member_id,
                    now,
                    today,
                    performed,
                    Some(&body.exercise),
                    &[set],
                )
                .await?;
                let set = logged.logged.into_iter().next().ok_or(ApiError::Internal)?;
                Ok((
                    StatusCode::CREATED,
                    Json(SetLoggedResponse {
                        set: set.into(),
                        workout: logged.state.into(),
                    }),
                ))
            }
            .await;
            result.into_response()
        })
        .await)
}

/// `POST /v1/workout/done`: close today's workout.
pub(super) async fn finish_workout(
    idempotency: Idempotency,
    caller: Caller,
    State(pool): State<SqlitePool>,
    State(tz): State<Tz>,
) -> Result<Response, ApiError> {
    let member_id = member_id(caller)?;
    Ok(idempotency
        .run(async move {
            let (now, today) = clock(tz);
            health_coach::close_workout(&pool, &member_id, now, today)
                .await
                .map(|summary| {
                    Json(WorkoutDoneResponse {
                        exercises: summary.into_iter().map(SummaryBody::from).collect(),
                    })
                })
                .map_err(ApiError::from)
                .into_response()
        })
        .await)
}

/// `DELETE /v1/workout/sets/{id}`: remove a set from the caller's open
/// workout. Another member's set, or one outside the open workout, is 404.
pub(super) async fn delete_set(
    idempotency: Idempotency,
    caller: Caller,
    State(pool): State<SqlitePool>,
    State(tz): State<Tz>,
    Path(set_id): Path<String>,
) -> Result<Response, ApiError> {
    let member_id = member_id(caller)?;
    Ok(idempotency
        .run(async move {
            let (now, today) = clock(tz);
            match health_coach::delete_set(&pool, &member_id, now, today, &set_id).await {
                Ok(deleted) => Json(SetDeletedResponse {
                    set: deleted.set.into(),
                    workout: deleted.state.into(),
                })
                .into_response(),
                Err(WorkoutError::NoOpenWorkout) => {
                    ApiError::from(WorkoutError::SetNotFound).into_response()
                }
                Err(err) => ApiError::from(err).into_response(),
            }
        })
        .await)
}

#[cfg(test)]
mod tests {
    use super::super::idempotency::IDEMPOTENCY_KEY;
    use super::super::tests::{request, set_count, test_auth, test_pool, ALEX_PHONE, KITCHEN_IPAD};
    use super::super::{router, ApiServer, ApiState};
    use chrono::{Duration, FixedOffset, SecondsFormat};
    use chrono_tz::Tz;
    use serde_json::{json, Value};
    use sqlx::SqlitePool;
    use tokio::net::TcpListener;

    struct App {
        server: ApiServer,
        base: String,
        client: reqwest::Client,
    }

    async fn serve(pool: SqlitePool) -> App {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let state = ApiState { pool, tz: Tz::UTC };
        let server = ApiServer::spawn(listener, router(test_auth(), state));
        App {
            server,
            base,
            client: reqwest::Client::new(),
        }
    }

    impl App {
        /// Sends one request; writes get `key` as their `Idempotency-Key`.
        async fn call(
            &self,
            method: reqwest::Method,
            path: &str,
            (token, member): (&str, &str),
            key: Option<&str>,
            body: Option<Value>,
        ) -> (u16, Value) {
            let mut builder = request(
                &self.client,
                method,
                format!("{}{path}", self.base),
                Some(token),
                Some(member),
            );
            if let Some(key) = key {
                builder = builder.header(IDEMPOTENCY_KEY, key);
            }
            if let Some(body) = body {
                builder = builder.json(&body);
            }
            let response = builder.send().await.unwrap();
            let status = response.status().as_u16();
            (status, response.json().await.unwrap())
        }

        async fn workout(&self, caller: (&str, &str)) -> (u16, Value) {
            self.call(reqwest::Method::GET, "/v1/workout", caller, None, None)
                .await
        }

        async fn log(&self, caller: (&str, &str), key: &str, body: Value) -> (u16, Value) {
            self.call(
                reqwest::Method::POST,
                "/v1/workout/sets",
                caller,
                Some(key),
                Some(body),
            )
            .await
        }

        async fn delete(&self, caller: (&str, &str), key: &str, id: &str) -> (u16, Value) {
            let path = format!("/v1/workout/sets/{id}");
            self.call(reqwest::Method::DELETE, &path, caller, Some(key), None)
                .await
        }

        async fn done(&self, key: &str) -> (u16, Value) {
            self.call(
                reqwest::Method::POST,
                "/v1/workout/done",
                ALEX,
                Some(key),
                None,
            )
            .await
        }
    }

    const ALEX: (&str, &str) = (ALEX_PHONE, "alex");
    const IPAD_ALEX: (&str, &str) = (KITCHEN_IPAD, "alex");
    const IPAD_JORDAN: (&str, &str) = (KITCHEN_IPAD, "jordan");

    fn bench(weight: f64) -> Value {
        json!({ "exercise": "bench press", "weight": weight, "unit": "lb", "reps": 8 })
    }

    /// The test server's clock (`Tz::UTC`), as Signal's `/log` would pass it.
    fn signal_clock() -> (chrono::DateTime<chrono::Utc>, chrono::NaiveDate) {
        let now = chrono::Utc::now();
        (now, now.date_naive())
    }

    /// `(exercise, weight_lb, reps)` per set in a `GET /v1/workout` body.
    fn sets(workout: &Value) -> Vec<(String, Value, u64)> {
        workout["workout"]["sets"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| {
                (
                    s["exercise"].as_str().unwrap().to_string(),
                    s["weight_lb"].clone(),
                    s["reps"].as_u64().unwrap(),
                )
            })
            .collect()
    }

    #[tokio::test]
    async fn signal_and_api_log_into_the_same_workout() {
        let (_dir, pool) = test_pool().await;
        let app = serve(pool.clone()).await;

        let (now, today) = signal_clock();
        health_coach::log_command_reply(&pool, "alex", now, today, "bench 135x8")
            .await
            .unwrap();
        let (status, workout) = app.workout(ALEX).await;
        assert_eq!(status, 200, "{workout}");
        assert_eq!(sets(&workout), vec![("Bench".to_string(), json!(135.0), 8)]);

        let squat = json!({ "exercise": "squat", "weight": 60, "unit": "kg", "reps": 5 });
        let (status, logged) = app.log(ALEX, "squat-1", squat).await;
        assert_eq!(status, 201, "{logged}");
        assert_eq!(logged["set"]["exercise"], "Squat");
        assert_eq!(
            logged["workout"]["session_id"],
            workout["workout"]["session_id"]
        );

        // Signal's `/log` status lists both lifts, the kg set converted to lb.
        let (now, today) = signal_clock();
        let status_text = health_coach::log_command_reply(&pool, "alex", now, today, "")
            .await
            .unwrap();
        assert!(
            status_text.starts_with("▶️ Workout resumed"),
            "{status_text}"
        );
        assert!(status_text.contains("Bench  ✓ 135 lb × 8"), "{status_text}");
        assert!(
            status_text.contains("Squat  ✓ 132.3 lb × 5"),
            "{status_text}"
        );

        app.server.shutdown().await;
    }

    #[tokio::test]
    async fn members_cannot_read_write_or_delete_each_others_sets() {
        let (_dir, pool) = test_pool().await;
        let app = serve(pool.clone()).await;

        let (status, logged) = app.log(ALEX, "a-1", bench(135.0)).await;
        assert_eq!(status, 201, "{logged}");
        let alex_set = logged["set"]["id"].as_str().unwrap().to_string();

        // Jordan, on the shared iPad, sees no workout and cannot delete Alex's set.
        assert_eq!(
            app.workout(IPAD_JORDAN).await,
            (200, json!({ "workout": null }))
        );
        let (status, body) = app.delete(IPAD_JORDAN, "j-1", &alex_set).await;
        assert_eq!(status, 404, "{body}");
        assert_eq!(set_count(&pool, "alex").await, 1);

        // Jordan's own set stays Jordan's.
        let (status, jordan) = app.log(IPAD_JORDAN, "j-2", bench(95.0)).await;
        assert_eq!(status, 201, "{jordan}");
        assert_eq!(set_count(&pool, "jordan").await, 1);
        let (_, alex_view) = app.workout(IPAD_ALEX).await;
        assert_eq!(
            sets(&alex_view),
            vec![("Bench press".to_string(), json!(135.0), 8)]
        );
        let jordan_set = jordan["set"]["id"].as_str().unwrap();
        let (status, _) = app.delete(ALEX, "a-2", jordan_set).await;
        assert_eq!(status, 404);
        assert_eq!(set_count(&pool, "jordan").await, 1);

        // Alex's phone may not act as Jordan at all.
        let as_jordan = (ALEX_PHONE, "jordan");
        assert_eq!(app.workout(as_jordan).await.0, 403);
        assert_eq!(app.log(as_jordan, "x-1", bench(95.0)).await.0, 403);
        assert_eq!(app.delete(as_jordan, "x-2", jordan_set).await.0, 403);

        // Alex deletes her own set from either of her devices.
        let (status, deleted) = app.delete(IPAD_ALEX, "a-3", &alex_set).await;
        assert_eq!(status, 200, "{deleted}");
        assert_eq!(deleted["set"]["id"], alex_set.as_str());
        assert_eq!(deleted["workout"]["sets"], json!([]));
        assert_eq!(set_count(&pool, "alex").await, 0);
        assert_eq!(set_count(&pool, "jordan").await, 1);

        app.server.shutdown().await;
    }

    #[tokio::test]
    async fn set_performed_two_hours_ago_lands_at_that_time() {
        let (_dir, pool) = test_pool().await;
        let app = serve(pool.clone()).await;

        let at = chrono::Utc::now() - Duration::hours(2);
        let ist = FixedOffset::east_opt(5 * 3600 + 1800).unwrap();
        let mut body = bench(135.0);
        body["performed_at"] = json!(at
            .with_timezone(&ist)
            .to_rfc3339_opts(SecondsFormat::Secs, false));
        let (status, logged) = app.log(ALEX, "late-1", body).await;
        assert_eq!(status, 201, "{logged}");

        let (created_at, date): (String, String) =
            sqlx::query_as("SELECT created_at, date FROM fitness_set_log WHERE id = ?")
                .bind(logged["set"]["id"].as_str().unwrap())
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(created_at, at.format("%Y-%m-%d %H:%M:%S").to_string());
        // The civil day is in the household timezone (UTC here), not the offset sent.
        assert_eq!(date, at.date_naive().format("%Y-%m-%d").to_string());

        app.server.shutdown().await;
    }

    #[tokio::test]
    async fn bad_performed_at_or_set_fields_are_rejected() {
        let (_dir, pool) = test_pool().await;
        let app = serve(pool.clone()).await;
        let now = chrono::Utc::now();

        for (key, performed_at, error) in [
            (
                "future",
                (now + Duration::minutes(10)).to_rfc3339(),
                "performed_at is more than 5 minutes in the future",
            ),
            (
                "stale",
                (now - Duration::days(8)).to_rfc3339(),
                "performed_at is more than 7 days old",
            ),
            (
                "no-offset",
                now.format("%Y-%m-%dT%H:%M:%S").to_string(),
                "performed_at must be an RFC 3339 timestamp with an offset",
            ),
        ] {
            let mut body = bench(135.0);
            body["performed_at"] = json!(performed_at);
            let response = app.log(ALEX, key, body).await;
            assert_eq!(response, (422, json!({ "error": error })), "{key}");
        }

        let bad_bodies = [
            json!({ "exercise": "bench", "weight": 135, "unit": "stone", "reps": 8 }),
            json!({ "exercise": "bench", "weight": 135, "reps": 8 }),
            json!({ "exercise": "bench", "unit": "lb", "reps": 8 }),
            json!({ "exercise": "bench", "weight": 135, "unit": "lb", "reps": 0 }),
            json!({ "exercise": "bench", "weight": 700, "unit": "kg", "reps": 8 }),
            json!({ "exercise": "  ", "weight": 135, "unit": "lb", "reps": 8 }),
        ];
        for (i, body) in bad_bodies.into_iter().enumerate() {
            let (status, response) = app.log(ALEX, &format!("bad-{i}"), body.clone()).await;
            assert_eq!(status, 422, "{body} -> {response}");
            assert!(response["error"].is_string(), "{response}");
        }
        assert_eq!(set_count(&pool, "alex").await, 0);

        // `weight: null` is bodyweight.
        let pullups = json!({ "exercise": "pull-ups", "weight": null, "unit": "lb", "reps": 6 });
        let (status, logged) = app.log(ALEX, "bw", pullups).await;
        assert_eq!(status, 201, "{logged}");
        assert_eq!(logged["set"]["weight_lb"], Value::Null);

        app.server.shutdown().await;
    }

    #[tokio::test]
    async fn repeated_set_request_logs_one_set() {
        let (_dir, pool) = test_pool().await;
        let app = serve(pool.clone()).await;

        let first = app.log(ALEX, "same-key", bench(135.0)).await;
        assert_eq!(first.0, 201, "{first:?}");
        let retry = app.log(IPAD_ALEX, "same-key", bench(135.0)).await;
        assert_eq!(retry, first);
        assert_eq!(set_count(&pool, "alex").await, 1);

        app.server.shutdown().await;
    }

    #[tokio::test]
    async fn done_closes_the_workout_and_summarizes_it() {
        let (_dir, pool) = test_pool().await;
        let app = serve(pool).await;

        assert_eq!(app.log(ALEX, "s-1", bench(135.0)).await.0, 201);
        assert_eq!(app.log(ALEX, "s-2", bench(140.0)).await.0, 201);
        let (status, summary) = app.done("done-1").await;
        assert_eq!(status, 200, "{summary}");
        assert_eq!(
            summary,
            json!({ "exercises": [{
                "name": "Bench press",
                "sets": [{ "weight_lb": 135.0, "reps": 8 }, { "weight_lb": 140.0, "reps": 8 }],
                "next": null,
            }] })
        );
        assert_eq!(app.workout(ALEX).await, (200, json!({ "workout": null })));
        // A retry replays the summary; a new request finds nothing open.
        assert_eq!(app.done("done-1").await, (200, summary));
        assert_eq!(
            app.done("done-2").await,
            (409, json!({ "error": "no workout in progress" }))
        );

        app.server.shutdown().await;
    }
}
