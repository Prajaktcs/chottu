pub mod agenda;
pub mod database;
pub mod due_parse;
pub mod family;
pub mod google_health;
pub mod ledger;
pub mod llm;
pub mod memory;
pub mod models;
pub mod oauth;
pub mod calendar;
pub mod open_food_facts;
pub mod quotes;
pub mod spend_budget;

pub use database::init_db;
pub use due_parse::{
    is_due_for_reminder, parse_due_phrase, split_task_add_args, ParsedDue,
};
pub use family::{
    health_refresh_token_env_key, load_config, resolve_health_refresh_token, AppConfig,
    FamilyMember, FamilySection, InvestmentPhilosophy, TargetAllocation, AllocationBucket,
    BucketHolding, SpendBudgets, fetch_exchange_rates, CalendarConfig, NutritionGoals,
};
pub use agenda::{
    compose_calendar_agenda, escape_md, fetch_family_events, find_conflicts,
    format_brief_calendar_section, local_day_bounds_utc, truncate, week_bounds_utc,
    CalendarConflict, CalendarWindow, FamilyCalendarError, FamilyEventsFetch,
};
pub use calendar::{
    build_calendar_client, default_calendar_timezone, schedule_timed_block, CalendarError,
    CalendarEvent, GoogleCalendarClient,
};
pub use google_health::{
    GoogleHealthClient, GoogleHealthFoodSummary, NutritionLogWrite, GOOGLE_HEALTH_OAUTH_SCOPES,
};
pub use ledger::{
    looks_like_non_transaction_alert, validate_ledger_amount, LedgerAmountReject,
    LEDGER_ABS_AMOUNT_HARD_MAX, LEDGER_USD_EQUIV_MAX,
};
pub use llm::{
    ChotuLlm, GeminiClient, OpenRouterClient, LlmError, NutritionEstimation, MissingSyncNutrition,
    LedgerExtraction, ActionItemExtraction, TravelItineraryExtraction, UpcomingBillExtraction,
    PersonalReferenceExtraction, IntentKind, IntentClassification, UserIntent,
    FoodPhotoAnalysis, FoodPhotoKind,
};
pub use open_food_facts::{lookup_barcode, OpenFoodFactsProduct};
pub use quotes::{fetch_stock_quotes, StockQuote, QuoteError};
pub use memory::{
    answer_memory_query, brain_dir, format_hit_list, spawn_background_reindex, MemoryHit,
    MemoryIndex, ReindexStats, SourceType, DEFAULT_EMBED_MODEL,
};
pub use models::{
    EmailClassification, EmailMetadata, EvaluationLog, FinancialLedgerEntry, FoodLog,
    HealthFamilySummary, OllamaClassificationResponse, PendingDocument, PortfolioHolding,
    DroppedDocumentType, ExtractedPortfolioHolding, DroppedDocumentExtraction,
};
pub use oauth::{
    exchange_google_code, format_xoauth2_string, refresh_oauth2_token, save_calendar_refresh_token,
    save_google_health_refresh_token, save_google_refresh_token, save_health_refresh_token,
    start_redirect_listener, FitbitTokenResponse, GoogleInitialTokenResponse, GoogleTokenResponse,
    OAuthError,
};
pub use spend_budget::{
    clear_budget_override, compute_budget_progress, current_budget_month, display_category,
    effective_budgets, format_budget_progress_markdown, mark_budget_alert_sent,
    pending_budget_alerts, set_budget_override, BudgetAlert, BudgetProgress, BUDGET_THRESHOLDS,
};

#[cfg(test)]
mod safety_tests {
    use std::fs;
    use std::path::PathBuf;

    #[test]
    fn test_prevent_email_sending_dependencies() {
        let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set");
        let root_dir = PathBuf::from(manifest_dir).parent().unwrap().to_path_buf();

        let paths_to_check = vec![
            root_dir.join("Cargo.toml"),
            root_dir.join("chotu-common").join("Cargo.toml"),
            root_dir.join("streamer").join("Cargo.toml"),
            root_dir.join("janitor").join("Cargo.toml"),
            root_dir.join("coordinator").join("Cargo.toml"),
            root_dir.join("health-coach").join("Cargo.toml"),
            root_dir.join("chotu-evals").join("Cargo.toml"),
        ];

        let forbidden_deps = vec![
            "lettre",
            "mail-send",
            "sendgrid",
            "smtp",
            "mailersend",
            "resend",
            "postal",
            "mailgun",
        ];

        for path in paths_to_check {
            if path.exists() {
                let content = fs::read_to_string(&path)
                    .unwrap_or_else(|_| panic!("Failed to read {:?}", path));
                
                for line in content.lines() {
                    let cleaned = line.trim().to_lowercase();
                    if cleaned.starts_with('#') {
                        continue;
                    }
                    for forbidden in &forbidden_deps {
                        if cleaned.contains(forbidden) {
                            panic!(
                                "SAFETY VIOLATION: Dependency '{}' detected in {:?}. Email sending libraries are forbidden in Project Chotu.",
                                forbidden, path
                            );
                        }
                    }
                }
            }
        }
    }
}
