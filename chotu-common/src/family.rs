use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CalendarConfig {
    /// Calendar provider — currently only "google" is supported.
    pub provider: String,
    /// The Google account email associated with this calendar.
    pub email: String,
}

/// Optional daily nutrition targets for a family member.
/// Any field may be omitted; only configured targets are shown in reports.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct NutritionGoals {
    pub calories: Option<i32>,
    pub protein_g: Option<f64>,
    pub carbs_g: Option<f64>,
    pub fats_g: Option<f64>,
    pub fiber_g: Option<f64>,
    pub steps: Option<i32>,
}

impl NutritionGoals {
    pub fn is_empty(&self) -> bool {
        self.calories.is_none()
            && self.protein_g.is_none()
            && self.carbs_g.is_none()
            && self.fats_g.is_none()
            && self.fiber_g.is_none()
            && self.steps.is_none()
    }

    /// Markdown progress block for today's (or average) intake vs goals.
    pub fn progress_markdown(
        &self,
        calories: i32,
        protein_g: f64,
        carbs_g: f64,
        fats_g: f64,
        fiber_g: f64,
        steps: i32,
    ) -> Option<String> {
        if self.is_empty() {
            return None;
        }

        let mut lines = Vec::new();
        if let Some(goal) = self.calories {
            lines.push(format_goal_line(
                "Calories",
                calories as f64,
                goal as f64,
                "kcal",
            ));
        }
        if let Some(goal) = self.protein_g {
            lines.push(format_goal_line("Protein", protein_g, goal, "g"));
        }
        if let Some(goal) = self.carbs_g {
            lines.push(format_goal_line("Carbs", carbs_g, goal, "g"));
        }
        if let Some(goal) = self.fats_g {
            lines.push(format_goal_line("Fat", fats_g, goal, "g"));
        }
        if let Some(goal) = self.fiber_g {
            lines.push(format_goal_line("Fiber", fiber_g, goal, "g"));
        }
        if let Some(goal) = self.steps {
            lines.push(format_goal_line("Steps", steps as f64, goal as f64, ""));
        }

        if lines.is_empty() {
            return None;
        }

        let mut out = String::from("• *Goals:*\n");
        for line in lines {
            out.push_str("  - ");
            out.push_str(&line);
            out.push('\n');
        }
        Some(out)
    }
}

fn format_goal_line(label: &str, actual: f64, goal: f64, unit: &str) -> String {
    let pct = if goal > 0.0 {
        (actual / goal) * 100.0
    } else {
        0.0
    };
    let bar = progress_bar(pct);
    let unit_suffix = if unit.is_empty() {
        String::new()
    } else {
        unit.to_string()
    };
    format!(
        "{}: {:.0}/{:.0}{} ({:.0}%) {}",
        label, actual, goal, unit_suffix, pct, bar
    )
}

fn progress_bar(pct: f64) -> String {
    let filled = ((pct / 10.0).round() as i32).clamp(0, 10) as usize;
    format!("[{}{}]", "█".repeat(filled), "░".repeat(10 - filled))
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FamilyMember {
    pub id: String,
    pub name: String,
    pub role: String, // e.g. "adult", "kid"
    /// Optional calendar configuration for this member.
    pub calendar: Option<CalendarConfig>,
    /// Optional daily nutrition / activity goals.
    #[serde(default)]
    pub nutrition_goals: Option<NutritionGoals>,
}

impl FamilyMember {
    /// Returns the environment variable key that holds this member's
    /// Google Calendar OAuth refresh token.
    /// e.g. member id "alex" → "CALENDAR_REFRESH_TOKEN_ALEX"
    pub fn calendar_refresh_token_env_key(&self) -> String {
        format!("CALENDAR_REFRESH_TOKEN_{}", self.id.to_uppercase())
    }

    /// Returns the environment variable key that holds this member's
    /// Google Health OAuth refresh token.
    /// e.g. member id "alex" → "HEALTH_REFRESH_TOKEN_ALEX"
    pub fn health_refresh_token_env_key(&self) -> String {
        format!("HEALTH_REFRESH_TOKEN_{}", self.id.to_uppercase())
    }
}

/// Env key for a member's Google Health refresh token (`HEALTH_REFRESH_TOKEN_{ID}`).
pub fn health_refresh_token_env_key(member_id: &str) -> String {
    format!("HEALTH_REFRESH_TOKEN_{}", member_id.to_uppercase())
}

/// Resolve a member's Google Health refresh token.
///
/// Prefers `HEALTH_REFRESH_TOKEN_{ID}`. For the primary (first) family member,
/// falls back to legacy `FITBIT_REFRESH_TOKEN` so existing single-account setups
/// keep working.
pub fn resolve_health_refresh_token(member_id: &str, config: &AppConfig) -> Option<String> {
    let key = health_refresh_token_env_key(member_id);
    if let Ok(token) = std::env::var(&key) {
        if !token.is_empty() {
            return Some(token);
        }
    }

    let is_primary = config
        .family
        .members
        .first()
        .is_some_and(|m| m.id.eq_ignore_ascii_case(member_id));
    if is_primary {
        if let Ok(token) = std::env::var("FITBIT_REFRESH_TOKEN") {
            if !token.is_empty() {
                return Some(token);
            }
        }
    }

    None
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FamilySection {
    pub members: Vec<FamilyMember>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct InvestmentPhilosophy {
    pub description: String,
    pub focus_areas: Vec<String>,
}

impl Default for InvestmentPhilosophy {
    fn default() -> Self {
        Self {
            description: "finding high-conviction micro-cap and small-cap stocks with potential for 100x returns ('hundred baggers')".to_string(),
            focus_areas: vec![
                "Massive market opportunity".to_string(),
                "Strong unit economics / high return on capital".to_string(),
                "Strong competitive advantage (moats)".to_string(),
                "Excellent capital allocation by management".to_string(),
            ],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BucketHolding {
    pub ticker: String,
    pub amount: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AllocationBucket {
    pub name: String,
    pub weight_percent: f64,
    pub monthly_buy: f64,
    pub holdings: Vec<BucketHolding>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TargetAllocation {
    pub monthly_budget: f64,
    pub buckets: Vec<AllocationBucket>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub family: FamilySection,
    pub investment_philosophy: Option<InvestmentPhilosophy>,
    pub target_allocation: Option<TargetAllocation>,
    pub currency: Option<String>,
    pub email_classifier_prompt_path: Option<String>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            family: FamilySection {
                members: vec![FamilyMember {
                    id: "alex".to_string(),
                    name: "Alex".to_string(),
                    role: "adult".to_string(),
                    calendar: None,
                    nutrition_goals: None,
                }],
            },
            investment_philosophy: Some(InvestmentPhilosophy::default()),
            target_allocation: None,
            currency: None,
            email_classifier_prompt_path: None,
        }
    }
}

pub async fn fetch_exchange_rates(base_currency: &str) -> std::collections::HashMap<String, f64> {
    let url = format!("https://open.er-api.com/v6/latest/{}", base_currency);
    match reqwest::Client::new()
        .get(&url)
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await
    {
        Ok(resp) => {
            if let Ok(data) = resp.json::<serde_json::Value>().await {
                let mut map = std::collections::HashMap::new();
                if let Some(rates) = data.get("rates").and_then(|r| r.as_object()) {
                    for (k, v) in rates {
                        if let Some(val) = v.as_f64() {
                            map.insert(k.to_uppercase(), val);
                        }
                    }
                    return map;
                }
            }
        }
        Err(e) => {
            eprintln!("Failed to fetch exchange rates from open.er-api.com: {:?}", e);
        }
    }
    std::collections::HashMap::new()
}

impl AppConfig {
    pub fn currency(&self) -> &str {
        self.currency.as_deref().unwrap_or("USD")
    }

    pub fn convert_to_base(
        &self,
        amount: f64,
        from_currency: &str,
        rates: &std::collections::HashMap<String, f64>,
    ) -> f64 {
        let base = self.currency();
        let from_upper = from_currency.to_uppercase();
        let base_upper = base.to_uppercase();

        if from_upper == base_upper {
            return amount;
        }

        if let Some(&rate) = rates.get(&from_upper) {
            if rate > 0.0 {
                return amount / rate;
            }
        }

        // Hardcoded fallbacks if API failed or currency not found in rates
        match (from_upper.as_str(), base_upper.as_str()) {
            ("USD", "CAD") => amount * 1.37,
            ("CAD", "USD") => amount * 0.73,
            ("EUR", "USD") => amount * 1.08,
            ("USD", "EUR") => amount * 0.93,
            ("GBP", "USD") => amount * 1.27,
            ("USD", "GBP") => amount * 0.79,
            ("INR", "USD") => amount * 0.012,
            ("USD", "INR") => amount * 83.5,
            ("INR", "CAD") => amount * 0.016,
            ("CAD", "INR") => amount * 61.0,
            _ => amount,
        }
    }
}


/// Loads application configuration from a file. If the file is missing or fails to parse,
/// logs a warning and returns default configuration.
pub fn load_config<P: AsRef<Path>>(path: P) -> AppConfig {
    let path_ref = path.as_ref();
    if !path_ref.exists() {
        println!(
            "Configuration file {:?} not found. Using default family configuration.",
            path_ref
        );
        return AppConfig::default();
    }

    match std::fs::read_to_string(path_ref) {
        Ok(content) => match serde_yaml::from_str::<AppConfig>(&content) {
            Ok(config) => {
                if config.family.members.is_empty() {
                    println!("Configuration file {:?} has no family members. Using default family configuration.", path_ref);
                    AppConfig::default()
                } else {
                    println!("Successfully loaded configuration from {:?}", path_ref);
                    config
                }
            }
            Err(e) => {
                eprintln!("Failed to parse configuration file {:?}: {:?}. Using default family configuration.", path_ref, e);
                AppConfig::default()
            }
        },
        Err(e) => {
            eprintln!(
                "Failed to read configuration file {:?}: {:?}. Using default family configuration.",
                path_ref, e
            );
            AppConfig::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::Mutex;
    use tempfile::NamedTempFile;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn with_env_var(key: &str, value: Option<&str>, f: impl FnOnce() + std::panic::UnwindSafe) {
        let prev = std::env::var(key).ok();
        match value {
            Some(v) => std::env::set_var(key, v),
            None => std::env::remove_var(key),
        }
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
        match prev {
            Some(v) => std::env::set_var(key, v),
            None => std::env::remove_var(key),
        }
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }

    #[test]
    fn test_default_config() {
        let default_config = AppConfig::default();
        assert_eq!(default_config.family.members.len(), 1);
        assert_eq!(default_config.family.members[0].id, "alex");
    }

    #[test]
    fn test_calendar_env_key() {
        let member = FamilyMember {
            id: "alex".to_string(),
            name: "Alex".to_string(),
            role: "adult".to_string(),
            calendar: Some(CalendarConfig {
                provider: "google".to_string(),
                email: "alex@example.com".to_string(),
            }),
            nutrition_goals: None,
        };
        assert_eq!(member.calendar_refresh_token_env_key(), "CALENDAR_REFRESH_TOKEN_ALEX");
        assert_eq!(member.health_refresh_token_env_key(), "HEALTH_REFRESH_TOKEN_ALEX");
    }

    #[test]
    fn test_load_valid_config() {
        let yaml_content = r#"
family:
  members:
    - id: alex
      name: Alex
      role: adult
      calendar:
        provider: google
        email: alex@example.com
      nutrition_goals:
        calories: 2200
        protein_g: 160
        carbs_g: 220
        fats_g: 70
        fiber_g: 30
        steps: 8000
    - id: jordan
      name: Jordan
      role: adult
    - id: sam
      name: Sam
      role: kid

currency: "CAD"
email_classifier_prompt_path: "prompts/email_classifier_system_prompt.txt"

target_allocation:
  monthly_budget: 3000
  buckets:
    - name: "Core Equities"
      weight_percent: 38.3
      monthly_buy: 1150
      holdings:
        - { ticker: "VFV", amount: 600 }
        - { ticker: "QQC", amount: 350 }
"#;
        let mut tmp_file = NamedTempFile::new().unwrap();
        write!(tmp_file, "{}", yaml_content).unwrap();

        let loaded = load_config(tmp_file.path());
        assert_eq!(loaded.family.members.len(), 3);
        assert_eq!(loaded.family.members[0].id, "alex");
        assert_eq!(loaded.family.members[1].id, "jordan");
        assert_eq!(loaded.family.members[2].id, "sam");
        assert_eq!(loaded.family.members[2].role, "kid");
        assert_eq!(loaded.currency, Some("CAD".to_string()));
        assert_eq!(loaded.currency(), "CAD");
        assert_eq!(loaded.email_classifier_prompt_path, Some("prompts/email_classifier_system_prompt.txt".to_string()));
        // Alex should have a calendar configured
        assert!(loaded.family.members[0].calendar.is_some());
        let cal = loaded.family.members[0].calendar.as_ref().unwrap();
        assert_eq!(cal.provider, "google");
        assert_eq!(cal.email, "alex@example.com");
        assert_eq!(loaded.family.members[0].calendar_refresh_token_env_key(), "CALENDAR_REFRESH_TOKEN_ALEX");
        // Nutrition goals
        let goals = loaded.family.members[0].nutrition_goals.as_ref().unwrap();
        assert_eq!(goals.calories, Some(2200));
        assert_eq!(goals.protein_g, Some(160.0));
        assert_eq!(goals.steps, Some(8000));
        assert!(loaded.family.members[2].nutrition_goals.is_none());
        // Sam has no calendar
        assert!(loaded.family.members[2].calendar.is_none());

        let target = loaded.target_allocation.unwrap();
        assert_eq!(target.monthly_budget, 3000.0);
        assert_eq!(target.buckets.len(), 1);
        assert_eq!(target.buckets[0].name, "Core Equities");
        assert_eq!(target.buckets[0].holdings.len(), 2);
        assert_eq!(target.buckets[0].holdings[0].ticker, "VFV");
        assert_eq!(target.buckets[0].holdings[0].amount, 600.0);
    }

    #[test]
    fn test_nutrition_goal_progress() {
        let goals = NutritionGoals {
            calories: Some(2000),
            protein_g: Some(150.0),
            carbs_g: None,
            fats_g: None,
            fiber_g: None,
            steps: Some(10000),
        };
        let md = goals
            .progress_markdown(1000, 75.0, 0.0, 0.0, 0.0, 5000)
            .unwrap();
        assert!(md.contains("Calories: 1000/2000kcal (50%)"));
        assert!(md.contains("Protein: 75/150g (50%)"));
        assert!(md.contains("Steps: 5000/10000 (50%)"));
    }

    #[test]
    fn test_load_missing_config_fallback() {
        let loaded = load_config("non_existent_file.yaml");
        assert_eq!(loaded.family.members.len(), 1);
        assert_eq!(loaded.family.members[0].id, "alex");
    }

    #[test]
    fn test_health_env_key_and_resolve() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let mut config = AppConfig::default();
        config.family.members.push(FamilyMember {
            id: "jordan".to_string(),
            name: "Jordan".to_string(),
            role: "adult".to_string(),
            calendar: None,
            nutrition_goals: None,
        });
        assert_eq!(
            health_refresh_token_env_key("jordan"),
            "HEALTH_REFRESH_TOKEN_JORDAN"
        );

        with_env_var("HEALTH_REFRESH_TOKEN_ALEX", None, || {
            with_env_var("HEALTH_REFRESH_TOKEN_JORDAN", None, || {
                with_env_var("FITBIT_REFRESH_TOKEN", Some("legacy-primary-token"), || {
                    assert_eq!(
                        resolve_health_refresh_token("alex", &config).as_deref(),
                        Some("legacy-primary-token")
                    );
                    assert!(resolve_health_refresh_token("jordan", &config).is_none());

                    with_env_var("HEALTH_REFRESH_TOKEN_JORDAN", Some("jordan-token"), || {
                        assert_eq!(
                            resolve_health_refresh_token("jordan", &config).as_deref(),
                            Some("jordan-token")
                        );
                    });

                    with_env_var("HEALTH_REFRESH_TOKEN_ALEX", Some("alex-per-member"), || {
                        assert_eq!(
                            resolve_health_refresh_token("alex", &config).as_deref(),
                            Some("alex-per-member")
                        );
                    });
                });
            });
        });
    }
}
