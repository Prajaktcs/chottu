use reqwest::Client;
use serde::{Deserialize, Serialize};

/// OAuth scopes for Google Health read + nutrition write (Telegram `/food` push).
pub const GOOGLE_HEALTH_OAUTH_SCOPES: &str = concat!(
    "https://www.googleapis.com/auth/googlehealth.health_metrics_and_measurements.readonly ",
    "https://www.googleapis.com/auth/googlehealth.nutrition.readonly ",
    "https://www.googleapis.com/auth/googlehealth.nutrition.writeonly ",
    "https://www.googleapis.com/auth/googlehealth.sleep.readonly ",
    "https://www.googleapis.com/auth/googlehealth.activity_and_fitness.readonly"
);

#[derive(Debug, Clone)]
pub struct GoogleHealthClient {
    client_id: String,
    client_secret: String,
    refresh_token: String,
}

/// One Google Health exercise / activity session with structured fields.
#[derive(Debug, Clone, PartialEq)]
pub struct ExerciseSession {
    pub activity_type: String,
    pub duration_minutes: i32,
    /// Active energy burned for the session; 0 when the API omits calories.
    pub active_calories: f64,
    pub start_at: Option<String>,
    pub end_at: Option<String>,
}

impl ExerciseSession {
    /// Human-readable blurb, e.g. `Strength Training (45 mins, 240 kcal)`.
    pub fn display(&self) -> String {
        if self.active_calories > 0.0 {
            format!(
                "{} ({} mins, {:.0} kcal)",
                self.activity_type, self.duration_minutes, self.active_calories
            )
        } else {
            format!("{} ({} mins)", self.activity_type, self.duration_minutes)
        }
    }
}

/// Parse `dataPoints` from a Google Health exercise reconcile response.
pub fn parse_exercise_data_points(data: &serde_json::Value) -> Vec<ExerciseSession> {
    let mut exercises = Vec::new();
    let Some(points) = data.get("dataPoints").and_then(|v| v.as_array()) else {
        return exercises;
    };
    for point in points {
        let Some(exercise_obj) = point.get("exercise") else {
            continue;
        };
        let activity_type = exercise_obj
            .get("activityType")
            .or_else(|| exercise_obj.get("exerciseType"))
            .and_then(|v| v.as_str())
            .unwrap_or("Workout")
            .to_string();

        let calories = exercise_obj
            .get("calories")
            .and_then(|v| v.as_f64())
            .or_else(|| {
                exercise_obj
                    .get("calories")
                    .and_then(|v| v.as_str())
                    .and_then(|s| s.parse::<f64>().ok())
            })
            .unwrap_or(0.0);

        let start_str = point
            .get("startTime")
            .and_then(|v| v.as_str())
            .or_else(|| {
                point
                    .get("interval")
                    .and_then(|i| i.get("startTime"))
                    .and_then(|v| v.as_str())
            })
            .or_else(|| {
                exercise_obj
                    .get("interval")
                    .and_then(|i| i.get("startTime"))
                    .and_then(|v| v.as_str())
            })
            .or_else(|| exercise_obj.get("startTime").and_then(|v| v.as_str()));
        let end_str = point
            .get("endTime")
            .and_then(|v| v.as_str())
            .or_else(|| {
                point
                    .get("interval")
                    .and_then(|i| i.get("endTime"))
                    .and_then(|v| v.as_str())
            })
            .or_else(|| {
                exercise_obj
                    .get("interval")
                    .and_then(|i| i.get("endTime"))
                    .and_then(|v| v.as_str())
            })
            .or_else(|| exercise_obj.get("endTime").and_then(|v| v.as_str()));

        let mut duration_mins = 0_i32;
        if let (Some(s_str), Some(e_str)) = (start_str, end_str) {
            if let (Ok(s_dt), Ok(e_dt)) = (
                chrono::DateTime::parse_from_rfc3339(s_str),
                chrono::DateTime::parse_from_rfc3339(e_str),
            ) {
                duration_mins = e_dt.signed_duration_since(s_dt).num_minutes() as i32;
            }
        }

        exercises.push(ExerciseSession {
            activity_type,
            duration_minutes: duration_mins,
            active_calories: calories,
            start_at: start_str.map(|s| s.to_string()),
            end_at: end_str.map(|s| s.to_string()),
        });
    }
    exercises
}

/// Anonymous nutrition-log payload for `dataPoints.create`.
#[derive(Debug, Clone)]
pub struct NutritionLogWrite {
    pub food_display_name: String,
    pub start_time: chrono::DateTime<chrono::Utc>,
    pub end_time: chrono::DateTime<chrono::Utc>,
    pub calories_kcal: f64,
    pub carbs_g: f64,
    pub fat_g: f64,
    pub protein_g: f64,
    pub cholesterol_mg: f64,
    pub saturated_fat_g: f64,
    pub unsaturated_fat_g: f64,
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
}

fn mg_to_grams(mg: f64) -> f64 {
    mg / 1_000.0
}

fn mcg_to_grams(mcg: f64) -> f64 {
    mcg / 1_000_000.0
}

/// Format an instant as RFC3339 UTC (`…Z`) for Google Health `startTime`/`endTime`.
fn rfc3339_utc(ts: chrono::DateTime<chrono::Utc>) -> String {
    ts.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// Local UTC offset as a Google `Duration` string (e.g. `"-14400s"` for EDT).
///
/// Google Health derives `civilStartTime` from `startTime` + `startUtcOffset`.
/// Sending only an offset-bearing RFC3339 timestamp is *not* enough — Google
/// normalizes to UTC with `startUtcOffset: "0s"`, so evening meals in the
/// Americas land on the next civil day. Explicit offset fields fix that.
fn utc_offset_duration_str(ts: chrono::DateTime<chrono::Utc>) -> String {
    let secs = ts.with_timezone(&chrono::Local).offset().local_minus_utc();
    format!("{secs}s")
}

/// Google Health MealType only allows BREAKFAST / LUNCH / DINNER / SNACK.
fn meal_type_for_timestamp(ts: chrono::DateTime<chrono::Utc>) -> &'static str {
    use chrono::Timelike;
    let hour = ts.with_timezone(&chrono::Local).hour();
    match hour {
        5..=10 => "BREAKFAST",
        11..=14 => "LUNCH",
        17..=21 => "DINNER",
        _ => "SNACK",
    }
}

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq)]
pub struct GoogleHealthFoodSummary {
    pub calories: f64,
    pub carbs: f64,
    pub fat: f64,
    pub protein: f64,
    pub cholesterol: f64,
    pub saturated_fat: f64,
    pub unsaturated_fat: f64,
    pub iron: f64,
    pub vitamin_b: f64,
    pub vitamin_c: f64,
    pub sugar: f64,
    pub fiber: f64,
    pub sodium: f64,
    pub potassium: f64,
    pub calcium: f64,
    pub magnesium: f64,
    pub zinc: f64,
    pub vitamin_a: f64,
    pub vitamin_d: f64,
    pub vitamin_e: f64,
    pub vitamin_k: f64,
    pub caffeine: f64,
    pub trans_fat: f64,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct GoogleHealthFoodLogResponse {
    pub summary: GoogleHealthFoodSummary,
}

/// Google Health nutrient quantities are always in grams. Convert to milligrams.
fn grams_to_mg(grams: f64) -> f64 {
    grams * 1_000.0
}

/// Google Health nutrient quantities are always in grams. Convert to micrograms.
fn grams_to_mcg(grams: f64) -> f64 {
    grams * 1_000_000.0
}

// Structs for deserializing Google Health API v4 responses
#[derive(Deserialize, Debug)]
struct GoogleHealthWeightQuantity {
    #[serde(rename = "gramsSum")]
    grams_sum: Option<f64>,
}

#[derive(Deserialize, Debug)]
struct GoogleHealthEnergyQuantity {
    #[serde(rename = "kcalSum")]
    kcal_sum: Option<f64>,
}

#[derive(Deserialize, Debug)]
struct GoogleHealthNutrientRollup {
    nutrient: String,
    quantity: Option<GoogleHealthWeightQuantity>,
}

#[derive(Deserialize, Debug)]
struct GoogleHealthNutritionLogRollup {
    energy: Option<GoogleHealthEnergyQuantity>,
    #[serde(rename = "totalCarbohydrate")]
    total_carbohydrate: Option<GoogleHealthWeightQuantity>,
    #[serde(rename = "totalFat")]
    total_fat: Option<GoogleHealthWeightQuantity>,
    nutrients: Option<Vec<GoogleHealthNutrientRollup>>,
}

#[derive(Deserialize, Debug)]
struct GoogleHealthRollupDataPoint {
    #[serde(rename = "nutritionLog")]
    nutrition_log: Option<GoogleHealthNutritionLogRollup>,
}

#[derive(Deserialize, Debug)]
struct GoogleHealthDailyRollupResponse {
    #[serde(rename = "rollupDataPoints")]
    rollup_data_points: Option<Vec<GoogleHealthRollupDataPoint>>,
}

impl GoogleHealthClient {
    pub fn new(client_id: String, client_secret: String, refresh_token: String) -> Self {
        Self {
            client_id,
            client_secret,
            refresh_token,
        }
    }

    fn oauth_credentials(&self) -> (String, String, String) {
        let client_id =
            std::env::var("FITBIT_CLIENT_ID").unwrap_or_else(|_| self.client_id.clone());
        let client_secret =
            std::env::var("FITBIT_CLIENT_SECRET").unwrap_or_else(|_| self.client_secret.clone());
        // Always use the token this client was constructed with so per-member
        // clients are not overwritten by a shared FITBIT_REFRESH_TOKEN env var.
        (client_id, client_secret, self.refresh_token.clone())
    }

    async fn access_token(&self) -> Result<String, anyhow::Error> {
        let (client_id, client_secret, refresh_token) = self.oauth_credentials();
        let token_res =
            crate::oauth::refresh_oauth2_token(&client_id, &client_secret, &refresh_token).await?;
        Ok(token_res.access_token)
    }

    /// Helper to query dailyRollUp endpoint for a given data type
    async fn query_daily_rollup(
        &self,
        access_token: &str,
        data_type: &str,
        date: &str,
    ) -> Result<serde_json::Value, anyhow::Error> {
        use chrono::Datelike;

        let start_date = chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d")?;
        let end_date = start_date + chrono::Duration::days(1);

        let request_body = serde_json::json!({
            "range": {
                "start": {
                    "date": {
                        "year": start_date.year(),
                        "month": start_date.month(),
                        "day": start_date.day()
                    }
                },
                "end": {
                    "date": {
                        "year": end_date.year(),
                        "month": end_date.month(),
                        "day": end_date.day()
                    }
                }
            },
            "windowSizeDays": 1
        });

        let client = Client::new();
        let url = format!(
            "https://health.googleapis.com/v4/users/me/dataTypes/{}/dataPoints:dailyRollUp",
            data_type
        );
        let response = client
            .post(&url)
            .bearer_auth(access_token)
            .json(&request_body)
            .send()
            .await?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(anyhow::anyhow!(
                "Google Health API dailyRollUp failed for data type '{}': status {}, body: {}",
                data_type,
                status,
                body
            ));
        }

        let data = response.json::<serde_json::Value>().await?;
        Ok(data)
    }

    /// Helper to query reconcile endpoint for a given data type
    async fn query_reconcile(
        &self,
        access_token: &str,
        data_type: &str,
        date: &str,
    ) -> Result<serde_json::Value, anyhow::Error> {
        let start_date = chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d")?;
        let end_date = start_date + chrono::Duration::days(1);
        let start_date_str = start_date.format("%Y-%m-%d").to_string();
        let end_date_str = end_date.format("%Y-%m-%d").to_string();

        let filter_str = format!(
            "{}.interval.civil_end_time >= \"{}\" AND {}.interval.civil_end_time < \"{}\"",
            data_type.replace('-', "_"),
            start_date_str,
            data_type.replace('-', "_"),
            end_date_str
        );

        let client = Client::new();
        let url = format!(
            "https://health.googleapis.com/v4/users/me/dataTypes/{}/dataPoints:reconcile",
            data_type
        );
        let response = client
            .get(&url)
            .bearer_auth(access_token)
            .query(&[("filter", &filter_str)])
            .send()
            .await?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(anyhow::anyhow!(
                "Google Health API reconcile failed for data type '{}': status {}, body: {}",
                data_type,
                status,
                body
            ));
        }

        let data = response.json::<serde_json::Value>().await?;
        Ok(data)
    }

    /// Fetches the nutrition summary from the Google Health API for a specific date (format: YYYY-MM-DD).
    pub async fn fetch_nutrition_summary(
        &self,
        date: &str,
    ) -> Result<GoogleHealthFoodSummary, anyhow::Error> {
        let access_token = self.access_token().await?;
        let data_val = self
            .query_daily_rollup(&access_token, "nutrition-log", date)
            .await?;
        let data: GoogleHealthDailyRollupResponse = serde_json::from_value(data_val)?;

        // Map rollup data points to summary
        let mut calories = 0.0;
        let mut carbs = 0.0;
        let mut fat = 0.0;
        let mut protein = 0.0;
        let mut cholesterol = 0.0;
        let mut saturated_fat = 0.0;
        let mut unsaturated_fat = 0.0;
        let mut iron = 0.0;
        let mut vitamin_b = 0.0;
        let mut vitamin_c = 0.0;
        let mut sugar = 0.0;
        let mut fiber = 0.0;
        let mut sodium = 0.0;
        let mut potassium = 0.0;
        let mut calcium = 0.0;
        let mut magnesium = 0.0;
        let mut zinc = 0.0;
        let mut vitamin_a = 0.0;
        let mut vitamin_d = 0.0;
        let mut vitamin_e = 0.0;
        let mut vitamin_k = 0.0;
        let mut caffeine = 0.0;
        let mut trans_fat = 0.0;

        if let Some(points) = data.rollup_data_points {
            if let Some(first_point) = points.first() {
                if let Some(log) = &first_point.nutrition_log {
                    if let Some(energy) = &log.energy {
                        calories = energy.kcal_sum.unwrap_or(0.0);
                    }
                    if let Some(carbohydrate) = &log.total_carbohydrate {
                        carbs = carbohydrate.grams_sum.unwrap_or(0.0);
                    }
                    if let Some(total_fat) = &log.total_fat {
                        fat = total_fat.grams_sum.unwrap_or(0.0);
                    }
                    if let Some(nutrients) = &log.nutrients {
                        for nutrient_rollup in nutrients {
                            // Google Health always reports nutrient mass in grams.
                            // Convert into the units used by health_family_summary / /food.
                            let grams = nutrient_rollup
                                .quantity
                                .as_ref()
                                .and_then(|q| q.grams_sum)
                                .unwrap_or(0.0);
                            match nutrient_rollup.nutrient.as_str() {
                                "PROTEIN" => protein = grams,
                                "SATURATED_FAT" => saturated_fat = grams,
                                "UNSATURATED_FAT" => unsaturated_fat = grams,
                                "SUGAR" => sugar = grams,
                                "DIETARY_FIBER" => fiber = grams,
                                "TRANS_FAT" => trans_fat = grams,
                                "CHOLESTEROL" => cholesterol = grams_to_mg(grams),
                                "IRON" => iron = grams_to_mg(grams),
                                "VITAMIN_C" => vitamin_c = grams_to_mg(grams),
                                "SODIUM" => sodium = grams_to_mg(grams),
                                "POTASSIUM" => potassium = grams_to_mg(grams),
                                "CALCIUM" => calcium = grams_to_mg(grams),
                                "MAGNESIUM" => magnesium = grams_to_mg(grams),
                                "ZINC" => zinc = grams_to_mg(grams),
                                "CAFFEINE" => caffeine = grams_to_mg(grams),
                                "VITAMIN_E" => vitamin_e = grams_to_mg(grams),
                                // Store B6/B12 mass in mg (shared vitamin_b_mg column).
                                "VITAMIN_B6" | "VITAMIN_B12" => vitamin_b += grams_to_mg(grams),
                                "VITAMIN_A" => vitamin_a = grams_to_mcg(grams),
                                "VITAMIN_D" => vitamin_d = grams_to_mcg(grams),
                                "VITAMIN_K" => vitamin_k = grams_to_mcg(grams),
                                _ => {}
                            }
                        }
                    }
                }
            }
        }

        Ok(GoogleHealthFoodSummary {
            calories,
            carbs,
            fat,
            protein,
            cholesterol,
            saturated_fat,
            unsaturated_fat,
            iron,
            vitamin_b,
            vitamin_c,
            sugar,
            fiber,
            sodium,
            potassium,
            calcium,
            magnesium,
            zinc,
            vitamin_a,
            vitamin_d,
            vitamin_e,
            vitamin_k,
            caffeine,
            trans_fat,
        })
    }

    /// Fetches the steps count from the Google Health API for a specific date (format: YYYY-MM-DD).
    pub async fn fetch_steps_summary(&self, date: &str) -> Result<i32, anyhow::Error> {
        let access_token = self.access_token().await?;
        let data = self.query_daily_rollup(&access_token, "steps", date).await?;

        let mut steps = 0;
        if let Some(points) = data.get("rollupDataPoints").and_then(|v| v.as_array()) {
            if let Some(first_point) = points.first() {
                if let Some(steps_obj) = first_point.get("steps") {
                    if let Some(count_sum_val) = steps_obj.get("countSum") {
                        if let Some(s) = count_sum_val.as_str() {
                            steps = s.parse::<i32>().unwrap_or(0);
                        } else if let Some(n) = count_sum_val.as_i64() {
                            steps = n as i32;
                        }
                    }
                }
            }
        }
        Ok(steps)
    }

    /// Fetches the active calories burned from the Google Health API for a specific date (format: YYYY-MM-DD).
    pub async fn fetch_active_energy_summary(&self, date: &str) -> Result<i32, anyhow::Error> {
        let access_token = self.access_token().await?;
        let data = self
            .query_daily_rollup(&access_token, "active-energy-burned", date)
            .await?;

        let mut active_calories = 0;
        if let Some(points) = data.get("rollupDataPoints").and_then(|v| v.as_array()) {
            if let Some(first_point) = points.first() {
                if let Some(energy_obj) = first_point.get("activeEnergyBurned") {
                    if let Some(val) = energy_obj.get("kcalSum").or_else(|| energy_obj.get("kcal")) {
                        if let Some(n) = val.as_f64() {
                            active_calories = n as i32;
                        } else if let Some(s) = val.as_str() {
                            active_calories = s.parse::<f64>().unwrap_or(0.0) as i32;
                        }
                    }
                }
            }
        }
        Ok(active_calories)
    }

    /// Fetches sleep duration in hours from the Google Health API for a specific date (format: YYYY-MM-DD).
    pub async fn fetch_sleep_summary(&self, date: &str) -> Result<f64, anyhow::Error> {
        let access_token = self.access_token().await?;
        let data = self.query_reconcile(&access_token, "sleep", date).await?;

        let mut total_duration_secs = 0.0;
        if let Some(points) = data.get("dataPoints").and_then(|v| v.as_array()) {
            for point in points {
                if let Some(sleep_obj) = point.get("sleep") {
                    let start_str = sleep_obj.get("interval").and_then(|i| i.get("startTime")).and_then(|t| t.as_str())
                        .or_else(|| sleep_obj.get("startTime").and_then(|t| t.as_str()))
                        .or_else(|| point.get("startTime").and_then(|t| t.as_str()));
                    let end_str = sleep_obj.get("interval").and_then(|i| i.get("endTime")).and_then(|t| t.as_str())
                        .or_else(|| sleep_obj.get("endTime").and_then(|t| t.as_str()))
                        .or_else(|| point.get("endTime").and_then(|t| t.as_str()));
                    if let (Some(s_str), Some(e_str)) = (start_str, end_str) {
                        if let (Ok(s_dt), Ok(e_dt)) = (chrono::DateTime::parse_from_rfc3339(s_str), chrono::DateTime::parse_from_rfc3339(e_str)) {
                            let dur = e_dt.signed_duration_since(s_dt);
                            total_duration_secs += dur.num_seconds() as f64;
                        }
                    }
                }
            }
        }
        let hours = total_duration_secs / 3600.0;
        Ok(hours)
    }

    /// Fetches structured exercise sessions from Google Health for a date (YYYY-MM-DD).
    pub async fn fetch_exercise_sessions(
        &self,
        date: &str,
    ) -> Result<Vec<ExerciseSession>, anyhow::Error> {
        let access_token = self.access_token().await?;
        let data = self.query_reconcile(&access_token, "exercise", date).await?;
        Ok(parse_exercise_data_points(&data))
    }

    /// Display strings for exercise sessions (compat wrapper).
    pub async fn fetch_exercise_summary(&self, date: &str) -> Result<Vec<String>, anyhow::Error> {
        Ok(self
            .fetch_exercise_sessions(date)
            .await?
            .into_iter()
            .map(|s| s.display())
            .collect())
    }

    /// Creates an anonymous nutrition-log data point. Returns the full resource `name`.
    pub async fn create_nutrition_log(
        &self,
        entry: &NutritionLogWrite,
    ) -> Result<String, anyhow::Error> {
        let access_token = self.access_token().await?;

        let mut nutrients = Vec::new();
        let mut push_g = |name: &str, grams: f64| {
            if grams.abs() > f64::EPSILON {
                nutrients.push(serde_json::json!({
                    "nutrient": name,
                    "quantity": { "grams": grams }
                }));
            }
        };

        push_g("PROTEIN", entry.protein_g);
        push_g("SATURATED_FAT", entry.saturated_fat_g);
        push_g("UNSATURATED_FAT", entry.unsaturated_fat_g);
        push_g("SUGAR", entry.sugar_g);
        push_g("DIETARY_FIBER", entry.fiber_g);
        push_g("TRANS_FAT", entry.trans_fat_g);
        push_g("CHOLESTEROL", mg_to_grams(entry.cholesterol_mg));
        push_g("IRON", mg_to_grams(entry.iron_mg));
        push_g("VITAMIN_C", mg_to_grams(entry.vitamin_c_mg));
        push_g("SODIUM", mg_to_grams(entry.sodium_mg));
        push_g("POTASSIUM", mg_to_grams(entry.potassium_mg));
        push_g("CALCIUM", mg_to_grams(entry.calcium_mg));
        push_g("MAGNESIUM", mg_to_grams(entry.magnesium_mg));
        push_g("ZINC", mg_to_grams(entry.zinc_mg));
        push_g("CAFFEINE", mg_to_grams(entry.caffeine_mg));
        push_g("VITAMIN_E", mg_to_grams(entry.vitamin_e_mg));
        // Combined B vitamins are stored as a single mg total locally.
        push_g("VITAMIN_B6", mg_to_grams(entry.vitamin_b_mg));
        push_g("VITAMIN_A", mcg_to_grams(entry.vitamin_a_mcg));
        push_g("VITAMIN_D", mcg_to_grams(entry.vitamin_d_mcg));
        push_g("VITAMIN_K", mcg_to_grams(entry.vitamin_k_mcg));

        let body = serde_json::json!({
            "nutritionLog": {
                "interval": {
                    "startTime": rfc3339_utc(entry.start_time),
                    "endTime": rfc3339_utc(entry.end_time),
                    "startUtcOffset": utc_offset_duration_str(entry.start_time),
                    "endUtcOffset": utc_offset_duration_str(entry.end_time)
                },
                "foodDisplayName": entry.food_display_name,
                "mealType": meal_type_for_timestamp(entry.start_time),
                "energy": { "kcal": entry.calories_kcal },
                "totalCarbohydrate": { "grams": entry.carbs_g },
                "totalFat": { "grams": entry.fat_g },
                "nutrients": nutrients,
                "serving": { "amount": 1.0 }
            }
        });

        let client = Client::new();
        let url =
            "https://health.googleapis.com/v4/users/me/dataTypes/nutrition-log/dataPoints";
        let response = client
            .post(url)
            .bearer_auth(&access_token)
            .json(&body)
            .send()
            .await?;

        let status = response.status();
        let data = response.json::<serde_json::Value>().await.unwrap_or_default();
        if !status.is_success() {
            return Err(anyhow::anyhow!(
                "Google Health create nutrition-log failed: status {}, body: {}",
                status,
                data
            ));
        }

        // create returns a long-running Operation; when done, response.name is the DataPoint.
        let name = data
            .get("response")
            .and_then(|r| r.get("name"))
            .and_then(|n| n.as_str())
            .or_else(|| data.get("name").and_then(|n| n.as_str()))
            .map(|s| s.to_string());

        match name {
            Some(n) if !n.is_empty() => Ok(n),
            _ => Err(anyhow::anyhow!(
                "Google Health create nutrition-log succeeded but returned no data point name: {}",
                data
            )),
        }
    }

    /// Deletes one or more nutrition-log data points by full resource name.
    pub async fn batch_delete_nutrition_logs(
        &self,
        names: &[String],
    ) -> Result<(), anyhow::Error> {
        if names.is_empty() {
            return Ok(());
        }

        let access_token = self.access_token().await?;
        let client = Client::new();
        let url =
            "https://health.googleapis.com/v4/users/me/dataTypes/nutrition-log/dataPoints:batchDelete";
        let body = serde_json::json!({ "names": names });
        let response = client
            .post(url)
            .bearer_auth(&access_token)
            .json(&body)
            .send()
            .await?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(anyhow::anyhow!(
                "Google Health batchDelete nutrition-log failed: status {}, body: {}",
                status,
                body
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_deserialize_google_health_response() {
        let raw_json = r#"{
            "rollupDataPoints": [
                {
                    "nutritionLog": {
                        "energy": {
                            "kcalSum": 2150.5
                        },
                        "totalCarbohydrate": {
                            "gramsSum": 245.0
                        },
                        "totalFat": {
                            "gramsSum": 72.3
                        },
                        "nutrients": [
                            {
                                "nutrient": "PROTEIN",
                                "quantity": {
                                    "gramsSum": 88.0
                                }
                            }
                        ]
                    }
                }
            ]
        }"#;

        let parsed: Result<GoogleHealthDailyRollupResponse, _> = serde_json::from_str(raw_json);
        assert!(parsed.is_ok());
        let res = parsed.unwrap();
        let points = res.rollup_data_points.unwrap();
        assert_eq!(points.len(), 1);
        let log = points[0].nutrition_log.as_ref().unwrap();
        assert_eq!(log.energy.as_ref().unwrap().kcal_sum, Some(2150.5));
        assert_eq!(
            log.total_carbohydrate.as_ref().unwrap().grams_sum,
            Some(245.0)
        );
        assert_eq!(log.total_fat.as_ref().unwrap().grams_sum, Some(72.3));

        let p = log
            .nutrients
            .as_ref()
            .unwrap()
            .iter()
            .find(|n| n.nutrient == "PROTEIN")
            .unwrap();
        assert_eq!(p.quantity.as_ref().unwrap().grams_sum, Some(88.0));
    }

    #[test]
    fn test_nutrient_unit_conversion_from_grams() {
        // Docs example: 74 mg sodium is logged as 0.074 grams.
        assert!((grams_to_mg(0.074) - 74.0).abs() < f64::EPSILON);
        assert!((grams_to_mg(0.018) - 18.0).abs() < f64::EPSILON); // iron
        // Vitamin A ~900 mcg RDA ≈ 0.0009 g
        assert!((grams_to_mcg(0.0009) - 900.0).abs() < f64::EPSILON);
        // Macro fats stay in grams (no conversion helper needed)
        assert!((grams_to_mg(1.0) - 1000.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_parse_nutrition_summary_converts_micros_from_grams() {
        let raw_json = r#"{
            "rollupDataPoints": [
                {
                    "nutritionLog": {
                        "energy": { "kcalSum": 500.0 },
                        "totalCarbohydrate": { "gramsSum": 40.0 },
                        "totalFat": { "gramsSum": 20.0 },
                        "nutrients": [
                            { "nutrient": "PROTEIN", "quantity": { "gramsSum": 30.0 } },
                            { "nutrient": "SODIUM", "quantity": { "gramsSum": 0.074 } },
                            { "nutrient": "IRON", "quantity": { "gramsSum": 0.008 } },
                            { "nutrient": "CHOLESTEROL", "quantity": { "gramsSum": 0.2 } },
                            { "nutrient": "VITAMIN_A", "quantity": { "gramsSum": 0.0005 } },
                            { "nutrient": "VITAMIN_D", "quantity": { "gramsSum": 0.00001 } },
                            { "nutrient": "SATURATED_FAT", "quantity": { "gramsSum": 5.0 } },
                            { "nutrient": "SUGAR", "quantity": { "gramsSum": 12.0 } }
                        ]
                    }
                }
            ]
        }"#;
        let data: GoogleHealthDailyRollupResponse = serde_json::from_str(raw_json).unwrap();
        let log = data
            .rollup_data_points
            .unwrap()
            .into_iter()
            .next()
            .unwrap()
            .nutrition_log
            .unwrap();

        let mut protein = 0.0;
        let mut sodium = 0.0;
        let mut iron = 0.0;
        let mut cholesterol = 0.0;
        let mut vitamin_a = 0.0;
        let mut vitamin_d = 0.0;
        let mut saturated_fat = 0.0;
        let mut sugar = 0.0;
        for nutrient_rollup in log.nutrients.unwrap() {
            let grams = nutrient_rollup
                .quantity
                .as_ref()
                .and_then(|q| q.grams_sum)
                .unwrap_or(0.0);
            match nutrient_rollup.nutrient.as_str() {
                "PROTEIN" => protein = grams,
                "SODIUM" => sodium = grams_to_mg(grams),
                "IRON" => iron = grams_to_mg(grams),
                "CHOLESTEROL" => cholesterol = grams_to_mg(grams),
                "VITAMIN_A" => vitamin_a = grams_to_mcg(grams),
                "VITAMIN_D" => vitamin_d = grams_to_mcg(grams),
                "SATURATED_FAT" => saturated_fat = grams,
                "SUGAR" => sugar = grams,
                _ => {}
            }
        }
        assert_eq!(protein, 30.0);
        assert!((sodium - 74.0).abs() < f64::EPSILON);
        assert!((iron - 8.0).abs() < f64::EPSILON);
        assert!((cholesterol - 200.0).abs() < f64::EPSILON);
        assert!((vitamin_a - 500.0).abs() < f64::EPSILON);
        assert!((vitamin_d - 10.0).abs() < f64::EPSILON);
        assert_eq!(saturated_fat, 5.0);
        assert_eq!(sugar, 12.0);
    }

    #[test]
    fn test_parse_steps_daily_rollup() {
        let raw_json = r#"{
            "rollupDataPoints": [
                {
                    "steps": {
                        "countSum": "8250"
                    }
                }
            ]
        }"#;
        let data: serde_json::Value = serde_json::from_str(raw_json).unwrap();
        let mut steps = 0;
        if let Some(points) = data.get("rollupDataPoints").and_then(|v| v.as_array()) {
            if let Some(first_point) = points.first() {
                if let Some(steps_obj) = first_point.get("steps") {
                    if let Some(count_sum_val) = steps_obj.get("countSum") {
                        if let Some(s) = count_sum_val.as_str() {
                            steps = s.parse::<i32>().unwrap_or(0);
                        } else if let Some(n) = count_sum_val.as_i64() {
                            steps = n as i32;
                        }
                    }
                }
            }
        }
        assert_eq!(steps, 8250);
    }

    #[test]
    fn test_parse_active_energy_daily_rollup() {
        let raw_json = r#"{
            "rollupDataPoints": [
                {
                    "activeEnergyBurned": {
                        "kcalSum": 345.5
                    }
                }
            ]
        }"#;
        let data: serde_json::Value = serde_json::from_str(raw_json).unwrap();
        let mut active_calories = 0;
        if let Some(points) = data.get("rollupDataPoints").and_then(|v| v.as_array()) {
            if let Some(first_point) = points.first() {
                if let Some(energy_obj) = first_point.get("activeEnergyBurned") {
                    if let Some(val) = energy_obj.get("kcalSum").or_else(|| energy_obj.get("kcal")) {
                        if let Some(n) = val.as_f64() {
                            active_calories = n as i32;
                        } else if let Some(s) = val.as_str() {
                            active_calories = s.parse::<f64>().unwrap_or(0.0) as i32;
                        }
                    }
                }
            }
        }
        assert_eq!(active_calories, 345);
    }

    #[test]
    fn test_parse_sleep_reconcile() {
        let raw_json = r#"{
            "dataPoints": [
                {
                    "sleep": {
                        "interval": {
                            "startTime": "2026-06-20T22:30:00Z",
                            "endTime": "2026-06-21T06:15:00Z"
                        }
                    }
                }
            ]
        }"#;
        let data: serde_json::Value = serde_json::from_str(raw_json).unwrap();
        let mut total_duration_secs = 0.0;
        if let Some(points) = data.get("dataPoints").and_then(|v| v.as_array()) {
            for point in points {
                if let Some(sleep_obj) = point.get("sleep") {
                    let start_str = sleep_obj.get("interval").and_then(|i| i.get("startTime")).and_then(|t| t.as_str())
                        .or_else(|| sleep_obj.get("startTime").and_then(|t| t.as_str()))
                        .or_else(|| point.get("startTime").and_then(|t| t.as_str()));
                    let end_str = sleep_obj.get("interval").and_then(|i| i.get("endTime")).and_then(|t| t.as_str())
                        .or_else(|| sleep_obj.get("endTime").and_then(|t| t.as_str()))
                        .or_else(|| point.get("endTime").and_then(|t| t.as_str()));
                    if let (Some(s_str), Some(e_str)) = (start_str, end_str) {
                        if let (Ok(s_dt), Ok(e_dt)) = (chrono::DateTime::parse_from_rfc3339(s_str), chrono::DateTime::parse_from_rfc3339(e_str)) {
                            let dur = e_dt.signed_duration_since(s_dt);
                            total_duration_secs += dur.num_seconds() as f64;
                        }
                    }
                }
            }
        }
        let hours = total_duration_secs / 3600.0;
        assert_eq!(hours, 7.75);
    }

    #[test]
    fn test_parse_exercise_reconcile() {
        let raw_json = r#"{
            "dataPoints": [
                {
                    "exercise": {
                        "activityType": "Strength Training",
                        "calories": 240.0,
                        "interval": {
                            "startTime": "2026-06-21T08:00:00Z",
                            "endTime": "2026-06-21T08:45:00Z"
                        }
                    }
                },
                {
                    "exercise": {
                        "activityType": "Running",
                        "calories": 300.0,
                        "interval": {
                            "startTime": "2026-06-21T17:00:00Z",
                            "endTime": "2026-06-21T17:30:00Z"
                        }
                    }
                }
            ]
        }"#;
        let data: serde_json::Value = serde_json::from_str(raw_json).unwrap();
        let exercises = parse_exercise_data_points(&data);
        assert_eq!(exercises.len(), 2);
        assert_eq!(exercises[0].activity_type, "Strength Training");
        assert_eq!(exercises[0].duration_minutes, 45);
        assert!((exercises[0].active_calories - 240.0).abs() < f64::EPSILON);
        assert_eq!(
            exercises[0].start_at.as_deref(),
            Some("2026-06-21T08:00:00Z")
        );
        assert_eq!(
            exercises[0].display(),
            "Strength Training (45 mins, 240 kcal)"
        );
        assert_eq!(exercises[1].activity_type, "Running");
        assert_eq!(exercises[1].duration_minutes, 30);
    }

    #[test]
    fn test_write_unit_helpers_roundtrip() {
        assert!((mg_to_grams(2.0) - 0.002).abs() < 1e-12);
        assert!((mcg_to_grams(500.0) * 1_000_000.0 - 500.0).abs() < 1e-9);
        assert!((grams_to_mg(mg_to_grams(12.5)) - 12.5).abs() < 1e-9);
        assert!((grams_to_mcg(mcg_to_grams(800.0)) - 800.0).abs() < 1e-6);
    }

    #[test]
    fn test_meal_type_for_timestamp() {
        use chrono::{Local, TimeZone};
        let breakfast = Local
            .with_ymd_and_hms(2026, 8, 2, 8, 0, 0)
            .unwrap()
            .with_timezone(&chrono::Utc);
        let lunch = Local
            .with_ymd_and_hms(2026, 8, 2, 12, 30, 0)
            .unwrap()
            .with_timezone(&chrono::Utc);
        let dinner = Local
            .with_ymd_and_hms(2026, 8, 2, 19, 0, 0)
            .unwrap()
            .with_timezone(&chrono::Utc);
        let snack = Local
            .with_ymd_and_hms(2026, 8, 2, 22, 0, 0)
            .unwrap()
            .with_timezone(&chrono::Utc);
        assert_eq!(meal_type_for_timestamp(breakfast), "BREAKFAST");
        assert_eq!(meal_type_for_timestamp(lunch), "LUNCH");
        assert_eq!(meal_type_for_timestamp(dinner), "DINNER");
        assert_eq!(meal_type_for_timestamp(snack), "SNACK");
    }

    #[test]
    fn test_utc_offset_duration_str_for_local_evening() {
        use chrono::{Local, TimeZone};

        // Construct via Local so the expected UTC instant and offset follow the
        // host timezone (UTC on CI, often America/New_York on laptops).
        let local = Local
            .with_ymd_and_hms(2026, 8, 2, 21, 49, 32)
            .unwrap();
        let utc = local.with_timezone(&chrono::Utc);
        let offset = utc_offset_duration_str(utc);
        assert_eq!(
            offset,
            format!("{}s", local.offset().local_minus_utc()),
            "Google Duration must mirror the host Local offset"
        );
        assert_eq!(
            rfc3339_utc(utc),
            utc.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
        );
    }
}
