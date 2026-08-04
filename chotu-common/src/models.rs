use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct FinancialLedgerEntry {
    pub id: String,
    pub timestamp: DateTime<Utc>,
    pub amount: f64,
    pub currency: String,
    pub institution: String,
    pub merchant: String,
    pub category: String,
    pub source_type: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct PortfolioHolding {
    pub ticker: String,
    pub shares_owned: f64,
    pub average_cost: f64,
    /// ISO currency for `average_cost` when known (from statement extraction).
    /// `None` means unknown — callers should fall back carefully.
    pub average_cost_currency: Option<String>,
    pub last_updated: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum EmailClassification {
    Trash,
    Archive,
    LedgerStream,
    ActionItem,
    TravelItinerary,
    FinancialBill,
    StatementDocument,
    Newsletter,
    PersonalReference,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmailMetadata {
    pub sender: String,
    pub subject: String,
    pub body_preview: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct OllamaClassificationResponse {
    pub classification: EmailClassification,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct HealthFamilySummary {
    pub date: String,
    pub family_member_id: String,
    pub total_calories_ingested: i32,
    pub protein_grams: f64,
    pub carbs_grams: f64,
    pub fats_grams: f64,
    pub step_count: i32,
    pub active_calories_burned: i32,
    pub sleep_hours: Option<f64>,
    pub perceived_energy: Option<i32>,
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
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct FoodLog {
    pub id: String,
    pub timestamp: DateTime<Utc>,
    pub family_member_id: String,
    pub raw_text_description: String,
    pub estimated_calories: i32,
    pub estimated_protein: f64,
    pub estimated_carbs: f64,
    pub estimated_fats: f64,
    pub estimated_omega_3_dha_mg: f64,
    pub estimated_cholesterol_mg: f64,
    pub estimated_saturated_fat_g: f64,
    pub estimated_unsaturated_fat_g: f64,
    pub estimated_triglycerides_mg: f64,
    pub estimated_iron_mg: f64,
    pub estimated_vitamin_b_mg: f64,
    pub estimated_vitamin_c_mg: f64,
    pub estimated_sugar_g: f64,
    pub estimated_fiber_g: f64,
    pub estimated_sodium_mg: f64,
    pub estimated_potassium_mg: f64,
    pub estimated_calcium_mg: f64,
    pub estimated_magnesium_mg: f64,
    pub estimated_zinc_mg: f64,
    pub estimated_vitamin_a_mcg: f64,
    pub estimated_vitamin_d_mcg: f64,
    pub estimated_vitamin_e_mg: f64,
    pub estimated_vitamin_k_mcg: f64,
    pub estimated_caffeine_mg: f64,
    pub estimated_trans_fat_g: f64,
    /// Full Google Health resource name when this entry was pushed upstream.
    /// `None` means local-only / not yet synced (additive on `/sync`).
    #[serde(default)]
    pub google_data_point_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct EvaluationLog {
    pub eval_id: String,
    pub test_timestamp: DateTime<Utc>,
    pub prompt_version: String,
    pub model_name: String,
    pub triage_accuracy: f64,
    pub extraction_faithfulness: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct PendingDocument {
    pub id: String,
    pub filename: String,
    pub filepath: String,
    pub status: String,
    pub received_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum DroppedDocumentType {
    Receipt,
    Portfolio,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ExtractedPortfolioHolding {
    pub ticker: String,
    pub shares_owned: f64,
    pub average_cost: f64,
    /// Currency of `average_cost` as shown on the statement (e.g. USD, CAD).
    #[serde(default)]
    pub average_cost_currency: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DroppedDocumentExtraction {
    pub document_type: DroppedDocumentType,
    pub receipt_transaction: Option<crate::llm::LedgerExtraction>,
    pub portfolio_holdings: Option<Vec<ExtractedPortfolioHolding>>,
}
