#![allow(deprecated)]

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Context;
use sqlx::SqlitePool;
use teloxide::prelude::*;
use teloxide::utils::command::BotCommands;
use tokio::sync::RwLock;

use chotu_common::{
    answer_memory_query, compose_calendar_agenda, exchange_google_code, fetch_exchange_rates,
    lookup_barcode, save_calendar_refresh_token, save_google_refresh_token,
    save_health_refresh_token, spawn_background_reindex, start_redirect_listener, AppConfig,
    CalendarWindow, ChotuLlm, FoodPhotoKind, GeminiClient, InvestmentPhilosophy, MemoryIndex,
    UserIntent,
};
use finance_advisor::{run_stock_research, StockResearcher};
use teloxide::net::Download;

#[derive(BotCommands, Clone)]
#[command(
    rename_rule = "lowercase",
    description = "These commands are supported:"
)]
pub enum Command {
    #[command(description = "display this text.")]
    Help,
    #[command(description = "log food. Usage: /food [member_id] <description>")]
    Food(String),
    #[command(description = "show today's status report.")]
    Status,
    #[command(description = "morning brief: calendar, tasks, bills, nutrition.")]
    Brief,
    #[command(description = "calendar agenda. Usage: /cal [today|tomorrow|week]")]
    Cal(String),
    #[command(description = "show multi-day nutrition trends. Usage: /trends [days]")]
    Trends(String),
    #[command(description = "list/manage tasks. Usage: /tasks [open|all|completed|snoozed] [|member]; /tasks complete|snooze|reassign|open <id> ...")]
    Tasks(String),
    #[command(description = "search personal memory (journals, digests, references, tasks). Usage: /memory <question> | /memory reindex")]
    Memory(String),
    #[command(description = "trigger evening reflection loop.")]
    Reflect,
    #[command(description = "show the current chat ID.")]
    Chat,
    #[command(
        description = "trigger stock hundred-bagger research. Usage: /research [optional_companies]"
    )]
    Research(String),
    #[command(description = "sync today's health metrics from Google Health.")]
    Sync,
    #[command(description = "login to Google Health, Gmail, or Calendar. Usage: /login <health <member_id>|gmail|calendar <member_id>> or /login code <...>")]
    Login(String),
    #[command(description = "clear today's food logs and summary. Usage: /clearfood [member_id]")]
    Clearfood(String),
    #[command(description = "manually override today's nutrition totals. Usage: /adjustfood [member_id] <calories> <protein> <carbs> <fats>")]
    Adjustfood(String),
    #[command(description = "delete the last food log entry and update today's summary. Usage: /undofood [member_id]")]
    Undofood(String),
    #[command(description = "show invested net worth (portfolio; cash not tracked yet).")]
    Networth,
    #[command(description = "show monthly transaction summary. Usage: /monthly [YYYY-MM]")]
    Monthly(String),
    #[command(description = "set stock portfolio holdings. Usage: /holdings <ticker>:<shares>:<avg_cost> ...")]
    Holdings(String),
}

#[derive(Debug, Clone)]
pub enum ConversationState {
    Idle,
    WaitingForReflection { date: String, prompt: String },
}

type StateMap = Arc<RwLock<HashMap<ChatId, ConversationState>>>;

pub async fn start_telegram_bot(
    pool: SqlitePool,
    llm: ChotuLlm,
    gemini_key: String,
    config: AppConfig,
) -> Result<(), anyhow::Error> {
    let token = std::env::var("TELEGRAM_BOT_TOKEN")
        .or_else(|_| std::env::var("TELOXIDE_TOKEN"))
        .context("Neither TELEGRAM_BOT_TOKEN nor TELOXIDE_TOKEN environment variable is set")?;
    let bot = Bot::new(token);
    let gemini_client = GeminiClient::new(gemini_key.clone());
    let researcher = StockResearcher::new(gemini_key);
    let conversation_states: StateMap = Arc::new(RwLock::new(HashMap::new()));

    // Spawn proactive evening reflection scheduler if TELEGRAM_CHAT_ID is set
    let sched_bot = bot.clone();
    let sched_pool = pool.clone();
    let sched_llm = llm.clone();
    let sched_states = conversation_states.clone();
    let sched_config = config.clone();
    tokio::spawn(async move {
        use chrono::Timelike;
        if let Ok(chat_id_val) = std::env::var("TELEGRAM_CHAT_ID") {
            if let Ok(chat_id_num) = chat_id_val.parse::<i64>() {
                let chat_id = ChatId(chat_id_num);
                println!(
                    "Telegram Bot: Proactive scheduler enabled for ChatId {:?}",
                    chat_id
                );
                let mut last_sent_date = String::new();
                loop {
                    let now = chrono::Local::now();
                    let date_str = now.format("%Y-%m-%d").to_string();
                    if now.hour() == 21 && now.minute() == 0 && date_str != last_sent_date {
                        println!("Telegram Bot: Scheduled time (9:00 PM) reached. Pushing evening reflection...");
                        if let Err(e) = handle_reflect_trigger(
                            &sched_bot,
                            chat_id,
                            &sched_pool,
                            &sched_llm,
                            sched_states.clone(),
                            &sched_config,
                        )
                        .await
                        {
                            eprintln!("Telegram Bot: failed to push scheduled reflection: {:?}", e);
                        } else {
                            last_sent_date = date_str;
                        }
                    }
                    tokio::time::sleep(tokio::time::Duration::from_secs(60)).await;
                }
            }
        } else {
            println!("Telegram Bot: TELEGRAM_CHAT_ID not configured. Proactive scheduled evening reflections are disabled. Use /reflect manually.");
        }
    });

    // Spawn proactive evening stock research scheduler if TELEGRAM_CHAT_ID is set
    let stock_bot = bot.clone();
    let stock_pool = pool.clone();
    let stock_researcher = researcher.clone();
    let stock_config = config.clone();
    tokio::spawn(async move {
        use chrono::Timelike;
        if let Ok(chat_id_val) = std::env::var("TELEGRAM_CHAT_ID") {
            if let Ok(chat_id_num) = chat_id_val.parse::<i64>() {
                let chat_id = ChatId(chat_id_num);
                println!(
                    "Telegram Bot: Stock researcher scheduled loop enabled for ChatId {:?}",
                    chat_id
                );
                let mut last_run_date = String::new();
                loop {
                    let now = chrono::Local::now();
                    let date_str = now.format("%Y-%m-%d").to_string();
                    // Trigger at 6:00 PM (18:00) every evening
                    if now.hour() == 18 && now.minute() == 0 && date_str != last_run_date {
                        println!("Telegram Bot: Scheduled time (6:00 PM) reached. Running evening stock research...");
                        if let Err(e) = run_and_log_stock_research(
                            &stock_bot,
                            chat_id,
                            &stock_pool,
                            &stock_researcher,
                            stock_config.investment_philosophy.as_ref(),
                            None,
                        )
                        .await
                        {
                            eprintln!(
                                "Telegram Bot: failed to run scheduled stock research: {:?}",
                                e
                            );
                        } else {
                            last_run_date = date_str;
                        }
                    }
                    tokio::time::sleep(tokio::time::Duration::from_secs(60)).await;
                }
            }
        } else {
            println!(
                "Telegram Bot: TELEGRAM_CHAT_ID not configured. Scheduled stock research disabled."
            );
        }
    });

    // Spawn proactive morning brief scheduler if TELEGRAM_CHAT_ID is set
    let brief_bot = bot.clone();
    let brief_pool = pool.clone();
    let brief_config = config.clone();
    tokio::spawn(async move {
        use chrono::Timelike;
        let brief_hour: u32 = std::env::var("MORNING_BRIEF_HOUR")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(7)
            .min(23);
        if let Ok(chat_id_val) = std::env::var("TELEGRAM_CHAT_ID") {
            if let Ok(chat_id_num) = chat_id_val.parse::<i64>() {
                let chat_id = ChatId(chat_id_num);
                println!(
                    "Telegram Bot: Morning brief scheduler enabled for ChatId {:?} at {:02}:00",
                    chat_id, brief_hour
                );
                let mut last_sent_date = String::new();
                loop {
                    let now = chrono::Local::now();
                    let date_str = now.format("%Y-%m-%d").to_string();
                    if now.hour() == brief_hour
                        && now.minute() == 0
                        && date_str != last_sent_date
                    {
                        println!(
                            "Telegram Bot: Scheduled time ({:02}:00) reached. Pushing morning brief...",
                            brief_hour
                        );
                        if let Err(e) =
                            handle_brief(&brief_bot, chat_id, &brief_pool, &brief_config).await
                        {
                            eprintln!("Telegram Bot: failed to push morning brief: {:?}", e);
                        } else {
                            last_sent_date = date_str;
                        }
                    }
                    tokio::time::sleep(tokio::time::Duration::from_secs(60)).await;
                }
            }
        } else {
            println!(
                "Telegram Bot: TELEGRAM_CHAT_ID not configured. Scheduled morning brief disabled. Use /brief manually."
            );
        }
    });

    // Scheduled Google Health sync is owned by the Health Coach agent (8:45 PM).
    println!("Telegram Bot: Google Health scheduled sync is handled by the Health Coach agent.");

    // Background catch-up for local memory RAG index (journals / digests / refs / tasks).
    spawn_background_reindex(pool.clone());

    let handler = dptree::entry().branch(
        Update::filter_message()
            .branch(
                dptree::entry()
                    .filter_command::<Command>()
                    .endpoint(handle_command),
            )
            .branch(dptree::endpoint(handle_message)),
    );

    println!("Telegram Bot: starting update loop...");
    Dispatcher::builder(bot, handler)
        .dependencies(dptree::deps![
            pool,
            llm,
            gemini_client,
            researcher,
            conversation_states,
            config
        ])
        .enable_ctrlc_handler()
        .build()
        .dispatch()
        .await;

    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn handle_command(
    bot: Bot,
    msg: Message,
    cmd: Command,
    pool: SqlitePool,
    llm: ChotuLlm,
    gemini_client: GeminiClient,
    researcher: StockResearcher,
    states: StateMap,
    config: AppConfig,
) -> Result<(), teloxide::RequestError> {
    let chat_id = msg.chat.id;
    let username = msg
        .from()
        .as_ref()
        .and_then(|u| u.username.as_deref())
        .unwrap_or("unknown");
    println!(
        "Telegram Bot: Received command from user '{}' in Chat ID: {:?}",
        username, chat_id
    );

    // Reset reflection state if they send a command
    {
        let mut s = states.write().await;
        s.insert(chat_id, ConversationState::Idle);
    }

    match cmd {
        Command::Help => {
            bot.send_message(chat_id, Command::descriptions().to_string())
                .await?;
        }
        Command::Food(args) => {
            handle_food_log(&bot, chat_id, args, &pool, &gemini_client, &config).await?;
        }
        Command::Status => {
            handle_status(&bot, chat_id, &pool, &config).await?;
        }
        Command::Brief => {
            handle_brief(&bot, chat_id, &pool, &config).await?;
        }
        Command::Cal(args) => {
            handle_cal(&bot, chat_id, args, &config).await?;
        }
        Command::Trends(args) => {
            handle_trends(&bot, chat_id, args, &pool, &config).await?;
        }
        Command::Tasks(args) => {
            handle_tasks(&bot, chat_id, args, &pool, &config).await?;
        }
        Command::Memory(args) => {
            handle_memory(&bot, chat_id, args, &pool, &llm, &gemini_client).await?;
        }
        Command::Reflect => {
            handle_reflect_trigger(&bot, chat_id, &pool, &llm, states, &config).await?;
        }
        Command::Chat => {
            bot.send_message(chat_id, format!("Current Chat ID: {}", chat_id))
                .await?;
        }
        Command::Research(args) => {
            let targets = if args.trim().is_empty() {
                None
            } else {
                Some(args.as_str())
            };
            if let Err(e) =
                run_and_log_stock_research(&bot, chat_id, &pool, &researcher, config.investment_philosophy.as_ref(), targets).await
            {
                eprintln!(
                    "Telegram Bot: manual stock research trigger failed: {:?}",
                    e
                );
                let _ = bot
                    .send_message(chat_id, format!("❌ Stock research failed: {}", e))
                    .await;
            }
        }
        Command::Sync => {
            if let Err(e) = sync_google_health_nutrition(&bot, chat_id, &pool, &gemini_client, &config).await {
                eprintln!("Telegram Bot: manual Google Health sync failed: {:?}", e);
                let _ = bot
                    .send_message(chat_id, format!("❌ Google Health sync failed: {}", e))
                    .await;
            }
        }
        Command::Login(args) => {
            let args_trimmed = args.trim();
            if args_trimmed.to_lowercase().starts_with("code") {
                let rest = args_trimmed[4..].trim();
                if let Err(e) = handle_manual_code(&bot, chat_id, rest, &config).await {
                    eprintln!("Telegram Bot: manual code exchange failed: {:?}", e);
                    let _ = bot.send_message(chat_id, format!("❌ Manual code exchange failed: {}", e)).await;
                }
            } else {
                let lower = args_trimmed.to_lowercase();
                let mut parts = lower.split_whitespace();
                let service = parts.next().unwrap_or("");
                if service == "fitbit" || service == "health" {
                    let original_member = args_trimmed
                        .split_whitespace()
                        .nth(1)
                        .unwrap_or("")
                        .to_string();
                    if let Err(e) =
                        handle_login_google_health(&bot, chat_id, &original_member, &config).await
                    {
                        eprintln!("Telegram Bot: Google Health login initialization failed: {:?}", e);
                        let _ = bot
                            .send_message(chat_id, format!("❌ Google Health login failed: {}", e))
                            .await;
                    }
                } else if service == "gmail" || service == "google" {
                    if let Err(e) = handle_login_google(&bot, chat_id).await {
                        eprintln!(
                            "Telegram Bot: Google/Gmail login initialization failed: {:?}",
                            e
                        );
                        let _ = bot
                            .send_message(chat_id, format!("❌ Google/Gmail login failed: {}", e))
                            .await;
                    }
                } else if service == "calendar" {
                    let member_id = parts.next().unwrap_or("").to_string();
                    // Preserve original casing from args for member lookup
                    let original_member = args_trimmed
                        .split_whitespace()
                        .nth(1)
                        .unwrap_or("")
                        .to_string();
                    let member_id = if original_member.is_empty() {
                        member_id
                    } else {
                        original_member
                    };
                    if let Err(e) = handle_login_calendar(&bot, chat_id, &member_id, &config).await {
                        eprintln!("Telegram Bot: Calendar login initialization failed: {:?}", e);
                        let _ = bot
                            .send_message(chat_id, format!("❌ Calendar login failed: {}", e))
                            .await;
                    }
                } else {
                    let _ = bot
                        .send_message(
                            chat_id,
                            "⚠️ Invalid service. Usage: `/login health <member_id>`, `/login gmail`, `/login calendar <member_id>`, or `/login code ...`",
                        )
                        .parse_mode(teloxide::types::ParseMode::Markdown)
                        .await;
                }
            }
        }
        Command::Clearfood(args) => {
            handle_clear_food(&bot, chat_id, args, &pool, &config).await?;
        }
        Command::Adjustfood(args) => {
            handle_adjust_food(&bot, chat_id, args, &pool, &config).await?;
        }
        Command::Undofood(args) => {
            handle_undo_food(&bot, chat_id, args, &pool, &config).await?;
        }
        Command::Networth => {
            handle_networth(&bot, chat_id, &pool, &gemini_client, &config).await?;
        }
        Command::Monthly(args) => {
            handle_monthly(&bot, chat_id, args, &pool, &config).await?;
        }
        Command::Holdings(args) => {
            handle_holdings(&bot, chat_id, args, &pool).await?;
        }
    }

    Ok(())
}

async fn handle_food_log(
    bot: &Bot,
    chat_id: ChatId,
    args: String,
    pool: &SqlitePool,
    gemini_client: &GeminiClient,
    config: &AppConfig,
) -> Result<(), teloxide::RequestError> {
    let args = args.trim();
    if args.is_empty() {
        let members_list = config
            .family
            .members
            .iter()
            .map(|m| format!("- {} ({})", m.id, m.name))
            .collect::<Vec<String>>()
            .join("\n");
        bot.send_message(
            chat_id,
            format!("Please provide a description, e.g. /food [member_id] <description>\n\nConfigured family members:\n{}", members_list)
        ).await?;
        return Ok(());
    }

    let (family_member_id, food_description) = resolve_food_member_and_description(args, config);
    if food_description.is_empty() {
        bot.send_message(
            chat_id,
            format!(
                "Please provide a food description after the member ID. E.g. /food {} salad",
                family_member_id
            ),
        )
        .await?;
        return Ok(());
    }

    bot.send_message(
        chat_id,
        format!("Estimating nutrition for {}...", family_member_id),
    )
    .await?;

    match gemini_client.approximate_nutrition(&food_description).await {
        Ok(est) => {
            persist_food_estimation(
                bot,
                chat_id,
                pool,
                config,
                &family_member_id,
                &food_description,
                &est,
            )
            .await?;
        }
        Err(e) => {
            eprintln!("Gemini client error: {:?}", e);
            bot.send_message(chat_id, format!("❌ Failed to estimate nutrition: {}", e))
                .await?;
        }
    }

    Ok(())
}

/// Parse optional leading member id from `/food` args or a photo caption.
fn resolve_food_member_and_description(args: &str, config: &AppConfig) -> (String, String) {
    let mut parts = args.splitn(2, |c: char| c.is_whitespace());
    let first_word = parts.next().unwrap_or("");
    let rest = parts.next().unwrap_or("").trim();

    for member in &config.family.members {
        if member.id.eq_ignore_ascii_case(first_word) {
            return (member.id.clone(), rest.to_string());
        }
    }

    let primary_member = &config.family.members[0];
    (primary_member.id.clone(), args.trim().to_string())
}

/// Insert food_log, update day totals, optionally push to Google Health, reply macros-first.
async fn persist_food_estimation(
    bot: &Bot,
    chat_id: ChatId,
    pool: &SqlitePool,
    config: &AppConfig,
    family_member_id: &str,
    food_description: &str,
    est: &chotu_common::NutritionEstimation,
) -> Result<(), teloxide::RequestError> {
    let log_id = uuid::Uuid::new_v4().to_string();
    let now = chrono::Utc::now();
    let date_str = chrono::Local::now().format("%Y-%m-%d").to_string();

    if let Err(e) = sqlx::query(
        "INSERT INTO food_log (id, timestamp, family_member_id, raw_text_description, \
         estimated_calories, estimated_protein, estimated_carbs, estimated_fats, \
         estimated_omega_3_dha_mg, estimated_cholesterol_mg, estimated_saturated_fat_g, estimated_unsaturated_fat_g, estimated_triglycerides_mg, \
         estimated_iron_mg, estimated_vitamin_b_mg, estimated_vitamin_c_mg, \
         estimated_sugar_g, estimated_fiber_g, estimated_sodium_mg, estimated_potassium_mg, estimated_calcium_mg, \
         estimated_magnesium_mg, estimated_zinc_mg, estimated_vitamin_a_mcg, estimated_vitamin_d_mcg, estimated_vitamin_e_mg, \
         estimated_vitamin_k_mcg, estimated_caffeine_mg, estimated_trans_fat_g) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&log_id)
    .bind(now)
    .bind(family_member_id)
    .bind(food_description)
    .bind(est.total_calories)
    .bind(est.protein_grams)
    .bind(est.carbs_grams)
    .bind(est.fats_grams)
    .bind(est.omega_3_dha_mg)
    .bind(est.cholesterol_mg)
    .bind(est.saturated_fat_g)
    .bind(est.unsaturated_fat_g)
    .bind(est.triglycerides_mg)
    .bind(est.iron_mg)
    .bind(est.vitamin_b_mg)
    .bind(est.vitamin_c_mg)
    .bind(est.sugar_g)
    .bind(est.fiber_g)
    .bind(est.sodium_mg)
    .bind(est.potassium_mg)
    .bind(est.calcium_mg)
    .bind(est.magnesium_mg)
    .bind(est.zinc_mg)
    .bind(est.vitamin_a_mcg)
    .bind(est.vitamin_d_mcg)
    .bind(est.vitamin_e_mg)
    .bind(est.vitamin_k_mcg)
    .bind(est.caffeine_mg)
    .bind(est.trans_fat_g)
    .execute(pool)
    .await
    {
        eprintln!("Failed to insert into food_log: {:?}", e);
        bot.send_message(chat_id, "Database error saving food log.")
            .await?;
        return Ok(());
    }

    let mut google_sync_note = String::new();
    if health_coach::member_health_credentials_configured(family_member_id, config) {
        match health_coach::google_health_client_for_member(family_member_id, config) {
            Ok(client) => {
                let pending = chotu_common::FoodLog {
                    id: log_id.clone(),
                    timestamp: now,
                    family_member_id: family_member_id.to_string(),
                    raw_text_description: food_description.to_string(),
                    estimated_calories: est.total_calories,
                    estimated_protein: est.protein_grams,
                    estimated_carbs: est.carbs_grams,
                    estimated_fats: est.fats_grams,
                    estimated_omega_3_dha_mg: est.omega_3_dha_mg,
                    estimated_cholesterol_mg: est.cholesterol_mg,
                    estimated_saturated_fat_g: est.saturated_fat_g,
                    estimated_unsaturated_fat_g: est.unsaturated_fat_g,
                    estimated_triglycerides_mg: est.triglycerides_mg,
                    estimated_iron_mg: est.iron_mg,
                    estimated_vitamin_b_mg: est.vitamin_b_mg,
                    estimated_vitamin_c_mg: est.vitamin_c_mg,
                    estimated_sugar_g: est.sugar_g,
                    estimated_fiber_g: est.fiber_g,
                    estimated_sodium_mg: est.sodium_mg,
                    estimated_potassium_mg: est.potassium_mg,
                    estimated_calcium_mg: est.calcium_mg,
                    estimated_magnesium_mg: est.magnesium_mg,
                    estimated_zinc_mg: est.zinc_mg,
                    estimated_vitamin_a_mcg: est.vitamin_a_mcg,
                    estimated_vitamin_d_mcg: est.vitamin_d_mcg,
                    estimated_vitamin_e_mg: est.vitamin_e_mg,
                    estimated_vitamin_k_mcg: est.vitamin_k_mcg,
                    estimated_caffeine_mg: est.caffeine_mg,
                    estimated_trans_fat_g: est.trans_fat_g,
                    google_data_point_id: None,
                };
                match health_coach::push_food_log_to_google(pool, &client, &pending).await {
                    Ok(_) => {
                        google_sync_note = "\n_Synced to Google Health_".to_string();
                    }
                    Err(e) => {
                        eprintln!("Failed to push /food to Google Health: {:?}", e);
                        google_sync_note =
                            "\n_Saved locally; Google Health sync pending (retry on /sync)_"
                                .to_string();
                    }
                }
            }
            Err(e) => {
                eprintln!("Google Health client unavailable for /food push: {:?}", e);
            }
        }
    }

    if let Err(e) = sqlx::query(
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
                    total_calories_ingested = health_family_summary.total_calories_ingested + excluded.total_calories_ingested,
                    protein_grams = health_family_summary.protein_grams + excluded.protein_grams,
                    carbs_grams = health_family_summary.carbs_grams + excluded.carbs_grams,
                    fats_grams = health_family_summary.fats_grams + excluded.fats_grams,
                    omega_3_dha_mg = health_family_summary.omega_3_dha_mg + excluded.omega_3_dha_mg,
                    cholesterol_mg = health_family_summary.cholesterol_mg + excluded.cholesterol_mg,
                    saturated_fat_g = health_family_summary.saturated_fat_g + excluded.saturated_fat_g,
                    unsaturated_fat_g = health_family_summary.unsaturated_fat_g + excluded.unsaturated_fat_g,
                    triglycerides_mg = health_family_summary.triglycerides_mg + excluded.triglycerides_mg,
                    iron_mg = health_family_summary.iron_mg + excluded.iron_mg,
                    vitamin_b_mg = health_family_summary.vitamin_b_mg + excluded.vitamin_b_mg,
                    vitamin_c_mg = health_family_summary.vitamin_c_mg + excluded.vitamin_c_mg,
                    sugar_g = health_family_summary.sugar_g + excluded.sugar_g,
                    fiber_g = health_family_summary.fiber_g + excluded.fiber_g,
                    sodium_mg = health_family_summary.sodium_mg + excluded.sodium_mg,
                    potassium_mg = health_family_summary.potassium_mg + excluded.potassium_mg,
                    calcium_mg = health_family_summary.calcium_mg + excluded.calcium_mg,
                    magnesium_mg = health_family_summary.magnesium_mg + excluded.magnesium_mg,
                    zinc_mg = health_family_summary.zinc_mg + excluded.zinc_mg,
                    vitamin_a_mcg = health_family_summary.vitamin_a_mcg + excluded.vitamin_a_mcg,
                    vitamin_d_mcg = health_family_summary.vitamin_d_mcg + excluded.vitamin_d_mcg,
                    vitamin_e_mg = health_family_summary.vitamin_e_mg + excluded.vitamin_e_mg,
                    vitamin_k_mcg = health_family_summary.vitamin_k_mcg + excluded.vitamin_k_mcg,
                    caffeine_mg = health_family_summary.caffeine_mg + excluded.caffeine_mg,
                    trans_fat_g = health_family_summary.trans_fat_g + excluded.trans_fat_g;
                "#,
    )
    .bind(&date_str)
    .bind(family_member_id)
    .bind(est.total_calories)
    .bind(est.protein_grams)
    .bind(est.carbs_grams)
    .bind(est.fats_grams)
    .bind(est.omega_3_dha_mg)
    .bind(est.cholesterol_mg)
    .bind(est.saturated_fat_g)
    .bind(est.unsaturated_fat_g)
    .bind(est.triglycerides_mg)
    .bind(est.iron_mg)
    .bind(est.vitamin_b_mg)
    .bind(est.vitamin_c_mg)
    .bind(est.sugar_g)
    .bind(est.fiber_g)
    .bind(est.sodium_mg)
    .bind(est.potassium_mg)
    .bind(est.calcium_mg)
    .bind(est.magnesium_mg)
    .bind(est.zinc_mg)
    .bind(est.vitamin_a_mcg)
    .bind(est.vitamin_d_mcg)
    .bind(est.vitamin_e_mg)
    .bind(est.vitamin_k_mcg)
    .bind(est.caffeine_mg)
    .bind(est.trans_fat_g)
    .execute(pool)
    .await
    {
        eprintln!("Failed to upsert health_family_summary: {:?}", e);
        bot.send_message(chat_id, "Database error updating health summary.")
            .await?;
        return Ok(());
    }

    let day_totals: Option<(i32, f64, f64, f64)> = sqlx::query_as(
        "SELECT total_calories_ingested, protein_grams, carbs_grams, fats_grams \
         FROM health_family_summary WHERE date = ? AND family_member_id = ?",
    )
    .bind(&date_str)
    .bind(family_member_id)
    .fetch_optional(pool)
    .await
    .unwrap_or(None);

    let mut notable = Vec::new();
    if est.fiber_g > 0.0 {
        notable.push(format!("Fiber {:.0}g", est.fiber_g));
    }
    if est.sugar_g > 0.0 {
        notable.push(format!("Sugar {:.0}g", est.sugar_g));
    }
    if est.sodium_mg > 0.0 {
        notable.push(format!("Sodium {:.0}mg", est.sodium_mg));
    }
    if est.caffeine_mg > 0.0 {
        notable.push(format!("Caffeine {:.0}mg", est.caffeine_mg));
    }
    let notable_line = if notable.is_empty() {
        String::new()
    } else {
        format!("\n• {}", notable.join(" · "))
    };

    let day_line = match day_totals {
        Some((cal, p, c, f)) => format!(
            "\n\n*Today:* {} kcal · {:.0}g P / {:.0}g C / {:.0}g F",
            cal, p, c, f
        ),
        None => String::new(),
    };

    let msg_text = format!(
        "✅ Logged for *{}*: _{}_\n\
         • {} kcal · {:.1}g P / {:.1}g C / {:.1}g F ({}){}{}{}",
        family_member_id,
        food_description,
        est.total_calories,
        est.protein_grams,
        est.carbs_grams,
        est.fats_grams,
        est.dominant_macro,
        notable_line,
        day_line,
        google_sync_note
    );

    bot.send_message(chat_id, msg_text)
        .parse_mode(teloxide::types::ParseMode::Markdown)
        .await?;

    Ok(())
}

async fn handle_clear_food(
    bot: &Bot,
    chat_id: ChatId,
    args: String,
    pool: &SqlitePool,
    config: &AppConfig,
) -> Result<(), teloxide::RequestError> {
    let member_id = args.trim();
    let target_member_id = if member_id.is_empty() {
        config.family.members[0].id.clone()
    } else {
        let mut found = config.family.members[0].id.clone();
        for m in &config.family.members {
            if m.id.eq_ignore_ascii_case(member_id) {
                found = m.id.clone();
                break;
            }
        }
        found
    };

    let date_str = chrono::Local::now().format("%Y-%m-%d").to_string();

    // Preserve Google Health (or other non-food_log) nutrition, then drop Telegram logs.
    let external = match health_coach::external_nutrition_base(pool, &target_member_id, &date_str).await
    {
        Ok(v) => v,
        Err(e) => {
            eprintln!("Failed to compute external nutrition base: {:?}", e);
            bot.send_message(chat_id, "❌ Database error reading today's summary.")
                .await?;
            return Ok(());
        }
    };

    // Remove any meals we previously pushed to Google Health.
    match health_coach::google_data_point_ids_for_day(pool, &target_member_id, &date_str).await {
        Ok(ids) => {
            if let Err(e) =
                health_coach::delete_google_nutrition_logs(&target_member_id, config, &ids).await
            {
                eprintln!("Failed to delete Google Health nutrition logs on clear: {:?}", e);
            }
        }
        Err(e) => eprintln!("Failed to list Google Health nutrition log IDs: {:?}", e),
    }

    if let Err(e) = sqlx::query(
        "DELETE FROM food_log WHERE family_member_id = ? AND date(timestamp, 'localtime') = ?",
    )
    .bind(&target_member_id)
    .bind(&date_str)
    .execute(pool)
    .await
    {
        eprintln!("Failed to clear food_log: {:?}", e);
        bot.send_message(chat_id, "❌ Database error clearing food logs.")
            .await?;
        return Ok(());
    }

    let rebuilt = match health_coach::rebuild_summary_from_food_log(
        pool,
        &target_member_id,
        &date_str,
        &external,
    )
    .await
    {
        Ok(v) => v,
        Err(e) => {
            eprintln!("Failed to rebuild health summary after clear: {:?}", e);
            bot.send_message(chat_id, "❌ Database error resetting health summary.")
                .await?;
            return Ok(());
        }
    };

    bot.send_message(
        chat_id,
        format!(
            "🧹 *Today's Telegram food logs cleared* for *{}*.\n\
             Remaining (e.g. Google Health): {} kcal · {:.0}g P / {:.0}g C / {:.0}g F",
            target_member_id, rebuilt.calories, rebuilt.protein, rebuilt.carbs, rebuilt.fats
        ),
    )
    .parse_mode(teloxide::types::ParseMode::Markdown)
    .await?;

    Ok(())
}

async fn handle_adjust_food(
    bot: &Bot,
    chat_id: ChatId,
    args: String,
    pool: &SqlitePool,
    config: &AppConfig,
) -> Result<(), teloxide::RequestError> {
    let tokens: Vec<&str> = args.split_whitespace().collect();
    if tokens.is_empty() {
        bot.send_message(
            chat_id,
            "⚠️ Usage: `/adjustfood [member_id] <calories> <protein> <carbs> <fats>`"
        ).await?;
        return Ok(());
    }

    let mut member_id = config.family.members[0].id.clone();
    let mut offset = 0;

    // Check if first token matches a member ID
    for member in &config.family.members {
        if member.id.eq_ignore_ascii_case(tokens[0]) {
            member_id = member.id.clone();
            offset = 1;
            break;
        }
    }

    let remaining_tokens = &tokens[offset..];
    if remaining_tokens.len() < 4 {
        bot.send_message(
            chat_id,
            format!(
                "⚠️ Missing values. Usage: `/adjustfood [member_id] <calories> <protein> <carbs> <fats>`\n\
                 Example: `/adjustfood {} 2000 150 200 60`",
                member_id
            ),
        )
        .await?;
        return Ok(());
    }

    let calories: i32 = match remaining_tokens[0].parse() {
        Ok(val) => val,
        Err(_) => {
            bot.send_message(chat_id, "❌ Invalid calories value. Must be an integer.").await?;
            return Ok(());
        }
    };

    let protein: f64 = match remaining_tokens[1].parse() {
        Ok(val) => val,
        Err(_) => {
            bot.send_message(chat_id, "❌ Invalid protein value. Must be a number.").await?;
            return Ok(());
        }
    };

    let carbs: f64 = match remaining_tokens[2].parse() {
        Ok(val) => val,
        Err(_) => {
            bot.send_message(chat_id, "❌ Invalid carbs value. Must be a number.").await?;
            return Ok(());
        }
    };

    let fats: f64 = match remaining_tokens[3].parse() {
        Ok(val) => val,
        Err(_) => {
            bot.send_message(chat_id, "❌ Invalid fats value. Must be a number.").await?;
            return Ok(());
        }
    };

    let date_str = chrono::Local::now().format("%Y-%m-%d").to_string();

    // Infer Google Health (etc.) base, then replace Telegram food_log with a delta
    // so that external + food_log == the absolute totals the user requested. That keeps
    // evening /sync (Google + food_log) consistent and makes /undofood rebuild cleanly.
    let external = match health_coach::external_nutrition_base(pool, &member_id, &date_str).await {
        Ok(v) => v,
        Err(e) => {
            eprintln!("Failed to compute external nutrition base: {:?}", e);
            bot.send_message(chat_id, "❌ Database error reading today's summary.")
                .await?;
            return Ok(());
        }
    };

    // Drop previously pushed Telegram meals from Google Health before replacing locally.
    match health_coach::google_data_point_ids_for_day(pool, &member_id, &date_str).await {
        Ok(ids) => {
            if let Err(e) =
                health_coach::delete_google_nutrition_logs(&member_id, config, &ids).await
            {
                eprintln!(
                    "Failed to delete Google Health nutrition logs before adjust: {:?}",
                    e
                );
            }
        }
        Err(e) => eprintln!("Failed to list Google Health nutrition log IDs: {:?}", e),
    }

    if let Err(e) = sqlx::query(
        "DELETE FROM food_log WHERE family_member_id = ? AND date(timestamp, 'localtime') = ?",
    )
    .bind(&member_id)
    .bind(&date_str)
    .execute(pool)
    .await
    {
        eprintln!("Failed to clear food_log before adjust: {:?}", e);
        bot.send_message(chat_id, "❌ Database error adjusting food log.")
            .await?;
        return Ok(());
    }

    // Keep micros from the external (Google) base; only macros are user-overridden.
    let mut desired = external.clone();
    desired.calories = calories as i64;
    desired.protein = protein;
    desired.carbs = carbs;
    desired.fats = fats;

    let delta = desired.saturating_sub(&external);
    let log_id = uuid::Uuid::new_v4().to_string();
    let now = chrono::Utc::now();
    let desc = format!(
        "Manual adjustment: {} kcal, {}g P, {}g C, {}g F",
        calories, protein, carbs, fats
    );

    if let Err(e) = sqlx::query(
        "INSERT INTO food_log (id, timestamp, family_member_id, raw_text_description, \
         estimated_calories, estimated_protein, estimated_carbs, estimated_fats) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&log_id)
    .bind(now)
    .bind(&member_id)
    .bind(&desc)
    .bind(delta.calories as i32)
    .bind(delta.protein)
    .bind(delta.carbs)
    .bind(delta.fats)
    .execute(pool)
    .await
    {
        eprintln!("Failed to insert manual adjustment audit: {:?}", e);
        bot.send_message(chat_id, "❌ Database error saving adjustment.")
            .await?;
        return Ok(());
    }

    if let Err(e) =
        health_coach::write_summary_nutrition(pool, &member_id, &date_str, &desired).await
    {
        eprintln!("Failed to adjust health summary: {:?}", e);
        bot.send_message(chat_id, "❌ Database error adjusting health summary.")
            .await?;
        return Ok(());
    }

    let msg = format!(
        "✅ *Nutrition Updated* for *{}* (Telegram food logs replaced):\n\n\
         • Calories: {} kcal\n\
         • Protein: {:.1}g\n\
         • Carbs: {:.1}g\n\
         • Fats: {:.1}g",
        member_id, calories, protein, carbs, fats
    );

    bot.send_message(chat_id, msg)
        .parse_mode(teloxide::types::ParseMode::Markdown)
        .await?;

    Ok(())
}

async fn handle_undo_food(
    bot: &Bot,
    chat_id: ChatId,
    args: String,
    pool: &SqlitePool,
    config: &AppConfig,
) -> Result<(), teloxide::RequestError> {
    let member_id = args.trim();
    let target_member_id = if member_id.is_empty() {
        config.family.members[0].id.clone()
    } else {
        let mut found = config.family.members[0].id.clone();
        for m in &config.family.members {
            if m.id.eq_ignore_ascii_case(member_id) {
                found = m.id.clone();
                break;
            }
        }
        found
    };

    let date_str = chrono::Local::now().format("%Y-%m-%d").to_string();

    // Snapshot the non-food_log base before mutating food_log.
    let external = match health_coach::external_nutrition_base(pool, &target_member_id, &date_str)
        .await
    {
        Ok(v) => v,
        Err(e) => {
            eprintln!("Failed to compute external nutrition base: {:?}", e);
            bot.send_message(chat_id, "❌ Database error reading today's summary.")
                .await?;
            return Ok(());
        }
    };

    let last_log: Option<chotu_common::FoodLog> = match sqlx::query_as::<_, chotu_common::FoodLog>(
        "SELECT * FROM food_log \
         WHERE family_member_id = ? AND date(timestamp, 'localtime') = ? \
         ORDER BY timestamp DESC LIMIT 1",
    )
    .bind(&target_member_id)
    .bind(&date_str)
    .fetch_optional(pool)
    .await
    {
        Ok(res) => res,
        Err(e) => {
            eprintln!("Failed to fetch last food log: {:?}", e);
            bot.send_message(chat_id, "❌ Database error retrieving last food entry.")
                .await?;
            return Ok(());
        }
    };

    let log_entry = match last_log {
        Some(entry) => entry,
        None => {
            bot.send_message(
                chat_id,
                format!(
                    "⚠️ No food log entries found for *{}* today.",
                    target_member_id
                ),
            )
            .parse_mode(teloxide::types::ParseMode::Markdown)
            .await?;
            return Ok(());
        }
    };

    if let Some(google_id) = log_entry.google_data_point_id.as_ref() {
        if let Err(e) = health_coach::delete_google_nutrition_logs(
            &target_member_id,
            config,
            &[google_id.clone()],
        )
        .await
        {
            eprintln!(
                "Failed to delete Google Health nutrition log on undo: {:?}",
                e
            );
        }
    }

    if let Err(e) = sqlx::query("DELETE FROM food_log WHERE id = ?")
        .bind(&log_entry.id)
        .execute(pool)
        .await
    {
        eprintln!("Failed to delete food log entry: {:?}", e);
        bot.send_message(chat_id, "❌ Database error deleting food log entry.")
            .await?;
        return Ok(());
    }

    let rebuilt = match health_coach::rebuild_summary_from_food_log(
        pool,
        &target_member_id,
        &date_str,
        &external,
    )
    .await
    {
        Ok(v) => v,
        Err(e) => {
            eprintln!("Failed to rebuild summary after undo: {:?}", e);
            bot.send_message(chat_id, "❌ Database error updating today's summary.")
                .await?;
            return Ok(());
        }
    };

    let msg = format!(
        "🗑️ *Last Food Entry Undone* for *{}*:\n\n\
         Removed: \"_{}_\"\n\
         • -{} kcal · -{:.1}g P / -{:.1}g C / -{:.1}g F\n\n\
         *Today now:* {} kcal · {:.0}g P / {:.0}g C / {:.0}g F",
        target_member_id,
        log_entry.raw_text_description,
        log_entry.estimated_calories,
        log_entry.estimated_protein,
        log_entry.estimated_carbs,
        log_entry.estimated_fats,
        rebuilt.calories,
        rebuilt.protein,
        rebuilt.carbs,
        rebuilt.fats
    );

    bot.send_message(chat_id, msg)
        .parse_mode(teloxide::types::ParseMode::Markdown)
        .await?;

    Ok(())
}

#[derive(Debug, Clone, sqlx::FromRow)]
struct TaskListRow {
    id: String,
    title: String,
    status: String,
    due_date: Option<String>,
    assigned_to: Option<String>,
    email_subject: Option<String>,
}

async fn handle_tasks(
    bot: &Bot,
    chat_id: ChatId,
    args: String,
    pool: &SqlitePool,
    config: &AppConfig,
) -> Result<(), teloxide::RequestError> {
    let tokens: Vec<&str> = args.split_whitespace().collect();
    let first = tokens.first().copied().unwrap_or("");
    let second = tokens.get(1).copied();
    let third = tokens.get(2).copied();

    // Mutating actions: complete / done / snooze / reassign / open (unsnooze)
    let action = first.to_lowercase();
    match action.as_str() {
        "complete" => {
            return match second {
                Some(id) if id.len() >= 4 => mark_task_complete(bot, chat_id, pool, id).await,
                _ => {
                    bot.send_message(
                        chat_id,
                        "⚠️ Usage: `/tasks complete <id>`",
                    )
                    .parse_mode(teloxide::types::ParseMode::Markdown)
                    .await?;
                    Ok(())
                }
            };
        }
        "done" if second.is_some_and(looks_like_task_id_prefix) => {
            return mark_task_complete(bot, chat_id, pool, second.unwrap()).await;
        }
        "snooze" => {
            return match second {
                Some(id) if id.len() >= 4 => {
                    let days = third
                        .and_then(|t| t.parse::<i64>().ok())
                        .unwrap_or(1)
                        .clamp(1, 90);
                    snooze_task(bot, chat_id, pool, id, days).await
                }
                _ => {
                    bot.send_message(
                        chat_id,
                        "⚠️ Usage: `/tasks snooze <id> [days]` (default 1 day)",
                    )
                    .parse_mode(teloxide::types::ParseMode::Markdown)
                    .await?;
                    Ok(())
                }
            };
        }
        "reassign" | "assign" => {
            return match (second, third) {
                (Some(id), Some(member_tok)) if id.len() >= 4 => {
                    let member_id = config
                        .family
                        .members
                        .iter()
                        .find(|m| m.id.eq_ignore_ascii_case(member_tok))
                        .map(|m| m.id.clone());
                    match member_id {
                        Some(mid) => reassign_task(bot, chat_id, pool, id, &mid).await,
                        None => {
                            let members = config
                                .family
                                .members
                                .iter()
                                .map(|m| m.id.as_str())
                                .collect::<Vec<_>>()
                                .join(", ");
                            bot.send_message(
                                chat_id,
                                format!(
                                    "⚠️ Unknown member `{}`. Configured: {}",
                                    member_tok, members
                                ),
                            )
                            .parse_mode(teloxide::types::ParseMode::Markdown)
                            .await?;
                            Ok(())
                        }
                    }
                }
                _ => {
                    bot.send_message(
                        chat_id,
                        "⚠️ Usage: `/tasks reassign <id> <member_id>`",
                    )
                    .parse_mode(teloxide::types::ParseMode::Markdown)
                    .await?;
                    Ok(())
                }
            };
        }
        "open" | "unsnooze" if second.is_some_and(looks_like_task_id_prefix) => {
            return reopen_task(bot, chat_id, pool, second.unwrap()).await;
        }
        _ => {}
    }

    let mut status_filter = "open";
    let mut member_filter: Option<String> = None;

    let resolve_member = |tok: &str| -> Option<String> {
        config
            .family
            .members
            .iter()
            .find(|m| m.id.eq_ignore_ascii_case(tok))
            .map(|m| m.id.clone())
    };

    if first.is_empty() {
        // defaults
    } else if let Some(m) = resolve_member(first) {
        member_filter = Some(m);
        if let Some(tok) = second {
            status_filter = normalize_task_status_filter(tok);
        }
    } else {
        status_filter = normalize_task_status_filter(first);
        if let Some(tok) = second {
            member_filter = resolve_member(tok);
        }
    }

    let (status_clause, label) = match status_filter {
        "all" => ("status IN ('open', 'snoozed')", "open/snoozed"),
        "done" | "completed" => ("status = 'done'", "completed"),
        "ignored" => ("status = 'ignored'", "ignored"),
        "snoozed" => ("status = 'snoozed'", "snoozed"),
        _ => ("status = 'open'", "open"),
    };

    // sqlx 0.9: build dynamic filters with QueryBuilder (SqlSafeStr rejects String).
    let mut qb = sqlx::QueryBuilder::new(
        "SELECT id, title, status, due_date, assigned_to, email_subject FROM tasks WHERE ",
    );
    qb.push(status_clause);
    if let Some(ref member_id) = member_filter {
        qb.push(" AND (assigned_to = ");
        qb.push_bind(member_id);
        qb.push(" OR assigned_to IS NULL)");
    }
    qb.push(" ORDER BY due_date IS NULL, due_date ASC, created_at DESC LIMIT 25");

    let rows: Vec<TaskListRow> = match qb.build_query_as::<TaskListRow>().fetch_all(pool).await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Failed to list tasks: {:?}", e);
            bot.send_message(chat_id, "❌ Database error listing tasks.")
                .await?;
            return Ok(());
        }
    };

    if rows.is_empty() {
        let scope = member_filter
            .as_deref()
            .map(|m| format!(" for *{}*", m))
            .unwrap_or_default();
        bot.send_message(
            chat_id,
            format!("✅ No *{}* tasks found{}.", label, scope),
        )
        .parse_mode(teloxide::types::ParseMode::Markdown)
        .await?;
        return Ok(());
    }

    let mut msg = format!("📋 *Tasks* ({}, {})\n\n", label, rows.len());
    for row in &rows {
        let short_id: String = row.id.chars().take(8).collect();
        let due = row
            .due_date
            .as_deref()
            .map(|d| format!(" · due {}", d))
            .unwrap_or_default();
        let assignee = row
            .assigned_to
            .as_deref()
            .map(|a| format!(" · @{}", a))
            .unwrap_or_default();
        let subject = row
            .email_subject
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(|s| format!("\n    _{}_", escape_md_basic(&truncate_chars(s, 60))))
            .unwrap_or_default();

        msg.push_str(&format!(
            "• `{}` {} ({}){}{}{}\n",
            short_id,
            escape_md_basic(&truncate_chars(&row.title, 80)),
            row.status,
            due,
            assignee,
            subject
        ));
    }

    if label == "open" || label == "open/snoozed" || label == "snoozed" {
        msg.push_str(
            "\n_Actions:_ `/tasks complete <id>` · `/tasks snooze <id> [days]` · \
             `/tasks reassign <id> <member>` · `/tasks open <id>`\n\
             _Dismiss email tasks:_ reply `unactionable` to the original reminder.",
        );
    }

    bot.send_message(chat_id, msg)
        .parse_mode(teloxide::types::ParseMode::Markdown)
        .await?;

    Ok(())
}

fn normalize_task_status_filter(tok: &str) -> &'static str {
    match tok.to_lowercase().as_str() {
        "all" => "all",
        "done" | "completed" => "completed",
        "ignored" => "ignored",
        "snoozed" => "snoozed",
        "open" => "open",
        _ => "open",
    }
}

fn looks_like_task_id_prefix(s: &str) -> bool {
    s.len() >= 4 && s.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
}

/// Resolve a unique task by id prefix. Sends Telegram errors on 0/ambiguous matches.
async fn find_task_by_prefix(
    bot: &Bot,
    chat_id: ChatId,
    pool: &SqlitePool,
    id_prefix: &str,
) -> Result<Option<(String, String, String)>, teloxide::RequestError> {
    let pattern = format!("{}%", id_prefix);
    let matches: Vec<(String, String, String)> = match sqlx::query_as(
        "SELECT id, title, status FROM tasks WHERE id LIKE ? COLLATE NOCASE LIMIT 5",
    )
    .bind(&pattern)
    .fetch_all(pool)
    .await
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Failed to look up task: {:?}", e);
            bot.send_message(chat_id, "❌ Database error looking up task.")
                .await?;
            return Ok(None);
        }
    };

    if matches.is_empty() {
        bot.send_message(
            chat_id,
            format!("⚠️ No task found starting with `{}`.", id_prefix),
        )
        .parse_mode(teloxide::types::ParseMode::Markdown)
        .await?;
        return Ok(None);
    }
    if matches.len() > 1 {
        let mut msg = format!(
            "⚠️ Ambiguous id `{}` — matches {} tasks. Use more characters:\n",
            id_prefix,
            matches.len()
        );
        for (id, title, status) in &matches {
            let short: String = id.chars().take(8).collect();
            msg.push_str(&format!(
                "• `{}` {} ({})\n",
                short,
                truncate_chars(title, 50),
                status
            ));
        }
        bot.send_message(chat_id, msg)
            .parse_mode(teloxide::types::ParseMode::Markdown)
            .await?;
        return Ok(None);
    }

    Ok(Some(matches.into_iter().next().unwrap()))
}

async fn mark_task_complete(
    bot: &Bot,
    chat_id: ChatId,
    pool: &SqlitePool,
    id_prefix: &str,
) -> Result<(), teloxide::RequestError> {
    let Some((id, title, status)) = find_task_by_prefix(bot, chat_id, pool, id_prefix).await? else {
        return Ok(());
    };

    if status == "done" {
        bot.send_message(
            chat_id,
            format!("ℹ️ Task already done: _{}_", escape_md_basic(&title)),
        )
        .parse_mode(teloxide::types::ParseMode::Markdown)
        .await?;
        return Ok(());
    }

    let now = chrono::Utc::now().to_rfc3339();
    if let Err(e) = sqlx::query("UPDATE tasks SET status = 'done', updated_at = ? WHERE id = ?")
        .bind(&now)
        .bind(&id)
        .execute(pool)
        .await
    {
        eprintln!("Failed to mark task done: {:?}", e);
        bot.send_message(chat_id, "❌ Database error updating task.")
            .await?;
        return Ok(());
    }

    bot.send_message(
        chat_id,
        format!("✅ Marked done: _{}_", escape_md_basic(&title)),
    )
    .parse_mode(teloxide::types::ParseMode::Markdown)
    .await?;

    refresh_task_memory(pool, &id).await;
    Ok(())
}

async fn snooze_task(
    bot: &Bot,
    chat_id: ChatId,
    pool: &SqlitePool,
    id_prefix: &str,
    days: i64,
) -> Result<(), teloxide::RequestError> {
    let Some((id, title, _)) = find_task_by_prefix(bot, chat_id, pool, id_prefix).await? else {
        return Ok(());
    };

    let due = (chrono::Local::now().date_naive() + chrono::Duration::days(days))
        .format("%Y-%m-%d")
        .to_string();
    let now = chrono::Utc::now().to_rfc3339();

    if let Err(e) = sqlx::query(
        "UPDATE tasks SET status = 'snoozed', due_date = ?, updated_at = ? WHERE id = ?",
    )
    .bind(&due)
    .bind(&now)
    .bind(&id)
    .execute(pool)
    .await
    {
        eprintln!("Failed to snooze task: {:?}", e);
        bot.send_message(chat_id, "❌ Database error snoozing task.")
            .await?;
        return Ok(());
    }

    bot.send_message(
        chat_id,
        format!(
            "😴 Snoozed until *{}*: _{}_",
            due,
            escape_md_basic(&title)
        ),
    )
    .parse_mode(teloxide::types::ParseMode::Markdown)
    .await?;

    refresh_task_memory(pool, &id).await;
    Ok(())
}

async fn reassign_task(
    bot: &Bot,
    chat_id: ChatId,
    pool: &SqlitePool,
    id_prefix: &str,
    member_id: &str,
) -> Result<(), teloxide::RequestError> {
    let Some((id, title, _)) = find_task_by_prefix(bot, chat_id, pool, id_prefix).await? else {
        return Ok(());
    };

    let now = chrono::Utc::now().to_rfc3339();
    if let Err(e) =
        sqlx::query("UPDATE tasks SET assigned_to = ?, updated_at = ? WHERE id = ?")
            .bind(member_id)
            .bind(&now)
            .bind(&id)
            .execute(pool)
            .await
    {
        eprintln!("Failed to reassign task: {:?}", e);
        bot.send_message(chat_id, "❌ Database error reassigning task.")
            .await?;
        return Ok(());
    }

    bot.send_message(
        chat_id,
        format!(
            "👤 Assigned to *{}*: _{}_",
            member_id,
            escape_md_basic(&title)
        ),
    )
    .parse_mode(teloxide::types::ParseMode::Markdown)
    .await?;

    refresh_task_memory(pool, &id).await;
    Ok(())
}

async fn reopen_task(
    bot: &Bot,
    chat_id: ChatId,
    pool: &SqlitePool,
    id_prefix: &str,
) -> Result<(), teloxide::RequestError> {
    let Some((id, title, status)) = find_task_by_prefix(bot, chat_id, pool, id_prefix).await? else {
        return Ok(());
    };

    if status == "open" {
        bot.send_message(
            chat_id,
            format!("ℹ️ Already open: _{}_", escape_md_basic(&title)),
        )
        .parse_mode(teloxide::types::ParseMode::Markdown)
        .await?;
        return Ok(());
    }

    let now = chrono::Utc::now().to_rfc3339();
    if let Err(e) = sqlx::query("UPDATE tasks SET status = 'open', updated_at = ? WHERE id = ?")
        .bind(&now)
        .bind(&id)
        .execute(pool)
        .await
    {
        eprintln!("Failed to reopen task: {:?}", e);
        bot.send_message(chat_id, "❌ Database error reopening task.")
            .await?;
        return Ok(());
    }

    bot.send_message(
        chat_id,
        format!("📂 Reopened: _{}_", escape_md_basic(&title)),
    )
    .parse_mode(teloxide::types::ParseMode::Markdown)
    .await?;

    refresh_task_memory(pool, &id).await;
    Ok(())
}

fn truncate_chars(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if count <= max {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{}…", truncated)
    }
}

fn escape_md_basic(s: &str) -> String {
    s.replace('_', "\\_")
        .replace('*', "\\*")
        .replace('`', "\\`")
        .replace('[', "\\[")
}

async fn handle_trends(
    bot: &Bot,
    chat_id: ChatId,
    args: String,
    pool: &SqlitePool,
    config: &AppConfig,
) -> Result<(), teloxide::RequestError> {
    let days = args
        .trim()
        .parse::<i64>()
        .unwrap_or(7)
        .clamp(2, 90);

    bot.send_message(
        chat_id,
        format!("📈 Building nutrition trends for the last {} days...", days),
    )
    .await?;

    match health_coach::build_nutrition_trend_reports(pool, config, days).await {
        Ok(reports) => {
            for report in reports {
                bot.send_message(chat_id, report)
                    .parse_mode(teloxide::types::ParseMode::Markdown)
                    .await?;
            }
        }
        Err(e) => {
            eprintln!("Trends query error: {:?}", e);
            bot.send_message(chat_id, format!("❌ Failed to build trends: {}", e))
                .await?;
        }
    }

    Ok(())
}

async fn handle_brief(
    bot: &Bot,
    chat_id: ChatId,
    pool: &SqlitePool,
    config: &AppConfig,
) -> Result<(), teloxide::RequestError> {
    bot.send_message(chat_id, "☀️ Building morning brief...")
        .await?;

    let report = crate::brief::compose_morning_brief(pool, config).await;
    bot.send_message(chat_id, report)
        .parse_mode(teloxide::types::ParseMode::Markdown)
        .await?;
    Ok(())
}

async fn handle_cal(
    bot: &Bot,
    chat_id: ChatId,
    args: String,
    config: &AppConfig,
) -> Result<(), teloxide::RequestError> {
    let trimmed = args.trim().to_lowercase();
    let window = if trimmed.is_empty()
        || trimmed == "today"
        || trimmed == "day"
        || trimmed == "tomorrow"
        || trimmed == "tmr"
        || trimmed == "tmrw"
        || trimmed == "week"
        || trimmed == "this week"
        || trimmed == "thisweek"
    {
        CalendarWindow::parse(&trimmed)
    } else {
        bot.send_message(
            chat_id,
            "⚠️ Usage: `/cal [today|tomorrow|week]`",
        )
        .parse_mode(teloxide::types::ParseMode::Markdown)
        .await?;
        return Ok(());
    };

    let report = compose_calendar_agenda(config, window).await;
    bot.send_message(chat_id, report)
        .parse_mode(teloxide::types::ParseMode::Markdown)
        .await?;
    Ok(())
}

async fn handle_memory(
    bot: &Bot,
    chat_id: ChatId,
    args: String,
    pool: &SqlitePool,
    llm: &ChotuLlm,
    gemini_client: &GeminiClient,
) -> Result<(), teloxide::RequestError> {
    let args = args.trim();
    if args.is_empty() {
        bot.send_message(
            chat_id,
            "Usage: `/memory <question>` — search journals, digests, personal references, and tasks.\n\
             Or `/memory reindex` to rebuild the embedding index.",
        )
        .parse_mode(teloxide::types::ParseMode::Markdown)
        .await?;
        return Ok(());
    }

    let index = MemoryIndex::from_env();

    if args.eq_ignore_ascii_case("reindex") {
        bot.send_message(chat_id, "🧠 Rebuilding memory index (this may take a while)...")
            .await?;
        match index.reindex_all(pool, true).await {
            Ok(stats) => {
                bot.send_message(
                    chat_id,
                    format!(
                        "✅ Memory reindex complete.\n• upserted: {}\n• skipped: {}\n• deleted: {}\n• errors: {}",
                        stats.upserted, stats.skipped, stats.deleted, stats.errors
                    ),
                )
                .await?;
            }
            Err(e) => {
                eprintln!("Memory reindex failed: {:?}", e);
                bot.send_message(chat_id, format!("❌ Memory reindex failed: {}", e))
                    .await?;
            }
        }
        return Ok(());
    }

    bot.send_message(chat_id, "🧠 Searching memory...")
        .await?;

    let hits = match index.search(pool, args, None).await {
        Ok(h) => h,
        Err(e) => {
            eprintln!("Memory search failed: {:?}", e);
            bot.send_message(
                chat_id,
                format!(
                    "❌ Memory search failed: {}.\nTip: run `/memory reindex` after `ollama pull nomic-embed-text`.",
                    e
                ),
            )
            .parse_mode(teloxide::types::ParseMode::Markdown)
            .await?;
            return Ok(());
        }
    };

    if hits.is_empty() {
        bot.send_message(
            chat_id,
            "I couldn't find anything relevant in journals, digests, personal references, or tasks.",
        )
        .await?;
        return Ok(());
    }

    bot.send_message(
        chat_id,
        format!(
            "📚 Found {} matches — drafting with local Ollama (usually ~10–30s; times out at 45s)…",
            hits.len()
        ),
    )
    .await?;

    // Prefer local Ollama (same model as email/intent); Gemini only if Ollama fails.
    let reply = match answer_memory_query(Some(llm), Some(gemini_client), args, &hits).await {
        Ok(text) => text,
        Err(e) => {
            eprintln!("Memory answer failed: {:?}", e);
            chotu_common::format_hit_list(&hits)
        }
    };

    bot.send_message(chat_id, reply)
        .parse_mode(teloxide::types::ParseMode::Markdown)
        .await?;
    Ok(())
}

/// Best-effort refresh of one task in the memory index after status/assignment changes.
async fn refresh_task_memory(pool: &SqlitePool, task_id: &str) {
    let row: Option<(
        String,
        String,
        Option<String>,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
    )> = match sqlx::query_as(
        "SELECT id, title, description, status, due_date, assigned_to, created_at FROM tasks WHERE id = ?",
    )
    .bind(task_id)
    .fetch_optional(pool)
    .await
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Memory: task lookup failed: {:?}", e);
            return;
        }
    };
    let Some((id, title, description, status, due_date, assigned_to, created_at)) = row else {
        return;
    };
    let index = MemoryIndex::from_env();
    if let Err(e) = index
        .index_task(
            pool,
            &id,
            &title,
            description.as_deref(),
            &status,
            due_date.as_deref(),
            assigned_to.as_deref(),
            created_at.as_deref(),
        )
        .await
    {
        eprintln!("Memory: failed to reindex task {}: {:?}", id, e);
    }
}

async fn handle_status(
    bot: &Bot,
    chat_id: ChatId,
    pool: &SqlitePool,
    config: &AppConfig,
) -> Result<(), teloxide::RequestError> {
    let date_str = chrono::Local::now().format("%Y-%m-%d").to_string();

    // Query daily financials and health summaries
    let (txs, healths) = match crate::reflection::get_daily_data(pool, &date_str, config).await {
        Ok(data) => data,
        Err(e) => {
            eprintln!("Status query error: {:?}", e);
            bot.send_message(chat_id, "Failed to retrieve today's logs from database.")
                .await?;
            return Ok(());
        }
    };

    // 1. Send Financial Ledger Status
    let base = config.currency();
    let rates = fetch_exchange_rates(base).await;
    let mut finance_report = format!("💳 *Financial Ledger ({})*\n\n", date_str);
    if txs.is_empty() {
        finance_report.push_str("_No transactions logged today._\n");
    } else {
        let total_spent: f64 = txs
            .iter()
            .map(|t| config.convert_to_base(t.amount, &t.currency, &rates))
            .sum();
        finance_report.push_str(&format!("• *Total Spend*: {:.2} {}\n", total_spent, base));
        finance_report.push_str("• *Transactions*:\n");
        for t in txs {
            finance_report.push_str(&format!(
                "  - spent {:.2} {} at *{}* ({})\n",
                t.amount, t.currency, t.merchant, t.category
            ));
        }
    }
    bot.send_message(chat_id, finance_report)
        .parse_mode(teloxide::types::ParseMode::Markdown)
        .await?;

    // 2. Send Individual Health Metrics for each member
    for h in healths {
        // Find member name from config
        let name = config
            .family
            .members
            .iter()
            .find(|m| m.id == h.family_member_id)
            .map(|m| m.name.as_str())
            .unwrap_or(h.family_member_id.as_str());

        let mut member_report = format!("🏃 *Health Status: {} ({})*\n\n", name, date_str);

        let has_activity = h.step_count > 0 || h.active_calories_burned > 0;
        let has_sleep = h.sleep_hours.is_some();
        let has_energy = h.perceived_energy.is_some();

        if has_activity || has_sleep || has_energy {
            member_report.push_str("• *Activity & Wellness*:\n");
            if has_activity {
                member_report.push_str(&format!(
                    "  - Burned: {} active kcal | {} steps\n",
                    h.active_calories_burned, h.step_count
                ));
            }
            if let Some(sleep) = h.sleep_hours {
                member_report.push_str(&format!("  - Sleep: {:.1} hours\n", sleep));
            }
            if let Some(energy) = h.perceived_energy {
                member_report.push_str(&format!("  - Energy Level: {}/10\n", energy));
            }
            member_report.push_str("\n");
        }

        let has_nutrition = h.total_calories_ingested > 0
            || h.protein_grams > 0.0
            || h.carbs_grams > 0.0
            || h.fats_grams > 0.0;

        if has_nutrition {
            member_report.push_str("• *Nutrition Summary*:\n");
            
            let mut table = String::new();
            table.push_str("```\n");
            table.push_str("Nutrient       | Amount    \n");
            table.push_str("---------------+-----------\n");
            table.push_str(&format!("{:<14} | {:<10}\n", "Calories", format!("{} kcal", h.total_calories_ingested)));
            table.push_str(&format!("{:<14} | {:<10}\n", "Protein", format!("{:.1}g", h.protein_grams)));
            table.push_str(&format!("{:<14} | {:<10}\n", "Carbs", format!("{:.1}g", h.carbs_grams)));
            table.push_str(&format!("{:<14} | {:<10}\n", "Fat", format!("{:.1}g", h.fats_grams)));

            // Fats breakdown (only non-zero values)
            if h.saturated_fat_g > 0.0 { table.push_str(&format!("{:<14} | {:<10}\n", "  Saturated", format!("{:.1}g", h.saturated_fat_g))); }
            if h.unsaturated_fat_g > 0.0 { table.push_str(&format!("{:<14} | {:<10}\n", "  Unsaturated", format!("{:.1}g", h.unsaturated_fat_g))); }
            if h.trans_fat_g > 0.0 { table.push_str(&format!("{:<14} | {:<10}\n", "  Trans Fat", format!("{:.1}g", h.trans_fat_g))); }
            if h.cholesterol_mg > 0.0 { table.push_str(&format!("{:<14} | {:<10}\n", "  Cholesterol", format!("{:.1}mg", h.cholesterol_mg))); }
            if h.triglycerides_mg > 0.0 { table.push_str(&format!("{:<14} | {:<10}\n", "  Triglycerides", format!("{:.1}mg", h.triglycerides_mg))); }
            if h.omega_3_dha_mg > 0.0 { table.push_str(&format!("{:<14} | {:<10}\n", "  Omega-3 DHA", format!("{:.1}mg", h.omega_3_dha_mg))); }

            // Vitamins (only non-zero values)
            if h.vitamin_a_mcg > 0.0 { table.push_str(&format!("{:<14} | {:<10}\n", "Vitamin A", format!("{:.1}mcg", h.vitamin_a_mcg))); }
            if h.vitamin_b_mg > 0.0 { table.push_str(&format!("{:<14} | {:<10}\n", "Vitamin B", format!("{:.1}mg", h.vitamin_b_mg))); }
            if h.vitamin_c_mg > 0.0 { table.push_str(&format!("{:<14} | {:<10}\n", "Vitamin C", format!("{:.1}mg", h.vitamin_c_mg))); }
            if h.vitamin_d_mcg > 0.0 { table.push_str(&format!("{:<14} | {:<10}\n", "Vitamin D", format!("{:.1}mcg", h.vitamin_d_mcg))); }
            if h.vitamin_e_mg > 0.0 { table.push_str(&format!("{:<14} | {:<10}\n", "Vitamin E", format!("{:.1}mg", h.vitamin_e_mg))); }
            if h.vitamin_k_mcg > 0.0 { table.push_str(&format!("{:<14} | {:<10}\n", "Vitamin K", format!("{:.1}mcg", h.vitamin_k_mcg))); }

            // Minerals (only non-zero values)
            if h.sodium_mg > 0.0 { table.push_str(&format!("{:<14} | {:<10}\n", "Sodium", format!("{:.1}mg", h.sodium_mg))); }
            if h.potassium_mg > 0.0 { table.push_str(&format!("{:<14} | {:<10}\n", "Potassium", format!("{:.1}mg", h.potassium_mg))); }
            if h.calcium_mg > 0.0 { table.push_str(&format!("{:<14} | {:<10}\n", "Calcium", format!("{:.1}mg", h.calcium_mg))); }
            if h.magnesium_mg > 0.0 { table.push_str(&format!("{:<14} | {:<10}\n", "Magnesium", format!("{:.1}mg", h.magnesium_mg))); }
            if h.zinc_mg > 0.0 { table.push_str(&format!("{:<14} | {:<10}\n", "Zinc", format!("{:.1}mg", h.zinc_mg))); }
            if h.iron_mg > 0.0 { table.push_str(&format!("{:<14} | {:<10}\n", "Iron", format!("{:.1}mg", h.iron_mg))); }

            // Other (only non-zero values)
            if h.fiber_g > 0.0 { table.push_str(&format!("{:<14} | {:<10}\n", "Fiber", format!("{:.1}g", h.fiber_g))); }
            if h.sugar_g > 0.0 { table.push_str(&format!("{:<14} | {:<10}\n", "Sugar", format!("{:.1}g", h.sugar_g))); }
            if h.caffeine_mg > 0.0 { table.push_str(&format!("{:<14} | {:<10}\n", "Caffeine", format!("{:.1}mg", h.caffeine_mg))); }

            table.push_str("```\n");
            member_report.push_str(&table);
        }

        if let Some(goals) = config
            .family
            .members
            .iter()
            .find(|m| m.id == h.family_member_id)
            .and_then(|m| m.nutrition_goals.as_ref())
        {
            if let Some(progress) = goals.progress_markdown(
                h.total_calories_ingested,
                h.protein_grams,
                h.carbs_grams,
                h.fats_grams,
                h.fiber_g,
                h.step_count,
            ) {
                member_report.push('\n');
                member_report.push_str(&progress);
            }
        }

        if !has_activity && !has_sleep && !has_energy && !has_nutrition {
            member_report.push_str("• _No health telemetry logged today._\n");
        }

        bot.send_message(chat_id, member_report)
            .parse_mode(teloxide::types::ParseMode::Markdown)
            .await?;
    }

    Ok(())
}

async fn handle_networth(
    bot: &Bot,
    chat_id: ChatId,
    pool: &SqlitePool,
    gemini_client: &GeminiClient,
    config: &AppConfig,
) -> Result<(), teloxide::RequestError> {
    let base = config.currency();
    let rates = fetch_exchange_rates(base).await;

    // Portfolio only for now — email ledger is a transaction log, not a cash balance.
    let holdings: Vec<chotu_common::PortfolioHolding> = match sqlx::query_as::<_, chotu_common::PortfolioHolding>(
        "SELECT ticker, shares_owned, average_cost, last_updated FROM portfolio_holdings"
    )
    .fetch_all(pool)
    .await {
        Ok(h) => h,
        Err(e) => {
            eprintln!("Failed to fetch portfolio holdings: {:?}", e);
            bot.send_message(chat_id, "❌ Database error retrieving portfolio.").await?;
            return Ok(());
        }
    };

    let mut msg = String::new();
    msg.push_str(&format!("💰 *Project Chotu Net Worth Summary* ({})\n\n", base));
    msg.push_str("• 💵 *Liquid Cash:* _not tracked yet_ (ledger is spend history, not balances)\n");

    if holdings.is_empty() {
        msg.push_str(&format!(
            "• 📈 *Stock Portfolio:* $0.00 {} (No holdings set. Use `/holdings` to add stocks)\n",
            base
        ));
        msg.push_str("\n━━━━━━━━━━━━━━━━━━━━━━━━\n");
        msg.push_str(&format!("✨ *Invested Net Worth:* $0.00 {}", base));
    } else {
        bot.send_message(chat_id, "🔍 Fetching real-time stock prices via Gemini...").await?;
        
        let tickers: Vec<String> = holdings.iter().map(|h| h.ticker.clone()).collect();
        let prices = match gemini_client.fetch_stock_prices(&tickers).await {
            Ok(p) => p,
            Err(e) => {
                eprintln!("Failed to fetch stock prices: {:?}", e);
                // Fallback: treat average_cost as already in base currency
                let mut portfolio_cost = 0.0;
                for h in &holdings {
                    portfolio_cost += h.shares_owned * h.average_cost;
                }
                msg.push_str(&format!(
                    "• 📈 *Stock Portfolio:* ${:.2} {} (Gemini price lookup failed, showing book cost)\n",
                    portfolio_cost, base
                ));
                msg.push_str("\n━━━━━━━━━━━━━━━━━━━━━━━━\n");
                msg.push_str(&format!(
                    "✨ *Invested Net Worth:* ${:.2} {} (estimated)",
                    portfolio_cost,
                    base
                ));
                bot.send_message(chat_id, msg).parse_mode(teloxide::types::ParseMode::Markdown).await?;
                return Ok(());
            }
        };

        let price_map: std::collections::HashMap<String, (f64, String)> = prices
            .into_iter()
            .map(|p| {
                (
                    p.ticker.to_uppercase(),
                    (
                        p.price,
                        p.currency.unwrap_or_else(|| "USD".to_string()),
                    ),
                )
            })
            .collect();

        let mut total_portfolio_value = 0.0;
        let mut total_portfolio_cost = 0.0;
        let mut breakdown = String::new();

        for h in &holdings {
            let ticker_upper = h.ticker.to_uppercase();
            let (raw_price, quote_currency) = price_map
                .get(&ticker_upper)
                .cloned()
                .unwrap_or_else(|| (h.average_cost, base.to_string()));

            let price_base = config.convert_to_base(raw_price, &quote_currency, &rates);
            // Assume book cost was entered in the ticker's native quote currency.
            let cost_base =
                config.convert_to_base(h.shares_owned * h.average_cost, &quote_currency, &rates);
            let value_base = h.shares_owned * price_base;
            total_portfolio_cost += cost_base;
            total_portfolio_value += value_base;

            let diff_percent = if cost_base > 0.0 {
                (value_base - cost_base) / cost_base * 100.0
            } else {
                0.0
            };
            let sign = if diff_percent >= 0.0 { "+" } else { "" };

            breakdown.push_str(&format!(
                "  - *{}*: {:.1} shares @ ${:.2} {} (Cost: ${:.2} | Value: ${:.2} | {}{:.1}%)\n",
                ticker_upper,
                h.shares_owned,
                price_base,
                base,
                cost_base,
                value_base,
                sign,
                diff_percent
            ));
        }

        let overall_diff_percent = if total_portfolio_cost > 0.0 {
            (total_portfolio_value - total_portfolio_cost) / total_portfolio_cost * 100.0
        } else {
            0.0
        };
        let overall_sign = if overall_diff_percent >= 0.0 { "+" } else { "" };

        msg.push_str(&format!(
            "• 📈 *Stock Portfolio:* ${:.2} {} (Cost: ${:.2} | {}{:.1}%)\n",
            total_portfolio_value, base, total_portfolio_cost, overall_sign, overall_diff_percent
        ));
        msg.push_str("\n━━━━━━━━━━━━━━━━━━━━━━━━\n");
        msg.push_str(&format!(
            "✨ *Invested Net Worth:* ${:.2} {}\n\n",
            total_portfolio_value,
            base
        ));
        msg.push_str("*Holdings Breakdown:*\n");
        msg.push_str(&breakdown);
    }

    bot.send_message(chat_id, msg)
        .parse_mode(teloxide::types::ParseMode::Markdown)
        .await?;

    Ok(())
}

async fn handle_holdings(
    bot: &Bot,
    chat_id: ChatId,
    args: String,
    pool: &SqlitePool,
) -> Result<(), teloxide::RequestError> {
    let tokens: Vec<&str> = args.split_whitespace().collect();
    if tokens.is_empty() {
        bot.send_message(
            chat_id,
            "⚠️ Usage: `/holdings <ticker>:<shares>:<avg_cost> ...`\nExample: `/holdings AAPL:100:175.50 MSFT:50:420.00`"
        ).await?;
        return Ok(());
    }

    let now = chrono::Utc::now();
    let mut updated = Vec::new();

    for token in tokens {
        let parts: Vec<&str> = token.split(':').collect();
        if parts.len() != 3 {
            bot.send_message(
                chat_id,
                format!("❌ Invalid format for '{}'. Must be TICKER:SHARES:COST (e.g. AAPL:10:180.50).", token)
            ).await?;
            return Ok(());
        }

        let ticker = parts[0].to_uppercase();
        let shares: f64 = match parts[1].parse() {
            Ok(s) => s,
            Err(_) => {
                bot.send_message(chat_id, format!("❌ Invalid shares for '{}'. Must be a number.", token)).await?;
                return Ok(());
            }
        };
        let cost: f64 = match parts[2].parse() {
            Ok(c) => c,
            Err(_) => {
                bot.send_message(chat_id, format!("❌ Invalid cost for '{}'. Must be a number.", token)).await?;
                return Ok(());
            }
        };

        if let Err(e) = sqlx::query(
            "INSERT INTO portfolio_holdings (ticker, shares_owned, average_cost, last_updated) \
             VALUES (?, ?, ?, ?) \
             ON CONFLICT(ticker) DO UPDATE SET \
                shares_owned = excluded.shares_owned, \
                average_cost = excluded.average_cost, \
                last_updated = excluded.last_updated"
        )
        .bind(&ticker)
        .bind(shares)
        .bind(cost)
        .bind(now)
        .execute(pool)
        .await {
            eprintln!("Failed to save holdings for {}: {:?}", ticker, e);
            bot.send_message(chat_id, "❌ Database error updating holdings.").await?;
            return Ok(());
        }

        updated.push(format!("*{}* ({} shares @ ${:.2})", ticker, shares, cost));
    }

    let msg = format!("✅ *Portfolio updated successfully!*\n\nSaved holdings:\n{}", updated.join("\n"));
    bot.send_message(chat_id, msg)
        .parse_mode(teloxide::types::ParseMode::Markdown)
        .await?;

    Ok(())
}

async fn handle_monthly(
    bot: &Bot,
    chat_id: ChatId,
    args: String,
    pool: &SqlitePool,
    config: &AppConfig,
) -> Result<(), teloxide::RequestError> {
    let date_str = args.trim().to_string();
    let target_month = if date_str.is_empty() {
        chrono::Local::now().format("%Y-%m").to_string()
    } else {
        if date_str.len() != 7 || !date_str.contains('-') {
            bot.send_message(chat_id, "⚠️ Invalid format. Usage: `/monthly [YYYY-MM]` (e.g. `/monthly 2026-06`)").await?;
            return Ok(());
        }
        date_str
    };

    // Query all transactions for that month
    let entries: Vec<chotu_common::FinancialLedgerEntry> = match sqlx::query_as::<_, chotu_common::FinancialLedgerEntry>(
        "SELECT id, timestamp, amount, currency, institution, merchant, category, source_type \
         FROM financial_ledger \
         WHERE strftime('%Y-%m', timestamp) = ? \
         ORDER BY timestamp DESC"
    )
    .bind(&target_month)
    .fetch_all(pool)
    .await {
        Ok(res) => res,
        Err(e) => {
            eprintln!("Failed to fetch monthly transactions: {:?}", e);
            bot.send_message(chat_id, "❌ Database error retrieving monthly ledger.").await?;
            return Ok(());
        }
    };

    if entries.is_empty() {
        bot.send_message(chat_id, format!("📅 *No transactions found for {}*.", target_month))
            .parse_mode(teloxide::types::ParseMode::Markdown)
            .await?;
        return Ok(());
    }

    let base = config.currency();
    let rates = fetch_exchange_rates(base).await;

    // Group and sum by category (converted to base currency)
    let mut category_totals: std::collections::HashMap<String, f64> = std::collections::HashMap::new();
    let mut total_spend = 0.0;
    let mut total_income = 0.0;

    for entry in &entries {
        let amt = config.convert_to_base(entry.amount, &entry.currency, &rates);
        if entry.category.to_lowercase() == "income" {
            total_income += amt;
        } else {
            total_spend += amt;
            *category_totals.entry(entry.category.clone()).or_insert(0.0) += amt;
        }
    }

    let mut msg = String::new();
    msg.push_str(&format!("📅 *Monthly Financial Summary: {}* ({})\n\n", target_month, base));
    msg.push_str(&format!("• 💳 *Total Spend:* ${:.2} {}\n", total_spend, base));
    if total_income > 0.0 {
        msg.push_str(&format!("• 💵 *Total Income:* ${:.2} {}\n", total_income, base));
    }
    msg.push_str("\n*Spend by Category:*\n");

    let mut cat_vec: Vec<(String, f64)> = category_totals.into_iter().collect();
    cat_vec.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    for (cat, amt) in &cat_vec {
        let percent = if total_spend > 0.0 {
            (amt / total_spend) * 100.0
        } else {
            0.0
        };
        msg.push_str(&format!("  - *{}*: ${:.2} ({:.1}%)\n", cat, amt, percent));
    }

    // List top 5 largest spend transactions (by base-currency amount)
    msg.push_str("\n*Largest Transactions:*\n");
    let mut spend_entries: Vec<&chotu_common::FinancialLedgerEntry> = entries
        .iter()
        .filter(|e| e.category.to_lowercase() != "income")
        .collect();
    spend_entries.sort_by(|a, b| {
        let a_base = config.convert_to_base(a.amount, &a.currency, &rates).abs();
        let b_base = config.convert_to_base(b.amount, &b.currency, &rates).abs();
        b_base.partial_cmp(&a_base).unwrap_or(std::cmp::Ordering::Equal)
    });

    for entry in spend_entries.iter().take(5) {
        let amt_base = config.convert_to_base(entry.amount, &entry.currency, &rates);
        msg.push_str(&format!(
            "  - {}: ${:.2} {} at *{}*\n",
            entry.category, amt_base, base, entry.merchant
        ));
    }

    // Append Target Allocation report if configured
    if let Some(ref target) = config.target_allocation {
        msg.push_str("\n━━━━━━━━━━━━━━━━━━━━━━━━\n\n");
        let target_report = finance_advisor::check_allocation_status(&target_month, target, &entries, config, &rates);
        msg.push_str(&target_report);
    }

    bot.send_message(chat_id, msg)
        .parse_mode(teloxide::types::ParseMode::Markdown)
        .await?;

    Ok(())
}

async fn handle_reflect_trigger(
    bot: &Bot,
    chat_id: ChatId,
    pool: &SqlitePool,
    llm: &ChotuLlm,
    states: StateMap,
    config: &AppConfig,
) -> Result<(), teloxide::RequestError> {
    let date_str = chrono::Local::now().format("%Y-%m-%d").to_string();

    bot.send_message(
        chat_id,
        "Querying daily metrics and generating evening reflection prompt via local Ollama...",
    )
    .await?;

    // 1. Get data
    let (txs, healths) = match crate::reflection::get_daily_data(pool, &date_str, config).await {
        Ok(data) => data,
        Err(e) => {
            eprintln!("Reflect prompt query error: {:?}", e);
            bot.send_message(chat_id, "Failed to retrieve today's logs from database.")
                .await?;
            return Ok(());
        }
    };

    // 2. Generate prompt
    match crate::reflection::generate_reflection_prompt(llm, &txs, &healths, &date_str).await {
        Ok(prompt) => {
            // Update state to wait for reflection response
            {
                let mut s = states.write().await;
                s.insert(
                    chat_id,
                    ConversationState::WaitingForReflection {
                        date: date_str,
                        prompt: prompt.clone(),
                    },
                );
            }

            let msg_text = format!(
                "📝 *Evening Journaling Reflection Prompt*:\n\n\
                 _{}_\n\n\
                 Reply directly to this message to record your daily reflection entry in your journal.",
                prompt
            );

            bot.send_message(chat_id, msg_text)
                .parse_mode(teloxide::types::ParseMode::Markdown)
                .await?;
        }
        Err(e) => {
            eprintln!("Failed to generate reflection prompt: {:?}", e);
            bot.send_message(
                chat_id,
                format!("❌ Failed to generate reflection prompt: {}", e),
            )
            .await?;
        }
    }

    Ok(())
}

async fn handle_message(
    bot: Bot,
    msg: Message,
    pool: SqlitePool,
    llm: ChotuLlm,
    gemini_client: GeminiClient,
    states: StateMap,
    config: AppConfig,
) -> Result<(), teloxide::RequestError> {
    let chat_id = msg.chat.id;
    let username = msg
        .from()
        .as_ref()
        .and_then(|u| u.username.as_deref())
        .unwrap_or("unknown");
    let text = msg.text().unwrap_or("");
    println!(
        "Telegram Bot: Received message from user '{}' in Chat ID: {:?}. Content: {:?}",
        username, chat_id, text
    );

    // Check if the message is a reply to a previous bot notification
    if let Some(reply_to_msg) = msg.reply_to_message() {
        let reply_text = text.trim().to_lowercase();
        let is_unactionable_cue = reply_text == "not useful"
            || reply_text == "unactionable"
            || reply_text == "ignore"
            || reply_text == "not worth it"
            || reply_text == "delete"
            || reply_text == "trash"
            || reply_text == "useless"
            || reply_text.contains("not useful")
            || reply_text.contains("unactionable")
            || reply_text.contains("ignore")
            || reply_text.contains("not worth")
            || reply_text.contains("useless");

        if is_unactionable_cue {
            let replied_msg_id = reply_to_msg.id.0;
            let task_opt: Option<(String, Option<String>, Option<String>, Option<String>)> = sqlx::query_as(
                "SELECT title, email_sender, email_subject, id FROM tasks WHERE telegram_message_id = ?"
            )
            .bind(replied_msg_id)
            .fetch_optional(&pool)
            .await
            .ok()
            .flatten();

            if let Some((title, email_sender, email_subject, task_id)) = task_opt {
                println!("Telegram Bot: Marking task as ignored and recording feedback for task: {}", title);

                sqlx::query("UPDATE tasks SET status = 'ignored' WHERE id = ?")
                    .bind(&task_id)
                    .execute(&pool)
                    .await
                    .ok();

                let feedback_id = uuid::Uuid::new_v4().to_string();
                let sender = email_sender.unwrap_or_else(|| "Unknown".to_string());
                let subject = email_subject.unwrap_or_else(|| "No Subject".to_string());

                sqlx::query(
                    "INSERT INTO unactionable_emails_feedback (id, sender, subject, task_description) VALUES (?, ?, ?, ?)"
                )
                .bind(&feedback_id)
                .bind(&sender)
                .bind(&subject)
                .bind(&title)
                .execute(&pool)
                .await
                .ok();

                bot.send_message(
                    chat_id,
                    format!("🗑️ Got it! Marked the task \"{}\" as unactionable. Similar emails will be filtered out in the future.", title)
                ).await?;
                return Ok(());
            }
        }
    }

    // Check if waiting for reflection
    let active_state = {
        let s = states.read().await;
        s.get(&chat_id).cloned()
    };

    if let Some(ConversationState::WaitingForReflection { date, prompt }) = active_state {
        let response_text = msg.text().unwrap_or("").trim();
        if response_text.is_empty() {
            bot.send_message(chat_id, "Reflection text cannot be empty. Please type your reflection or send a command to cancel.").await?;
            return Ok(());
        }

        bot.send_message(chat_id, "Saving reflection entry to your local journal...")
            .await?;

        // Query day data again to write in YAML
        let (txs, healths) = match crate::reflection::get_daily_data(&pool, &date, &config).await {
            Ok(data) => data,
            Err(e) => {
                eprintln!("Failed to query daily data during save: {:?}", e);
                bot.send_message(
                    chat_id,
                    "Failed to retrieve today's logs to compile journal header.",
                )
                .await?;
                return Ok(());
            }
        };

        match crate::reflection::save_reflection(&date, &prompt, response_text, &txs, &healths)
            .await
        {
            Ok(filepath) => {
                // Clear state
                {
                    let mut s = states.write().await;
                    s.insert(chat_id, ConversationState::Idle);
                }

                let index = MemoryIndex::from_env();
                if let Err(e) = index.index_journal_file(&pool, &filepath).await {
                    eprintln!("Memory: failed to index journal {:?}: {:?}", filepath, e);
                }

                let filename = filepath
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("journal.md");
                bot.send_message(
                    chat_id,
                    format!("✨ *Reflection Recorded!*\nSaved file `{}` inside `~/chotu_brain/Journal/`.", filename)
                )
                .parse_mode(teloxide::types::ParseMode::Markdown)
                .await?;
            }
            Err(e) => {
                eprintln!("Failed to save journal reflection file: {:?}", e);
                bot.send_message(chat_id, format!("❌ Failed to write journal file: {}", e))
                    .await?;
            }
        }
    } else if msg.photo().is_some() {
        handle_food_photo(&bot, chat_id, &msg, &pool, &gemini_client, &config).await?;
    } else {
        dispatch_free_text_intent(
            &bot,
            chat_id,
            text,
            &pool,
            &llm,
            &gemini_client,
            &config,
        )
        .await?;
    }

    Ok(())
}

/// Download a Telegram food photo, analyze with Gemini (+ Open Food Facts for barcodes), persist.
async fn handle_food_photo(
    bot: &Bot,
    chat_id: ChatId,
    msg: &Message,
    pool: &SqlitePool,
    gemini_client: &GeminiClient,
    config: &AppConfig,
) -> Result<(), teloxide::RequestError> {
    let photos = match msg.photo() {
        Some(p) if !p.is_empty() => p,
        _ => {
            bot.send_message(chat_id, "Couldn't read that photo. Try sending it again.")
                .await?;
            return Ok(());
        }
    };
    // Last PhotoSize is the largest resolution.
    let best = photos.last().expect("non-empty photo sizes");
    let caption = msg.caption().unwrap_or("").trim();

    bot.send_message(
        chat_id,
        "🔍 Analyzing food photo (barcode / package / plate)...",
    )
    .await?;

    let file = match bot.get_file(&best.file.id).await {
        Ok(f) => f,
        Err(e) => {
            eprintln!("Failed to get Telegram file metadata: {:?}", e);
            bot.send_message(chat_id, "❌ Couldn't download that photo from Telegram.")
                .await?;
            return Ok(());
        }
    };

    let mut image_bytes: Vec<u8> = Vec::new();
    if let Err(e) = bot.download_file(&file.path, &mut image_bytes).await {
        eprintln!("Failed to download Telegram photo: {:?}", e);
        bot.send_message(chat_id, "❌ Couldn't download that photo from Telegram.")
            .await?;
        return Ok(());
    }

    let analysis = match gemini_client
        .approximate_nutrition_from_image(&image_bytes, "image/jpeg", caption)
        .await
    {
        Ok(a) => a,
        Err(e) => {
            eprintln!("Gemini food-photo analysis failed: {:?}", e);
            bot.send_message(
                chat_id,
                format!("❌ Failed to analyze food photo: {}", e),
            )
            .await?;
            return Ok(());
        }
    };

    if analysis.kind == FoodPhotoKind::Unknown {
        bot.send_message(
            chat_id,
            "Doesn't look like food — send a barcode, product package, or plated meal \
             (optional caption like `praj half the bowl`).",
        )
        .await?;
        return Ok(());
    }

    let (member_id, caption_rest) = resolve_food_member_and_description(caption, config);

    let (description, nutrition, source_note) = if let Some(ref barcode) = analysis.barcode {
        match lookup_barcode(barcode).await {
            Ok(Some(product)) => {
                let desc = if caption_rest.is_empty() {
                    format!("{} [barcode {}]", product.product_name, barcode)
                } else {
                    format!(
                        "{} — {} [barcode {}]",
                        product.product_name, caption_rest, barcode
                    )
                };
                (
                    desc,
                    product.nutrition,
                    format!("Open Food Facts ({})", barcode),
                )
            }
            Ok(None) => {
                let desc = if analysis.description.trim().is_empty() {
                    format!("Barcode {} (not in Open Food Facts)", barcode)
                } else if caption_rest.is_empty() {
                    analysis.description.clone()
                } else {
                    format!("{} ({})", analysis.description, caption_rest)
                };
                (
                    desc,
                    analysis.nutrition,
                    format!("Gemini vision; barcode {} not in Open Food Facts", barcode),
                )
            }
            Err(e) => {
                eprintln!("Open Food Facts lookup error: {:?}", e);
                let desc = if caption_rest.is_empty() {
                    analysis.description.clone()
                } else {
                    format!("{} ({})", analysis.description, caption_rest)
                };
                (desc, analysis.nutrition, "Gemini vision (OFF lookup failed)".to_string())
            }
        }
    } else {
        let desc = if analysis.description.trim().is_empty() {
            if caption_rest.is_empty() {
                "Food photo".to_string()
            } else {
                caption_rest.clone()
            }
        } else if caption_rest.is_empty()
            || analysis
                .description
                .to_lowercase()
                .contains(&caption_rest.to_lowercase())
        {
            analysis.description.clone()
        } else {
            format!("{} ({})", analysis.description, caption_rest)
        };
        (desc, analysis.nutrition, "Gemini vision".to_string())
    };

    println!(
        "Telegram Bot: food photo kind={:?} source={} member={}",
        analysis.kind, source_note, member_id
    );

    bot.send_message(
        chat_id,
        format!("Using {} for *{}*…", source_note, member_id),
    )
    .parse_mode(teloxide::types::ParseMode::Markdown)
    .await?;

    persist_food_estimation(
        bot,
        chat_id,
        pool,
        config,
        &member_id,
        &description,
        &nutrition,
    )
    .await?;

    Ok(())
}

/// Classify idle free-text with local Ollama and reuse existing command handlers.
async fn dispatch_free_text_intent(
    bot: &Bot,
    chat_id: ChatId,
    text: &str,
    pool: &SqlitePool,
    llm: &ChotuLlm,
    gemini_client: &GeminiClient,
    config: &AppConfig,
) -> Result<(), teloxide::RequestError> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        bot.send_message(
            chat_id,
            "Send a message like \"what's today\", \"morning brief\", \"open tasks\", or \"log eggs for praj\".",
        )
        .await?;
        return Ok(());
    }

    let member_ids: Vec<String> = config
        .family
        .members
        .iter()
        .map(|m| m.id.clone())
        .collect();

    let classification = match llm.classify_intent(trimmed, &member_ids).await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Intent classification failed: {:?}", e);
            bot.send_message(
                chat_id,
                "I couldn't understand that just now. Try a slash command (`/status`, `/tasks`, `/food`) or rephrase.",
            )
            .await?;
            return Ok(());
        }
    };

    println!(
        "Telegram Bot: free-text intent={:?} reason={}",
        classification.intent, classification.reason
    );

    match classification.into_user_intent() {
        UserIntent::Status => handle_status(bot, chat_id, pool, config).await?,
        UserIntent::Brief => handle_brief(bot, chat_id, pool, config).await?,
        UserIntent::Calendar { window } => {
            handle_cal(bot, chat_id, window, config).await?;
        }
        UserIntent::Trends { days } => {
            let args = days.map(|d| d.to_string()).unwrap_or_default();
            handle_trends(bot, chat_id, args, pool, config).await?;
        }
        UserIntent::Tasks { filter } => {
            handle_tasks(bot, chat_id, filter, pool, config).await?;
        }
        UserIntent::Memory { query } => {
            handle_memory(bot, chat_id, query, pool, llm, gemini_client).await?;
        }
        UserIntent::Sync => {
            if let Err(e) =
                sync_google_health_nutrition(bot, chat_id, pool, gemini_client, config).await
            {
                eprintln!("Free-text sync failed: {:?}", e);
                bot.send_message(chat_id, format!("❌ Sync failed: {}", e))
                    .await?;
            }
        }
        UserIntent::Food {
            member_id,
            description,
        } => {
            let args = match member_id {
                Some(id) => format!("{} {}", id, description),
                None => description,
            };
            handle_food_log(bot, chat_id, args, pool, gemini_client, config).await?;
        }
        UserIntent::Networth => {
            handle_networth(bot, chat_id, pool, gemini_client, config).await?;
        }
        UserIntent::Monthly { yyyy_mm } => {
            handle_monthly(
                bot,
                chat_id,
                yyyy_mm.unwrap_or_default(),
                pool,
                config,
            )
            .await?;
        }
        UserIntent::Help => {
            let help_text = format!(
                "👋 Hi! I'm Chotu. You can use slash commands or plain English \
                 (calendar, brief, status, tasks, memory, food, sync, trends, net worth, monthly).\n\n{}",
                Command::descriptions()
            );
            bot.send_message(chat_id, help_text).await?;
        }
        UserIntent::Unknown { clarify_question } => {
            bot.send_message(chat_id, clarify_question).await?;
        }
    }

    Ok(())
}

async fn run_and_log_stock_research(
    bot: &Bot,
    chat_id: ChatId,
    pool: &SqlitePool,
    researcher: &StockResearcher,
    philosophy: Option<&InvestmentPhilosophy>,
    targets: Option<&str>,
) -> Result<(), anyhow::Error> {
    let default_philosophy = InvestmentPhilosophy::default();
    let p = philosophy.unwrap_or(&default_philosophy);
    let status_msg = format!("🔍 Initiating stock research matching our philosophy (focusing on: {})...", p.description);
    bot.send_message(chat_id, status_msg).await?;

    let report = run_stock_research(pool, researcher, philosophy, targets)
        .await
        .map_err(|e| anyhow::anyhow!("Stock research failed: {:?}", e))?;

    // Split the report into chunks under 4000 characters to respect Telegram's message limit
    let chunks = split_message(&report, 4000);
    for chunk in chunks {
        // Try sending each chunk with Markdown, fallback to plain text if parsing fails
        if let Err(e) = bot
            .send_message(chat_id, &chunk)
            .parse_mode(teloxide::types::ParseMode::Markdown)
            .await
        {
            eprintln!(
                "Telegram Bot: failed to send report chunk with Markdown format ({:?}). Falling back to plain text...",
                e
            );
            bot.send_message(chat_id, &chunk).await?;
        }
    }

    Ok(())
}

async fn sync_google_health_nutrition(
    bot: &Bot,
    chat_id: ChatId,
    pool: &SqlitePool,
    gemini_client: &GeminiClient,
    config: &AppConfig,
) -> Result<(), anyhow::Error> {
    bot.send_message(
        chat_id,
        "🔄 Connecting to Google Health API and pulling today's health metrics...",
    )
    .await?;

    match health_coach::sync_configured_members_today(pool, Some(gemini_client), config).await {
        Ok(reports) => {
            for report in reports {
                bot.send_message(chat_id, report.telegram_markdown())
                    .parse_mode(teloxide::types::ParseMode::Markdown)
                    .await?;
            }
        }
        Err(e) => {
            bot.send_message(chat_id, format!("❌ Google Health sync failed: {}", e))
                .await?;
        }
    }

    Ok(())
}

fn split_message(text: &str, limit: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut current_chunk = String::new();

    for line in text.lines() {
        if current_chunk.len() + line.len() + 1 > limit {
            if !current_chunk.is_empty() {
                chunks.push(current_chunk.clone());
                current_chunk.clear();
            }

            if line.len() > limit {
                let mut line_str = line.to_string();
                while line_str.len() > limit {
                    let part: String = line_str.chars().take(limit).collect();
                    chunks.push(part);
                    line_str = line_str.chars().skip(limit).collect();
                }
                current_chunk = line_str;
            } else {
                current_chunk.push_str(line);
                current_chunk.push('\n');
            }
        } else {
            current_chunk.push_str(line);
            current_chunk.push('\n');
        }
    }

    if !current_chunk.is_empty() {
        chunks.push(current_chunk);
    }

    chunks
}

async fn handle_login_calendar(
    bot: &Bot,
    chat_id: ChatId,
    member_id: &str,
    config: &AppConfig,
) -> Result<(), anyhow::Error> {
    if member_id.is_empty() {
        let members: Vec<String> = config
            .family
            .members
            .iter()
            .filter(|m| m.calendar.is_some())
            .map(|m| format!("`{}` ({})", m.id, m.name))
            .collect();
        let list = if members.is_empty() {
            "_No members have a calendar block in config.yaml_".to_string()
        } else {
            members.join(", ")
        };
        bot.send_message(
            chat_id,
            format!(
                "⚠️ Usage: `/login calendar <member_id>`\n\nConfigured calendar members: {}",
                list
            ),
        )
        .parse_mode(teloxide::types::ParseMode::Markdown)
        .await?;
        return Ok(());
    }

    let member = match config.family.members.iter().find(|m| m.id == member_id) {
        Some(m) => m,
        None => {
            bot.send_message(
                chat_id,
                format!(
                    "❌ Unknown member `{}`. Check `family.members` in config.yaml.",
                    member_id
                ),
            )
            .parse_mode(teloxide::types::ParseMode::Markdown)
            .await?;
            return Ok(());
        }
    };

    if member.calendar.is_none() {
        bot.send_message(
            chat_id,
            format!(
                "❌ Member `{}` has no `calendar:` block in config.yaml.",
                member_id
            ),
        )
        .parse_mode(teloxide::types::ParseMode::Markdown)
        .await?;
        return Ok(());
    }

    let client_id = match std::env::var("CHOTU_OAUTH_CLIENT_ID") {
        Ok(val) => val,
        Err(_) => {
            bot.send_message(
                chat_id,
                "❌ *Calendar Setup Required*\n\nConfigure `CHOTU_OAUTH_CLIENT_ID` and `CHOTU_OAUTH_CLIENT_SECRET` in `.env` (same Google OAuth client as Gmail).",
            )
            .parse_mode(teloxide::types::ParseMode::Markdown)
            .await?;
            return Ok(());
        }
    };
    let client_secret = match std::env::var("CHOTU_OAUTH_CLIENT_SECRET") {
        Ok(val) => val,
        Err(_) => {
            bot.send_message(
                chat_id,
                "❌ Configure `CHOTU_OAUTH_CLIENT_SECRET` in `.env`.",
            )
            .await?;
            return Ok(());
        }
    };

    let auth_url = format!(
        "https://accounts.google.com/o/oauth2/v2/auth?prompt=consent&access_type=offline&response_type=code&client_id={}&redirect_uri=http%3A%2F%2Flocalhost%3A8080%2Fcallback&scope=https%3A%2F%2Fwww.googleapis.com%2Fauth%2Fcalendar",
        client_id
    );

    let env_key = member.calendar_refresh_token_env_key();
    let msg = format!(
        "🔗 *Google Calendar Login for {}*\n\n\
         Sign in with *{}* and authorize calendar access:\n{}\n\n\
         Waiting up to 5 minutes for the localhost callback...\n\
         Or paste the code manually: `/login code calendar {} <code_or_url>`",
        member.name,
        member
            .calendar
            .as_ref()
            .map(|c| c.email.as_str())
            .unwrap_or("the member's Google account"),
        auth_url,
        member.id
    );
    bot.send_message(chat_id, msg)
        .parse_mode(teloxide::types::ParseMode::Markdown)
        .await?;

    let bot_clone = bot.clone();
    let member_id_owned = member.id.clone();
    tokio::spawn(async move {
        let listener = tokio::time::timeout(
            tokio::time::Duration::from_secs(300),
            start_redirect_listener(8080),
        )
        .await;

        match listener {
            Ok(Ok(code)) => {
                match exchange_google_code(
                    &client_id,
                    &client_secret,
                    &code,
                    "http://localhost:8080/callback",
                )
                .await
                {
                    Ok(tokens) => {
                        if let Err(e) =
                            save_calendar_refresh_token(&member_id_owned, &tokens.refresh_token)
                        {
                            let _ = bot_clone
                                .send_message(
                                    chat_id,
                                    format!("❌ Failed to save calendar token: {}", e),
                                )
                                .await;
                            return;
                        }
                        let _ = bot_clone
                            .send_message(
                                chat_id,
                                format!(
                                    "✅ *Calendar Authorization Successful!*\nSaved `{}` to `.env`.",
                                    env_key
                                ),
                            )
                            .parse_mode(teloxide::types::ParseMode::Markdown)
                            .await;
                    }
                    Err(e) => {
                        let _ = bot_clone
                            .send_message(
                                chat_id,
                                format!("❌ Calendar token exchange failed: {}", e),
                            )
                            .await;
                    }
                }
            }
            Ok(Err(e)) => {
                let _ = bot_clone
                    .send_message(chat_id, format!("❌ Calendar OAuth listener error: {}", e))
                    .await;
            }
            Err(_) => {
                let _ = bot_clone
                    .send_message(
                        chat_id,
                        format!(
                            "❌ *Calendar Login Timeout*\n\nTry again with `/login calendar {}` or `/login code calendar {} <code>`.",
                            member_id_owned, member_id_owned
                        ),
                    )
                    .parse_mode(teloxide::types::ParseMode::Markdown)
                    .await;
            }
        }
    });

    Ok(())
}

async fn handle_login_google_health(
    bot: &Bot,
    chat_id: ChatId,
    member_id: &str,
    config: &AppConfig,
) -> Result<(), anyhow::Error> {
    let member_id = if member_id.is_empty() {
        match config.family.members.first() {
            Some(m) => m.id.clone(),
            None => {
                bot.send_message(
                    chat_id,
                    "⚠️ Usage: `/login health <member_id>`\n\nNo family members configured in config.yaml.",
                )
                .parse_mode(teloxide::types::ParseMode::Markdown)
                .await?;
                return Ok(());
            }
        }
    } else {
        member_id.to_string()
    };

    let member = match config
        .family
        .members
        .iter()
        .find(|m| m.id.eq_ignore_ascii_case(&member_id))
    {
        Some(m) => m,
        None => {
            let members: Vec<String> = config
                .family
                .members
                .iter()
                .map(|m| format!("`{}` ({})", m.id, m.name))
                .collect();
            bot.send_message(
                chat_id,
                format!(
                    "❌ Unknown member `{}`.\n\nConfigured family members: {}",
                    member_id,
                    members.join(", ")
                ),
            )
            .parse_mode(teloxide::types::ParseMode::Markdown)
            .await?;
            return Ok(());
        }
    };
    let member_id = member.id.clone();
    let member_name = member.name.clone();
    let is_primary = config
        .family
        .members
        .first()
        .is_some_and(|m| m.id == member_id);
    let env_key = member.health_refresh_token_env_key();

    let client_id = match std::env::var("FITBIT_CLIENT_ID") {
        Ok(val) => val,
        Err(_) => {
            let msg = "❌ *Google Health Setup Required*\n\n\
                Please configure `FITBIT_CLIENT_ID` and `FITBIT_CLIENT_SECRET` in your `.env` file first.\n\n\
                1. Go to the [Google Cloud Console](https://console.cloud.google.com/), create a project, and enable the Google Health API.\n\
                2. Configure the OAuth Consent Screen (add each family member's email as a test user).\n\
                3. Go to Credentials, create a client ID for a **Web Application**, and set the redirect URI to `http://localhost:8080/callback`.\n\
                4. Paste the client credentials into your `.env` file as `FITBIT_CLIENT_ID` and `FITBIT_CLIENT_SECRET`, then restart the agent.";
            bot.send_message(chat_id, msg)
                .parse_mode(teloxide::types::ParseMode::Markdown)
                .await?;
            return Ok(());
        }
    };
    let client_secret = match std::env::var("FITBIT_CLIENT_SECRET") {
        Ok(val) => val,
        Err(_) => {
            let msg = "❌ *Google Health Setup Required*\n\n\
                Please configure `FITBIT_CLIENT_SECRET` in your `.env` file.";
            bot.send_message(chat_id, msg)
                .parse_mode(teloxide::types::ParseMode::Markdown)
                .await?;
            return Ok(());
        }
    };

    let auth_url = format!(
        "https://accounts.google.com/o/oauth2/v2/auth?prompt=consent&access_type=offline&response_type=code&client_id={}&redirect_uri=http%3A%2F%2Flocalhost%3A8080%2Fcallback&scope={}",
        client_id,
        chotu_common::GOOGLE_HEALTH_OAUTH_SCOPES.replace(' ', "%20")
    );

    let setup_msg = format!(
        "🔗 *Google Health Login for {}*\n\n\
        Sign in with *{}*'s Google account and authorize health access:\n\
        [Authorize Google Health]({})\n\n\
        Waiting up to 5 minutes for the localhost callback...\n\
        Or paste the code manually: `/login code health {} <code_or_url>`\n\n\
        Token will be saved as `{}`.",
        member_name, member_name, auth_url, member_id, env_key
    );

    bot.send_message(chat_id, setup_msg)
        .parse_mode(teloxide::types::ParseMode::Markdown)
        .await?;

    let bot_clone = bot.clone();
    let member_id_owned = member_id.clone();
    let env_key_owned = env_key.clone();
    tokio::spawn(async move {
        println!(
            "OAuth: Starting Google Health redirect listener on port 8080 for {}...",
            member_id_owned
        );
        let listener_result = tokio::time::timeout(
            std::time::Duration::from_secs(300),
            start_redirect_listener(8080),
        )
        .await;

        match listener_result {
            Ok(Ok(code)) => {
                println!("OAuth: Received authorization code. Exchanging for tokens...");
                let _ = bot_clone
                    .send_message(
                        chat_id,
                        "⏳ Received authorization code. Swapping for tokens...",
                    )
                    .await;

                match exchange_google_code(
                    &client_id,
                    &client_secret,
                    &code,
                    "http://localhost:8080/callback",
                )
                .await
                {
                    Ok(tokens) => {
                        match save_health_refresh_token(
                            &member_id_owned,
                            &tokens.refresh_token,
                            is_primary,
                        ) {
                            Ok(_) => {
                                let success_msg = format!(
                                    "✅ *Google Health Authorization Successful!*\n\n\
                                     Saved `{}` for *{}*.\n\
                                     `/sync` and `/food {}` will use this account.",
                                    env_key_owned, member_id_owned, member_id_owned
                                );
                                let _ = bot_clone
                                    .send_message(chat_id, success_msg)
                                    .parse_mode(teloxide::types::ParseMode::Markdown)
                                    .await;
                                println!(
                                    "OAuth: Google Health refresh token saved as {}",
                                    env_key_owned
                                );
                            }
                            Err(e) => {
                                let err_msg = format!(
                                    "❌ Failed to write Google Health refresh token to `.env`: {}",
                                    e
                                );
                                let _ = bot_clone.send_message(chat_id, err_msg).await;
                                eprintln!("OAuth: Failed to write Google Health token: {:?}", e);
                            }
                        }
                    }
                    Err(e) => {
                        let err_msg = format!("❌ Google Health token exchange failed: {}", e);
                        let _ = bot_clone.send_message(chat_id, err_msg).await;
                        eprintln!("OAuth: Google Health token exchange failed: {:?}", e);
                    }
                }
            }
            Ok(Err(e)) => {
                let err_msg = format!("❌ Google Health callback server failed: {}", e);
                let _ = bot_clone.send_message(chat_id, err_msg).await;
                eprintln!("OAuth: Google Health callback listener failed: {:?}", e);
            }
            Err(_) => {
                let timeout_msg = format!(
                    "❌ *Google Health Login Timeout*\n\n\
                     Try again with `/login health {}` or `/login code health {} <code>`.",
                    member_id_owned, member_id_owned
                );
                let _ = bot_clone
                    .send_message(chat_id, timeout_msg)
                    .parse_mode(teloxide::types::ParseMode::Markdown)
                    .await;
                println!("OAuth: Google Health redirect listener timed out");
            }
        }
    });

    Ok(())
}

async fn handle_login_google(bot: &Bot, chat_id: ChatId) -> Result<(), anyhow::Error> {
    let client_id = match std::env::var("CHOTU_OAUTH_CLIENT_ID") {
        Ok(val) => val,
        Err(_) => {
            let msg = "❌ *Google Setup Required*\n\n\
                Please configure `CHOTU_OAUTH_CLIENT_ID` and `CHOTU_OAUTH_CLIENT_SECRET` in your `.env` file first.\n\n\
                1. Go to the [Google Cloud Console](https://console.cloud.google.com/), create a project and OAuth 2.0 Credentials.\n\
                2. Set the Redirect URI to `http://localhost:8080/callback`.\n\
                3. Paste the client credentials into your `.env` file and restart the agent.";
            bot.send_message(chat_id, msg)
                .parse_mode(teloxide::types::ParseMode::Markdown)
                .await?;
            return Ok(());
        }
    };
    let client_secret = match std::env::var("CHOTU_OAUTH_CLIENT_SECRET") {
        Ok(val) => val,
        Err(_) => {
            let msg = "❌ *Google Setup Required*\n\n\
                Please configure `CHOTU_OAUTH_CLIENT_SECRET` in your `.env` file.";
            bot.send_message(chat_id, msg)
                .parse_mode(teloxide::types::ParseMode::Markdown)
                .await?;
            return Ok(());
        }
    };

    let auth_url = format!(
        "https://accounts.google.com/o/oauth2/v2/auth?prompt=consent&access_type=offline&response_type=code&client_id={}&redirect_uri=http%3A%2F%2Flocalhost%3A8080%2Fcallback&scope=https%3A%2F%2Fmail.google.com%2F",
        client_id
    );

    let setup_msg = format!(
        "🔗 *Google Gmail Login Flow Initiated*\n\n\
        1. Click the link below to log in and authorize Gmail access:\n\
        [Authorize Google]({})\n\n\
        2. Once authorized, you will be redirected to `http://localhost:8080/callback`, and the agent will automatically complete the setup.\n\n\
        _Note: The login listener will automatically timeout in 5 minutes._",
        auth_url
    );

    bot.send_message(chat_id, setup_msg)
        .parse_mode(teloxide::types::ParseMode::Markdown)
        .await?;

    let bot_clone = bot.clone();
    tokio::spawn(async move {
        println!("OAuth: Starting Google redirect listener on port 8080...");
        // 5-minute timeout for user to authorize
        let listener_result = tokio::time::timeout(
            std::time::Duration::from_secs(300),
            start_redirect_listener(8080),
        )
        .await;

        match listener_result {
            Ok(Ok(code)) => {
                println!("OAuth: Received Google authorization code. Exchanging for tokens...");
                let _ = bot_clone
                    .send_message(
                        chat_id,
                        "⏳ Received authorization code. Swapping for tokens...",
                    )
                    .await;

                match exchange_google_code(
                    &client_id,
                    &client_secret,
                    &code,
                    "http://localhost:8080/callback",
                )
                .await
                {
                    Ok(tokens) => match save_google_refresh_token(&tokens.refresh_token) {
                        Ok(_) => {
                            let success_msg = "✅ *Google/Gmail Authorization Successful!*\n\n\
                                    The new refresh token has been successfully written to your `.env` file.\n\
                                    Google statement/receipt email sync is now active!";
                            let _ = bot_clone
                                .send_message(chat_id, success_msg)
                                .parse_mode(teloxide::types::ParseMode::Markdown)
                                .await;
                            println!("OAuth: Google refresh token successfully saved to .env");
                        }
                        Err(e) => {
                            let err_msg =
                                format!("❌ Failed to write Google refresh token to `.env`: {}", e);
                            let _ = bot_clone.send_message(chat_id, err_msg).await;
                            eprintln!("OAuth: Failed to write Google token: {:?}", e);
                        }
                    },
                    Err(e) => {
                        let err_msg = format!("❌ Google token exchange failed: {}", e);
                        let _ = bot_clone.send_message(chat_id, err_msg).await;
                        eprintln!("OAuth: Google token exchange failed: {:?}", e);
                    }
                }
            }
            Ok(Err(e)) => {
                let err_msg = format!("❌ Google callback server failed: {}", e);
                let _ = bot_clone.send_message(chat_id, err_msg).await;
                eprintln!("OAuth: Google callback listener failed: {:?}", e);
            }
            Err(_) => {
                let timeout_msg = "❌ *Google Login Timeout*\n\nThe login listener timed out after 5 minutes. Please try again with `/login gmail`.";
                let _ = bot_clone
                    .send_message(chat_id, timeout_msg)
                    .parse_mode(teloxide::types::ParseMode::Markdown)
                    .await;
                println!("OAuth: Google redirect listener timed out");
            }
        }
    });

    Ok(())
}

fn clean_oauth_code(code_or_url: &str) -> String {
    if let Some(pos) = code_or_url.find("code=") {
        let after_code = &code_or_url[pos + 5..];
        if let Some(end) = after_code.find('&') {
            return after_code[..end].to_string();
        }
        return after_code.to_string();
    }
    code_or_url.trim().to_string()
}

async fn handle_manual_code(
    bot: &Bot,
    chat_id: ChatId,
    args: &str,
    config: &AppConfig,
) -> Result<(), anyhow::Error> {
    let mut parts = args.split_whitespace();
    let service = match parts.next() {
        Some(s) => s.to_lowercase(),
        None => {
            bot.send_message(
                chat_id,
                "⚠️ Usage: `/login code <gmail|health|calendar> ...`",
            )
            .await?;
            return Ok(());
        }
    };

    if service == "gmail" || service == "google" {
        let code_raw = match parts.next() {
            Some(c) => c,
            None => {
                bot.send_message(chat_id, "⚠️ Usage: `/login code gmail <code_or_url>`").await?;
                return Ok(());
            }
        };
        let code = clean_oauth_code(code_raw);

        let client_id = std::env::var("CHOTU_OAUTH_CLIENT_ID")?;
        let client_secret = std::env::var("CHOTU_OAUTH_CLIENT_SECRET")?;

        bot.send_message(chat_id, "⏳ Swapping manual code for Google/Gmail tokens...").await?;
        match exchange_google_code(&client_id, &client_secret, &code, "http://localhost:8080/callback").await {
            Ok(tokens) => {
                save_google_refresh_token(&tokens.refresh_token)?;
                bot.send_message(chat_id, "✅ *Google/Gmail Authorization Successful!*\nRefresh token saved manually.").await?;
            }
            Err(e) => {
                bot.send_message(chat_id, format!("❌ Gmail Token exchange failed: {}", e)).await?;
            }
        }
    } else if service == "fitbit" || service == "health" {
        let first = match parts.next() {
            Some(c) => c.to_string(),
            None => {
                bot.send_message(
                    chat_id,
                    "⚠️ Usage: `/login code health <member_id> <code_or_url>`",
                )
                .await?;
                return Ok(());
            }
        };
        let second = parts.next().map(|s| s.to_string());

        let (member_id, code_raw) = match second.as_deref() {
            Some(code) => (first, code.to_string()),
            None => {
                // Back-compat: `/login code health <code>` → primary member
                if config.family.members.iter().any(|m| m.id == first) {
                    bot.send_message(
                        chat_id,
                        "⚠️ Usage: `/login code health <member_id> <code_or_url>`",
                    )
                    .await?;
                    return Ok(());
                }
                let primary = config
                    .family
                    .members
                    .first()
                    .map(|m| m.id.clone())
                    .unwrap_or_else(|| "alex".to_string());
                (primary, first)
            }
        };

        let member = match config
            .family
            .members
            .iter()
            .find(|m| m.id.eq_ignore_ascii_case(&member_id))
        {
            Some(m) => m,
            None => {
                bot.send_message(chat_id, format!("❌ Unknown member `{}`.", member_id))
                    .parse_mode(teloxide::types::ParseMode::Markdown)
                    .await?;
                return Ok(());
            }
        };
        let member_id = member.id.clone();
        let is_primary = config
            .family
            .members
            .first()
            .is_some_and(|m| m.id == member_id);
        let env_key = member.health_refresh_token_env_key();
        let code = clean_oauth_code(&code_raw);

        let client_id = std::env::var("FITBIT_CLIENT_ID")?;
        let client_secret = std::env::var("FITBIT_CLIENT_SECRET")?;

        bot.send_message(chat_id, "⏳ Swapping manual code for Google Health tokens...")
            .await?;
        match exchange_google_code(
            &client_id,
            &client_secret,
            &code,
            "http://localhost:8080/callback",
        )
        .await
        {
            Ok(tokens) => {
                save_health_refresh_token(&member_id, &tokens.refresh_token, is_primary)?;
                bot.send_message(
                    chat_id,
                    format!(
                        "✅ *Google Health Authorization Successful!*\nSaved `{}`.",
                        env_key
                    ),
                )
                .parse_mode(teloxide::types::ParseMode::Markdown)
                .await?;
            }
            Err(e) => {
                bot.send_message(
                    chat_id,
                    format!("❌ Google Health Token exchange failed: {}", e),
                )
                .await?;
            }
        }
    } else if service == "calendar" {
        let member_id = match parts.next() {
            Some(m) => m.to_string(),
            None => {
                bot.send_message(
                    chat_id,
                    "⚠️ Usage: `/login code calendar <member_id> <code_or_url>`",
                )
                .await?;
                return Ok(());
            }
        };
        if !config.family.members.iter().any(|m| m.id == member_id) {
            bot.send_message(
                chat_id,
                format!("❌ Unknown member `{}`.", member_id),
            )
            .parse_mode(teloxide::types::ParseMode::Markdown)
            .await?;
            return Ok(());
        }
        let code_raw = match parts.next() {
            Some(c) => c,
            None => {
                bot.send_message(
                    chat_id,
                    "⚠️ Usage: `/login code calendar <member_id> <code_or_url>`",
                )
                .await?;
                return Ok(());
            }
        };
        let code = clean_oauth_code(code_raw);
        let client_id = std::env::var("CHOTU_OAUTH_CLIENT_ID")?;
        let client_secret = std::env::var("CHOTU_OAUTH_CLIENT_SECRET")?;

        bot.send_message(chat_id, "⏳ Swapping manual code for Calendar tokens...")
            .await?;
        match exchange_google_code(
            &client_id,
            &client_secret,
            &code,
            "http://localhost:8080/callback",
        )
        .await
        {
            Ok(tokens) => {
                save_calendar_refresh_token(&member_id, &tokens.refresh_token)?;
                bot.send_message(
                    chat_id,
                    format!(
                        "✅ *Calendar Authorization Successful!*\nSaved `CALENDAR_REFRESH_TOKEN_{}`.",
                        member_id.to_uppercase()
                    ),
                )
                .parse_mode(teloxide::types::ParseMode::Markdown)
                .await?;
            }
            Err(e) => {
                bot.send_message(chat_id, format!("❌ Calendar token exchange failed: {}", e))
                    .await?;
            }
        }
    } else {
        bot.send_message(
            chat_id,
            "⚠️ Unknown service. Supported: `gmail`, `health`, `calendar`",
        )
        .await?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_split_message() {
        let sample = "Line 1\nLine 2\nLine 3\nLine 4";
        let chunks = split_message(sample, 15);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0], "Line 1\nLine 2\n");
        assert_eq!(chunks[1], "Line 3\nLine 4\n");
    }
}
