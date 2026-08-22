use anyhow::{Context, Result};
use chotu_common::ChotuLlm;

mod brief;
mod reflection;
mod telegram;

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();

    println!("=== Booting Project Chotu Supervisor ===");

    // Run non-blocking startup OAuth checks in the background
    tokio::spawn(async move {
        if let Err(e) = perform_startup_oauth_checks().await {
            eprintln!("Error in startup OAuth checks: {:?}", e);
        }
    });

    // 1. Setup database pool
    let db_path = std::env::var("DATABASE_PATH").unwrap_or_else(|_| "chotu.db".to_string());
    println!("Initializing SQLite connection pool at: {}", db_path);
    let pool = chotu_common::init_db(&db_path)
        .await
        .context("Failed to initialize SQLite database pool")?;
    println!("Database migrations checked and executed.");

    // 1.5 Load configuration file (config.yaml)
    let config_path =
        std::env::var("CHOTU_CONFIG_PATH").unwrap_or_else(|_| "config.yaml".to_string());
    println!("Loading configuration from: {}", config_path);
    let config = chotu_common::load_config(&config_path);
    std::env::set_var("CHOTU_TIMEZONE", config.resolved_timezone_name());
    println!(
        "Agent timezone: {} (IANA tz database; instants in SQLite stay UTC)",
        config.resolved_timezone_name()
    );

    // 2. Setup LLM client
    let host = std::env::var("OLLAMA_HOST").unwrap_or_else(|_| "http://localhost".to_string());
    let port = std::env::var("OLLAMA_PORT")
        .unwrap_or_else(|_| "11434".to_string())
        .parse::<u16>()
        .unwrap_or(11434);
    let model = std::env::var("OLLAMA_MODEL").unwrap_or_else(|_| "qwen3.5:9b".to_string());

    println!(
        "Initializing Ollama classification client ({}:{} / model: {})",
        host, port, model
    );
    let llm = ChotuLlm::new(&host, port, &model)
        .with_prompt_path(config.email_classifier_prompt_path.clone());

    // 3. Spawn the agents concurrently as Tokio tasks
    println!("Spawning agent tasks on Tokio runtime...");

    // Streamer Agent task
    let streamer_pool = pool.clone();
    let streamer_llm = llm.clone();
    let streamer_config = config.clone();
    let streamer_task = tokio::spawn(async move {
        if let Err(e) = streamer::run(streamer_pool, streamer_llm, streamer_config).await {
            eprintln!("Error in Streamer Agent task: {:?}", e);
            return Err(e);
        }
        Ok(())
    });

    // Janitor Agent task
    let janitor_pool = pool.clone();
    let janitor_config = config.clone();
    let janitor_task = tokio::spawn(async move {
        if let Err(e) = janitor::run(janitor_pool, janitor_config).await {
            eprintln!("Error in Janitor Agent task: {:?}", e);
            return Err(e);
        }
        Ok(())
    });

    // Health Coach Agent task
    let health_coach_pool = pool.clone();
    let health_coach_config = config.clone();
    let health_coach_task = tokio::spawn(async move {
        if let Err(e) = health_coach::run(health_coach_pool, health_coach_config).await {
            eprintln!("Error in Health Coach Agent task: {:?}", e);
            return Err(e);
        }
        Ok(())
    });

    // Coordinator own loop (running as the Bookkeeper / reflection coordinator)
    let coordinator_pool = pool.clone();
    let coordinator_llm = llm.clone();
    let coordinator_config = config.clone();
    let coordinator_task = tokio::spawn(async move {
        println!("Coordinator Agent initiated.");
        // Verify connection by running a query
        let row: (i32,) = sqlx::query_as("SELECT 1")
            .fetch_one(&coordinator_pool)
            .await?;
        println!("Coordinator Agent verified DB connection: {}", row.0);

        println!("Coordinator Agent: starting Telegram Bot update loop...");
        let gemini_key = std::env::var("GEMINI_API_KEY")
            .context("GEMINI_API_KEY environment variable is required")?;

        telegram::start_telegram_bot(
            coordinator_pool,
            coordinator_llm,
            gemini_key,
            coordinator_config,
        )
        .await?;

        Ok::<(), anyhow::Error>(())
    });

    // Wait for the shutdown signal or any of the tasks to exit
    tokio::select! {
        res = streamer_task => {
            println!("Streamer task exited: {:?}", res);
        }
        res = janitor_task => {
            println!("Janitor task exited: {:?}", res);
        }
        res = health_coach_task => {
            println!("Health Coach task exited: {:?}", res);
        }
        res = coordinator_task => {
            println!("Coordinator task exited: {:?}", res);
        }
        _ = tokio::signal::ctrl_c() => {
            println!("\n[Ctrl-C] Shutdown signal received. Exiting Chotu gracefully...");
        }
    }

    println!("Graceful shutdown complete.");
    Ok(())
}

async fn perform_startup_oauth_checks() -> Result<()> {
    use chotu_common::{
        exchange_google_code, save_google_health_refresh_token, save_google_refresh_token,
        start_redirect_listener,
    };

    // 1. Google Health / Fitbit Check
    if let (Ok(client_id), Ok(client_secret)) = (
        std::env::var("FITBIT_CLIENT_ID"),
        std::env::var("FITBIT_CLIENT_SECRET"),
    ) {
        let has_legacy = std::env::var("FITBIT_REFRESH_TOKEN")
            .ok()
            .is_some_and(|t| !t.is_empty());
        let has_per_member = std::env::vars()
            .any(|(k, v)| k.starts_with("HEALTH_REFRESH_TOKEN_") && !v.is_empty());
        if !has_legacy && !has_per_member {
            let auth_url = format!(
                "https://accounts.google.com/o/oauth2/v2/auth?prompt=consent&access_type=offline&response_type=code&client_id={}&redirect_uri=http%3A%2F%2Flocalhost%3A8080%2Fcallback&scope={}",
                client_id,
                chotu_common::GOOGLE_HEALTH_OAUTH_SCOPES.replace(' ', "%20")
            );

            println!("\n================================================================");
            println!("⚠️  GOOGLE HEALTH AUTHORIZATION REQUIRED");
            println!("Please open the following link in your browser to authorize");
            println!("(primary account — use `/login health <member_id>` for others):");
            println!("{}", auth_url);
            open_browser(&auth_url);
            println!("Starting temporary listener on port 8080...");
            println!("================================================================\n");

            // Listen with a 5-minute timeout
            let listener_res = tokio::time::timeout(
                std::time::Duration::from_secs(300),
                start_redirect_listener(8080),
            )
            .await;

            match listener_res {
                Ok(Ok(code)) => {
                    println!("OAuth: Received authorization code. Swapping for tokens...");
                    match exchange_google_code(
                        &client_id,
                        &client_secret,
                        &code,
                        "http://localhost:8080/callback",
                    )
                    .await
                    {
                        Ok(tokens) => {
                            // Startup flow is primary-only; Telegram `/login health <id>`
                            // is the multi-member path.
                            save_google_health_refresh_token(&tokens.refresh_token)?;
                            println!("\n================================================================");
                            println!("✅ GOOGLE HEALTH AUTHORIZATION SUCCESSFUL!");
                            println!(
                                "Refresh token saved to .env as FITBIT_REFRESH_TOKEN (primary)."
                            );
                            println!(
                                "Link additional family members with `/login health <member_id>`."
                            );
                            println!("================================================================\n");
                        }
                        Err(e) => {
                            eprintln!("OAuth: Google Health token exchange failed: {:?}", e);
                        }
                    }
                }
                Ok(Err(e)) => {
                    eprintln!("OAuth: Google Health redirect listener error: {:?}", e);
                }
                Err(_) => {
                    println!("OAuth: Google Health redirect listener timed out after 5 minutes.");
                }
            }
        }
    }

    // 2. Google / Gmail Check
    if let (Ok(client_id), Ok(client_secret)) = (
        std::env::var("CHOTU_OAUTH_CLIENT_ID"),
        std::env::var("CHOTU_OAUTH_CLIENT_SECRET"),
    ) {
        if std::env::var("CHOTU_OAUTH_REFRESH_TOKEN").is_err() {
            let auth_url = format!(
                "https://accounts.google.com/o/oauth2/v2/auth?prompt=consent&access_type=offline&response_type=code&client_id={}&redirect_uri=http%3A%2F%2Flocalhost%3A8080%2Fcallback&scope=https%3A%2F%2Fmail.google.com%2F",
                client_id
            );

            println!("\n================================================================");
            println!("⚠️  GOOGLE GMAIL IMAP AUTHORIZATION REQUIRED");
            println!("Please open the following link in your browser to authorize:");
            println!("{}", auth_url);
            open_browser(&auth_url);
            println!("Starting temporary listener on port 8080...");
            println!("================================================================\n");

            // Listen with a 5-minute timeout
            let listener_res = tokio::time::timeout(
                std::time::Duration::from_secs(300),
                start_redirect_listener(8080),
            )
            .await;

            match listener_res {
                Ok(Ok(code)) => {
                    println!("OAuth: Received authorization code. Swapping for tokens...");
                    match exchange_google_code(
                        &client_id,
                        &client_secret,
                        &code,
                        "http://localhost:8080/callback",
                    )
                    .await
                    {
                        Ok(tokens) => {
                            save_google_refresh_token(&tokens.refresh_token)?;
                            println!("\n================================================================");
                            println!("✅ GOOGLE GMAIL AUTHORIZATION SUCCESSFUL!");
                            println!("Refresh token saved to .env. Gmail sync is now active!");
                            println!("================================================================\n");
                        }
                        Err(e) => {
                            eprintln!("OAuth: Google Gmail token exchange failed: {:?}", e);
                        }
                    }
                }
                Ok(Err(e)) => {
                    eprintln!("OAuth: Google Gmail redirect listener error: {:?}", e);
                }
                Err(_) => {
                    println!("OAuth: Google Gmail redirect listener timed out after 5 minutes.");
                }
            }
        }
    }

    Ok(())
}

fn open_browser(url: &str) {
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(url).spawn();
    #[cfg(target_os = "windows")]
    let _ = std::process::Command::new("cmd")
        .args(&["/C", "start", url])
        .spawn();
    #[cfg(target_os = "linux")]
    let _ = std::process::Command::new("xdg-open").arg(url).spawn();
}
