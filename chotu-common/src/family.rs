use base64::Engine;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::Path;

use crate::{
    chat::{ChatAddress, ChatProvider, ConversationKind},
    schedule::{resolve_timezone_name, resolve_tz, AgentSchedules, DEFAULT_TIMEZONE},
};

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

/// Allowed `fitness_goals.focus` values.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum FitnessFocus {
    Cut,
    Bulk,
    Recomp,
    Endurance,
    General,
}

impl FitnessFocus {
    pub fn as_str(self) -> &'static str {
        match self {
            FitnessFocus::Cut => "cut",
            FitnessFocus::Bulk => "bulk",
            FitnessFocus::Recomp => "recomp",
            FitnessFocus::Endurance => "endurance",
            FitnessFocus::General => "general",
        }
    }
}

/// Allowed `fitness_goals.equipment` values.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum FitnessEquipment {
    Home,
    Gym,
    Mixed,
}

impl FitnessEquipment {
    pub fn as_str(self) -> &'static str {
        match self {
            FitnessEquipment::Home => "home",
            FitnessEquipment::Gym => "gym",
            FitnessEquipment::Mixed => "mixed",
        }
    }
}

/// Optional weekly training targets under [`FitnessGoals`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct FitnessWeeklyTargets {
    pub strength_sessions: Option<i32>,
    pub cardio_minutes: Option<i32>,
    /// Daily minimum Google Health *active energy burned* (kcal from movement /
    /// workouts) — not food-intake calories.
    pub active_calories: Option<i32>,
}

/// Long-horizon outcome + training policy (complements daily [`NutritionGoals`]).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct FitnessGoals {
    /// Outcome phrasing, e.g. "lean beach body / visible abs".
    pub intent: Option<String>,
    /// Target date as YYYY-MM-DD.
    pub target_date: Option<String>,
    /// Validated: cut | bulk | recomp | endurance | general
    pub focus: Option<FitnessFocus>,
    pub sessions_per_week: Option<i32>,
    pub session_minutes: Option<i32>,
    /// Validated: home | gym | mixed
    pub equipment: Option<FitnessEquipment>,
    /// User-authored limits (not medical records). Forward hook for later.
    #[serde(default)]
    pub constraints: Vec<String>,
    #[serde(default)]
    pub weekly_targets: Option<FitnessWeeklyTargets>,
}

impl FitnessGoals {
    pub fn is_empty(&self) -> bool {
        self.intent
            .as_ref()
            .map(|s| s.trim().is_empty())
            .unwrap_or(true)
            && self
                .target_date
                .as_ref()
                .map(|s| s.trim().is_empty())
                .unwrap_or(true)
            && self.focus.is_none()
            && self.sessions_per_week.is_none()
            && self.session_minutes.is_none()
            && self.equipment.is_none()
            && self.constraints.iter().all(|c| c.trim().is_empty())
            && self
                .weekly_targets
                .as_ref()
                .map(|t| {
                    t.strength_sessions.is_none()
                        && t.cardio_minutes.is_none()
                        && t.active_calories.is_none()
                })
                .unwrap_or(true)
    }

    /// Days from `as_of` until `target_date` (negative if past). None if unset/invalid.
    pub fn days_until_target(&self, as_of: chrono::NaiveDate) -> Option<i64> {
        let raw = self.target_date.as_ref()?.trim();
        let target = chrono::NaiveDate::parse_from_str(raw, "%Y-%m-%d").ok()?;
        Some((target - as_of).num_days())
    }

    /// Validate `target_date` shape and positive weekly numeric targets.
    /// Returns human-readable warnings (does not mutate).
    pub fn validation_warnings(&self, member_id: &str) -> Vec<String> {
        let mut warnings = Vec::new();
        if let Some(raw) = self
            .target_date
            .as_ref()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
        {
            if chrono::NaiveDate::parse_from_str(raw, "%Y-%m-%d").is_err() {
                warnings.push(format!(
                    "member '{}': fitness_goals.target_date '{}' must be YYYY-MM-DD",
                    member_id, raw
                ));
            }
        }
        if let Some(n) = self.sessions_per_week {
            if n < 0 || n > 14 {
                warnings.push(format!(
                    "member '{}': fitness_goals.sessions_per_week {} looks unreasonable (0–14)",
                    member_id, n
                ));
            }
        }
        if let Some(n) = self.session_minutes {
            if n < 0 || n > 300 {
                warnings.push(format!(
                    "member '{}': fitness_goals.session_minutes {} looks unreasonable (0–300)",
                    member_id, n
                ));
            }
        }
        if let Some(wt) = self.weekly_targets.as_ref() {
            if let Some(n) = wt.strength_sessions {
                if n < 0 || n > 14 {
                    warnings.push(format!(
                        "member '{}': weekly_targets.strength_sessions {} looks unreasonable",
                        member_id, n
                    ));
                }
            }
            if let Some(n) = wt.cardio_minutes {
                if n < 0 || n > 1000 {
                    warnings.push(format!(
                        "member '{}': weekly_targets.cardio_minutes {} looks unreasonable",
                        member_id, n
                    ));
                }
            }
            if let Some(n) = wt.active_calories {
                if n < 0 || n > 5000 {
                    warnings.push(format!(
                        "member '{}': weekly_targets.active_calories {} looks unreasonable \
                         (daily active-energy burned floor, not food calories)",
                        member_id, n
                    ));
                }
            }
        }
        warnings
    }

    /// Short Markdown block for briefs / status (countdown + intent).
    pub fn outcome_markdown(&self, as_of: chrono::NaiveDate) -> Option<String> {
        if self.is_empty() {
            return None;
        }
        let mut lines = Vec::new();
        if let Some(intent) = self
            .intent
            .as_ref()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
        {
            lines.push(format!("Outcome: {}", intent));
        }
        if let Some(days) = self.days_until_target(as_of) {
            if days > 0 {
                lines.push(format!(
                    "{} days to target ({})",
                    days,
                    self.target_date.as_deref().unwrap_or("")
                ));
            } else if days == 0 {
                lines.push("Target day is today".to_string());
            } else {
                lines.push(format!(
                    "Target date {} was {} days ago",
                    self.target_date.as_deref().unwrap_or(""),
                    -days
                ));
            }
        }
        if let Some(focus) = self.focus {
            lines.push(format!("Focus: {}", focus.as_str()));
        }
        if lines.is_empty() {
            return None;
        }
        let mut out = String::from("• *Fitness:*\n");
        for line in lines {
            out.push_str("  - ");
            out.push_str(&line);
            out.push('\n');
        }
        Some(out)
    }
}

fn default_lag_window() -> [i32; 2] {
    [1, 3]
}

fn default_condition_check_in() -> bool {
    true
}

/// Chronic condition the member is tracking (definitions in gitignored config).
/// Watchlists and scores live in SQLite; see `docs/condition-tracking-spec.md`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HealthCondition {
    /// Slug, unique per member (e.g. `psoriasis`).
    pub id: String,
    /// Display name (e.g. `plaque psoriasis`).
    pub label: String,
    /// When true, evening `/reflect` asks for a 0–5 score.
    #[serde(default = "default_condition_check_in")]
    pub check_in: bool,
    /// Correlate food tags from these many days prior `[min, max]`, inclusive.
    #[serde(default = "default_lag_window")]
    pub lag_window: [i32; 2],
    /// Optional member-authored notes (not diagnoses).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
}

impl HealthCondition {
    pub fn lag_min(&self) -> i32 {
        self.lag_window[0]
    }

    pub fn lag_max(&self) -> i32 {
        self.lag_window[1]
    }
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ProviderIds {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub telegram: Option<String>,
}

impl ProviderIds {
    pub fn get(&self, provider: ChatProvider) -> Option<&str> {
        match provider {
            ChatProvider::Signal => self.signal.as_deref(),
            ChatProvider::Telegram => self.telegram.as_deref(),
        }
    }

    fn get_mut(&mut self, provider: ChatProvider) -> &mut Option<String> {
        match provider {
            ChatProvider::Signal => &mut self.signal,
            ChatProvider::Telegram => &mut self.telegram,
        }
    }

    fn is_empty(&self) -> bool {
        self.signal.is_none() && self.telegram.is_none()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct FamilyMember {
    pub id: String,
    pub name: String,
    pub role: String, // e.g. "adult", "kid"
    /// Optional calendar configuration for this member.
    pub calendar: Option<CalendarConfig>,
    /// Optional daily nutrition / activity goals.
    #[serde(default)]
    pub nutrition_goals: Option<NutritionGoals>,
    /// Optional long-horizon fitness outcome + training policy.
    #[serde(default)]
    pub fitness_goals: Option<FitnessGoals>,
    /// Optional chronic conditions (empty = none). Watchlists live in the DB.
    #[serde(default)]
    pub health_conditions: Vec<HealthCondition>,
    /// Provider-scoped direct conversation identifiers.
    #[serde(default, skip_serializing_if = "ProviderIds::is_empty")]
    pub chat_ids: ProviderIds,
    /// Legacy Signal ACI accepted for one compatibility release.
    #[serde(default, skip_serializing)]
    pub signal_aci: Option<String>,
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

    /// Soft checks for `health_conditions` (does not mutate).
    pub fn health_condition_warnings(&self) -> Vec<String> {
        let mut warnings = Vec::new();
        let mut seen = HashSet::new();
        for (i, cond) in self.health_conditions.iter().enumerate() {
            if cond.id.trim().is_empty() {
                warnings.push(format!(
                    "member '{}': health_conditions[{i}].id is empty",
                    self.id
                ));
            }
            if cond.label.trim().is_empty() {
                warnings.push(format!(
                    "member '{}': health_conditions[{i}].label is empty",
                    self.id
                ));
            }
            let key = cond.id.trim().to_ascii_lowercase();
            if !key.is_empty() && !seen.insert(key) {
                warnings.push(format!(
                    "member '{}': duplicate health_conditions.id '{}'",
                    self.id, cond.id
                ));
            }
            let min = cond.lag_min();
            let max = cond.lag_max();
            if min < 0 || max > 14 || min > max {
                warnings.push(format!(
                    "member '{}': health_conditions[{i}] (id '{}') lag_window [{}, {}] must be [min, max] with 0 <= min <= max <= 14",
                    self.id, cond.id, min, max
                ));
            }
        }
        warnings
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

/// A single Brené-style core value with a personal definition.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CoreValue {
    pub name: String,
    pub definition: String,
}

/// Personal operating values used to train evening reflection (alongside health logs).
/// Keep the anchor list to two; satellites (courage, humility, etc.) live under `practices`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CoreValues {
    /// Exactly two preferred; more than two weakens the compass.
    pub anchors: Vec<CoreValue>,
    /// How integrity / courage / humility relate (tools, not listed as anchors).
    #[serde(default)]
    pub integrity_note: Option<String>,
    /// Daily micro-practices and evening check-in lenses.
    #[serde(default)]
    pub practices: Vec<String>,
}

impl Default for CoreValues {
    fn default() -> Self {
        Self {
            anchors: vec![
                CoreValue {
                    name: "Growth".to_string(),
                    definition: "Valuing the state of not knowing and becoming a better person \
                        than yesterday; noticing ego flares (defensiveness, needing to be right) \
                        and choosing to learn anyway."
                        .to_string(),
                },
                CoreValue {
                    name: "Contribution".to_string(),
                    definition: "Putting ego aside to serve the situation and the people in it — \
                        macro impact, daily climate of a room, and lifting others. Speaking up \
                        when silence withholds a needed perspective is an act of contribution."
                        .to_string(),
                },
            ],
            integrity_note: Some(
                "Integrity is not a listed value — it is the alignment sensor between actions \
                 and the two anchors. Courage fuels Growth; humility guards Contribution."
                    .to_string(),
            ),
            practices: vec![
                "Evening: Did I leave anything unspoken that sits heavy in my body? What stopped me?"
                    .to_string(),
                "Growth: Where did I choose 'I don't know / let's figure it out' over looking smart?"
                    .to_string(),
                "Growth: Ego autopsy — what was ego protecting (competence, status, comfort)?"
                    .to_string(),
                "Contribution: Did I bring a brick (clarify, insight, support) rather than perform?"
                    .to_string(),
                "Contribution via voice: when hesitation hit, did I reframe silence as withholding?"
                    .to_string(),
            ],
        }
    }
}

impl CoreValues {
    /// Soft checks for a Brené-style two-anchor setup (does not mutate).
    pub fn validation_warnings(&self) -> Vec<String> {
        let mut warnings = Vec::new();
        let n = self.anchors.len();
        if n != 2 {
            warnings.push(format!(
                "core_values.anchors has {} entries; exactly two preferred (weakens the compass)",
                n
            ));
        }
        for (i, a) in self.anchors.iter().enumerate() {
            if a.name.trim().is_empty() {
                warnings.push(format!("core_values.anchors[{i}].name is empty"));
            }
            if a.definition.trim().is_empty() {
                warnings.push(format!("core_values.anchors[{i}].definition is empty"));
            }
        }
        warnings
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

fn default_true() -> bool {
    true
}

/// Household monthly spend limits by ledger category (e.g. Food, Shopping).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct SpendBudgets {
    /// Map of category name → monthly limit in base currency.
    #[serde(default)]
    pub categories: std::collections::HashMap<String, f64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChatConfig {
    #[serde(default, skip_serializing_if = "ProviderIds::is_empty")]
    pub household_ids: ProviderIds,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub family: FamilySection,
    #[serde(default)]
    pub chat: ChatConfig,
    pub investment_philosophy: Option<InvestmentPhilosophy>,
    /// Personal core values that shape evening reflection prompts (with health logs).
    #[serde(default)]
    pub core_values: Option<CoreValues>,
    pub target_allocation: Option<TargetAllocation>,
    pub spend_budgets: Option<SpendBudgets>,
    pub currency: Option<String>,
    pub email_classifier_prompt_path: Option<String>,
    /// Whether the Streamer connects to IMAP and pulls incoming email.
    #[serde(default = "default_true")]
    pub email_sync_enabled: bool,
    /// IANA tz database name (e.g. `America/Toronto`). Agent wall clock; DB stores UTC.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
    /// Proactive send/sync times (`HH:MM` in `timezone`). Blank/omitted jobs are not scheduled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schedules: Option<AgentSchedules>,
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
                    fitness_goals: None,
                    health_conditions: vec![],
                    chat_ids: ProviderIds::default(),
                    signal_aci: None,
                }],
            },
            chat: ChatConfig::default(),
            investment_philosophy: Some(InvestmentPhilosophy::default()),
            core_values: Some(CoreValues::default()),
            target_allocation: None,
            spend_budgets: None,
            currency: None,
            email_classifier_prompt_path: None,
            email_sync_enabled: true,
            timezone: Some(DEFAULT_TIMEZONE.to_string()),
            schedules: None,
        }
    }
}

/// Member linked to a provider-scoped direct conversation, if any.
pub fn member_for_chat_id<'a>(
    config: &'a AppConfig,
    provider: ChatProvider,
    id: &str,
) -> Option<&'a FamilyMember> {
    config
        .family
        .members
        .iter()
        .find(|member| member.chat_ids.get(provider) == Some(id))
}

/// Default member id for a direct conversation: linked member, else primary member.
pub fn default_member_id<'a>(
    config: &'a AppConfig,
    provider: ChatProvider,
    direct_id: &str,
) -> &'a str {
    member_for_chat_id(config, provider, direct_id)
        .map(|member| member.id.as_str())
        .or_else(|| {
            config
                .family
                .members
                .first()
                .map(|member| member.id.as_str())
        })
        .unwrap_or("alex")
}

/// Food writes from a linked direct conversation may only target that member.
pub fn ensure_food_mutation_allowed(
    config: &AppConfig,
    provider: ChatProvider,
    direct_id: &str,
    target_member_id: &str,
) -> Result<(), String> {
    match member_for_chat_id(config, provider, direct_id) {
        Some(linked) if !linked.id.eq_ignore_ascii_case(target_member_id) => Err(format!(
            "This direct conversation is linked as {}. Food commands here only work for you — \
             use the household group to log or change food for someone else.",
            linked.id
        )),
        _ => Ok(()),
    }
}

pub fn has_any_chat_link(config: &AppConfig, provider: ChatProvider) -> bool {
    config
        .family
        .members
        .iter()
        .any(|member| member.chat_ids.get(provider).is_some())
}

/// Authorize direct members and the exact configured household group.
pub fn is_chat_conversation_allowed(config: &AppConfig, address: &ChatAddress) -> bool {
    match address.kind {
        ConversationKind::Direct => {
            member_for_chat_id(config, address.provider, &address.id).is_some()
        }
        ConversationKind::Group => {
            config.chat.household_ids.get(address.provider) == Some(address.id.as_str())
        }
    }
}

/// Provider-scoped direct address linked to `member_id`, if any.
pub fn chat_address_for_member(
    config: &AppConfig,
    provider: ChatProvider,
    member_id: &str,
) -> Option<ChatAddress> {
    config
        .family
        .members
        .iter()
        .find(|member| member.id.eq_ignore_ascii_case(member_id))
        .and_then(|member| member.chat_ids.get(provider))
        .map(|id| ChatAddress::direct(provider, id))
}

/// Unique direct and household-group delivery targets for the selected provider.
pub fn chat_delivery_targets(config: &AppConfig, provider: ChatProvider) -> Vec<ChatAddress> {
    let mut targets = Vec::new();
    for member in &config.family.members {
        if let Some(id) = member.chat_ids.get(provider) {
            let target = ChatAddress::direct(provider, id);
            if !targets.contains(&target) {
                targets.push(target);
            }
        }
    }
    if let Some(group_id) = config.chat.household_ids.get(provider) {
        let target = ChatAddress::group(provider, group_id);
        if !targets.contains(&target) {
            targets.push(target);
        }
    }
    targets
}

pub fn has_chat_delivery(config: &AppConfig, provider: ChatProvider) -> bool {
    !chat_delivery_targets(config, provider).is_empty()
}

/// Set one provider-scoped member id in config.yaml.
pub fn set_member_chat_id<P: AsRef<Path>>(
    path: P,
    provider: ChatProvider,
    member_id: &str,
    chat_id: &str,
) -> Result<AppConfig, String> {
    let path_ref = path.as_ref();
    let mut config = load_config_for_provider(path_ref, provider)?;
    let Some(member_index) = config
        .family
        .members
        .iter()
        .position(|member| member.id.eq_ignore_ascii_case(member_id))
    else {
        return Err(format!("Unknown member `{member_id}`"));
    };
    if let Some(existing) = config.family.members[member_index].chat_ids.get(provider) {
        if existing != chat_id {
            return Err(format!(
                "Member `{member_id}` is already linked to {provider} id `{existing}`"
            ));
        }
        return Ok(config);
    }
    validate_direct_id(provider, chat_id)?;
    for (index, member) in config.family.members.iter_mut().enumerate() {
        let slot = member.chat_ids.get_mut(provider);
        if index == member_index {
            *slot = Some(chat_id.to_owned());
        } else if slot.as_deref() == Some(chat_id) {
            *slot = None;
        }
    }
    let yaml = serde_yaml::to_string(&config)
        .map_err(|error| format!("Failed to serialize config.yaml: {error}"))?;
    std::fs::write(path_ref, yaml)
        .map_err(|error| format!("Failed to write {:?}: {error}", path_ref))?;
    Ok(config)
}

/// Path used for config load/save (`CHOTU_CONFIG_PATH` or `config.yaml`).
pub fn config_path() -> String {
    std::env::var("CHOTU_CONFIG_PATH").unwrap_or_else(|_| "config.yaml".to_string())
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
            eprintln!(
                "Failed to fetch exchange rates from open.er-api.com: {:?}",
                e
            );
        }
    }
    std::collections::HashMap::new()
}

impl AppConfig {
    pub fn currency(&self) -> &str {
        self.currency.as_deref().unwrap_or("USD")
    }

    /// IANA tz database name the agent uses for civil clocks (default `America/Toronto`).
    pub fn resolved_timezone_name(&self) -> String {
        resolve_timezone_name(self.timezone.as_deref())
    }

    pub fn resolved_tz(&self) -> chrono_tz::Tz {
        resolve_tz(self.timezone.as_deref())
    }

    pub fn now_in_tz(&self) -> chrono::DateTime<chrono_tz::Tz> {
        crate::schedule::now_in_tz(self.resolved_tz())
    }

    /// Configured clock for a named job, if that job is enabled.
    pub fn schedule_clock(
        &self,
        slot: fn(&AgentSchedules) -> Option<crate::schedule::ClockTime>,
    ) -> Option<crate::schedule::ClockTime> {
        self.schedules.as_ref().and_then(slot)
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

/// Load configuration with Signal compatibility defaults.
pub fn load_config<P: AsRef<Path>>(path: P) -> Result<AppConfig, String> {
    load_config_for_provider(path, ChatProvider::Signal)
}

/// Load, normalize, and validate routing for the selected provider.
pub fn load_config_for_provider<P: AsRef<Path>>(
    path: P,
    provider: ChatProvider,
) -> Result<AppConfig, String> {
    let path_ref = path.as_ref();
    if !path_ref.exists() {
        return Err(format!(
            "Configuration file {:?} not found. Copy config.yaml.example and edit it.",
            path_ref
        ));
    }
    let content = std::fs::read_to_string(path_ref)
        .map_err(|error| format!("Failed to read configuration file {:?}: {error}", path_ref))?;
    let mut config: AppConfig = serde_yaml::from_str(&content).map_err(|error| {
        format!(
            "Failed to parse configuration file {:?}: {error}. Match config.yaml.example.",
            path_ref
        )
    })?;
    if config.family.members.is_empty() {
        return Err(format!(
            "Configuration file {:?} has no family members.",
            path_ref
        ));
    }
    normalize_legacy_chat_config(&mut config, provider)?;
    validate_chat_config(&config, provider)?;

    for member in &config.family.members {
        if let Some(fg) = member.fitness_goals.as_ref() {
            for warning in fg.validation_warnings(&member.id) {
                eprintln!("Config warning: {warning}");
            }
        }
        for warning in member.health_condition_warnings() {
            eprintln!("Config warning: {warning}");
        }
    }
    if let Some(cv) = config.core_values.as_ref() {
        for warning in cv.validation_warnings() {
            eprintln!("Config warning: {warning}");
        }
    }
    for warning in crate::schedule::timezone_validation_warnings(config.timezone.as_deref()) {
        eprintln!("Config warning: {warning}");
    }
    if let Some(schedules) = config.schedules.as_ref() {
        for warning in schedules.validation_warnings() {
            eprintln!("Config warning: {warning}");
        }
    }
    println!(
        "Successfully loaded configuration from {:?} (timezone {}, chat provider {})",
        path_ref,
        config.resolved_timezone_name(),
        provider
    );
    Ok(config)
}

fn normalize_legacy_chat_config(
    config: &mut AppConfig,
    selected_provider: ChatProvider,
) -> Result<(), String> {
    let mut used_legacy_member_id = false;
    for member in &mut config.family.members {
        let Some(legacy) = member.signal_aci.take() else {
            continue;
        };
        match member.chat_ids.signal.as_deref() {
            Some(current) if current != legacy => {
                return Err(format!(
                    "member `{}` has conflicting `signal_aci` and `chat_ids.signal` values",
                    member.id
                ));
            }
            Some(_) => {}
            None => {
                member.chat_ids.signal = Some(legacy);
                used_legacy_member_id = true;
            }
        }
    }
    if used_legacy_member_id {
        eprintln!(
            "Config compatibility: migrate `family.members[].signal_aci` to `chat_ids.signal`."
        );
    }

    if selected_provider == ChatProvider::Signal {
        if let Some(legacy_group) = std::env::var("SIGNAL_GROUP_ID")
            .ok()
            .filter(|value| !value.trim().is_empty())
        {
            match config.chat.household_ids.signal.as_deref() {
                Some(current) if current != legacy_group => {
                    return Err(
                        "conflicting `SIGNAL_GROUP_ID` and `chat.household_ids.signal` values"
                            .into(),
                    );
                }
                Some(_) => {}
                None => {
                    config.chat.household_ids.signal = Some(legacy_group);
                    eprintln!(
                        "Config compatibility: migrate `SIGNAL_GROUP_ID` to `chat.household_ids.signal`."
                    );
                }
            }
        }
    }
    Ok(())
}

fn validate_chat_config(config: &AppConfig, selected_provider: ChatProvider) -> Result<(), String> {
    for provider in [ChatProvider::Signal, ChatProvider::Telegram] {
        for member in &config.family.members {
            if let Some(id) = member.chat_ids.get(provider) {
                validate_direct_id(provider, id).map_err(|error| {
                    format!(
                        "member `{}` has invalid {provider} chat id: {error}",
                        member.id
                    )
                })?;
            }
        }
        if let Some(id) = config.chat.household_ids.get(provider) {
            validate_household_id(provider, id)
                .map_err(|error| format!("invalid {provider} household id: {error}"))?;
        }
    }

    let mut direct_ids = HashSet::new();
    for member in &config.family.members {
        if let Some(id) = member.chat_ids.get(selected_provider) {
            if !direct_ids.insert(id) {
                return Err(format!(
                    "duplicate {selected_provider} direct id `{id}` across family members"
                ));
            }
        }
    }
    if let Some(group_id) = config.chat.household_ids.get(selected_provider) {
        if direct_ids.contains(group_id) {
            return Err(format!(
                "{selected_provider} household id `{group_id}` also belongs to a member direct chat"
            ));
        }
    }
    Ok(())
}

fn validate_direct_id(provider: ChatProvider, id: &str) -> Result<(), String> {
    let id = nonblank_id(id)?;
    match provider {
        ChatProvider::Signal => {
            let uuid = id.strip_prefix("aci:").unwrap_or(id);
            uuid::Uuid::parse_str(uuid)
                .map(|_| ())
                .map_err(|_| "expected a Signal ACI UUID".into())
        }
        ChatProvider::Telegram => {
            let value = parse_telegram_id(id)?;
            (value > 0)
                .then_some(())
                .ok_or_else(|| "expected a positive Telegram private-chat id".into())
        }
    }
}

fn validate_household_id(provider: ChatProvider, id: &str) -> Result<(), String> {
    let id = nonblank_id(id)?;
    match provider {
        ChatProvider::Signal => {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(id)
                .map_err(|_| "expected a base64 Signal group id".to_string())?;
            (!bytes.is_empty())
                .then_some(())
                .ok_or_else(|| "expected a non-empty base64 Signal group id".into())
        }
        ChatProvider::Telegram => {
            let value = parse_telegram_id(id)?;
            (value < 0)
                .then_some(())
                .ok_or_else(|| "expected a negative Telegram group or supergroup id".into())
        }
    }
}

fn nonblank_id(id: &str) -> Result<&str, String> {
    let id = id.trim();
    (!id.is_empty())
        .then_some(id)
        .ok_or_else(|| "id must not be blank".into())
}

fn parse_telegram_id(id: &str) -> Result<i64, String> {
    id.parse::<i64>()
        .map_err(|_| "expected a signed decimal integer".into())
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
            fitness_goals: None,
            health_conditions: vec![],
            chat_ids: ProviderIds::default(),
            signal_aci: None,
        };
        assert_eq!(
            member.calendar_refresh_token_env_key(),
            "CALENDAR_REFRESH_TOKEN_ALEX"
        );
        assert_eq!(
            member.health_refresh_token_env_key(),
            "HEALTH_REFRESH_TOKEN_ALEX"
        );
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
        steps: 10000
      fitness_goals:
        intent: "lean beach body"
        target_date: "2027-06-01"
        focus: "recomp"
        sessions_per_week: 4
        session_minutes: 45
        equipment: "gym"
        constraints:
          - "prefer low-impact cardio"
        weekly_targets:
          strength_sessions: 3
          cardio_minutes: 90
          active_calories: 400
      health_conditions:
        - id: psoriasis
          label: "plaque psoriasis"
          check_in: true
          lag_window: [1, 3]
          notes: "example notes"
    - id: jordan
      name: Jordan
      role: adult
    - id: sam
      name: Sam
      role: kid

currency: "CAD"
email_classifier_prompt_path: "prompts/email_classifier_system_prompt.txt"
email_sync_enabled: false

spend_budgets:
  categories:
    Food: 800
    Shopping: 400

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

        let loaded = load_config_for_provider(tmp_file.path(), ChatProvider::Telegram)
            .expect("valid config");
        assert_eq!(loaded.family.members.len(), 3);
        assert_eq!(loaded.family.members[0].id, "alex");
        assert_eq!(loaded.family.members[1].id, "jordan");
        assert_eq!(loaded.family.members[2].id, "sam");
        assert_eq!(loaded.family.members[2].role, "kid");
        assert_eq!(loaded.currency, Some("CAD".to_string()));
        assert_eq!(loaded.currency(), "CAD");
        assert_eq!(
            loaded.email_classifier_prompt_path,
            Some("prompts/email_classifier_system_prompt.txt".to_string())
        );
        assert!(!loaded.email_sync_enabled);
        // Alex should have a calendar configured
        assert!(loaded.family.members[0].calendar.is_some());
        let cal = loaded.family.members[0].calendar.as_ref().unwrap();
        assert_eq!(cal.provider, "google");
        assert_eq!(cal.email, "alex@example.com");
        assert_eq!(
            loaded.family.members[0].calendar_refresh_token_env_key(),
            "CALENDAR_REFRESH_TOKEN_ALEX"
        );
        // Nutrition goals
        let goals = loaded.family.members[0].nutrition_goals.as_ref().unwrap();
        assert_eq!(goals.calories, Some(2200));
        assert_eq!(goals.protein_g, Some(160.0));
        assert_eq!(goals.steps, Some(10000));
        assert!(loaded.family.members[2].nutrition_goals.is_none());
        let fitness = loaded.family.members[0].fitness_goals.as_ref().unwrap();
        assert_eq!(fitness.intent.as_deref(), Some("lean beach body"));
        assert_eq!(fitness.target_date.as_deref(), Some("2027-06-01"));
        assert_eq!(fitness.focus, Some(FitnessFocus::Recomp));
        assert_eq!(fitness.equipment, Some(FitnessEquipment::Gym));
        assert_eq!(fitness.sessions_per_week, Some(4));
        assert_eq!(
            fitness
                .weekly_targets
                .as_ref()
                .and_then(|t| t.strength_sessions),
            Some(3)
        );
        assert!(fitness.validation_warnings("alex").is_empty());
        let conditions = &loaded.family.members[0].health_conditions;
        assert_eq!(conditions.len(), 1);
        assert_eq!(conditions[0].id, "psoriasis");
        assert_eq!(conditions[0].label, "plaque psoriasis");
        assert!(conditions[0].check_in);
        assert_eq!(conditions[0].lag_window, [1, 3]);
        assert_eq!(conditions[0].notes.as_deref(), Some("example notes"));
        assert!(loaded.family.members[0]
            .health_condition_warnings()
            .is_empty());
        assert!(loaded.family.members[2].fitness_goals.is_none());
        assert!(loaded.family.members[2].health_conditions.is_empty());
        // Sam has no calendar
        assert!(loaded.family.members[2].calendar.is_none());

        let as_of = chrono::NaiveDate::from_ymd_opt(2026, 8, 9).unwrap();
        assert_eq!(fitness.days_until_target(as_of), Some(296));

        let budgets = loaded.spend_budgets.as_ref().unwrap();
        assert_eq!(budgets.categories.get("Food"), Some(&800.0));
        assert_eq!(budgets.categories.get("Shopping"), Some(&400.0));

        let target = loaded.target_allocation.unwrap();
        assert_eq!(target.monthly_budget, 3000.0);
        assert_eq!(target.buckets.len(), 1);
        assert_eq!(target.buckets[0].name, "Core Equities");
        assert_eq!(target.buckets[0].holdings.len(), 2);
        assert_eq!(target.buckets[0].holdings[0].ticker, "VFV");
        assert_eq!(target.buckets[0].holdings[0].amount, 600.0);
    }

    #[test]
    fn test_load_timezone_and_schedules() {
        let yaml = r#"
family:
  members:
    - id: alex
      name: Alex
      role: adult
timezone: America/Toronto
schedules:
  morning_brief: "07:00"
  portfolio: ""
  reflection:
  health_evening_sync: "20:45"
  health_late_steps: "23:00"
"#;
        let mut tmp = NamedTempFile::new().unwrap();
        write!(tmp, "{}", yaml).unwrap();
        let loaded =
            load_config_for_provider(tmp.path(), ChatProvider::Telegram).expect("valid config");
        assert!(loaded.email_sync_enabled);
        assert_eq!(loaded.resolved_timezone_name(), "America/Toronto");
        let s = loaded.schedules.as_ref().unwrap();
        assert_eq!(s.morning_brief().unwrap().hour, 7);
        assert!(s.portfolio().is_none());
        assert!(s.reflection().is_none());
        assert_eq!(s.health_evening_sync().unwrap().minute, 45);
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
    fn test_fitness_outcome_markdown() {
        let goals = FitnessGoals {
            intent: Some("beach body".into()),
            target_date: Some("2027-06-01".into()),
            focus: Some(FitnessFocus::Recomp),
            sessions_per_week: None,
            session_minutes: None,
            equipment: None,
            constraints: vec![],
            weekly_targets: None,
        };
        let as_of = chrono::NaiveDate::from_ymd_opt(2026, 8, 9).unwrap();
        let md = goals.outcome_markdown(as_of).unwrap();
        assert!(md.contains("beach body"));
        assert!(md.contains("296 days to target"));
        assert!(md.contains("Focus: recomp"));
    }

    #[test]
    fn test_fitness_focus_and_equipment_reject_unknown() {
        let bad_focus = r#"
family:
  members:
    - id: alex
      name: Alex
      role: adult
      fitness_goals:
        focus: "yoga"
"#;
        let mut tmp = NamedTempFile::new().unwrap();
        write!(tmp, "{}", bad_focus).unwrap();
        assert!(load_config_for_provider(tmp.path(), ChatProvider::Telegram).is_err());

        let bad_equip = r#"
family:
  members:
    - id: alex
      name: Alex
      role: adult
      fitness_goals:
        equipment: "peloton"
"#;
        let mut tmp2 = NamedTempFile::new().unwrap();
        write!(tmp2, "{}", bad_equip).unwrap();
        assert!(load_config_for_provider(tmp2.path(), ChatProvider::Telegram).is_err());
    }

    #[test]
    fn test_fitness_validation_warnings_bad_date() {
        let goals = FitnessGoals {
            intent: None,
            target_date: Some("not-a-date".into()),
            focus: Some(FitnessFocus::Cut),
            sessions_per_week: Some(99),
            session_minutes: None,
            equipment: Some(FitnessEquipment::Home),
            constraints: vec![],
            weekly_targets: None,
        };
        let warnings = goals.validation_warnings("alex");
        assert!(warnings.iter().any(|w| w.contains("target_date")));
        assert!(warnings.iter().any(|w| w.contains("sessions_per_week")));
    }

    #[test]
    fn test_core_values_validation_warnings() {
        assert!(CoreValues::default().validation_warnings().is_empty());

        let bad = CoreValues {
            anchors: vec![CoreValue {
                name: "   ".into(),
                definition: "".into(),
            }],
            integrity_note: None,
            practices: vec![],
        };
        let warnings = bad.validation_warnings();
        assert!(warnings.iter().any(|w| w.contains("exactly two")));
        assert!(warnings.iter().any(|w| w.contains("name is empty")));
        assert!(warnings.iter().any(|w| w.contains("definition is empty")));
    }

    #[test]
    fn test_health_condition_warnings() {
        let member = FamilyMember {
            id: "alex".to_string(),
            name: "Alex".to_string(),
            role: "adult".to_string(),
            calendar: None,
            nutrition_goals: None,
            fitness_goals: None,
            health_conditions: vec![
                HealthCondition {
                    id: "psoriasis".into(),
                    label: "plaque psoriasis".into(),
                    check_in: true,
                    lag_window: [1, 3],
                    notes: None,
                },
                HealthCondition {
                    id: "PSORIASIS".into(),
                    label: "   ".into(),
                    check_in: false,
                    lag_window: [5, 2],
                    notes: None,
                },
                HealthCondition {
                    id: "  ".into(),
                    label: "unnamed".into(),
                    check_in: true,
                    lag_window: [0, 14],
                    notes: None,
                },
            ],
            chat_ids: ProviderIds::default(),
            signal_aci: None,
        };
        let warnings = member.health_condition_warnings();
        assert!(warnings.iter().any(|w| w.contains("duplicate")));
        assert!(warnings.iter().any(|w| w.contains("label is empty")));
        assert!(warnings.iter().any(|w| w.contains("lag_window")));
        assert!(warnings.iter().any(|w| w.contains("id is empty")));
    }

    #[test]
    fn test_health_condition_defaults_from_yaml() {
        let yaml = r#"
family:
  members:
    - id: alex
      name: Alex
      role: adult
      health_conditions:
        - id: psoriasis
          label: "plaque psoriasis"
"#;
        let mut tmp = NamedTempFile::new().unwrap();
        write!(tmp, "{}", yaml).unwrap();
        let loaded =
            load_config_for_provider(tmp.path(), ChatProvider::Telegram).expect("valid config");
        let cond = &loaded.family.members[0].health_conditions[0];
        assert!(cond.check_in);
        assert_eq!(cond.lag_window, [1, 3]);
        assert!(loaded.family.members[0]
            .health_condition_warnings()
            .is_empty());
    }

    #[test]
    fn test_load_missing_config_errors() {
        let err =
            load_config_for_provider("non_existent_file.yaml", ChatProvider::Telegram).unwrap_err();
        assert!(err.contains("not found"), "{err}");
    }

    #[test]
    fn test_load_legacy_telegram_chat_id_errors() {
        let legacy = r#"
family:
  members:
    - id: alex
      name: Alex
      role: adult
      telegram_chat_id: 424242
"#;
        let mut tmp = NamedTempFile::new().unwrap();
        write!(tmp, "{}", legacy).unwrap();
        let err = load_config_for_provider(tmp.path(), ChatProvider::Telegram).unwrap_err();
        assert!(
            err.contains("telegram_chat_id") || err.contains("Failed to parse"),
            "{err}"
        );
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
            fitness_goals: None,
            health_conditions: vec![],
            chat_ids: ProviderIds::default(),
            signal_aci: None,
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

    fn two_member_config() -> AppConfig {
        let mut config = AppConfig::default();
        config.family.members[0].chat_ids = ProviderIds {
            signal: Some("00000000-0000-0000-0000-000000000001".into()),
            telegram: Some("101".into()),
        };
        config.family.members.push(FamilyMember {
            id: "jordan".to_string(),
            name: "Jordan".to_string(),
            role: "adult".to_string(),
            calendar: None,
            nutrition_goals: None,
            fitness_goals: None,
            health_conditions: vec![],
            chat_ids: ProviderIds {
                signal: Some("00000000-0000-0000-0000-000000000002".into()),
                telegram: Some("202".into()),
            },
            signal_aci: None,
        });
        config.chat.household_ids = ProviderIds {
            signal: Some("aG91c2Vob2xk".into()),
            telegram: Some("-100303".into()),
        };
        config
    }

    #[test]
    fn member_lookup_and_authorization_are_provider_scoped() {
        let config = two_member_config();
        assert_eq!(
            member_for_chat_id(
                &config,
                ChatProvider::Signal,
                "00000000-0000-0000-0000-000000000001"
            )
            .map(|member| member.id.as_str()),
            Some("alex")
        );
        assert_eq!(
            member_for_chat_id(&config, ChatProvider::Telegram, "101")
                .map(|member| member.id.as_str()),
            Some("alex")
        );
        assert!(member_for_chat_id(&config, ChatProvider::Signal, "101").is_none());

        assert!(is_chat_conversation_allowed(
            &config,
            &ChatAddress::direct(ChatProvider::Telegram, "101")
        ));
        assert!(!is_chat_conversation_allowed(
            &config,
            &ChatAddress::direct(ChatProvider::Telegram, "999")
        ));
        assert!(is_chat_conversation_allowed(
            &config,
            &ChatAddress::group(ChatProvider::Telegram, "-100303")
        ));
        assert!(!is_chat_conversation_allowed(
            &config,
            &ChatAddress::group(ChatProvider::Telegram, "-100404")
        ));
    }

    #[test]
    fn private_mutation_and_delivery_stay_on_selected_provider() {
        let config = two_member_config();
        assert!(
            ensure_food_mutation_allowed(&config, ChatProvider::Telegram, "101", "alex").is_ok()
        );
        assert!(
            ensure_food_mutation_allowed(&config, ChatProvider::Telegram, "101", "jordan").is_err()
        );
        assert_eq!(
            chat_delivery_targets(&config, ChatProvider::Telegram),
            vec![
                ChatAddress::direct(ChatProvider::Telegram, "101"),
                ChatAddress::direct(ChatProvider::Telegram, "202"),
                ChatAddress::group(ChatProvider::Telegram, "-100303"),
            ]
        );
    }

    #[test]
    fn legacy_signal_id_loads_and_serializes_only_new_shape() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        with_env_var("SIGNAL_GROUP_ID", Some("aG91c2Vob2xk"), || {
            let yaml = r#"
family:
  members:
    - id: alex
      name: Alex
      role: adult
      signal_aci: 00000000-0000-0000-0000-000000000001
"#;
            let mut file = NamedTempFile::new().unwrap();
            write!(file, "{yaml}").unwrap();
            let loaded = load_config_for_provider(file.path(), ChatProvider::Signal).unwrap();
            assert_eq!(
                loaded.family.members[0].chat_ids.signal.as_deref(),
                Some("00000000-0000-0000-0000-000000000001")
            );
            assert!(loaded.family.members[0].signal_aci.is_none());
            assert_eq!(
                loaded.chat.household_ids.signal.as_deref(),
                Some("aG91c2Vob2xk")
            );
            let serialized = serde_yaml::to_string(&loaded).unwrap();
            assert!(serialized.contains("chat_ids:"));
            assert!(!serialized.contains("signal_aci:"));
        });
    }

    #[test]
    fn conflicting_and_malformed_chat_ids_fail_loading() {
        let conflicting = r#"
family:
  members:
    - id: alex
      name: Alex
      role: adult
      signal_aci: 00000000-0000-0000-0000-000000000001
      chat_ids:
        signal: 00000000-0000-0000-0000-000000000002
"#;
        let mut file = NamedTempFile::new().unwrap();
        write!(file, "{conflicting}").unwrap();
        assert!(load_config_for_provider(file.path(), ChatProvider::Signal)
            .unwrap_err()
            .contains("conflicting"));

        let malformed = r#"
family:
  members:
    - id: alex
      name: Alex
      role: adult
      chat_ids:
        telegram: not-a-number
"#;
        let mut file = NamedTempFile::new().unwrap();
        write!(file, "{malformed}").unwrap();
        assert!(
            load_config_for_provider(file.path(), ChatProvider::Telegram)
                .unwrap_err()
                .contains("signed decimal")
        );
    }

    #[test]
    fn telegram_chat_kind_requires_matching_id_sign() {
        assert!(validate_direct_id(ChatProvider::Telegram, "101").is_ok());
        assert!(validate_direct_id(ChatProvider::Telegram, "-101").is_err());
        assert!(validate_direct_id(ChatProvider::Telegram, "0").is_err());
        assert!(validate_household_id(ChatProvider::Telegram, "-100303").is_ok());
        assert!(validate_household_id(ChatProvider::Telegram, "101").is_err());
        assert!(validate_household_id(ChatProvider::Telegram, "0").is_err());
    }

    #[test]
    fn duplicate_selected_provider_ids_fail_loading() {
        let yaml = r#"
family:
  members:
    - id: alex
      name: Alex
      role: adult
      chat_ids:
        telegram: "101"
    - id: jordan
      name: Jordan
      role: adult
      chat_ids:
        telegram: "101"
"#;
        let mut file = NamedTempFile::new().unwrap();
        write!(file, "{yaml}").unwrap();
        assert!(
            load_config_for_provider(file.path(), ChatProvider::Telegram)
                .unwrap_err()
                .contains("duplicate telegram direct id")
        );
    }
    #[test]
    fn example_config_loads_for_each_provider() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        with_env_var("SIGNAL_GROUP_ID", None, || {
            let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .unwrap()
                .join("config.yaml.example");
            load_config_for_provider(&path, ChatProvider::Signal).unwrap();
            load_config_for_provider(&path, ChatProvider::Telegram).unwrap();
        });
    }
}
