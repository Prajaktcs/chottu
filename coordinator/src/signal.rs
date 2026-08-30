use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Context;
use sqlx::SqlitePool;
use tokio::sync::RwLock;

use chotu_common::{
    answer_memory_query, build_calendar_client, clear_budget_override, complete_all_open_tasks,
    compose_calendar_agenda, compute_budget_progress, config_path, current_budget_month,
    default_member_id, display_category, ensure_food_mutation_allowed, exchange_google_code,
    fetch_exchange_rates, fetch_stock_quotes_near_cost, format_budget_progress_markdown,
    has_signal_delivery, is_signal_conversation_allowed, list_completable_open_tasks,
    looks_like_task_add_query, lookup_barcode, mark_budget_alert_sent, member_for_signal_aci,
    effective_food_time, parse_due_phrase_tz, pending_budget_alerts, resolve_food_log_timing,
    assign_food_tags, delete_food_log_tags, delete_food_log_tags_for_member_day,
    insert_food_log_tags, reschedule_at, save_calendar_refresh_token, save_google_refresh_token,
    save_health_refresh_token, schedule_at, set_budget_override, set_member_signal_aci,
    spawn_background_reindex, split_task_add_args, start_redirect_listener,
    signal_aci_for_member, signal_delivery_targets, AppConfig, AssignedFoodTags,
    CalendarWindow, ChotuLlm, CostHint, FoodPhotoKind, GeminiClient, GoogleCalendarClient,
    InvestmentPhilosophy, MemoryIndex, SignalClient, SignalError, SignalInbound, SignalRecipient,
    UserIntent, TASK_CALENDAR_DURATION_MINUTES,
};
use finance_advisor::{run_stock_research_with_progress, ResearchProgress, StockResearcher};

type Bot = SignalClient;
type ChatId = SignalRecipient;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Help, Food(String), Status, Plan(String), Brief, Cal(String), Trends(String),
    Tasks(String), Task(String), Memory(String), Reflect, Chat, Link(String), Whoami,
    Research(String), Sync, Login(String), Clearfood(String), Adjustfood(String),
    Undofood(String), Networth, Monthly(String), Budget(String),
}

const HELP_TEXT: &str = "\
These commands are supported:
/help — display this text.
/food [member_id] <description> — log food.
/status — show today's status report.
/plan [new] — weekly training plan.
/brief — morning brief: calendar, tasks, bills, nutrition.
/cal [today|tomorrow|week] — calendar agenda.
/trends [days] — nutrition trends.
/tasks [open|all|completed|snoozed] [|member]
/tasks add [member] <title> [due|by <when>]
/tasks complete <id|all|all confirm>
/tasks snooze <id> [days]
/tasks reassign <id> <member>
/tasks open <id>
/task <title> [by|due <when>] — add a task
/memory <question> | /memory reindex
/reflect — evening reflection.
/chat — show this Signal conversation.
/link <member_id> — link this direct conversation.
/whoami — show the linked family member.
/research [companies] — stock research.
/sync — sync today's health metrics.
/login <health <member_id>|gmail|calendar <member_id>> or /login code <...>
/clearfood [member_id] — clear today's food logs.
/adjustfood [member_id] <calories> <protein> <carbs> <fats>
/undofood [member_id] — undo last food log.
/networth — invested net worth.
/monthly [YYYY-MM] — monthly summary.
/budget | /budget set <Category> <amount> | /budget clear <Category>";

fn parse_command(input: &str) -> Option<Command> {
    let input = input.trim();
    let body = input.strip_prefix('/')?;
    let (name, args) = match body.split_once(char::is_whitespace) {
        Some((name, rest)) => (name, rest.trim().to_string()),
        None => (body, String::new()),
    };
    if name.contains('@') {
        return None;
    }
    match name.to_ascii_lowercase().as_str() {
        "help" => Some(Command::Help),
        "food" => Some(Command::Food(args)),
        "status" => Some(Command::Status),
        "plan" => Some(Command::Plan(args)),
        "brief" => Some(Command::Brief),
        "cal" => Some(Command::Cal(args)),
        "trends" => Some(Command::Trends(args)),
        "tasks" => Some(Command::Tasks(args)),
        "task" => Some(Command::Task(args)),
        "memory" => Some(Command::Memory(args)),
        "reflect" => Some(Command::Reflect),
        "chat" => Some(Command::Chat),
        "link" => Some(Command::Link(args)),
        "whoami" => Some(Command::Whoami),
        "research" => Some(Command::Research(args)),
        "sync" => Some(Command::Sync),
        "login" => Some(Command::Login(args)),
        "clearfood" => Some(Command::Clearfood(args)),
        "adjustfood" => Some(Command::Adjustfood(args)),
        "undofood" => Some(Command::Undofood(args)),
        "networth" => Some(Command::Networth),
        "monthly" => Some(Command::Monthly(args)),
        "budget" => Some(Command::Budget(args)),
        _ => None,
    }
}

fn conversation_allowed(config: &AppConfig, chat_id: &ChatId, sender_aci: &str) -> bool {
    is_signal_conversation_allowed(config, sender_aci, chat_id.group_id())
}

fn task_complete_snooze_help(short_id: &str) -> String {
    format!("/tasks complete {short_id} · /tasks snooze {short_id} [days]")
}

async fn send_signal(
    bot: &Bot,
    chat_id: &ChatId,
    text: impl AsRef<str>,
) -> Result<i64, SignalError> {
    bot.send_text(chat_id, text.as_ref()).await
}

#[derive(Debug, Clone)]
pub enum ConversationState {
    Idle,
    WaitingForReflection { date: String, prompt: String },
}

type StateMap = Arc<RwLock<HashMap<ChatId, ConversationState>>>;
type SharedConfig = Arc<RwLock<AppConfig>>;

/// Send a household message to every linked member DM plus optional SIGNAL_GROUP_ID.
async fn send_household(bot: &Bot, config: &AppConfig, text: impl Into<String>) -> bool {
    send_household_attempts(bot, config, text, 1).await
}

async fn send_household_attempts(
    bot: &Bot,
    config: &AppConfig,
    text: impl Into<String>,
    attempts: u32,
) -> bool {
    let text = text.into();
    let targets = signal_delivery_targets(config);
    if targets.is_empty() {
        eprintln!("Signal: no delivery targets (link a member or set SIGNAL_GROUP_ID)");
        return false;
    }
    let mut any_ok = false;
    for cid in targets {
        if retry_scheduled_push(attempts, "household send", &cid, || {
            let bot = bot.clone();
            let text = text.clone();
            let cid = cid.clone();
            async move { send_signal(&bot, &cid, text).await.map(|_| ()) }
        })
        .await
        .is_ok()
        {
            any_ok = true;
        }
    }
    any_ok
}

const SCHEDULED_SIGNAL_ATTEMPTS: u32 = 3;

fn signal_error_is_retryable(err: &SignalError) -> bool {
    matches!(err, SignalError::Io(_) | SignalError::Eof | SignalError::Reconnecting)
}

async fn retry_scheduled_push<F, Fut>(
    attempts: u32,
    label: &str,
    chat_id: &ChatId,
    mut op: F,
) -> Result<(), SignalError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<(), SignalError>>,
{
    let attempts = attempts.max(1);
    for attempt in 1..=attempts {
        match op().await {
            Ok(()) => return Ok(()),
            Err(e) => {
                let retry = attempt < attempts && signal_error_is_retryable(&e);
                eprintln!(
                    "Signal: {} failed for {} (attempt {}/{}): {:?}",
                    label, chat_id, attempt, attempts, e
                );
                if !retry {
                    return Err(e);
                }
                let delay = std::time::Duration::from_secs(2 * u64::from(attempt));
                eprintln!("Signal: retrying {} for {} in {:?}.", label, chat_id, delay);
                tokio::time::sleep(delay).await;
            }
        }
    }
    unreachable!("retry loop always returns Ok or Err");
}

async fn send_markdown_retry(
    bot: &Bot,
    chat_id: &ChatId,
    text: impl Into<String>,
    attempts: u32,
    label: &str,
) -> Result<(), SignalError> {
    send_text_retry(bot, chat_id, text, attempts, label).await
}

async fn send_plain_retry(
    bot: &Bot,
    chat_id: &ChatId,
    text: impl Into<String>,
    attempts: u32,
    label: &str,
) -> Result<(), SignalError> {
    send_text_retry(bot, chat_id, text, attempts, label).await
}

async fn send_text_retry(
    bot: &Bot,
    chat_id: &ChatId,
    text: impl Into<String>,
    attempts: u32,
    label: &str,
) -> Result<(), SignalError> {
    let text = text.into();
    retry_scheduled_push(attempts, label, chat_id, || {
        let bot = bot.clone();
        let text = text.clone();
        let chat_id = chat_id.clone();
        async move { send_signal(&bot, &chat_id, text).await.map(|_| ()) }
    })
    .await
}

async fn push_scheduled_brief(
    bot: &Bot,
    chat_id: &ChatId,
    pool: &SqlitePool,
    config: &AppConfig,
) -> Result<(), SignalError> {
    let _ = send_signal(bot, chat_id, "Building morning brief...").await;
    let for_member = member_for_signal_aci(config, chat_id.lookup_aci()).map(|m| m.id.as_str());
    let report = crate::brief::compose_morning_brief(pool, config, for_member).await;
    send_markdown_retry(bot, chat_id, report, SCHEDULED_SIGNAL_ATTEMPTS, "scheduled morning brief").await
}

async fn reject_unlinked_chat(bot: &Bot, chat_id: &ChatId) -> Result<(), SignalError> {
    send_signal(
        bot,
        chat_id,
        format!(
            "This conversation is not linked to a family member.\n\
DM Chotu and run `/link <member_id>` (this conversation: `{chat_id}`)."
        ),
    )
    .await?;
    Ok(())
}

fn command_bypasses_allowlist(cmd: &Command) -> bool {
    matches!(cmd, Command::Chat | Command::Link(_))
}

pub async fn start_signal_client(
    pool: SqlitePool,
    llm: ChotuLlm,
    gemini_key: String,
    config: AppConfig,
) -> Result<(), anyhow::Error> {
    let socket = std::env::var("SIGNAL_CLI_SOCKET")
        .context("SIGNAL_CLI_SOCKET environment variable is required")?;
    let bot = SignalClient::connect(&socket)
        .await
        .context("failed to connect to signal-cli Unix socket")?;
    let gemini_client = GeminiClient::new(gemini_key);
    let researcher = StockResearcher::from_env();
    if !researcher.is_configured() {
        eprintln!("Signal: OPENROUTER_API_KEY not set — /research disabled.");
    } else {
        println!(
            "Signal: Stock research ready (shared-universe: {} → judge {})",
            researcher.panel_display_names(),
            researcher.judge_model()
        );
    }
    let conversation_states: StateMap = Arc::new(RwLock::new(HashMap::new()));
    {
        let describe = |label: &str, clock: Option<chotu_common::ClockTime>| match clock {
            Some(t) => format!("{label} {:02}:{:02}", t.hour, t.minute),
            None => format!("{label} off"),
        };
        println!(
            "Signal: timezone {} (IANA). {} · {} · {} (sends when linked chats or SIGNAL_GROUP_ID exist).",
            config.resolved_timezone_name(),
            describe("brief", config.schedule_clock(chotu_common::AgentSchedules::morning_brief)),
            describe("portfolio", config.schedule_clock(chotu_common::AgentSchedules::portfolio)),
            describe("reflection", config.schedule_clock(chotu_common::AgentSchedules::reflection)),
        );
    }
    let shared_config: SharedConfig = Arc::new(RwLock::new(config));

    let sched_bot = bot.clone();
    let sched_pool = pool.clone();
    let sched_llm = llm.clone();
    let sched_states = conversation_states.clone();
    let sched_config = shared_config.clone();
    tokio::spawn(async move {
        let mut last_brief = String::new();
        let mut last_portfolio = String::new();
        let mut last_reflect = String::new();
        loop {
            let cfg = sched_config.read().await.clone();
            let now = cfg.now_in_tz();
            let date_str = now.format("%Y-%m-%d").to_string();
            let targets = signal_delivery_targets(&cfg);
            let tz_name = cfg.resolved_timezone_name();

            if let Some(clock) = cfg.schedule_clock(chotu_common::AgentSchedules::morning_brief) {
                if clock.matches(now) && date_str != last_brief && !targets.is_empty() {
                    println!("Signal: scheduled morning brief ({:02}:{:02} {}).", clock.hour, clock.minute, tz_name);
                    let mut any_ok = false;
                    for cid in &targets {
                        if push_scheduled_brief(&sched_bot, cid, &sched_pool, &cfg).await.is_ok() {
                            any_ok = true;
                        }
                    }
                    if any_ok {
                        last_brief = date_str.clone();
                    }
                }
            }

            if let Some(clock) = cfg.schedule_clock(chotu_common::AgentSchedules::portfolio) {
                if clock.matches(now) && date_str != last_portfolio && !targets.is_empty() {
                    println!("Signal: scheduled portfolio overview ({:02}:{:02} {}).", clock.hour, clock.minute, tz_name);
                    match build_networth_summary(&sched_pool, &cfg).await {
                        Ok(msg) => {
                            if send_household_attempts(&sched_bot, &cfg, msg, SCHEDULED_SIGNAL_ATTEMPTS).await {
                                last_portfolio = date_str.clone();
                            }
                        }
                        Err(e) => {
                            eprintln!("Signal: failed to build scheduled portfolio overview: {}", e);
                            let _ = send_household_attempts(
                                &sched_bot,
                                &cfg,
                                format!("Portfolio overview failed: {}", e),
                                SCHEDULED_SIGNAL_ATTEMPTS,
                            )
                            .await;
                        }
                    }
                }
            }

            if let Some(clock) = cfg.schedule_clock(chotu_common::AgentSchedules::reflection) {
                if clock.matches(now) && date_str != last_reflect && !targets.is_empty() {
                    println!("Signal: scheduled evening reflection ({:02}:{:02} {}).", clock.hour, clock.minute, tz_name);
                    let mut any_ok = false;
                    for cid in &targets {
                        if handle_reflect_trigger(
                            &sched_bot,
                            cid,
                            &sched_pool,
                            &sched_llm,
                            sched_states.clone(),
                            &cfg,
                            SCHEDULED_SIGNAL_ATTEMPTS,
                        )
                        .await
                        .is_ok()
                        {
                            any_ok = true;
                        }
                    }
                    if any_ok {
                        last_reflect = date_str;
                    }
                }
            }

            tokio::time::sleep(tokio::time::Duration::from_secs(60)).await;
        }
    });

    let remind_bot = bot.clone();
    let remind_pool = pool.clone();
    let remind_config = shared_config.clone();
    tokio::spawn(async move {
        println!("Signal: Task reminder poller running (delivers when linked chats or SIGNAL_GROUP_ID exist).");
        loop {
            let cfg = remind_config.read().await.clone();
            if has_signal_delivery(&cfg) {
                if let Err(e) = poll_due_task_reminders(&remind_bot, &remind_pool, &cfg).await {
                    eprintln!("Signal: task reminder poll failed: {:?}", e);
                }
            }
            tokio::time::sleep(tokio::time::Duration::from_secs(60)).await;
        }
    });

    let budget_bot = bot.clone();
    let budget_pool = pool.clone();
    let budget_config = shared_config.clone();
    tokio::spawn(async move {
        println!("Signal: Spend budget alert poller running (delivers when linked chats or SIGNAL_GROUP_ID exist).");
        loop {
            let cfg = budget_config.read().await.clone();
            if has_signal_delivery(&cfg) {
                if let Err(e) = poll_spend_budget_alerts(&budget_bot, &budget_pool, &cfg).await {
                    eprintln!("Signal: spend budget alert poll failed: {:?}", e);
                }
            }
            tokio::time::sleep(tokio::time::Duration::from_secs(30 * 60)).await;
        }
    });

    println!("Signal: Google Health scheduled sync is handled by the Health Coach agent.");
    spawn_background_reindex(pool.clone());

    let mut inbound = bot
        .subscribe_receive()
        .await
        .context("failed to subscribe to signal-cli receive notifications")?;
    println!("Signal: connected on {socket}, receiving…");
    loop {
        match inbound.recv().await {
            Ok(message) => {
                if let Err(e) = handle_inbound(
                    bot.clone(),
                    message,
                    pool.clone(),
                    llm.clone(),
                    gemini_client.clone(),
                    researcher.clone(),
                    conversation_states.clone(),
                    shared_config.clone(),
                )
                .await
                {
                    eprintln!("Signal: inbound handler failed: {:?}", e);
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                eprintln!("Signal: dropped {skipped} inbound notifications");
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                return Err(anyhow::anyhow!("signal-cli receive subscription closed"));
            }
        }
    }
}

async fn handle_inbound(
    bot: Bot,
    inbound: SignalInbound,
    pool: SqlitePool,
    llm: ChotuLlm,
    gemini_client: GeminiClient,
    researcher: StockResearcher,
    states: StateMap,
    shared_config: SharedConfig,
) -> Result<(), SignalError> {
    let chat_id = inbound.recipient.clone();
    let sender_aci = inbound.sender_aci.clone();
    let config = shared_config.read().await.clone();

    if inbound.sender_aci.is_empty() {
        send_signal(&bot, &chat_id, "I can only accept Signal messages that include a sender ACI.").await?;
        return Ok(());
    }

    let unsupported = inbound.attachments.iter().find(|attachment| {
        !attachment.content_type.starts_with("image/") || attachment.id.is_empty()
    });
    if let Some(attachment) = unsupported {
        let reason = if !attachment.content_type.starts_with("image/") {
            "I only accept image attachments for food logging."
        } else {
            "That image is missing an attachment id, so I cannot download it."
        };
        send_signal(&bot, &chat_id, reason).await?;
        return Ok(());
    }

    let text = inbound.text.clone().unwrap_or_default();
    if let Some(cmd) = parse_command(&text) {
        if !command_bypasses_allowlist(&cmd) && !conversation_allowed(&config, &chat_id, &sender_aci) {
            return reject_unlinked_chat(&bot, &chat_id).await;
        }
        return handle_command(
            bot,
            chat_id,
            sender_aci,
            cmd,
            pool,
            llm,
            gemini_client,
            researcher,
            states,
            shared_config,
        )
        .await;
    }

    if !conversation_allowed(&config, &chat_id, &sender_aci) {
        return reject_unlinked_chat(&bot, &chat_id).await;
    }
    handle_message(bot, chat_id, sender_aci, inbound, pool, llm, gemini_client, states, shared_config).await
}


#[allow(clippy::too_many_arguments)]
async fn handle_command(
    bot: Bot,
    chat_id: ChatId,
    sender_aci: String,
    cmd: Command,
    pool: SqlitePool,
    llm: ChotuLlm,
    gemini_client: GeminiClient,
    researcher: StockResearcher,
    states: StateMap,
    shared_config: SharedConfig,
) -> Result<(), SignalError> {
    println!("Signal: Received command from {} in {}", sender_aci, chat_id);
    {
        let mut s = states.write().await;
        s.insert(chat_id.clone(), ConversationState::Idle);
    }
    let config = shared_config.read().await.clone();
    match cmd {
        Command::Help => { send_signal(&bot, &chat_id, HELP_TEXT).await?; }
        Command::Food(args) => { handle_food_log(&bot, &chat_id, args, &pool, &llm, &gemini_client, &config).await?; }
        Command::Status => { handle_status(&bot, &chat_id, &pool, &config, &llm).await?; }
        Command::Plan(args) => { handle_plan(&bot, &chat_id, args, &pool, &config, &llm).await?; }
        Command::Brief => { handle_brief(&bot, &chat_id, &pool, &config).await?; }
        Command::Cal(args) => { handle_cal(&bot, &chat_id, args, &config).await?; }
        Command::Trends(args) => { handle_trends(&bot, &chat_id, args, &pool, &config, &llm).await?; }
        Command::Tasks(args) | Command::Task(args) => { handle_tasks(&bot, &chat_id, args, &pool, &config).await?; }
        Command::Memory(args) => { handle_memory(&bot, &chat_id, args, &pool, &config, &llm, &gemini_client).await?; }
        Command::Reflect => { handle_reflect_trigger(&bot, &chat_id, &pool, &llm, states, &config, 1).await?; }
        Command::Chat => {
            send_signal(&bot, &chat_id, format!("Current Signal conversation: {chat_id}")).await?;
        }
        Command::Link(args) => { handle_link(&bot, &chat_id, &sender_aci, args, &shared_config).await?; }
        Command::Whoami => { handle_whoami(&bot, &chat_id, &sender_aci, &config).await?; }
        Command::Research(args) => {
            let targets = if args.trim().is_empty() { None } else { Some(args.as_str()) };
            if let Err(e) = run_and_log_stock_research(&bot, &chat_id, &pool, &researcher, config.investment_philosophy.as_ref(), targets).await {
                eprintln!("Signal: manual stock research trigger failed: {:?}", e);
                let _ = send_signal(&bot, &chat_id, format!("Stock research failed: {}", e)).await;
            }
        }
        Command::Sync => {
            if let Err(e) = sync_google_health_nutrition(&bot, &chat_id, &pool, &gemini_client, &config).await {
                eprintln!("Signal: manual Google Health sync failed: {:?}", e);
                let _ = send_signal(&bot, &chat_id, format!("Google Health sync failed: {}", e)).await;
            }
        }
        Command::Login(args) => {
            let args_trimmed = args.trim();
            if args_trimmed.to_lowercase().starts_with("code") {
                let rest = args_trimmed[4..].trim();
                if let Err(e) = handle_manual_code(&bot, &chat_id, rest, &config).await {
                    eprintln!("Signal: manual code exchange failed: {:?}", e);
                    let _ = send_signal(&bot, &chat_id, format!("Manual code exchange failed: {}", e)).await;
                }
            } else {
                let lower = args_trimmed.to_lowercase();
                let mut parts = lower.split_whitespace();
                let service = parts.next().unwrap_or("");
                if service == "fitbit" || service == "health" {
                    let original_member = args_trimmed.split_whitespace().nth(1).unwrap_or("").to_string();
                    if let Err(e) = handle_login_google_health(&bot, &chat_id, &original_member, &config).await {
                        eprintln!("Signal: Google Health login initialization failed: {:?}", e);
                        let _ = send_signal(&bot, &chat_id, format!("Google Health login failed: {}", e)).await;
                    }
                } else if service == "gmail" || service == "google" {
                    if let Err(e) = handle_login_google(&bot, &chat_id).await {
                        eprintln!("Signal: Google/Gmail login initialization failed: {:?}", e);
                        let _ = send_signal(&bot, &chat_id, format!("Google/Gmail login failed: {}", e)).await;
                    }
                } else if service == "calendar" {
                    let original_member = args_trimmed.split_whitespace().nth(1).unwrap_or("").to_string();
                    if let Err(e) = handle_login_calendar(&bot, &chat_id, &original_member, &config).await {
                        eprintln!("Signal: Calendar login initialization failed: {:?}", e);
                        let _ = send_signal(&bot, &chat_id, format!("Calendar login failed: {}", e)).await;
                    }
                } else {
                    let _ = send_signal(
                        &bot,
                        &chat_id,
                        "Invalid service. Usage: `/login health <member_id>`, `/login gmail`, `/login calendar <member_id>`, or `/login code ...`",
                    ).await;
                }
            }
        }
        Command::Clearfood(args) => { handle_clear_food(&bot, &chat_id, args, &pool, &config).await?; }
        Command::Adjustfood(args) => { handle_adjust_food(&bot, &chat_id, args, &pool, &config).await?; }
        Command::Undofood(args) => { handle_undo_food(&bot, &chat_id, args, &pool, &config).await?; }
        Command::Networth => { handle_networth(&bot, &chat_id, &pool, &config).await?; }
        Command::Monthly(args) => { handle_monthly(&bot, &chat_id, args, &pool, &config).await?; }
        Command::Budget(args) => { handle_budget(&bot, &chat_id, args, &pool, &config).await?; }
    }
    Ok(())
}

async fn handle_link(
    bot: &Bot,
    chat_id: &ChatId,
    sender_aci: &str,
    args: String,
    shared_config: &SharedConfig,
) -> Result<(), SignalError> {
    if !matches!(chat_id, SignalRecipient::Direct { .. }) {
        send_signal(bot, chat_id, "⚠️ `/link` only works in a direct Signal conversation (not groups).").await?;
        return Ok(());
    }
    let member_tok = args.trim();
    if member_tok.is_empty() {
        let config = shared_config.read().await;
        let members = config.family.members.iter().map(|m| format!("- `{}` ({})", m.id, m.name)).collect::<Vec<_>>().join("\n");
        send_signal(bot, chat_id, format!("⚠️ Usage: `/link <member_id>`\n\nConfigured members:\n{members}")).await?;
        return Ok(());
    }
    let path = config_path();
    match set_member_signal_aci(&path, member_tok, sender_aci) {
        Ok(updated) => {
            let member = updated.family.members.iter().find(|m| m.id.eq_ignore_ascii_case(member_tok)).cloned();
            {
                let mut cfg = shared_config.write().await;
                *cfg = updated;
            }
            if let Some(m) = member {
                send_signal(
                    bot,
                    chat_id,
                    format!(
                        "✅ Linked this conversation (`{chat_id}`) to {} (`{}`).\nFood/tasks without a member id now default to you. Use `/whoami` anytime.",
                        m.name, m.id
                    ),
                ).await?;
            }
        }
        Err(e) => {
            send_signal(bot, chat_id, format!("❌ Failed to link conversation: {e}")).await?;
        }
    }
    Ok(())
}

async fn handle_whoami(
    bot: &Bot,
    chat_id: &ChatId,
    sender_aci: &str,
    config: &AppConfig,
) -> Result<(), SignalError> {
    match member_for_signal_aci(config, sender_aci) {
        Some(m) => {
            send_signal(&bot, chat_id, format!(
                    "You are linked as *{}* (`{}`).\nChat id: `{}`",
                    (m.name),
                    m.id,
                    chat_id
                ),)
            .await?;
        }
        None => {
            send_signal(&bot, chat_id, format!(
                    "This chat (`{}`) is not linked to a family member.\n\
Run `/link <member_id>` to claim it.",
                    chat_id
                ),)
            .await?;
        }
    }
    Ok(())
}

async fn handle_food_log(
    bot: &Bot,
    chat_id: &ChatId,
    args: String,
    pool: &SqlitePool,
    llm: &ChotuLlm,
    gemini_client: &GeminiClient,
    config: &AppConfig,
) -> Result<(), SignalError> {
    let args = args.trim();
    if args.is_empty() {
        let members_list = config
            .family
            .members
            .iter()
            .map(|m| format!("- {} ({})", m.id, m.name))
            .collect::<Vec<String>>()
            .join("\n");
        send_signal(&bot, chat_id, format!("Please provide a description, e.g. /food [member_id] <description>\n\nConfigured family members:\n{}", members_list)).await?;
        return Ok(());
    }

    let (family_member_id, food_description) =
        resolve_food_member_and_description(args, config, chat_id.lookup_aci());
    if reject_foreign_food_mutation(bot, chat_id, config, &family_member_id).await? {
        return Ok(());
    }
    if food_description.is_empty() {
        send_signal(&bot, chat_id, format!(
                "Please provide a food description after the member ID. E.g. /food {} salad",
                family_member_id
            ),)
        .await?;
        return Ok(());
    }

    send_signal(&bot, chat_id, format!("Got it — logging food for {}...", family_member_id),)
    .await?;

    // Let the LLM resolve relative days/times ("yesterday's dinner…") into YYYY-MM-DD / HH:MM.
    let (description, food_date, food_time) = with_typing_indicator(bot, chat_id, async {
        match llm.extract_food_log_context(&food_description).await {
            Ok(ctx) => {
                let desc = if ctx.food_description.trim().is_empty() {
                    food_description.clone()
                } else {
                    ctx.food_description.trim().to_string()
                };
                (desc, ctx.food_date, ctx.food_time)
            }
            Err(e) => {
                eprintln!(
                    "Food log context extract failed (falling back to raw text/today): {:?}",
                    e
                );
                (food_description.clone(), None, None)
            }
        }
    })
    .await;

    log_food_for_member(
        bot,
        chat_id,
        pool,
        gemini_client,
        config,
        &family_member_id,
        &description,
        food_date.as_deref(),
        food_time.as_deref(),
        &food_description,
    )
    .await
}

/// Estimate nutrition and persist a food log for an optional resolved day/time.
/// `timing_utterance` is the original user text (meal words may be stripped from
/// `food_description`); used to map lunch/snacks/dinner onto household windows.
async fn log_food_for_member(
    bot: &Bot,
    chat_id: &ChatId,
    pool: &SqlitePool,
    gemini_client: &GeminiClient,
    config: &AppConfig,
    family_member_id: &str,
    food_description: &str,
    food_date: Option<&str>,
    food_time: Option<&str>,
    timing_utterance: &str,
) -> Result<(), SignalError> {
    let food_time = effective_food_time(timing_utterance, food_time);
    let timing = resolve_food_log_timing(food_date, food_time.as_deref());

    send_signal(&bot, chat_id, format!(
            "Estimating nutrition for {}… (usually under a minute)",
            family_member_id
        ),)
    .await?;

    let estimate = {
        let _nudge = ProgressNudge::spawn(
            bot.clone(),
            chat_id.clone(),
            20,
            format!(
                "Still estimating macros for {} — hang tight, this can take a bit…",
                family_member_id
            ),
        );
        with_typing_indicator(bot, chat_id, async {
            gemini_client.approximate_nutrition(food_description).await
        })
        .await
    };

    match estimate {
        Ok(est) => {
            persist_food_estimation(
                bot,
                chat_id,
                pool,
                config,
                family_member_id,
                food_description,
                &est,
                &timing,
            )
            .await?;
        }
        Err(e) => {
            eprintln!("Gemini client error: {:?}", e);
            send_signal(&bot, chat_id, format!("❌ Failed to estimate nutrition: {}", e))
                .await?;
        }
    }

    Ok(())
}

async fn with_typing_indicator<F, T>(_bot: &Bot, _chat_id: &ChatId, fut: F) -> T
where
    F: std::future::Future<Output = T>,
{
    fut.await
}

/// Background "still working…" nudge that aborts on drop (safe under handler cancellation).
struct ProgressNudge {
    handle: Option<tokio::task::JoinHandle<()>>,
}

impl ProgressNudge {
    fn spawn(bot: Bot, chat_id: ChatId, delay_secs: u64, text: String) -> Self {
        let handle = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(delay_secs)).await;
            if let Err(e) = send_signal(&bot, &chat_id, text).await {
                eprintln!("Signal: failed to send progress nudge: {:?}", e);
            }
        });
        Self {
            handle: Some(handle),
        }
    }
}

impl Drop for ProgressNudge {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            handle.abort();
        }
    }
}

/// Strip a leading `/food` or `/food@bot` so photo captions like `/food pray`
/// parse as member/description instead of becoming the food description.
fn strip_leading_food_command(input: &str) -> &str {
    let trimmed = input.trim();
    const PREFIX: &str = "/food";
    let Some(head) = trimmed.get(..PREFIX.len()) else {
        return trimmed;
    };
    if !head.eq_ignore_ascii_case(PREFIX) {
        return trimmed;
    }
    // PREFIX is ASCII, so PREFIX.len() is a char boundary after the get() check.
    let rest = trimmed.get(PREFIX.len()..).unwrap_or("");
    if rest.is_empty() {
        return "";
    }
    if rest.chars().next().is_some_and(char::is_whitespace) {
        return rest.trim();
    }
    trimmed
}

/// Parse optional leading member id from `/food` args or a photo caption.
/// When omitted, defaults to the member linked to this chat (else primary).
fn resolve_food_member_and_description(
    args: &str,
    config: &AppConfig,
    chat_id: &str,
) -> (String, String) {
    let mut parts = args.splitn(2, |c: char| c.is_whitespace());
    let first_word = parts.next().unwrap_or("");
    let rest = parts.next().unwrap_or("").trim();

    for member in &config.family.members {
        if member.id.eq_ignore_ascii_case(first_word) {
            return (member.id.clone(), rest.to_string());
        }
    }

    (
        default_member_id(config, chat_id).to_string(),
        args.trim().to_string(),
    )
}

/// Resolve optional `[member_id]` arg; empty → linked/default member for this chat.
fn resolve_optional_member_arg(args: &str, config: &AppConfig, chat_id: &str) -> String {
    let member_id = args.trim();
    if member_id.is_empty() {
        return default_member_id(config, chat_id).to_string();
    }
    config
        .family
        .members
        .iter()
        .find(|m| m.id.eq_ignore_ascii_case(member_id))
        .map(|m| m.id.clone())
        .unwrap_or_else(|| default_member_id(config, chat_id).to_string())
}

/// Linked DMs may only mutate food for their own member. Returns `true` if blocked.
async fn reject_foreign_food_mutation(
    bot: &Bot,
    chat_id: &ChatId,
    config: &AppConfig,
    target_member_id: &str,
) -> Result<bool, SignalError> {
    if let Err(msg) = ensure_food_mutation_allowed(config, chat_id.lookup_aci(), target_member_id) {
        // Plain text: member ids must not go through Telegram Markdown parse mode.
        send_signal(&bot, chat_id, msg).await?;
        return Ok(true);
    }
    Ok(false)
}

/// Insert food_log + tags + day summary in one transaction so a tag failure
/// cannot leave an orphaned meal or a skipped summary bump.
async fn persist_food_log_and_tags(
    pool: &SqlitePool,
    log_id: &str,
    log_ts: chrono::DateTime<chrono::Utc>,
    family_member_id: &str,
    food_description: &str,
    est: &chotu_common::NutritionEstimation,
    date_str: &str,
    assigned: &AssignedFoodTags,
) -> anyhow::Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query(
        "INSERT INTO food_log (id, timestamp, family_member_id, raw_text_description, \
         estimated_calories, estimated_protein, estimated_carbs, estimated_fats, \
         estimated_omega_3_dha_mg, estimated_cholesterol_mg, estimated_saturated_fat_g, estimated_unsaturated_fat_g, estimated_triglycerides_mg, \
         estimated_iron_mg, estimated_vitamin_b_mg, estimated_vitamin_c_mg, \
         estimated_sugar_g, estimated_fiber_g, estimated_sodium_mg, estimated_potassium_mg, estimated_calcium_mg, \
         estimated_magnesium_mg, estimated_zinc_mg, estimated_vitamin_a_mcg, estimated_vitamin_d_mcg, estimated_vitamin_e_mg, \
         estimated_vitamin_k_mcg, estimated_caffeine_mg, estimated_trans_fat_g) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(log_id)
    .bind(log_ts)
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
    .execute(&mut *tx)
    .await?;

    insert_food_log_tags(&mut tx, log_id, assigned).await?;

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
    .bind(date_str)
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
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(())
}

/// Insert food_log, update day totals, optionally push to Google Health, reply macros-first.
async fn persist_food_estimation(
    bot: &Bot,
    chat_id: &ChatId,
    pool: &SqlitePool,
    config: &AppConfig,
    family_member_id: &str,
    food_description: &str,
    est: &chotu_common::NutritionEstimation,
    timing: &chotu_common::FoodLogTiming,
) -> Result<(), SignalError> {
    let log_id = uuid::Uuid::new_v4().to_string();
    let log_ts = timing.timestamp;
    let date_str = timing.date.clone();
    let today_str = chrono::Local::now().format("%Y-%m-%d").to_string();
    let yesterday_str = (chrono::Local::now().date_naive() - chrono::Duration::days(1))
        .format("%Y-%m-%d")
        .to_string();

    let assigned = assign_food_tags(&est.tags, food_description);
    if let Err(e) = persist_food_log_and_tags(
        pool,
        &log_id,
        log_ts,
        family_member_id,
        food_description,
        est,
        &date_str,
        &assigned,
    )
    .await
    {
        eprintln!("Failed to persist food_log + tags + summary: {:?}", e);
        send_signal(&bot, chat_id, "Database error saving food log.")
            .await?;
        return Ok(());
    }

    let mut google_sync_note = String::new();
    if health_coach::member_health_credentials_configured(family_member_id, config) {
        match health_coach::google_health_client_for_member(family_member_id, config) {
            Ok(client) => {
                let pending = chotu_common::FoodLog {
                    id: log_id.clone(),
                    timestamp: log_ts,
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

    let day_label = if date_str == today_str {
        "Today".to_string()
    } else if date_str == yesterday_str {
        "Yesterday".to_string()
    } else {
        date_str.clone()
    };

    let day_line = match day_totals {
        Some((cal, p, c, f)) => format!(
            "\n\n*{}:* {} kcal · {:.0}g P / {:.0}g C / {:.0}g F",
            day_label, cal, p, c, f
        ),
        None => String::new(),
    };

    let when_note = if timing.date_was_explicit && date_str != today_str {
        format!(" ({})", day_label)
    } else {
        String::new()
    };

    let msg_text = format!(
        "✅ Logged for *{}*{}: _{}_\n\
         • {} kcal · {:.1}g P / {:.1}g C / {:.1}g F ({}){}{}{}",
        family_member_id,
        when_note,
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

    send_signal(&bot, chat_id, msg_text)
        .await?;

    Ok(())
}

async fn handle_clear_food(
    bot: &Bot,
    chat_id: &ChatId,
    args: String,
    pool: &SqlitePool,
    config: &AppConfig,
) -> Result<(), SignalError> {
    let target_member_id = resolve_optional_member_arg(&args, config, chat_id.lookup_aci());
    if reject_foreign_food_mutation(bot, chat_id, config, &target_member_id).await? {
        return Ok(());
    }

    let date_str = chrono::Local::now().format("%Y-%m-%d").to_string();

    // Preserve Google Health (or other non-food_log) nutrition, then drop Telegram logs.
    let external = match health_coach::external_nutrition_base(pool, &target_member_id, &date_str).await
    {
        Ok(v) => v,
        Err(e) => {
            eprintln!("Failed to compute external nutrition base: {:?}", e);
            send_signal(&bot, chat_id, "❌ Database error reading today's summary.")
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

    if let Err(e) = (async {
        let mut tx = pool.begin().await?;
        delete_food_log_tags_for_member_day(&mut tx, &target_member_id, &date_str).await?;
        sqlx::query(
            "DELETE FROM food_log WHERE family_member_id = ? AND date(timestamp, 'localtime') = ?",
        )
        .bind(&target_member_id)
        .bind(&date_str)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        anyhow::Ok(())
    })
    .await
    {
        eprintln!("Failed to clear food_log + tags: {:?}", e);
        send_signal(&bot, chat_id, "❌ Database error clearing food logs.")
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
            send_signal(&bot, chat_id, "❌ Database error resetting health summary.")
                .await?;
            return Ok(());
        }
    };

    send_signal(&bot, chat_id, format!(
            "🧹 *Today's Telegram food logs cleared* for *{}*.\n\
             Remaining (e.g. Google Health): {} kcal · {:.0}g P / {:.0}g C / {:.0}g F",
            target_member_id, rebuilt.calories, rebuilt.protein, rebuilt.carbs, rebuilt.fats
        ),)
    .await?;

    Ok(())
}

async fn handle_adjust_food(
    bot: &Bot,
    chat_id: &ChatId,
    args: String,
    pool: &SqlitePool,
    config: &AppConfig,
) -> Result<(), SignalError> {
    let tokens: Vec<&str> = args.split_whitespace().collect();
    if tokens.is_empty() {
        send_signal(&bot, chat_id, "⚠️ Usage: `/adjustfood [member_id] <calories> <protein> <carbs> <fats>`").await?;
        return Ok(());
    }

    let mut member_id = default_member_id(config, chat_id.lookup_aci()).to_string();
    let mut offset = 0;

    // Check if first token matches a member ID
    for member in &config.family.members {
        if member.id.eq_ignore_ascii_case(tokens[0]) {
            member_id = member.id.clone();
            offset = 1;
            break;
        }
    }

    if reject_foreign_food_mutation(bot, chat_id, config, &member_id).await? {
        return Ok(());
    }

    let remaining_tokens = &tokens[offset..];
    if remaining_tokens.len() < 4 {
        send_signal(&bot, chat_id, format!(
                "⚠️ Missing values. Usage: `/adjustfood [member_id] <calories> <protein> <carbs> <fats>`\n\
                 Example: `/adjustfood {} 2000 150 200 60`",
                member_id
            ),)
        .await?;
        return Ok(());
    }

    let calories: i32 = match remaining_tokens[0].parse() {
        Ok(val) => val,
        Err(_) => {
            send_signal(&bot, chat_id, "❌ Invalid calories value. Must be an integer.").await?;
            return Ok(());
        }
    };

    let protein: f64 = match remaining_tokens[1].parse() {
        Ok(val) => val,
        Err(_) => {
            send_signal(&bot, chat_id, "❌ Invalid protein value. Must be a number.").await?;
            return Ok(());
        }
    };

    let carbs: f64 = match remaining_tokens[2].parse() {
        Ok(val) => val,
        Err(_) => {
            send_signal(&bot, chat_id, "❌ Invalid carbs value. Must be a number.").await?;
            return Ok(());
        }
    };

    let fats: f64 = match remaining_tokens[3].parse() {
        Ok(val) => val,
        Err(_) => {
            send_signal(&bot, chat_id, "❌ Invalid fats value. Must be a number.").await?;
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
            send_signal(&bot, chat_id, "❌ Database error reading today's summary.")
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

    let assigned = assign_food_tags(Vec::<String>::new(), &desc);
    if let Err(e) = (async {
        let mut tx = pool.begin().await?;
        delete_food_log_tags_for_member_day(&mut tx, &member_id, &date_str).await?;
        sqlx::query(
            "DELETE FROM food_log WHERE family_member_id = ? AND date(timestamp, 'localtime') = ?",
        )
        .bind(&member_id)
        .bind(&date_str)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
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
        .execute(&mut *tx)
        .await?;
        insert_food_log_tags(&mut tx, &log_id, &assigned).await?;
        tx.commit().await?;
        anyhow::Ok(())
    })
    .await
    {
        eprintln!("Failed to replace food_log + tags on adjust: {:?}", e);
        send_signal(&bot, chat_id, "❌ Database error adjusting food log.")
            .await?;
        return Ok(());
    }

    if let Err(e) =
        health_coach::write_summary_nutrition(pool, &member_id, &date_str, &desired).await
    {
        eprintln!("Failed to adjust health summary: {:?}", e);
        send_signal(&bot, chat_id, "❌ Database error adjusting health summary.")
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

    send_signal(&bot, chat_id, msg)
        .await?;

    Ok(())
}

async fn handle_undo_food(
    bot: &Bot,
    chat_id: &ChatId,
    args: String,
    pool: &SqlitePool,
    config: &AppConfig,
) -> Result<(), SignalError> {
    let target_member_id = resolve_optional_member_arg(&args, config, chat_id.lookup_aci());
    if reject_foreign_food_mutation(bot, chat_id, config, &target_member_id).await? {
        return Ok(());
    }

    let date_str = chrono::Local::now().format("%Y-%m-%d").to_string();

    // Snapshot the non-food_log base before mutating food_log.
    let external = match health_coach::external_nutrition_base(pool, &target_member_id, &date_str)
        .await
    {
        Ok(v) => v,
        Err(e) => {
            eprintln!("Failed to compute external nutrition base: {:?}", e);
            send_signal(&bot, chat_id, "❌ Database error reading today's summary.")
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
            send_signal(&bot, chat_id, "❌ Database error retrieving last food entry.")
                .await?;
            return Ok(());
        }
    };

    let log_entry = match last_log {
        Some(entry) => entry,
        None => {
            send_signal(&bot, chat_id, format!(
                    "⚠️ No food log entries found for *{}* today.",
                    target_member_id
                ),)
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

    if let Err(e) = (async {
        let mut tx = pool.begin().await?;
        delete_food_log_tags(&mut tx, &log_entry.id).await?;
        sqlx::query("DELETE FROM food_log WHERE id = ?")
            .bind(&log_entry.id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        anyhow::Ok(())
    })
    .await
    {
        eprintln!("Failed to delete food_log + tags on undo: {:?}", e);
        send_signal(&bot, chat_id, "❌ Database error deleting food log entry.")
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
            send_signal(&bot, chat_id, "❌ Database error updating today's summary.")
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

    send_signal(&bot, chat_id, msg)
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
    chat_id: &ChatId,
    args: String,
    pool: &SqlitePool,
    config: &AppConfig,
) -> Result<(), SignalError> {
    let tokens: Vec<&str> = args.split_whitespace().collect();
    let first = tokens.first().copied().unwrap_or("");
    let second = tokens.get(1).copied();
    let third = tokens.get(2).copied();

    // Mutating actions: add / complete / done / snooze / reassign / open (unsnooze)
    let action = first.to_lowercase();
    match action.as_str() {
        "add" | "create" | "new" | "remind" => {
            let rest = args
                .trim()
                .strip_prefix(first)
                .unwrap_or("")
                .trim()
                .to_string();
            return add_manual_task(bot, chat_id, pool, config, &rest).await;
        }
        "complete" => {
            return match second.map(|s| s.to_lowercase()) {
                Some(ref s) if s == "all" => {
                    let confirm = third.is_some_and(|t| t.eq_ignore_ascii_case("confirm"));
                    mark_all_tasks_complete(bot, chat_id, pool, config, confirm).await
                }
                Some(ref id) if id.len() >= 4 => {
                    mark_task_complete(bot, chat_id, pool, config, id).await
                }
                _ => {
                    send_signal(&bot, chat_id, "⚠️ Usage: `/tasks complete <id>` or `/tasks complete all` \
                         (linked DM: yours + unassigned; household: add `confirm`)",)
                    .await?;
                    Ok(())
                }
            };
        }
        "done" if second.is_some_and(|s| s.eq_ignore_ascii_case("all")) => {
            let confirm = third.is_some_and(|t| t.eq_ignore_ascii_case("confirm"));
            return mark_all_tasks_complete(bot, chat_id, pool, config, confirm).await;
        }
        "done" if second.is_some_and(looks_like_task_id_prefix) => {
            return mark_task_complete(bot, chat_id, pool, config, second.unwrap()).await;
        }
        "snooze" => {
            return match second {
                Some(id) if id.len() >= 4 => {
                    let days = third
                        .and_then(|t| t.parse::<i64>().ok())
                        .unwrap_or(1)
                        .clamp(1, 90);
                    snooze_task(bot, chat_id, pool, config, id, days).await
                }
                _ => {
                    send_signal(&bot, chat_id, "⚠️ Usage: `/tasks snooze <id> [days]` (default 1 day)",)
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
                            send_signal(&bot, chat_id, format!(
                                    "⚠️ Unknown member `{}`. Configured: {}",
                                    member_tok, members
                                ),)
                            .await?;
                            Ok(())
                        }
                    }
                }
                _ => {
                    send_signal(&bot, chat_id, "⚠️ Usage: `/tasks reassign <id> <member_id>`",)
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

    // `/task change battery…` or `/tasks buy milk by tomorrow` — treat as add when
    // args aren't a plain list filter (status / member / status+member).
    let member_ids: Vec<String> = config.family.members.iter().map(|m| m.id.clone()).collect();
    if looks_like_task_add_query(&tokens, &member_ids) {
        return add_manual_task(bot, chat_id, pool, config, args.trim()).await;
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
            send_signal(&bot, chat_id, "❌ Database error listing tasks.")
                .await?;
            return Ok(());
        }
    };

    if rows.is_empty() {
        let scope = member_filter
            .as_deref()
            .map(|m| format!(" for *{}*", m))
            .unwrap_or_default();
        send_signal(&bot, chat_id, format!("✅ No *{}* tasks found{}.", label, scope),)
        .await?;
        return Ok(());
    }

    let mut msg = format!("📋 *Tasks* ({}, {})\n\n", label, rows.len());
    let mut actionable: Vec<(String, String)> = Vec::new();
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
            .map(|s| format!("\n    _{}_", (truncate_chars(s, 60))))
            .unwrap_or_default();

        msg.push_str(&format!(
            "• `{}` {} ({}){}{}{}\n",
            short_id,
            (truncate_chars(&row.title, 80)),
            row.status,
            due,
            assignee,
            subject
        ));
        if row.status == "open" || row.status == "snoozed" {
            actionable.push((row.id.clone(), row.title.clone()));
        }
    }

    if label == "open" || label == "open/snoozed" || label == "snoozed" {
        msg.push_str(
            "\n_Tap ✅ / 😴 below, or:_ `/tasks add [member] <title> [due|by <when>]` · `/task <title> [by <when>]` · `/tasks complete <id|all|all confirm>` · \
             `/tasks snooze <id> [days]` · `/tasks reassign <id> <member>` · `/tasks open <id>`\n\
             _Dismiss email tasks:_ reply `unactionable` to the original reminder.",
        );
    }

    send_signal(bot, chat_id, msg).await?;

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
    chat_id: &ChatId,
    pool: &SqlitePool,
    id_prefix: &str,
) -> Result<Option<(String, String, String)>, SignalError> {
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
            send_signal(&bot, chat_id, "❌ Database error looking up task.")
                .await?;
            return Ok(None);
        }
    };

    if matches.is_empty() {
        send_signal(&bot, chat_id, format!("⚠️ No task found starting with `{}`.", id_prefix),)
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
        send_signal(&bot, chat_id, msg)
            .await?;
        return Ok(None);
    }

    Ok(Some(matches.into_iter().next().unwrap()))
}

async fn add_manual_task(
    bot: &Bot,
    chat_id: &ChatId,
    pool: &SqlitePool,
    config: &AppConfig,
    args: &str,
) -> Result<(), SignalError> {
    let member_ids: Vec<String> = config.family.members.iter().map(|m| m.id.clone()).collect();
    let Some((member_id, title, due_raw)) = split_task_add_args(args, &member_ids) else {
        send_signal(&bot, chat_id, "⚠️ Usage: `/tasks add [member] <title> [due|by <when>]`\n\
             Examples: `/task change battery for fob by today 3 pm` · `/tasks add praj call dentist due tomorrow 15:00`",)
        .await?;
        return Ok(());
    };

    let member_id = member_id.or_else(|| Some(default_member_id(config, chat_id.lookup_aci()).to_string()));
    create_manual_task(bot, chat_id, pool, config, member_id, title, due_raw).await
}

async fn create_manual_task(
    bot: &Bot,
    chat_id: &ChatId,
    pool: &SqlitePool,
    config: &AppConfig,
    member_id: Option<String>,
    title: String,
    due_raw: Option<String>,
) -> Result<(), SignalError> {
    let title = title.trim().to_string();
    if title.is_empty() {
        send_signal(&bot, chat_id, "⚠️ Task title cannot be empty.")
            .await?;
        return Ok(());
    }

    let parsed_due = match due_raw.as_deref() {
        Some(raw) => match parse_due_phrase_tz(raw, config.resolved_tz()) {
            Some(p) => {
                if let Ok(due_dt) = chrono::DateTime::parse_from_rfc3339(&p.due_at) {
                    if due_dt.with_timezone(&chrono::Utc) <= chrono::Utc::now() {
                        send_signal(&bot, chat_id, format!(
                                "⚠️ Due `{}` is already in the past. Try a future time \
                                 (e.g. `tomorrow 9am`, `friday 15:00`).",
                                (raw)
                            ),)
                        .await?;
                        return Ok(());
                    }
                }
                Some(p)
            }
            None => {
                send_signal(&bot, chat_id, format!(
                        "⚠️ Couldn't parse due `{}`. Try `tomorrow 3pm`, `friday`, or `2026-08-10`.",
                        (raw)
                    ),)
                .await?;
                return Ok(());
            }
        },
        None => None,
    };

    let mut calendar_event_id: Option<String> = None;
    let mut calendar_note: Option<&'static str> = None;
    if let Some(ref due) = parsed_due {
        if let Some(ref mid) = member_id {
            if let Some(member) = config.family.members.iter().find(|m| &m.id == mid) {
                match build_calendar_client(member) {
                    Some(cal_client) => {
                        match chrono::DateTime::parse_from_rfc3339(&due.due_at) {
                            Ok(due_dt) => {
                                let start = due_dt.with_timezone(&chrono::Utc);
                                match schedule_at(
                                    &cal_client,
                                    &title,
                                    Some("Created via Telegram"),
                                    start,
                                    TASK_CALENDAR_DURATION_MINUTES,
                                )
                                .await
                                {
                                    Ok(event_id) => {
                                        println!(
                                            "Signal: scheduled task on calendar: {}",
                                            event_id
                                        );
                                        calendar_event_id = Some(event_id);
                                    }
                                    Err(e) => {
                                        eprintln!(
                                            "Signal: failed to schedule task on calendar: {:?}",
                                            e
                                        );
                                        calendar_note = Some("calendar schedule failed");
                                    }
                                }
                            }
                            Err(e) => {
                                eprintln!(
                                    "Signal: invalid due_at for calendar: {:?}",
                                    e
                                );
                                calendar_note = Some("calendar schedule failed");
                            }
                        }
                    }
                    None => {
                        calendar_note = Some("calendar not linked");
                    }
                }
            }
        }
    }

    let id = uuid::Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339();
    let due_date = parsed_due.as_ref().map(|p| p.due_date.clone());
    let due_at = parsed_due.as_ref().map(|p| p.due_at.clone());

    if let Err(e) = sqlx::query(
        "INSERT INTO tasks (id, created_at, updated_at, title, assigned_to, due_date, due_at, status, source, calendar_event_id) \
         VALUES (?, ?, ?, ?, ?, ?, ?, 'open', 'manual', ?)",
    )
    .bind(&id)
    .bind(&now)
    .bind(&now)
    .bind(&title)
    .bind(member_id.as_deref())
    .bind(due_date.as_deref())
    .bind(due_at.as_deref())
    .bind(calendar_event_id.as_deref())
    .execute(pool)
    .await
    {
        eprintln!("Failed to create task: {:?}", e);
        send_signal(&bot, chat_id, "❌ Database error creating task.")
            .await?;
        return Ok(());
    }

    let short_id: String = id.chars().take(8).collect();
    let mut msg = format!(
        "✅ Added task `{}`: _{}_",
        short_id,
        (title)
    );
    if let Some(ref mid) = member_id {
        msg.push_str(&format!(" · @{}", mid));
    }
    if let Some(ref due) = due_date {
        if let Some(ref at) = due_at {
            if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(at) {
                let local = dt.with_timezone(&chrono::Local);
                msg.push_str(&format!(
                    " · due {} {}",
                    due,
                    local.format("%H:%M")
                ));
            } else {
                msg.push_str(&format!(" · due {}", due));
            }
        } else {
            msg.push_str(&format!(" · due {}", due));
        }
    }
    if calendar_event_id.is_some() {
        msg.push_str(" · 📅 calendar");
    } else if let Some(note) = calendar_note {
        msg.push_str(&format!(" · _{}_", note));
    }

    send_signal(&bot, chat_id, msg)
        .await?;

    refresh_task_memory(pool, &id).await;
    Ok(())
}

async fn poll_due_task_reminders(
    bot: &Bot,
    pool: &SqlitePool,
    config: &AppConfig,
) -> Result<(), anyhow::Error> {
    let now = chrono::Utc::now();
    let now_s = now.to_rfc3339();
    let rows: Vec<(String, String, Option<String>, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT id, title, due_date, due_at, assigned_to FROM tasks \
         WHERE status = 'open' \
           AND due_at IS NOT NULL \
           AND due_at <= ? \
           AND reminded_at IS NULL \
         ORDER BY due_at ASC LIMIT 20",
    )
    .bind(&now_s)
    .fetch_all(pool)
    .await?;

    for (id, title, due_date, due_at, assigned_to) in rows {
        // Claim first so concurrent pollers cannot double-send.
        let claimed = sqlx::query(
            "UPDATE tasks SET reminded_at = ?, updated_at = ? \
             WHERE id = ? AND status = 'open' AND reminded_at IS NULL \
               AND due_at IS NOT NULL AND due_at <= ?",
        )
        .bind(&now_s)
        .bind(&now_s)
        .bind(&id)
        .bind(&now_s)
        .execute(pool)
        .await?;

        if claimed.rows_affected() == 0 {
            continue;
        }

        let short_id: String = id.chars().take(8).collect();
        let when = due_at
            .as_deref()
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| {
                dt.with_timezone(&chrono::Local)
                    .format("%a %b %e %H:%M")
                    .to_string()
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .or(due_date)
            .unwrap_or_else(|| "now".to_string());

        let msg = format!(
            "Reminder\n`{}` {}\nDue {}\n{}",
            short_id,
            title,
            when,
            task_complete_snooze_help(&short_id)
        );
        let targets: Vec<ChatId> = match assigned_to.as_deref() {
            Some(mid) => match signal_aci_for_member(config, mid) {
                Some(aci) => vec![SignalRecipient::Direct { aci }],
                None => signal_delivery_targets(config),
            },
            None => signal_delivery_targets(config),
        };

        let mut send_ok = false;
        for cid in &targets {
            if let Err(e) = send_signal(bot, cid, msg.clone()).await
            {
                eprintln!(
                    "Signal: failed to send task reminder {} to {}: {:?}",
                    id, cid, e
                );
            } else {
                send_ok = true;
            }
        }

        if !send_ok {
            eprintln!("Signal: failed to send task reminder {}: no delivery", id);
            // Release claim so the next poll can retry.
            if let Err(reset_err) = sqlx::query(
                "UPDATE tasks SET reminded_at = NULL, updated_at = ? WHERE id = ? AND reminded_at = ?",
            )
            .bind(&now_s)
            .bind(&id)
            .bind(&now_s)
            .execute(pool)
            .await
            {
                eprintln!(
                    "Signal: failed to release reminder claim for {}: {:?}",
                    id, reset_err
                );
            }
        }
    }

    Ok(())
}


#[derive(Debug)]
enum TaskMutateOutcome {
    Done { title: String, already: bool },
    Snoozed { title: String, due: String },
    NotFound,
    DbError,
}

async fn load_task_title_status(
    pool: &SqlitePool,
    task_id: &str,
) -> Result<Option<(String, String)>, sqlx::Error> {
    sqlx::query_as::<_, (String, String)>("SELECT title, status FROM tasks WHERE id = ?")
        .bind(task_id)
        .fetch_optional(pool)
        .await
}

async fn complete_task_by_id(pool: &SqlitePool, task_id: &str) -> TaskMutateOutcome {
    let row = match load_task_title_status(pool, task_id).await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Failed to load task {task_id}: {e:?}");
            return TaskMutateOutcome::DbError;
        }
    };
    let Some((title, status)) = row else {
        return TaskMutateOutcome::NotFound;
    };
    if status == "done" {
        return TaskMutateOutcome::Done { title, already: true };
    }
    let now = chrono::Utc::now().to_rfc3339();
    if let Err(e) = sqlx::query("UPDATE tasks SET status = 'done', updated_at = ? WHERE id = ?")
        .bind(&now)
        .bind(task_id)
        .execute(pool)
        .await
    {
        eprintln!("Failed to mark task done: {e:?}");
        return TaskMutateOutcome::DbError;
    }
    TaskMutateOutcome::Done { title, already: false }
}

async fn snooze_task_by_id(
    pool: &SqlitePool,
    task_id: &str,
    days: i64,
    config: &AppConfig,
) -> TaskMutateOutcome {
    let row = match load_task_title_status(pool, task_id).await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Failed to load task {task_id}: {e:?}");
            return TaskMutateOutcome::DbError;
        }
    };
    let Some((title, _)) = row else {
        return TaskMutateOutcome::NotFound;
    };
    let due = (config.now_in_tz().date_naive() + chrono::Duration::days(days))
        .format("%Y-%m-%d")
        .to_string();
    let now = chrono::Utc::now().to_rfc3339();
    let due_at = parse_due_phrase_tz(&due, config.resolved_tz()).map(|p| p.due_at);
    if let Err(e) = sqlx::query(
        "UPDATE tasks SET status = 'snoozed', due_date = ?, due_at = ?, reminded_at = NULL, updated_at = ? WHERE id = ?",
    )
    .bind(&due)
    .bind(due_at.as_deref())
    .bind(&now)
    .bind(task_id)
    .execute(pool)
    .await
    {
        eprintln!("Failed to snooze task: {e:?}");
        return TaskMutateOutcome::DbError;
    }
    TaskMutateOutcome::Snoozed { title, due }
}

async fn mark_task_complete(
    bot: &Bot,
    chat_id: &ChatId,
    pool: &SqlitePool,
    config: &AppConfig,
    id_prefix: &str,
) -> Result<(), SignalError> {
    let Some((id, _title, _status)) = find_task_by_prefix(bot, chat_id, pool, id_prefix).await? else {
        return Ok(());
    };

            match complete_task_by_id(pool, &id).await {
        TaskMutateOutcome::Done { title, already } => {
            let msg = if already {
                format!("ℹ️ Task already done: _{}_", (title))
            } else {
                format!("✅ Marked done: _{}_", (title))
            };
            if !already {
                sync_calendar_after_complete(pool, config, &id).await;
                refresh_task_memory(pool, &id).await;
            }
            send_signal(&bot, chat_id, msg)
                .await?;
        }
        TaskMutateOutcome::NotFound => {
            send_signal(&bot, chat_id, "⚠️ Task not found.").await?;
        }
        TaskMutateOutcome::DbError => {
            send_signal(&bot, chat_id, "❌ Database error updating task.")
                .await?;
        }
        TaskMutateOutcome::Snoozed { .. } => unreachable!(),
    }
    Ok(())
}

/// Mark open/snoozed tasks done.
///
/// Linked personal DMs: only that member's tasks + unassigned (immediate).
/// Household / unlinked chats: preview unless `confirm` is true.
async fn mark_all_tasks_complete(
    bot: &Bot,
    chat_id: &ChatId,
    pool: &SqlitePool,
    config: &AppConfig,
    confirm: bool,
) -> Result<(), SignalError> {
    let linked_member = member_for_signal_aci(config, chat_id.lookup_aci());
    let assignee_filter = linked_member.map(|m| m.id.as_str());

    // Household wipe requires an explicit confirm step.
    if linked_member.is_none() && !confirm {
        let rows = match list_completable_open_tasks(pool, None).await {
            Ok(r) => r,
            Err(e) => {
                eprintln!("Failed to preview complete-all tasks: {:?}", e);
                send_signal(&bot, chat_id, "❌ Database error listing tasks.")
                    .await?;
                return Ok(());
            }
        };
        if rows.is_empty() {
            send_signal(&bot, chat_id, "✅ No open or snoozed tasks to complete.")
                .await?;
            return Ok(());
        }
        let count = rows.len();
        let mut msg = format!(
            "⚠️ This will mark *{}* open/snoozed task{} done *household-wide*.\n\n",
            count,
            if count == 1 { "" } else { "s" }
        );
        for (i, (_id, title)) in rows.iter().enumerate() {
            if i >= 10 {
                msg.push_str(&format!("_…and {} more_\n", count - 10));
                break;
            }
            msg.push_str(&format!(
                "• {}\n",
                (truncate_chars(title, 80))
            ));
        }
        msg.push_str(
            "\nReply `/tasks complete all confirm` to proceed \
             (linked DMs only clear your tasks + unassigned).",
        );
        send_signal(&bot, chat_id, msg)
            .await?;
        return Ok(());
    }

    let now = chrono::Utc::now().to_rfc3339();
    let rows = match complete_all_open_tasks(pool, &now, assignee_filter).await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Failed to mark all tasks done: {:?}", e);
            send_signal(&bot, chat_id, "❌ Database error updating tasks.")
                .await?;
            return Ok(());
        }
    };

    if rows.is_empty() {
        let empty_msg = if linked_member.is_some() {
            "✅ No open or snoozed tasks of yours (or unassigned) to complete."
        } else {
            "✅ No open or snoozed tasks to complete."
        };
        send_signal(&bot, chat_id, empty_msg).await?;
        return Ok(());
    }

    for row in &rows {
        if row.calendar_event_id.is_some() {
            let link = TaskCalendarLink {
                calendar_event_id: row.calendar_event_id.clone(),
                assigned_to: row.assigned_to.clone(),
                duration_minutes: None,
            };
            if delete_linked_calendar_event(config, &link).await {
                clear_task_calendar_event_id(pool, &row.id).await;
            }
        }
    }

    let count = rows.len();
    let mut msg = if linked_member.is_some() {
        format!(
            "✅ Marked *{}* of your open/snoozed tasks (and unassigned) done:\n",
            count
        )
    } else {
        format!(
            "✅ Marked *{}* open/snoozed task{} done household-wide:\n",
            count,
            if count == 1 { "" } else { "s" }
        )
    };
    for (i, row) in rows.iter().enumerate() {
        if i >= 15 {
            msg.push_str(&format!("_…and {} more_", count - 15));
            break;
        }
        msg.push_str(&format!(
            "• {}\n",
            (truncate_chars(&row.title, 80))
        ));
    }

    send_signal(&bot, chat_id, msg)
        .await?;

    for row in &rows {
        refresh_task_memory(pool, &row.id).await;
    }
    Ok(())
}

async fn snooze_task(
    bot: &Bot,
    chat_id: &ChatId,
    pool: &SqlitePool,
    config: &AppConfig,
    id_prefix: &str,
    days: i64,
) -> Result<(), SignalError> {
    let Some((id, _title, _)) = find_task_by_prefix(bot, chat_id, pool, id_prefix).await? else {
        return Ok(());
    };

    match snooze_task_by_id(pool, &id, days, config).await {
        TaskMutateOutcome::Snoozed { title, due } => {
            let calendar_note = sync_calendar_after_snooze(pool, config, &id, &due).await;
            let mut msg = format!(
                "😴 Snoozed until *{}*: _{}_",
                due,
                (title)
            );
            if let Some(note) = calendar_note {
                msg.push_str(&format!(" · _{}_", note));
            }
            send_signal(&bot, chat_id, msg)
                .await?;
            refresh_task_memory(pool, &id).await;
        }
        TaskMutateOutcome::NotFound => {
            send_signal(&bot, chat_id, "⚠️ Task not found.").await?;
        }
        TaskMutateOutcome::DbError => {
            send_signal(&bot, chat_id, "❌ Database error snoozing task.")
                .await?;
        }
        TaskMutateOutcome::Done { .. } => unreachable!(),
    }
    Ok(())
}

#[derive(Debug, Clone)]
struct TaskCalendarLink {
    calendar_event_id: Option<String>,
    assigned_to: Option<String>,
    duration_minutes: Option<i64>,
}

async fn load_task_calendar_link(pool: &SqlitePool, task_id: &str) -> Option<TaskCalendarLink> {
    let row: Option<(Option<String>, Option<String>, Option<i64>)> = sqlx::query_as(
        "SELECT calendar_event_id, assigned_to, duration_minutes FROM tasks WHERE id = ?",
    )
    .bind(task_id)
    .fetch_optional(pool)
    .await
    .unwrap_or_else(|e| {
        eprintln!(
            "Signal: failed to load calendar link for task {}: {:?}",
            task_id, e
        );
        None
    });

    row.map(|(calendar_event_id, assigned_to, duration_minutes)| TaskCalendarLink {
        calendar_event_id,
        assigned_to,
        duration_minutes,
    })
}

fn calendar_client_for_assignee(
    config: &AppConfig,
    assigned_to: Option<&str>,
) -> Option<GoogleCalendarClient> {
    let mid = assigned_to?;
    let member = config.family.members.iter().find(|m| m.id == mid)?;
    build_calendar_client(member)
}

async fn delete_linked_calendar_event(config: &AppConfig, link: &TaskCalendarLink) -> bool {
    let Some(event_id) = link
        .calendar_event_id
        .as_deref()
        .filter(|s| !s.is_empty())
    else {
        return true;
    };
    let Some(client) = calendar_client_for_assignee(config, link.assigned_to.as_deref()) else {
        eprintln!(
            "Signal: skip calendar delete for event {} (no client for assignee {:?})",
            event_id, link.assigned_to
        );
        return false;
    };
    match client.delete_event(event_id).await {
        Ok(()) => {
            println!("Signal: deleted calendar event {}", event_id);
            true
        }
        Err(e) => {
            eprintln!(
                "Signal: failed to delete calendar event {}: {:?}",
                event_id, e
            );
            false
        }
    }
}

async fn clear_task_calendar_event_id(pool: &SqlitePool, task_id: &str) {
    let now = chrono::Utc::now().to_rfc3339();
    if let Err(e) = sqlx::query(
        "UPDATE tasks SET calendar_event_id = NULL, updated_at = ? WHERE id = ?",
    )
    .bind(&now)
    .bind(task_id)
    .execute(pool)
    .await
    {
        eprintln!(
            "Signal: failed to clear calendar_event_id for {}: {:?}",
            task_id, e
        );
    }
}

/// Delete the linked Google event after a task is marked done; clear the stored
/// id only when delete succeeds (404 counts as success).
async fn sync_calendar_after_complete(pool: &SqlitePool, config: &AppConfig, task_id: &str) {
    let Some(link) = load_task_calendar_link(pool, task_id).await else {
        return;
    };
    if link.calendar_event_id.is_none() {
        return;
    }
    if delete_linked_calendar_event(config, &link).await {
        clear_task_calendar_event_id(pool, task_id).await;
    }
}

/// Reschedule the linked Google event after a snooze. Returns an optional
/// user-facing note for the confirmation message.
async fn sync_calendar_after_snooze(
    pool: &SqlitePool,
    config: &AppConfig,
    task_id: &str,
    due_yyyy_mm_dd: &str,
) -> Option<&'static str> {
    let link = load_task_calendar_link(pool, task_id).await?;
    if link.calendar_event_id.is_none() {
        return None;
    }
    let due_at = parse_due_phrase_tz(due_yyyy_mm_dd, config.resolved_tz())?.due_at;
    match reschedule_linked_calendar_event(config, pool, task_id, &link, &due_at).await {
        CalendarRescheduleOutcome::Updated => Some("📅 calendar updated"),
        CalendarRescheduleOutcome::NoOp => None,
        CalendarRescheduleOutcome::StaleCleared => Some("calendar event was already gone"),
        CalendarRescheduleOutcome::Failed => Some("calendar update failed"),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CalendarRescheduleOutcome {
    Updated,
    NoOp,
    StaleCleared,
    Failed,
}

/// Reschedule the linked Google event to `due_at_rfc3339`.
async fn reschedule_linked_calendar_event(
    config: &AppConfig,
    pool: &SqlitePool,
    task_id: &str,
    link: &TaskCalendarLink,
    due_at_rfc3339: &str,
) -> CalendarRescheduleOutcome {
    let Some(event_id) = link
        .calendar_event_id
        .as_deref()
        .filter(|s| !s.is_empty())
    else {
        return CalendarRescheduleOutcome::NoOp;
    };
    let Some(client) = calendar_client_for_assignee(config, link.assigned_to.as_deref()) else {
        eprintln!(
            "Signal: skip calendar reschedule for event {} (no client for assignee {:?})",
            event_id, link.assigned_to
        );
        return CalendarRescheduleOutcome::Failed;
    };
    let Ok(due_dt) = chrono::DateTime::parse_from_rfc3339(due_at_rfc3339) else {
        eprintln!(
            "Signal: invalid due_at for calendar reschedule: {}",
            due_at_rfc3339
        );
        return CalendarRescheduleOutcome::Failed;
    };
    let start = due_dt.with_timezone(&chrono::Utc);
    let duration = link
        .duration_minutes
        .unwrap_or(TASK_CALENDAR_DURATION_MINUTES);
    match reschedule_at(&client, event_id, start, duration).await {
        Ok(()) => {
            println!(
                "Signal: rescheduled calendar event {} to {}",
                event_id, due_at_rfc3339
            );
            CalendarRescheduleOutcome::Updated
        }
        Err(chotu_common::CalendarError::Api { status: 404, .. }) => {
            eprintln!(
                "Signal: calendar event {} missing on snooze; clearing stored id",
                event_id
            );
            clear_task_calendar_event_id(pool, task_id).await;
            CalendarRescheduleOutcome::StaleCleared
        }
        Err(e) => {
            eprintln!(
                "Signal: failed to reschedule calendar event {}: {:?}",
                event_id, e
            );
            CalendarRescheduleOutcome::Failed
        }
    }
}

async fn reassign_task(
    bot: &Bot,
    chat_id: &ChatId,
    pool: &SqlitePool,
    id_prefix: &str,
    member_id: &str,
) -> Result<(), SignalError> {
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
        send_signal(&bot, chat_id, "❌ Database error reassigning task.")
            .await?;
        return Ok(());
    }

    send_signal(&bot, chat_id, format!(
            "👤 Assigned to *{}*: _{}_",
            member_id,
            (title)
        ),)
    .await?;

    refresh_task_memory(pool, &id).await;
    Ok(())
}

async fn reopen_task(
    bot: &Bot,
    chat_id: &ChatId,
    pool: &SqlitePool,
    id_prefix: &str,
) -> Result<(), SignalError> {
    let Some((id, title, status)) = find_task_by_prefix(bot, chat_id, pool, id_prefix).await? else {
        return Ok(());
    };

    if status == "open" {
        send_signal(&bot, chat_id, format!("ℹ️ Already open: _{}_", (title)),)
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
        send_signal(&bot, chat_id, "❌ Database error reopening task.")
            .await?;
        return Ok(());
    }

    send_signal(&bot, chat_id, format!("📂 Reopened: _{}_", (title)),)
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

async fn handle_trends(
    bot: &Bot,
    chat_id: &ChatId,
    args: String,
    pool: &SqlitePool,
    config: &AppConfig,
    llm: &ChotuLlm,
) -> Result<(), SignalError> {
    let days = args
        .trim()
        .parse::<i64>()
        .unwrap_or(7)
        .clamp(2, 90);

    send_signal(&bot, chat_id, format!("📈 Building nutrition trends for the last {} days...", days),)
    .await?;

    let only_member_id = member_for_signal_aci(config, chat_id.lookup_aci()).map(|m| m.id.as_str());
    match health_coach::build_nutrition_trend_reports(
        pool,
        config,
        days,
        Some(llm),
        only_member_id,
    )
    .await
    {
        Ok(reports) => {
            if reports.is_empty() {
                send_signal(&bot, chat_id, "_No trends to show for your linked member in this window._",)
                .await?;
            } else {
                for report in reports {
                    send_signal(&bot, chat_id, report)
                        .await?;
                }
            }
        }
        Err(e) => {
            eprintln!("Trends query error: {:?}", e);
            send_signal(&bot, chat_id, format!("❌ Failed to build trends: {}", e))
                .await?;
        }
    }

    Ok(())
}

async fn handle_brief(
    bot: &Bot,
    chat_id: &ChatId,
    pool: &SqlitePool,
    config: &AppConfig,
) -> Result<(), SignalError> {
    send_signal(&bot, chat_id, "☀️ Building morning brief...")
        .await?;

    // Linked DMs get private calendar/tasks/nutrition/training; household chat stays family-wide.
    let for_member = member_for_signal_aci(config, chat_id.lookup_aci()).map(|m| m.id.as_str());
    let report = crate::brief::compose_morning_brief(pool, config, for_member).await;
    send_signal(&bot, chat_id, report)
        .await?;
    Ok(())
}

async fn handle_plan(
    bot: &Bot,
    chat_id: &ChatId,
    args: String,
    pool: &SqlitePool,
    config: &AppConfig,
    llm: &ChotuLlm,
) -> Result<(), SignalError> {
    let regenerate = matches!(
        args.trim().to_lowercase().as_str(),
        "new" | "regen" | "regenerate" | "refresh" | "redo"
    );
    if !args.trim().is_empty() && !regenerate {
        send_signal(&bot, chat_id, "Usage: `/plan` (show this week) or `/plan new` (regenerate).",)
        .await?;
        return Ok(());
    }

    let member_id = default_member_id(config, chat_id.lookup_aci()).to_string();
    let member = config
        .family
        .members
        .iter()
        .find(|m| m.id.eq_ignore_ascii_case(&member_id));
    if member
        .and_then(|m| m.fitness_goals.as_ref())
        .map(|g| g.is_empty())
        .unwrap_or(true)
    {
        send_signal(&bot, chat_id, format!(
                "⚠️ No `fitness_goals` for *{}* in config.yaml yet.\n\
                 Add intent / target_date / sessions_per_week, then try `/plan` again.",
                member_id
            ),)
        .await?;
        return Ok(());
    }

    let week_start = health_coach::current_week_start_str();
    if !regenerate {
        if let Ok(Some(stored)) =
            health_coach::load_weekly_plan(pool, &member_id, &week_start).await
        {
            let mut msg = stored.plan_md.clone();
            let today = chrono::Local::now().date_naive();
            if let Some(session) = health_coach::session_for_date_from_stored(&stored, today) {
                let notes = session.notes.trim();
                msg.push_str("\n📌 *Today:* ");
                if notes.is_empty() {
                    msg.push_str(session.kind.as_str());
                } else {
                    msg.push_str(&format!("{} — {}", session.kind.as_str(), notes));
                }
                msg.push('\n');
            }
            if let Some(progress) = health_coach::plan_week_progress_line(
                pool,
                &member_id,
                &week_start,
                &stored.plan_json,
                today,
            )
            .await
            {
                msg.push('\n');
                msg.push_str(&progress);
                msg.push('\n');
            }
            send_signal(&bot, chat_id, msg)
                .await?;
            return Ok(());
        }
    }

    send_signal(&bot, chat_id, if regenerate {
            "🏋️ Regenerating this week's training plan (local Ollama)…"
        } else {
            "🏋️ Building this week's training plan (local Ollama)…"
        },)
    .await?;

    match health_coach::generate_and_store_weekly_plan(pool, llm, config, &member_id, &week_start)
        .await
    {
        Ok(stored) => {
            let mut msg = stored.plan_md;
            let today = chrono::Local::now().date_naive();
            if let Ok(plan) = health_coach::parse_plan_json(&stored.plan_json) {
                if let Some(session) =
                    health_coach::session_for_date(&stored.week_start, &plan, today)
                {
                    let notes = session.notes.trim();
                    msg.push_str("\n📌 *Today:* ");
                    if notes.is_empty() {
                        msg.push_str(session.kind.as_str());
                    } else {
                        msg.push_str(&format!("{} — {}", session.kind.as_str(), notes));
                    }
                    msg.push('\n');
                }
            }
            if let Some(progress) = health_coach::plan_week_progress_line(
                pool,
                &member_id,
                &week_start,
                &stored.plan_json,
                today,
            )
            .await
            {
                msg.push('\n');
                msg.push_str(&progress);
                msg.push('\n');
            }
            send_signal(&bot, chat_id, msg)
                .await?;
        }
        Err(e) => {
            eprintln!("Training plan generation failed: {:?}", e);
            send_signal(&bot, chat_id, format!("❌ Could not build plan: {}", e))
                .await?;
        }
    }
    Ok(())
}

async fn handle_cal(
    bot: &Bot,
    chat_id: &ChatId,
    args: String,
    config: &AppConfig,
) -> Result<(), SignalError> {
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
        send_signal(&bot, chat_id, "⚠️ Usage: `/cal [today|tomorrow|week]`",)
        .await?;
        return Ok(());
    };

    let report = compose_calendar_agenda(
        config,
        window,
        member_for_signal_aci(config, chat_id.lookup_aci()).map(|m| m.id.as_str()),
    )
    .await;
    send_signal(&bot, chat_id, report)
        .await?;
    Ok(())
}

async fn handle_memory(
    bot: &Bot,
    chat_id: &ChatId,
    args: String,
    pool: &SqlitePool,
    config: &AppConfig,
    llm: &ChotuLlm,
    gemini_client: &GeminiClient,
) -> Result<(), SignalError> {
    let args = args.trim();
    if args.is_empty() {
        send_signal(&bot, chat_id, concat!(
                "Usage: `/memory <question>`\n",
                "Household chat searches journals, digests, personal references, and tasks.\n",
                "Linked DMs search your journals and tasks (including unassigned), not digests or personal references.\n",
                "Or `/memory reindex` to rebuild the embedding index.",
            ),)
        .await?;
        return Ok(());
    }

    let index = MemoryIndex::from_env();

    if args.eq_ignore_ascii_case("reindex") {
        send_signal(&bot, chat_id, "🧠 Rebuilding memory index (this may take a while)...")
            .await?;
        match index.reindex_all(pool, true).await {
            Ok(stats) => {
                send_signal(&bot, chat_id, format!(
                        "✅ Memory reindex complete.\n• upserted: {}\n• skipped: {}\n• deleted: {}\n• errors: {}",
                        stats.upserted, stats.skipped, stats.deleted, stats.errors
                    ),)
                .await?;
            }
            Err(e) => {
                eprintln!("Memory reindex failed: {:?}", e);
                send_signal(&bot, chat_id, format!("❌ Memory reindex failed: {}", e))
                    .await?;
            }
        }
        return Ok(());
    }

    send_signal(&bot, chat_id, "🧠 Searching memory...")
        .await?;

    let for_member_id = member_for_signal_aci(config, chat_id.lookup_aci()).map(|m| m.id.as_str());
    let hits = match index.search(pool, args, None, for_member_id).await {
        Ok(h) => h,
        Err(e) => {
            eprintln!("Memory search failed: {:?}", e);
            send_signal(&bot, chat_id, format!(
                    "❌ Memory search failed: {}.\nTip: run `/memory reindex` after `ollama pull nomic-embed-text`.",
                    e
                ),)
            .await?;
            return Ok(());
        }
    };

    if hits.is_empty() {
        send_signal(&bot, chat_id, "I couldn't find anything relevant in journals, digests, personal references, or tasks.",)
        .await?;
        return Ok(());
    }

    send_signal(&bot, chat_id, format!(
            "📚 Found {} matches — drafting with local Ollama (usually ~10–30s; times out at 45s)…",
            hits.len()
        ),)
    .await?;

    // Prefer local Ollama (same model as email/intent); Gemini only if Ollama fails.
    let reply = match answer_memory_query(Some(llm), Some(gemini_client), args, &hits).await {
        Ok(text) => text,
        Err(e) => {
            eprintln!("Memory answer failed: {:?}", e);
            chotu_common::format_hit_list(&hits)
        }
    };

    send_signal(&bot, chat_id, reply)
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
    chat_id: &ChatId,
    pool: &SqlitePool,
    config: &AppConfig,
    llm: &ChotuLlm,
) -> Result<(), SignalError> {
    let date_str = chrono::Local::now().format("%Y-%m-%d").to_string();

    // Query daily financials and health summaries
    let (txs, healths) = match crate::reflection::get_daily_data(pool, &date_str, config).await {
        Ok(data) => data,
        Err(e) => {
            eprintln!("Status query error: {:?}", e);
            send_signal(&bot, chat_id, "Failed to retrieve today's logs from database.")
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
    send_signal(&bot, chat_id, finance_report)
        .await?;

    // 2. Build per-member health reports, then coach tips in parallel.
    // Linked personal DMs only see their own health/fitness (goals stay private).
    let status_member_id = member_for_signal_aci(config, chat_id.lookup_aci()).map(|m| m.id.clone());
    let mut pending: Vec<(String, Option<health_coach::NutritionCoachContext>)> = Vec::new();

    for h in &healths {
        if let Some(ref only) = status_member_id {
            if !h.family_member_id.eq_ignore_ascii_case(only) {
                continue;
            }
        }
        let member = config
            .family
            .members
            .iter()
            .find(|m| m.id == h.family_member_id);
        let name = member
            .map(|m| m.name.as_str())
            .unwrap_or(h.family_member_id.as_str());

        let mut member_report = format!("🏃 *Health Status: {} ({})*\n\n", name, date_str);

        let has_activity = h.step_count > 0 || h.active_calories_burned > 0;
        let has_sleep = h.sleep_hours.is_some();
        let has_energy = h.perceived_energy.is_some();

        let exercises = health_coach::exercises_for_day(pool, &h.family_member_id, &date_str)
            .await
            .unwrap_or_default();

        if has_activity || has_sleep || has_energy || !exercises.is_empty() {
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
            if !exercises.is_empty() {
                member_report.push_str("  - Exercises:\n");
                for e in exercises.iter().take(6) {
                    member_report.push_str(&format!("    • {}\n", e));
                }
            }
            member_report.push_str("\n");
        }

        if let Some(fitness) = member.and_then(|m| m.fitness_goals.as_ref()).filter(|g| !g.is_empty())
        {
            let today = chrono::Local::now().date_naive();
            if let Some(block) = fitness.outcome_markdown(today) {
                member_report.push_str(&block);
                member_report.push('\n');
            }
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
            table.push_str(&format!(
                "{:<14} | {:<10}\n",
                "Calories",
                format!("{} kcal", h.total_calories_ingested)
            ));
            table.push_str(&format!(
                "{:<14} | {:<10}\n",
                "Protein",
                format!("{:.1}g", h.protein_grams)
            ));
            table.push_str(&format!(
                "{:<14} | {:<10}\n",
                "Carbs",
                format!("{:.1}g", h.carbs_grams)
            ));
            table.push_str(&format!(
                "{:<14} | {:<10}\n",
                "Fat",
                format!("{:.1}g", h.fats_grams)
            ));

            if h.saturated_fat_g > 0.0 {
                table.push_str(&format!(
                    "{:<14} | {:<10}\n",
                    "  Saturated",
                    format!("{:.1}g", h.saturated_fat_g)
                ));
            }
            if h.unsaturated_fat_g > 0.0 {
                table.push_str(&format!(
                    "{:<14} | {:<10}\n",
                    "  Unsaturated",
                    format!("{:.1}g", h.unsaturated_fat_g)
                ));
            }
            if h.trans_fat_g > 0.0 {
                table.push_str(&format!(
                    "{:<14} | {:<10}\n",
                    "  Trans Fat",
                    format!("{:.1}g", h.trans_fat_g)
                ));
            }
            if h.cholesterol_mg > 0.0 {
                table.push_str(&format!(
                    "{:<14} | {:<10}\n",
                    "  Cholesterol",
                    format!("{:.1}mg", h.cholesterol_mg)
                ));
            }
            if h.triglycerides_mg > 0.0 {
                table.push_str(&format!(
                    "{:<14} | {:<10}\n",
                    "  Triglycerides",
                    format!("{:.1}mg", h.triglycerides_mg)
                ));
            }
            if h.omega_3_dha_mg > 0.0 {
                table.push_str(&format!(
                    "{:<14} | {:<10}\n",
                    "  Omega-3 DHA",
                    format!("{:.1}mg", h.omega_3_dha_mg)
                ));
            }

            if h.vitamin_a_mcg > 0.0 {
                table.push_str(&format!(
                    "{:<14} | {:<10}\n",
                    "Vitamin A",
                    format!("{:.1}mcg", h.vitamin_a_mcg)
                ));
            }
            if h.vitamin_b_mg > 0.0 {
                table.push_str(&format!(
                    "{:<14} | {:<10}\n",
                    "Vitamin B",
                    format!("{:.1}mg", h.vitamin_b_mg)
                ));
            }
            if h.vitamin_c_mg > 0.0 {
                table.push_str(&format!(
                    "{:<14} | {:<10}\n",
                    "Vitamin C",
                    format!("{:.1}mg", h.vitamin_c_mg)
                ));
            }
            if h.vitamin_d_mcg > 0.0 {
                table.push_str(&format!(
                    "{:<14} | {:<10}\n",
                    "Vitamin D",
                    format!("{:.1}mcg", h.vitamin_d_mcg)
                ));
            }
            if h.vitamin_e_mg > 0.0 {
                table.push_str(&format!(
                    "{:<14} | {:<10}\n",
                    "Vitamin E",
                    format!("{:.1}mg", h.vitamin_e_mg)
                ));
            }
            if h.vitamin_k_mcg > 0.0 {
                table.push_str(&format!(
                    "{:<14} | {:<10}\n",
                    "Vitamin K",
                    format!("{:.1}mcg", h.vitamin_k_mcg)
                ));
            }

            if h.sodium_mg > 0.0 {
                table.push_str(&format!(
                    "{:<14} | {:<10}\n",
                    "Sodium",
                    format!("{:.1}mg", h.sodium_mg)
                ));
            }
            if h.potassium_mg > 0.0 {
                table.push_str(&format!(
                    "{:<14} | {:<10}\n",
                    "Potassium",
                    format!("{:.1}mg", h.potassium_mg)
                ));
            }
            if h.calcium_mg > 0.0 {
                table.push_str(&format!(
                    "{:<14} | {:<10}\n",
                    "Calcium",
                    format!("{:.1}mg", h.calcium_mg)
                ));
            }
            if h.magnesium_mg > 0.0 {
                table.push_str(&format!(
                    "{:<14} | {:<10}\n",
                    "Magnesium",
                    format!("{:.1}mg", h.magnesium_mg)
                ));
            }
            if h.zinc_mg > 0.0 {
                table.push_str(&format!(
                    "{:<14} | {:<10}\n",
                    "Zinc",
                    format!("{:.1}mg", h.zinc_mg)
                ));
            }
            if h.iron_mg > 0.0 {
                table.push_str(&format!(
                    "{:<14} | {:<10}\n",
                    "Iron",
                    format!("{:.1}mg", h.iron_mg)
                ));
            }

            if h.fiber_g > 0.0 {
                table.push_str(&format!(
                    "{:<14} | {:<10}\n",
                    "Fiber",
                    format!("{:.1}g", h.fiber_g)
                ));
            }
            if h.sugar_g > 0.0 {
                table.push_str(&format!(
                    "{:<14} | {:<10}\n",
                    "Sugar",
                    format!("{:.1}g", h.sugar_g)
                ));
            }
            if h.caffeine_mg > 0.0 {
                table.push_str(&format!(
                    "{:<14} | {:<10}\n",
                    "Caffeine",
                    format!("{:.1}mg", h.caffeine_mg)
                ));
            }

            table.push_str("```\n");
            member_report.push_str(&table);
        }

        let goals = member.and_then(|m| m.nutrition_goals.as_ref());
        if let Some(goals) = goals {
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

        let has_fitness = member
            .and_then(|m| m.fitness_goals.as_ref())
            .map(|g| !g.is_empty())
            .unwrap_or(false);

        if !has_activity && !has_sleep && !has_energy && !has_nutrition && exercises.is_empty() && !has_fitness
        {
            member_report.push_str("• _No health telemetry logged today._\n");
            pending.push((member_report, None));
        } else {
            let ctx = health_coach::NutritionCoachContext::from_day_summary(name, h, goals);
            let ctx = health_coach::enrich_coach_context(
                pool,
                config,
                &h.family_member_id,
                ctx,
                health_coach::CoachEnrichOpts::for_day(&date_str),
            )
            .await;
            pending.push((member_report, Some(ctx)));
        }
    }

    // Parallel Ollama tips for members with health data
    let mut tips: Vec<Option<String>> = vec![None; pending.len()];
    let mut set: tokio::task::JoinSet<(usize, Option<String>)> = tokio::task::JoinSet::new();
    for (idx, (_, ctx_opt)) in pending.iter().enumerate() {
        if let Some(ctx) = ctx_opt.clone() {
            // Clone the owned client (not the &ChotuLlm) so the task is 'static.
            let llm = (*llm).clone();
            set.spawn(async move {
                let tip = health_coach::generate_nutrition_coach_tip(&llm, &ctx)
                    .await
                    .ok();
                (idx, tip)
            });
        }
    }
    while let Some(joined) = set.join_next().await {
        match joined {
            Ok((idx, tip)) => {
                if let Some(tip) = tip {
                    tips[idx] = Some(tip);
                }
            }
            Err(e) => eprintln!("Nutrition coach tip task join failed: {:?}", e),
        }
    }

    for (i, (mut report, _)) in pending.into_iter().enumerate() {
        if let Some(tip) = tips[i].take() {
            report.push_str("\n• *Coach:* ");
            report.push_str(&tip);
            if !tip.ends_with('\n') {
                report.push('\n');
            }
        }
        send_signal(&bot, chat_id, report)
            .await?;
    }

    Ok(())
}

async fn handle_networth(
    bot: &Bot,
    chat_id: &ChatId,
    pool: &SqlitePool,
    config: &AppConfig,
) -> Result<(), SignalError> {
    let has_holdings: bool =
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM portfolio_holdings")
            .fetch_one(pool)
            .await
            .unwrap_or(0)
            > 0;
    if has_holdings {
        send_signal(&bot, chat_id, "🔍 Fetching live quotes via Yahoo Finance...")
            .await?;
    }

    match build_networth_summary(pool, config).await {
        Ok(msg) => {
            send_signal(&bot, chat_id, msg)
                .await?;
        }
        Err(e) => {
            send_signal(&bot, chat_id, format!("❌ {}", e)).await?;
        }
    }

    Ok(())
}

/// One DB + FX/quote fetch shared by `/networth` and the evening scheduler.
async fn build_networth_summary(pool: &SqlitePool, config: &AppConfig) -> Result<String, String> {
    let base = config.currency();
    let rates = fetch_exchange_rates(base).await;

    // Portfolio only for now — email ledger is a transaction log, not a cash balance.
    let holdings: Vec<chotu_common::PortfolioHolding> = sqlx::query_as::<_, chotu_common::PortfolioHolding>(
        "SELECT ticker, shares_owned, average_cost, average_cost_currency, last_updated FROM portfolio_holdings"
    )
    .fetch_all(pool)
    .await
    .map_err(|e| {
        eprintln!("Failed to fetch portfolio holdings: {:?}", e);
        "Database error retrieving portfolio.".to_string()
    })?;

    let mut msg = String::new();
    msg.push_str(&format!("💰 *Project Chotu Net Worth Summary* ({})\n\n", base));
    msg.push_str("• 💵 *Liquid Cash:* _not tracked yet_ (ledger is spend history, not balances)\n");

    if holdings.is_empty() {
        msg.push_str(&format!(
            "• 📈 *Stock Portfolio:* $0.00 {} (No holdings yet — drop a portfolio statement to sync)\n",
            base
        ));
        msg.push_str("\n━━━━━━━━━━━━━━━━━━━━━━━━\n");
        msg.push_str(&format!("✨ *Invested Net Worth:* $0.00 {}", base));
        return Ok(msg);
    }

    let tickers: Vec<(String, Option<CostHint>)> = holdings
        .iter()
        .map(|h| {
            (
                h.ticker.clone(),
                Some(CostHint {
                    average_cost: h.average_cost,
                    currency: h.average_cost_currency.clone(),
                }),
            )
        })
        .collect();
    let prices = match fetch_stock_quotes_near_cost(&tickers).await {
        Ok(p) => p,
        Err(e) => {
            eprintln!("Failed to fetch stock prices: {:?}", e);
            let mut portfolio_cost = 0.0;
            for h in &holdings {
                let converted = holding_values_in_base(h, None, config, &rates);
                portfolio_cost += converted.cost_base;
            }
            msg.push_str(&format!(
                "• 📈 *Stock Portfolio:* ${:.2} {} (quote lookup failed, showing FX'd book cost)\n",
                portfolio_cost, base
            ));
            msg.push_str("\n━━━━━━━━━━━━━━━━━━━━━━━━\n");
            msg.push_str(&format!(
                "✨ *Invested Net Worth:* ${:.2} {} (estimated)",
                portfolio_cost, base
            ));
            return Ok(msg);
        }
    };

    let price_map: std::collections::HashMap<String, (f64, String)> = prices
        .into_iter()
        .map(|p| (p.ticker.to_uppercase(), (p.price, p.currency)))
        .collect();

    let mut total_portfolio_value = 0.0;
    let mut total_portfolio_cost = 0.0;
    let mut breakdown = String::new();
    let mut missing_tickers: Vec<String> = Vec::new();

    for h in &holdings {
        let ticker_upper = h.ticker.to_uppercase();
        let quote = price_map.get(&ticker_upper).map(|(p, c)| (*p, c.as_str()));
        let converted = holding_values_in_base(h, quote, config, &rates);
        if converted.missing_quote {
            missing_tickers.push(ticker_upper.clone());
        }
        total_portfolio_cost += converted.cost_base;
        total_portfolio_value += converted.value_base;

        let diff_percent = if converted.cost_base > 0.0 {
            (converted.value_base - converted.cost_base) / converted.cost_base * 100.0
        } else {
            0.0
        };
        let sign = if diff_percent >= 0.0 { "+" } else { "" };

        breakdown.push_str(&format!(
            "  - *{}*: {:.1} shares @ ${:.2} {} (Cost: ${:.2} | Value: ${:.2} | {}{:.1}%)\n",
            ticker_upper,
            h.shares_owned,
            converted.price_base,
            base,
            converted.cost_base,
            converted.value_base,
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
    if !missing_tickers.is_empty() {
        msg.push_str(&format!(
            "  _({} holding(s) used book cost: {})_\n",
            missing_tickers.len(),
            missing_tickers.join(", ")
        ));
    }
    msg.push_str("\n━━━━━━━━━━━━━━━━━━━━━━━━\n");
    msg.push_str(&format!(
        "✨ *Invested Net Worth:* ${:.2} {}\n\n",
        total_portfolio_value, base
    ));
    msg.push_str("*Holdings Breakdown:*\n");
    msg.push_str(&breakdown);

    Ok(msg)
}

/// Per-share price and book cost converted into the configured base currency.
struct HoldingValuesInBase {
    price_base: f64,
    cost_base: f64,
    value_base: f64,
    missing_quote: bool,
}

/// Convert a holding's live price and book cost into base currency.
///
/// Cost currency preference:
/// 1. persisted `average_cost_currency` from statement extraction
/// 2. otherwise the Yahoo quote currency (best available hint)
/// 3. otherwise treat amounts as already in base
fn holding_values_in_base(
    holding: &chotu_common::PortfolioHolding,
    quote: Option<(f64, &str)>,
    config: &AppConfig,
    rates: &std::collections::HashMap<String, f64>,
) -> HoldingValuesInBase {
    let base = config.currency();
    let book_cost = holding.shares_owned * holding.average_cost;
    let stored_cost_ccy = holding
        .average_cost_currency
        .as_deref()
        .map(str::trim)
        .filter(|c| !c.is_empty());

    match quote {
        Some((raw_price, quote_currency)) => {
            let cost_currency = stored_cost_ccy.unwrap_or(quote_currency);
            let price_base = config.convert_to_base(raw_price, quote_currency, rates);
            let cost_base = config.convert_to_base(book_cost, cost_currency, rates);
            HoldingValuesInBase {
                price_base,
                cost_base,
                value_base: holding.shares_owned * price_base,
                missing_quote: false,
            }
        }
        None => {
            let cost_currency = stored_cost_ccy.unwrap_or(base);
            let price_base = config.convert_to_base(holding.average_cost, cost_currency, rates);
            let cost_base = config.convert_to_base(book_cost, cost_currency, rates);
            HoldingValuesInBase {
                price_base,
                cost_base,
                value_base: holding.shares_owned * price_base,
                missing_quote: true,
            }
        }
    }
}

async fn handle_monthly(
    bot: &Bot,
    chat_id: &ChatId,
    args: String,
    pool: &SqlitePool,
    config: &AppConfig,
) -> Result<(), SignalError> {
    let date_str = args.trim().to_string();
    let target_month = if date_str.is_empty() {
        chrono::Local::now().format("%Y-%m").to_string()
    } else {
        if date_str.len() != 7 || !date_str.contains('-') {
            send_signal(&bot, chat_id, "⚠️ Invalid format. Usage: `/monthly [YYYY-MM]` (e.g. `/monthly 2026-06`)").await?;
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
            send_signal(&bot, chat_id, "❌ Database error retrieving monthly ledger.").await?;
            return Ok(());
        }
    };

    if entries.is_empty() {
        send_signal(&bot, chat_id, format!("📅 *No transactions found for {}*.", target_month))
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

    match compute_budget_progress(pool, config, &target_month).await {
        Ok(rows) if !rows.is_empty() => {
            msg.push_str("\n━━━━━━━━━━━━━━━━━━━━━━━━\n\n");
            msg.push_str(&format_budget_progress_markdown(
                &target_month,
                base,
                &rows,
            ));
        }
        Ok(_) => {}
        Err(e) => {
            eprintln!("Failed to compute budget progress for monthly: {:?}", e);
        }
    }

    send_signal(&bot, chat_id, msg)
        .await?;

    Ok(())
}

async fn handle_budget(
    bot: &Bot,
    chat_id: &ChatId,
    args: String,
    pool: &SqlitePool,
    config: &AppConfig,
) -> Result<(), SignalError> {
    let trimmed = args.trim();
    if trimmed.is_empty() {
        return send_budget_progress(bot, chat_id, pool, config).await;
    }

    let mut parts = trimmed.split_whitespace();
    let action = parts.next().unwrap_or("").to_lowercase();
    match action.as_str() {
        "set" => {
            let category = parts.next().unwrap_or("").trim();
            let amount_raw = parts.next().unwrap_or("").trim();
            if category.is_empty() || amount_raw.is_empty() {
                send_signal(&bot, chat_id, "⚠️ Usage: `/budget set <Category> <amount>` (e.g. `/budget set Food 800`)",)
                .await?;
                return Ok(());
            }
            let Ok(amount) = amount_raw.replace(',', "").parse::<f64>() else {
                send_signal(&bot, chat_id, "⚠️ Amount must be a number (e.g. `800`).")
                    .await?;
                return Ok(());
            };
            if amount <= 0.0 {
                send_signal(&bot, chat_id, "⚠️ Amount must be greater than zero.")
                    .await?;
                return Ok(());
            }
            let display = display_category(category);
            if display.is_empty() || display.to_lowercase() == "income" {
                send_signal(&bot, chat_id, "⚠️ Invalid category name.")
                    .await?;
                return Ok(());
            }
            match set_budget_override(pool, &display, amount).await {
                Ok(()) => {
                    let base = config.currency();
                    send_signal(&bot, chat_id, format!(
                            "✅ Budget set: *{}* → ${:.0} {} / month\n\n{}",
                            (display),
                            amount,
                            base,
                            "_Telegram override (wins over config.yaml)._"
                        ),)
                    .await?;
                    send_budget_progress(bot, chat_id, pool, config).await?;
                }
                Err(e) => {
                    eprintln!("Failed to set budget override: {:?}", e);
                    send_signal(&bot, chat_id, "❌ Failed to save budget override.")
                        .await?;
                }
            }
        }
        "clear" => {
            let category = parts.next().unwrap_or("").trim();
            if category.is_empty() {
                send_signal(&bot, chat_id, "⚠️ Usage: `/budget clear <Category>` (e.g. `/budget clear Entertainment`)",)
                .await?;
                return Ok(());
            }
            let display = display_category(category);
            match clear_budget_override(pool, &display).await {
                Ok(true) => {
                    send_signal(&bot, chat_id, format!(
                            "✅ Cleared Telegram override for *{}* (falls back to config.yaml if set).",
                            (display)
                        ),)
                    .await?;
                    send_budget_progress(bot, chat_id, pool, config).await?;
                }
                Ok(false) => {
                    send_signal(&bot, chat_id, format!(
                            "ℹ️ No Telegram override found for *{}*. YAML budgets are unchanged.",
                            (display)
                        ),)
                    .await?;
                }
                Err(e) => {
                    eprintln!("Failed to clear budget override: {:?}", e);
                    send_signal(&bot, chat_id, "❌ Failed to clear budget override.")
                        .await?;
                }
            }
        }
        _ => {
            send_signal(&bot, chat_id, "⚠️ Usage:\n• `/budget` — this month's progress\n\
                 • `/budget set <Category> <amount>`\n\
                 • `/budget clear <Category>`",)
            .await?;
        }
    }
    Ok(())
}

async fn send_budget_progress(
    bot: &Bot,
    chat_id: &ChatId,
    pool: &SqlitePool,
    config: &AppConfig,
) -> Result<(), SignalError> {
    let month = current_budget_month();
    let base = config.currency();
    match compute_budget_progress(pool, config, &month).await {
        Ok(rows) => {
            let msg = format_budget_progress_markdown(&month, base, &rows);
            send_signal(&bot, chat_id, msg)
                .await?;
        }
        Err(e) => {
            eprintln!("Failed to compute budget progress: {:?}", e);
            send_signal(&bot, chat_id, "❌ Database error retrieving budgets.")
                .await?;
        }
    }
    Ok(())
}

async fn poll_spend_budget_alerts(
    bot: &Bot,
    pool: &SqlitePool,
    config: &AppConfig,
) -> Result<(), SignalError> {
    let month = current_budget_month();
    let base = config.currency();
    let alerts = match pending_budget_alerts(pool, config, &month).await {
        Ok(a) => a,
        Err(e) => {
            eprintln!("Signal: pending_budget_alerts failed: {:?}", e);
            return Ok(());
        }
    };
    if alerts.is_empty() {
        return Ok(());
    }

    let mut msg = String::new();
    for (i, alert) in alerts.iter().enumerate() {
        if i > 0 {
            msg.push_str("\n\n");
        }
        msg.push_str(&alert.format_markdown(base));
    }
    println!(
        "Signal: Pushing {} spend budget alert(s) for {}",
        alerts.len(),
        month
    );
    if !send_household(bot, config, msg).await {
        eprintln!(
            "Signal: spend budget alerts not delivered; will retry on next poll"
        );
        return Ok(());
    }

    for alert in &alerts {
        if let Err(e) =
            mark_budget_alert_sent(pool, &month, &alert.category, alert.threshold).await
        {
            eprintln!(
                "Signal: failed to mark budget alert sent ({}/{}): {:?}",
                alert.category, alert.threshold, e
            );
        }
    }
    Ok(())
}

async fn handle_reflect_trigger(
    bot: &Bot,
    chat_id: &ChatId,
    pool: &SqlitePool,
    llm: &ChotuLlm,
    states: StateMap,
    config: &AppConfig,
    prompt_attempts: u32,
) -> Result<(), SignalError> {
    let date_str = chrono::Local::now().format("%Y-%m-%d").to_string();

    let ping = send_signal(&bot, chat_id, "Querying daily metrics and generating evening reflection prompt via local Ollama...",)
        .await;
    // Scheduled runs ignore a failed status ping so we can still deliver the prompt.
    // Interactive `/reflect` still surfaces the send error.
    if prompt_attempts <= 1 {
        ping?;
    }

    let (txs, healths) = match crate::reflection::get_daily_data(pool, &date_str, config).await {
        Ok(data) => data,
        Err(e) => {
            eprintln!("Reflect prompt query error: {:?}", e);
            send_plain_retry(
                bot,
                chat_id,
                "Failed to retrieve today's logs from database.",
                prompt_attempts,
                "evening reflection db error",
            )
            .await?;
            return Ok(());
        }
    };

    match crate::reflection::generate_reflection_prompt(
        llm,
        &txs,
        &healths,
        &date_str,
        config.core_values.as_ref(),
    )
    .await
    {
        Ok(prompt) => {
            let msg_text = format!(
                "📝 *Evening Journaling Reflection Prompt*:\n\n\
                 _{}_\n\n\
                 Reply directly to this message to record your daily reflection entry in your journal.",
                prompt
            );

            send_markdown_retry(
                bot,
                chat_id,
                msg_text,
                prompt_attempts,
                "evening reflection prompt",
            )
            .await?;

            let mut s = states.write().await;
            s.insert(
                chat_id.clone(),
                ConversationState::WaitingForReflection {
                    date: date_str,
                    prompt,
                },
            );
        }
        Err(e) => {
            eprintln!("Failed to generate reflection prompt: {:?}", e);
            send_plain_retry(
                bot,
                chat_id,
                format!("❌ Failed to generate reflection prompt: {}", e),
                prompt_attempts,
                "evening reflection llm error",
            )
            .await?;
        }
    }

    Ok(())
}

async fn handle_message(
    bot: Bot,
    chat_id: ChatId,
    sender_aci: String,
    inbound: SignalInbound,
    pool: SqlitePool,
    llm: ChotuLlm,
    gemini_client: GeminiClient,
    states: StateMap,
    shared_config: SharedConfig,
) -> Result<(), SignalError> {
    let text = inbound.text.clone().unwrap_or_default();
    println!("Signal: Received message from {} in {}. Content: {:?}", sender_aci, chat_id, text);
    let config = shared_config.read().await.clone();

    if let Some(quote_timestamp) = inbound.quote_timestamp {
        let reply_text = text.trim().to_lowercase();
        let is_unactionable_cue = [
            "not useful", "unactionable", "ignore", "not worth it", "delete", "trash", "useless",
        ].iter().any(|cue| reply_text == *cue || reply_text.contains(cue) || reply_text.contains("not worth"));
        if is_unactionable_cue {
            let (kind, recipient_id) = match &chat_id {
                SignalRecipient::Direct { aci } => ("direct", aci.as_str()),
                SignalRecipient::Group { group_id } => ("group", group_id.as_str()),
            };
            let task_opt: Option<(String, Option<String>, Option<String>, Option<String>)> = sqlx::query_as(
                "SELECT t.title, t.email_sender, t.email_subject, t.id \
                 FROM tasks t \
                 JOIN task_signal_messages m ON m.task_id = t.id \
                 WHERE m.recipient_kind = ? AND m.recipient_id = ? AND m.message_timestamp = ?"
            )
            .bind(kind)
            .bind(recipient_id)
            .bind(quote_timestamp)
            .fetch_optional(&pool)
            .await
            .ok()
            .flatten();
            if let Some((title, email_sender, email_subject, task_id)) = task_opt {
                println!("Signal: Marking task as ignored and recording feedback for task: {}", title);
                sqlx::query("UPDATE tasks SET status = 'ignored' WHERE id = ?").bind(&task_id).execute(&pool).await.ok();
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
                send_signal(&bot, &chat_id, format!("Got it! Marked the task \"{title}\" as unactionable. Similar emails will be filtered out in the future.")).await?;
                return Ok(());
            }
        }
    }

    let active_state = {
        let s = states.read().await;
        s.get(&chat_id).cloned()
    };

    if let Some(ConversationState::WaitingForReflection { date, prompt }) = active_state {
        if inbound.attachments.iter().any(|a| a.content_type.starts_with("image/")) {
            handle_food_photo(&bot, &chat_id, &inbound, &pool, &llm, &gemini_client, &config).await?;
            send_signal(&bot, &chat_id, "Evening reflection is still open — type your journal reply, or send a command to cancel.").await?;
            return Ok(());
        }
        let response_text = text.trim();
        if response_text.is_empty() {
            send_signal(&bot, &chat_id, "Reflection text cannot be empty. Please type your reflection or send a command to cancel.").await?;
            return Ok(());
        }
        send_signal(&bot, &chat_id, "Saving reflection entry to your local journal...").await?;
        let (txs, healths) = match crate::reflection::get_daily_data(&pool, &date, &config).await {
            Ok(data) => data,
            Err(e) => {
                eprintln!("Failed to query daily data during save: {:?}", e);
                send_signal(&bot, &chat_id, "Failed to retrieve today's logs to compile journal header.").await?;
                return Ok(());
            }
        };
        match crate::reflection::save_reflection(
            &date,
            &prompt,
            response_text,
            &txs,
            &healths,
            member_for_signal_aci(&config, chat_id.lookup_aci()).map(|m| m.id.as_str()),
        ).await {
            Ok(filepath) => {
                {
                    let mut s = states.write().await;
                    s.insert(chat_id.clone(), ConversationState::Idle);
                }
                let index = MemoryIndex::from_env();
                if let Err(e) = index.index_journal_file(&pool, &filepath).await {
                    eprintln!("Memory: failed to index journal {:?}: {:?}", filepath, e);
                }
                let filename = filepath.file_name().and_then(|n| n.to_str()).unwrap_or("journal.md");
                send_signal(&bot, &chat_id, format!("Reflection recorded.\nSaved file `{filename}` inside `~/chotu_brain/Journal/`.")).await?;
            }
            Err(e) => {
                eprintln!("Failed to save journal reflection file: {:?}", e);
                send_signal(&bot, &chat_id, format!("Failed to write journal file: {e}")).await?;
            }
        }
    } else if inbound.attachments.iter().any(|a| a.content_type.starts_with("image/")) {
        handle_food_photo(&bot, &chat_id, &inbound, &pool, &llm, &gemini_client, &config).await?;
    } else {
        dispatch_free_text_intent(&bot, &chat_id, &text, &pool, &llm, &gemini_client, &config).await?;
    }
    Ok(())
}

/// Download a Signal food photo, analyze with Gemini (+ Open Food Facts for barcodes), persist.
async fn handle_food_photo(
    bot: &Bot,
    chat_id: &ChatId,
    inbound: &SignalInbound,
    pool: &SqlitePool,
    llm: &ChotuLlm,
    gemini_client: &GeminiClient,
    config: &AppConfig,
) -> Result<(), SignalError> {
    let Some(attachment) = inbound.attachments.iter().find(|a| a.content_type.starts_with("image/") && !a.id.is_empty()) else {
        send_signal(bot, chat_id, "Couldn't read that photo. Try sending it again.").await?;
        return Ok(());
    };
    let body = inbound.text.as_deref().unwrap_or("").trim();
    let caption_src = if !body.is_empty() { body } else { attachment.caption.as_deref().unwrap_or("") };
    let caption = strip_leading_food_command(caption_src);

    let (member_id, caption_rest) = resolve_food_member_and_description(caption, config, chat_id.lookup_aci());
    if reject_foreign_food_mutation(bot, chat_id, config, &member_id).await? {
        return Ok(());
    }

    send_signal(bot, chat_id, "Analyzing food photo (barcode / package / plate)… usually under a minute").await?;
    let image_bytes = match bot.get_attachment(chat_id, &attachment.id).await {
        Ok(bytes) => bytes,
        Err(e) => {
            eprintln!("Failed to download Signal photo: {:?}", e);
            send_signal(bot, chat_id, "Couldn't download that photo from Signal.").await?;
            return Ok(());
        }
    };

    let analysis = {
        let _photo_nudge = ProgressNudge::spawn(
            bot.clone(),
            chat_id.clone(),
            20,
            "Still analyzing that photo — hang tight…".to_string(),
        );
        gemini_client.approximate_nutrition_from_image(&image_bytes, &attachment.content_type, caption).await
    };
    let analysis = match analysis {
        Ok(a) => a,
        Err(e) => {
            eprintln!("Gemini food-photo analysis failed: {:?}", e);
            send_signal(bot, chat_id, format!("Failed to analyze food photo: {e}")).await?;
            return Ok(());
        }
    };
    if analysis.kind == FoodPhotoKind::Unknown {
        send_signal(bot, chat_id, "Doesn't look like food — send a barcode, product package, or plated meal (optional caption like `praj half the bowl`).").await?;
        return Ok(());
    }

    let (description, nutrition, source_note) = if let Some(ref barcode) = analysis.barcode {
        match lookup_barcode(barcode).await {
            Ok(Some(product)) => {
                let desc = if caption_rest.is_empty() {
                    format!("{} [barcode {}]", product.product_name, barcode)
                } else {
                    format!("{} — {} [barcode {}]", product.product_name, caption_rest, barcode)
                };
                (desc, product.nutrition, format!("Open Food Facts ({})", barcode))
            }
            Ok(None) => {
                let desc = if analysis.description.trim().is_empty() {
                    format!("Barcode {} (not in Open Food Facts)", barcode)
                } else if caption_rest.is_empty() {
                    analysis.description.clone()
                } else {
                    format!("{} ({})", analysis.description, caption_rest)
                };
                (desc, analysis.nutrition, format!("Gemini vision; barcode {} not in Open Food Facts", barcode))
            }
            Err(e) => {
                eprintln!("Open Food Facts lookup error: {:?}", e);
                let desc = if caption_rest.is_empty() { analysis.description.clone() } else { format!("{} ({})", analysis.description, caption_rest) };
                (desc, analysis.nutrition, "Gemini vision (OFF lookup failed)".to_string())
            }
        }
    } else {
        let desc = if analysis.description.trim().is_empty() {
            if caption_rest.is_empty() { "Food photo".to_string() } else { caption_rest.clone() }
        } else if caption_rest.is_empty() || analysis.description.to_lowercase().contains(&caption_rest.to_lowercase()) {
            analysis.description.clone()
        } else {
            format!("{} ({})", analysis.description, caption_rest)
        };
        (desc, analysis.nutrition, "Gemini vision".to_string())
    };

    println!("Signal: food photo kind={:?} source={} member={}", analysis.kind, source_note, member_id);
    send_signal(bot, chat_id, format!("Using {} for *{}*…", source_note, member_id)).await?;

    let timing = if caption_rest.trim().is_empty() {
        resolve_food_log_timing(None, None)
    } else {
        match llm.extract_food_log_context(&caption_rest).await {
            Ok(ctx) => {
                let food_time = effective_food_time(&caption_rest, ctx.food_time.as_deref());
                resolve_food_log_timing(ctx.food_date.as_deref(), food_time.as_deref())
            }
            Err(e) => {
                eprintln!("Food photo caption timing extract failed (using now): {:?}", e);
                resolve_food_log_timing(None, None)
            }
        }
    };

    persist_food_estimation(bot, chat_id, pool, config, &member_id, &description, &nutrition, &timing).await?;
    Ok(())
}

/// Classify idle free-text with local Ollama and reuse existing command handlers.
async fn dispatch_free_text_intent(
    bot: &Bot,
    chat_id: &ChatId,
    text: &str,
    pool: &SqlitePool,
    llm: &ChotuLlm,
    gemini_client: &GeminiClient,
    config: &AppConfig,
) -> Result<(), SignalError> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        send_signal(&bot, chat_id, "Send a message like \"what's today\", \"morning brief\", \"open tasks\", or \"log eggs for praj\".",
        )
        .await?;
        return Ok(());
    }

    // Local Ollama classification can take a while — acknowledge immediately so the chat
    // doesn't look frozen (food logs especially felt stuck for ~1–2 minutes).
    send_signal(&bot, chat_id, "Got it — working on that…")
        .await?;

    let member_ids: Vec<String> = config
        .family
        .members
        .iter()
        .map(|m| m.id.clone())
        .collect();

    let classification = {
        let _classify_nudge = ProgressNudge::spawn(
            bot.clone(),
            chat_id.clone(),
            20,
            "Still figuring that out — local model is thinking…".to_string(),
        );
        with_typing_indicator(bot, chat_id, async {
            llm.classify_intent(trimmed, &member_ids).await
        })
        .await
    };

    let classification = match classification {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Intent classification failed: {:?}", e);
            send_signal(&bot, chat_id, "I couldn't understand that just now. Try a slash command (`/status`, `/tasks`, `/food`) or rephrase.",)
            .await?;
            return Ok(());
        }
    };

    println!(
        "Signal: free-text intent={:?} reason={}",
        classification.intent, classification.reason
    );

    match classification.into_user_intent() {
        UserIntent::Status => handle_status(bot, chat_id, pool, config, llm).await?,
        UserIntent::Brief => handle_brief(bot, chat_id, pool, config).await?,
        UserIntent::Plan { regenerate } => {
            let args = if regenerate {
                "new".to_string()
            } else {
                String::new()
            };
            handle_plan(bot, chat_id, args, pool, config, llm).await?;
        }
        UserIntent::Calendar { window } => {
            handle_cal(bot, chat_id, window, config).await?;
        }
        UserIntent::Trends { days } => {
            let args = days.map(|d| d.to_string()).unwrap_or_default();
            handle_trends(bot, chat_id, args, pool, config, llm).await?;
        }
        UserIntent::Tasks { filter } => {
            handle_tasks(bot, chat_id, filter, pool, config).await?;
        }
        UserIntent::TaskAdd {
            member_id,
            title,
            due_raw,
        } => {
            let member_id =
                member_id.or_else(|| Some(default_member_id(config, chat_id.lookup_aci()).to_string()));
            create_manual_task(bot, chat_id, pool, config, member_id, title, due_raw).await?;
        }
        UserIntent::Memory { query } => {
            handle_memory(bot, chat_id, query, pool, config, llm, gemini_client).await?;
        }
        UserIntent::Sync => {
            if let Err(e) =
                sync_google_health_nutrition(bot, chat_id, pool, gemini_client, config).await
            {
                eprintln!("Free-text sync failed: {:?}", e);
                send_signal(&bot, chat_id, format!("❌ Sync failed: {}", e))
                    .await?;
            }
        }
        UserIntent::Food {
            member_id,
            description,
            date,
            time,
        } => {
            let family_member_id = member_id.unwrap_or_else(|| {
                default_member_id(config, chat_id.lookup_aci()).to_string()
            });
            if reject_foreign_food_mutation(bot, chat_id, config, &family_member_id).await? {
                return Ok(());
            }
            log_food_for_member(
                bot,
                chat_id,
                pool,
                gemini_client,
                config,
                &family_member_id,
                &description,
                date.as_deref(),
                time.as_deref(),
                trimmed,
            )
            .await?;
        }
        UserIntent::Networth => {
            handle_networth(bot, chat_id, pool, config).await?;
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
        UserIntent::Budget => {
            handle_budget(bot, chat_id, String::new(), pool, config).await?;
        }
        UserIntent::Help => {
            let help_text = format!(
                "👋 Hi! I'm Chotu. You can use slash commands or plain English \
                 (calendar, brief, status, tasks, remind me, memory, food, sync, trends, net worth, monthly, budget).\n\n{}",

                HELP_TEXT
            );
            send_signal(&bot, chat_id, help_text).await?;
        }
        UserIntent::Unknown { clarify_question } => {
            send_signal(&bot, chat_id, clarify_question).await?;
        }
    }

    Ok(())
}

fn format_research_progress(event: &ResearchProgress, elapsed_secs: u64) -> String {
    let elapsed = format_elapsed(elapsed_secs);
    match event {
        ResearchProgress::Started {
            total_stages,
            seeded,
            panel,
            judge,
        } => {
            if *seeded {
                format!(
                    "🔍 Research started ({total_stages} stages, seeded list).\nScorers: {panel} · Judge: {judge}\n⏱ {elapsed} — this usually takes several minutes."
                )
            } else {
                format!(
                    "🔍 Research started ({total_stages} stages: propose → universe → score → judge).\nPanel: {panel} · Judge: {judge}\n⏱ {elapsed} — expect several minutes (frontier models in parallel)."
                )
            }
        }
        ResearchProgress::Proposing {
            stage,
            total_stages,
            model_count,
        } => format!(
            "📡 [{stage}/{total_stages}] Proposing tickers with {model_count} models…\n⏱ {elapsed}"
        ),
        ResearchProgress::UniverseReady {
            stage,
            total_stages,
            tickers,
            from_propose,
            finnhub_filtered,
            dropped_count,
            lookup_misses,
        } => {
            let source = if *from_propose {
                "from panel proposals"
            } else {
                "from your /research args"
            };
            let finnhub_note = if *finnhub_filtered {
                format!(
                    " · Finnhub verified (dropped {dropped_count}, misses {lookup_misses})"
                )
            } else {
                " · Finnhub off (model-estimated caps)".to_string()
            };
            format!(
                "✅ [{stage}/{total_stages}] Shared universe ready ({source}){finnhub_note}: {}\n⏱ {elapsed}",
                tickers.join(", ")
            )
        }
        ResearchProgress::Scoring {
            stage,
            total_stages,
            universe_size,
            model_count,
        } => format!(
            "📊 [{stage}/{total_stages}] Scoring {universe_size} names with {model_count} models…\n⏱ {elapsed} — often the longest step."
        ),
        ResearchProgress::ScoringDone {
            stage,
            total_stages,
            succeeded,
            failed,
        } => {
            if *failed == 0 {
                format!(
                    "✅ [{stage}/{total_stages}] Scoring done ({succeeded} scorers).\n⏱ {elapsed}"
                )
            } else {
                format!(
                    "⚠️ [{stage}/{total_stages}] Scoring done ({succeeded} ok, {failed} failed).\n⏱ {elapsed}"
                )
            }
        }
        ResearchProgress::Judging {
            stage,
            total_stages,
            judge,
        } => format!(
            "🧠 [{stage}/{total_stages}] Judge ({judge}) synthesizing shortlist…\n⏱ {elapsed}"
        ),
        ResearchProgress::Saving {
            stage,
            total_stages,
        } => format!("💾 [{stage}/{total_stages}] Saving report to disk…\n⏱ {elapsed}"),
    }
}

fn format_elapsed(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s elapsed")
    } else {
        format!("{}m {}s elapsed", secs / 60, secs % 60)
    }
}

async fn run_and_log_stock_research(
    bot: &Bot,
    chat_id: &ChatId,
    pool: &SqlitePool,
    researcher: &StockResearcher,
    philosophy: Option<&InvestmentPhilosophy>,
    targets: Option<&str>,
) -> Result<(), anyhow::Error> {
    run_and_log_stock_research_multi(bot, &[chat_id.clone()], pool, researcher, philosophy, targets).await
}

async fn run_and_log_stock_research_multi(
    bot: &Bot,
    chat_ids: &[ChatId],
    pool: &SqlitePool,
    researcher: &StockResearcher,
    philosophy: Option<&InvestmentPhilosophy>,
    targets: Option<&str>,
) -> Result<(), anyhow::Error> {
    let Some(progress_chat) = chat_ids.first().cloned() else {
        return Ok(());
    };

    if !researcher.is_configured() {
        for chat_id in chat_ids {
            send_signal(&bot, chat_id, "❌ Stock research requires `OPENROUTER_API_KEY` in `.env`. Gemini is not used for `/research`.",)
            .await?;
        }
        return Ok(());
    }

    let (progress_tx, mut progress_rx) = tokio::sync::mpsc::channel::<ResearchProgress>(16);
    let progress_bot = bot.clone();
    let started = std::time::Instant::now();
    let progress_task = tokio::spawn(async move {
        while let Some(event) = progress_rx.recv().await {
            let msg = format_research_progress(&event, started.elapsed().as_secs());
            if let Err(e) = send_signal(&progress_bot, &progress_chat, msg).await {
                eprintln!("Signal: failed to send research progress: {:?}", e);
            }
        }
    });

    let report =
        match run_stock_research_with_progress(pool, researcher, philosophy, targets, Some(progress_tx))
            .await
        {
            Ok(r) => r,
            Err(e) => {
                let _ = progress_task.await;
                for chat_id in chat_ids {
                    send_signal(&bot, chat_id, format!(
                            "❌ Stock research failed after {}: {}",
                            format_elapsed(started.elapsed().as_secs()),
                            e
                        ),)
                    .await?;
                }
                return Err(anyhow::anyhow!("Stock research failed: {:?}", e));
            }
        };

    let _ = progress_task.await;

    for chat_id in chat_ids {
        send_signal(&bot, chat_id, format!(
                "✅ Research complete in {}. Sending report…",
                format_elapsed(started.elapsed().as_secs())
            ),)
        .await?;
    }

    // Split the report into chunks under 4000 characters to keep long research reports segmented
    let chunks = split_message(&report, 4000);
    for chunk in chunks {
        for chat_id in chat_ids {
            if let Err(e) = send_signal(&bot, chat_id, &chunk)
                .await
            {
                eprintln!(
                    "Signal: failed to send report chunk with Markdown format ({:?}). Falling back to plain text...",
                    e
                );
                send_signal(&bot, chat_id, &chunk).await?;
            }
        }
    }

    Ok(())
}

async fn sync_google_health_nutrition(
    bot: &Bot,
    chat_id: &ChatId,
    pool: &SqlitePool,
    gemini_client: &GeminiClient,
    config: &AppConfig,
) -> Result<(), anyhow::Error> {
    send_signal(&bot, chat_id, "🔄 Connecting to Google Health API and pulling today's health metrics...",)
    .await?;

    match health_coach::sync_configured_members_today(pool, Some(gemini_client), config).await {
        Ok(reports) => {
            let only_id = member_for_signal_aci(config, chat_id.lookup_aci()).map(|m| m.id.clone());
            let mut shown = 0usize;
            for report in &reports {
                if let Some(ref only) = only_id {
                    if !report.member_id.eq_ignore_ascii_case(only) {
                        continue;
                    }
                }
                send_signal(&bot, chat_id, report.telegram_markdown())
                    .await?;
                shown += 1;
            }
            if let Some(ref only) = only_id {
                if shown == 0 {
                    send_signal(&bot, chat_id, format!(
                            "✅ Sync finished for the household, but no Google Health data for *{}* \
                             (link Health with `/login health {}`).",
                            only, only
                        ),)
                    .await?;
                } else if reports.len() > 1 {
                    send_signal(&bot, chat_id, format!(
                            "_Synced {} member(s); showing only your private metrics._",
                            reports.len()
                        ),)
                    .await?;
                }
            } else if reports.is_empty() {
                send_signal(&bot, chat_id, "_Sync finished — no member reports._")
                    .await?;
            }
        }
        Err(e) => {
            send_signal(&bot, chat_id, format!("❌ Google Health sync failed: {}", e))
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
    chat_id: &ChatId,
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
        send_signal(&bot, chat_id, format!(
                "⚠️ Usage: `/login calendar <member_id>`\n\nConfigured calendar members: {}",
                list
            ),)
        .await?;
        return Ok(());
    }

    let member = match config.family.members.iter().find(|m| m.id == member_id) {
        Some(m) => m,
        None => {
            send_signal(&bot, chat_id, format!(
                    "❌ Unknown member `{}`. Check `family.members` in config.yaml.",
                    member_id
                ),)
            .await?;
            return Ok(());
        }
    };

    if member.calendar.is_none() {
        send_signal(&bot, chat_id, format!(
                "❌ Member `{}` has no `calendar:` block in config.yaml.",
                member_id
            ),)
        .await?;
        return Ok(());
    }

    let client_id = match std::env::var("CHOTU_OAUTH_CLIENT_ID") {
        Ok(val) => val,
        Err(_) => {
            send_signal(&bot, chat_id, "❌ *Calendar Setup Required*\n\nConfigure `CHOTU_OAUTH_CLIENT_ID` and `CHOTU_OAUTH_CLIENT_SECRET` in `.env` (same Google OAuth client as Gmail).",)
            .await?;
            return Ok(());
        }
    };
    let client_secret = match std::env::var("CHOTU_OAUTH_CLIENT_SECRET") {
        Ok(val) => val,
        Err(_) => {
            send_signal(&bot, chat_id, "❌ Configure `CHOTU_OAUTH_CLIENT_SECRET` in `.env`.",)
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
    send_signal(&bot, chat_id, msg)
        .await?;

    let bot_clone = bot.clone();
    let chat_owned = chat_id.clone();
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
                            let _ = send_signal(&bot_clone, &chat_owned, format!("❌ Failed to save calendar token: {}", e),)
                                .await;
                            return;
                        }
                        let _ = send_signal(&bot_clone, &chat_owned, format!(
                                    "✅ *Calendar Authorization Successful!*\nSaved `{}` to `.env`.",
                                    env_key
                                ),)
                            .await;
                    }
                    Err(e) => {
                        let _ = send_signal(&bot_clone, &chat_owned, format!("❌ Calendar token exchange failed: {}", e),)
                            .await;
                    }
                }
            }
            Ok(Err(e)) => {
                let _ = send_signal(&bot_clone, &chat_owned, format!("❌ Calendar OAuth listener error: {}", e))
                    .await;
            }
            Err(_) => {
                let _ = send_signal(&bot_clone, &chat_owned, format!(
                            "❌ *Calendar Login Timeout*\n\nTry again with `/login calendar {}` or `/login code calendar {} <code>`.",
                            member_id_owned, member_id_owned
                        ),)
                    .await;
            }
        }
    });

    Ok(())
}

async fn handle_login_google_health(
    bot: &Bot,
    chat_id: &ChatId,
    member_id: &str,
    config: &AppConfig,
) -> Result<(), anyhow::Error> {
    let member_id = if member_id.is_empty() {
        match config.family.members.first() {
            Some(m) => m.id.clone(),
            None => {
                send_signal(&bot, chat_id, "⚠️ Usage: `/login health <member_id>`\n\nNo family members configured in config.yaml.",)
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
            send_signal(&bot, chat_id, format!(
                    "❌ Unknown member `{}`.\n\nConfigured family members: {}",
                    member_id,
                    members.join(", ")
                ),)
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
            send_signal(&bot, chat_id, msg)
                .await?;
            return Ok(());
        }
    };
    let client_secret = match std::env::var("FITBIT_CLIENT_SECRET") {
        Ok(val) => val,
        Err(_) => {
            let msg = "❌ *Google Health Setup Required*\n\n\
                Please configure `FITBIT_CLIENT_SECRET` in your `.env` file.";
            send_signal(&bot, chat_id, msg)
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

    send_signal(&bot, chat_id, setup_msg)
        .await?;

    let bot_clone = bot.clone();
    let chat_owned = chat_id.clone();
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
                let _ = send_signal(&bot_clone, &chat_owned, "⏳ Received authorization code. Swapping for tokens...",)
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
                                let _ = send_signal(&bot_clone, &chat_owned, success_msg)
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
                                let _ = send_signal(&bot_clone, &chat_owned, err_msg).await;
                                eprintln!("OAuth: Failed to write Google Health token: {:?}", e);
                            }
                        }
                    }
                    Err(e) => {
                        let err_msg = format!("❌ Google Health token exchange failed: {}", e);
                        let _ = send_signal(&bot_clone, &chat_owned, err_msg).await;
                        eprintln!("OAuth: Google Health token exchange failed: {:?}", e);
                    }
                }
            }
            Ok(Err(e)) => {
                let err_msg = format!("❌ Google Health callback server failed: {}", e);
                let _ = send_signal(&bot_clone, &chat_owned, err_msg).await;
                eprintln!("OAuth: Google Health callback listener failed: {:?}", e);
            }
            Err(_) => {
                let timeout_msg = format!(
                    "❌ *Google Health Login Timeout*\n\n\
                     Try again with `/login health {}` or `/login code health {} <code>`.",
                    member_id_owned, member_id_owned
                );
                let _ = send_signal(&bot_clone, &chat_owned, timeout_msg)
                    .await;
                println!("OAuth: Google Health redirect listener timed out");
            }
        }
    });

    Ok(())
}

async fn handle_login_google(bot: &Bot, chat_id: &ChatId) -> Result<(), anyhow::Error> {
    let client_id = match std::env::var("CHOTU_OAUTH_CLIENT_ID") {
        Ok(val) => val,
        Err(_) => {
            let msg = "❌ *Google Setup Required*\n\n\
                Please configure `CHOTU_OAUTH_CLIENT_ID` and `CHOTU_OAUTH_CLIENT_SECRET` in your `.env` file first.\n\n\
                1. Go to the [Google Cloud Console](https://console.cloud.google.com/), create a project and OAuth 2.0 Credentials.\n\
                2. Set the Redirect URI to `http://localhost:8080/callback`.\n\
                3. Paste the client credentials into your `.env` file and restart the agent.";
            send_signal(&bot, chat_id, msg)
                .await?;
            return Ok(());
        }
    };
    let client_secret = match std::env::var("CHOTU_OAUTH_CLIENT_SECRET") {
        Ok(val) => val,
        Err(_) => {
            let msg = "❌ *Google Setup Required*\n\n\
                Please configure `CHOTU_OAUTH_CLIENT_SECRET` in your `.env` file.";
            send_signal(&bot, chat_id, msg)
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

    send_signal(&bot, chat_id, setup_msg)
        .await?;

    let bot_clone = bot.clone();
    let chat_owned = chat_id.clone();
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
                let _ = send_signal(&bot_clone, &chat_owned, "⏳ Received authorization code. Swapping for tokens...",)
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
                            let _ = send_signal(&bot_clone, &chat_owned, success_msg)
                                .await;
                            println!("OAuth: Google refresh token successfully saved to .env");
                        }
                        Err(e) => {
                            let err_msg =
                                format!("❌ Failed to write Google refresh token to `.env`: {}", e);
                            let _ = send_signal(&bot_clone, &chat_owned, err_msg).await;
                            eprintln!("OAuth: Failed to write Google token: {:?}", e);
                        }
                    },
                    Err(e) => {
                        let err_msg = format!("❌ Google token exchange failed: {}", e);
                        let _ = send_signal(&bot_clone, &chat_owned, err_msg).await;
                        eprintln!("OAuth: Google token exchange failed: {:?}", e);
                    }
                }
            }
            Ok(Err(e)) => {
                let err_msg = format!("❌ Google callback server failed: {}", e);
                let _ = send_signal(&bot_clone, &chat_owned, err_msg).await;
                eprintln!("OAuth: Google callback listener failed: {:?}", e);
            }
            Err(_) => {
                let timeout_msg = "❌ *Google Login Timeout*\n\nThe login listener timed out after 5 minutes. Please try again with `/login gmail`.";
                let _ = send_signal(&bot_clone, &chat_owned, timeout_msg)
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
    chat_id: &ChatId,
    args: &str,
    config: &AppConfig,
) -> Result<(), anyhow::Error> {
    let mut parts = args.split_whitespace();
    let service = match parts.next() {
        Some(s) => s.to_lowercase(),
        None => {
            send_signal(&bot, chat_id, "⚠️ Usage: `/login code <gmail|health|calendar> ...`",)
            .await?;
            return Ok(());
        }
    };

    if service == "gmail" || service == "google" {
        let code_raw = match parts.next() {
            Some(c) => c,
            None => {
                send_signal(&bot, chat_id, "⚠️ Usage: `/login code gmail <code_or_url>`").await?;
                return Ok(());
            }
        };
        let code = clean_oauth_code(code_raw);

        let client_id = std::env::var("CHOTU_OAUTH_CLIENT_ID")?;
        let client_secret = std::env::var("CHOTU_OAUTH_CLIENT_SECRET")?;

        send_signal(&bot, chat_id, "⏳ Swapping manual code for Google/Gmail tokens...").await?;
        match exchange_google_code(&client_id, &client_secret, &code, "http://localhost:8080/callback").await {
            Ok(tokens) => {
                save_google_refresh_token(&tokens.refresh_token)?;
                send_signal(&bot, chat_id, "✅ *Google/Gmail Authorization Successful!*\nRefresh token saved manually.").await?;
            }
            Err(e) => {
                send_signal(&bot, chat_id, format!("❌ Gmail Token exchange failed: {}", e)).await?;
            }
        }
    } else if service == "fitbit" || service == "health" {
        let first = match parts.next() {
            Some(c) => c.to_string(),
            None => {
                send_signal(&bot, chat_id, "⚠️ Usage: `/login code health <member_id> <code_or_url>`",)
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
                    send_signal(&bot, chat_id, "⚠️ Usage: `/login code health <member_id> <code_or_url>`",)
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
                send_signal(&bot, chat_id, format!("❌ Unknown member `{}`.", member_id))
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

        send_signal(&bot, chat_id, "⏳ Swapping manual code for Google Health tokens...")
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
                send_signal(&bot, chat_id, format!(
                        "✅ *Google Health Authorization Successful!*\nSaved `{}`.",
                        env_key
                    ),)
                .await?;
            }
            Err(e) => {
                send_signal(&bot, chat_id, format!("❌ Google Health Token exchange failed: {}", e),)
                .await?;
            }
        }
    } else if service == "calendar" {
        let member_id = match parts.next() {
            Some(m) => m.to_string(),
            None => {
                send_signal(&bot, chat_id, "⚠️ Usage: `/login code calendar <member_id> <code_or_url>`",)
                .await?;
                return Ok(());
            }
        };
        if !config.family.members.iter().any(|m| m.id == member_id) {
            send_signal(&bot, chat_id, format!("❌ Unknown member `{}`.", member_id),)
            .await?;
            return Ok(());
        }
        let code_raw = match parts.next() {
            Some(c) => c,
            None => {
                send_signal(&bot, chat_id, "⚠️ Usage: `/login code calendar <member_id> <code_or_url>`",)
                .await?;
                return Ok(());
            }
        };
        let code = clean_oauth_code(code_raw);
        let client_id = std::env::var("CHOTU_OAUTH_CLIENT_ID")?;
        let client_secret = std::env::var("CHOTU_OAUTH_CLIENT_SECRET")?;

        send_signal(&bot, chat_id, "⏳ Swapping manual code for Calendar tokens...")
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
                send_signal(&bot, chat_id, format!(
                        "✅ *Calendar Authorization Successful!*\nSaved `CALENDAR_REFRESH_TOKEN_{}`.",
                        member_id.to_uppercase()
                    ),)
                .await?;
            }
            Err(e) => {
                send_signal(&bot, chat_id, format!("❌ Calendar token exchange failed: {}", e))
                    .await?;
            }
        }
    } else {
        send_signal(&bot, chat_id, "⚠️ Unknown service. Supported: `gmail`, `health`, `calendar`",)
        .await?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use std::collections::HashMap;

    fn holding(ticker: &str, shares: f64, avg_cost: f64, currency: Option<&str>) -> chotu_common::PortfolioHolding {
        chotu_common::PortfolioHolding {
            ticker: ticker.to_string(),
            shares_owned: shares,
            average_cost: avg_cost,
            average_cost_currency: currency.map(|c| c.to_string()),
            last_updated: Utc::now(),
        }
    }

    fn cad_config() -> AppConfig {
        AppConfig {
            currency: Some("CAD".to_string()),
            ..AppConfig::default()
        }
    }

    #[test]
    fn strip_leading_food_command_from_captions() {
        assert_eq!(strip_leading_food_command("/food pray"), "pray");
        assert_eq!(strip_leading_food_command("/FOOD pray leftover"), "pray leftover");
        assert_eq!(strip_leading_food_command("  /food  praj oats "), "praj oats");
        assert_eq!(strip_leading_food_command("/food"), "");
        assert_eq!(
            strip_leading_food_command("pray leftover rice"),
            "pray leftover rice"
        );
        assert_eq!(
            strip_leading_food_command("/foodie special"),
            "/foodie special"
        );
    }

    #[test]
    fn test_split_message() {
        let sample = "Line 1\nLine 2\nLine 3\nLine 4";
        let chunks = split_message(sample, 15);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0], "Line 1\nLine 2\n");
        assert_eq!(chunks[1], "Line 3\nLine 4\n");
    }

    #[test]
    fn holding_values_converts_quote_and_stored_cost_currency() {
        let config = cad_config();
        let rates = HashMap::from([("USD".to_string(), 0.73)]); // 1 CAD = 0.73 USD
        let h = holding("AAPL", 10.0, 100.0, Some("USD"));

        let v = holding_values_in_base(&h, Some((200.0, "USD")), &config, &rates);
        assert!(!v.missing_quote);
        // USD → CAD via amount / rate
        assert!((v.price_base - 200.0 / 0.73).abs() < 1e-9);
        assert!((v.cost_base - 1000.0 / 0.73).abs() < 1e-9);
        assert!((v.value_base - 10.0 * v.price_base).abs() < 1e-9);
    }

    #[test]
    fn holding_values_uses_stored_cost_currency_even_when_quote_differs() {
        let config = cad_config();
        let rates = HashMap::from([("USD".to_string(), 0.73)]);
        // CAD-listed ETF quoted in CAD, but statement cost was USD
        let h = holding("VFV.TO", 5.0, 80.0, Some("USD"));

        let v = holding_values_in_base(&h, Some((120.0, "CAD")), &config, &rates);
        assert!(!v.missing_quote);
        assert!((v.price_base - 120.0).abs() < 1e-9); // already CAD
        assert!((v.cost_base - 400.0 / 0.73).abs() < 1e-9); // 5 * 80 USD → CAD
    }

    #[test]
    fn holding_values_missing_quote_falls_back_to_cost_currency() {
        let config = cad_config();
        let rates = HashMap::from([("USD".to_string(), 0.73)]);
        let h = holding("AAPL", 2.0, 50.0, Some("USD"));

        let v = holding_values_in_base(&h, None, &config, &rates);
        assert!(v.missing_quote);
        assert!((v.price_base - 50.0 / 0.73).abs() < 1e-9);
        assert!((v.cost_base - 100.0 / 0.73).abs() < 1e-9);
    }

    #[test]
    fn holding_values_missing_quote_and_currency_assumes_base() {
        let config = cad_config();
        let rates = HashMap::new();
        let h = holding("XYZ", 3.0, 10.0, None);

        let v = holding_values_in_base(&h, None, &config, &rates);
        assert!(v.missing_quote);
        assert!((v.price_base - 10.0).abs() < 1e-9);
        assert!((v.cost_base - 30.0).abs() < 1e-9);
    }
    #[test]
    fn parse_command_reads_leading_slash_case_insensitively() {
        assert!(matches!(parse_command("/HELP"), Some(Command::Help)));
        assert!(matches!(parse_command("  /Food eggs"), Some(Command::Food(s)) if s == "eggs"));
        assert!(parse_command("/food@bot eggs").is_none());
        assert!(parse_command("food eggs").is_none());
        assert!(matches!(parse_command("/tasks complete abcdef12"), Some(Command::Tasks(s)) if s == "complete abcdef12"));
    }

    #[test]
    fn task_instructions_are_plain_text_commands() {
        let help = task_complete_snooze_help("abcdef12");
        assert!(help.contains("/tasks complete abcdef12"));
        assert!(help.contains("/tasks snooze abcdef12"));
        assert!(!help.contains("callback"));
        assert!(!help.contains("t:d:"));
    }

    fn inbound_direct(aci: &str, text: &str) -> SignalInbound {
        SignalInbound {
            sender_aci: aci.to_string(),
            recipient: SignalRecipient::Direct { aci: aci.to_string() },
            text: Some(text.to_string()),
            quote_timestamp: None,
            attachments: vec![],
        }
    }

    #[test]
    fn inbound_fixtures_cover_direct_group_and_missing_aci() {
        let direct = inbound_direct("aci-alex", "/whoami");
        assert!(matches!(direct.recipient, SignalRecipient::Direct { .. }));
        assert_eq!(direct.sender_aci, "aci-alex");
        let group = SignalInbound {
            sender_aci: "aci-alex".into(),
            recipient: SignalRecipient::Group { group_id: "household".into() },
            text: Some("/tasks".into()),
            quote_timestamp: None,
            attachments: vec![],
        };
        assert_eq!(group.recipient.group_id(), Some("household"));
        let missing = SignalInbound {
            sender_aci: String::new(),
            recipient: SignalRecipient::Direct { aci: String::new() },
            text: Some("/help".into()),
            quote_timestamp: None,
            attachments: vec![],
        };
        assert!(missing.sender_aci.is_empty());
        let bad_file = chotu_common::SignalAttachment {
            id: String::new(),
            content_type: "application/pdf".into(),
            size: None,
            caption: None,
        };
        assert!(!bad_file.content_type.starts_with("image/") || bad_file.id.is_empty());
    }
}
