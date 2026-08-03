use anyhow::{Context, Result};
use chotu_common::{ChotuLlm, EmailMetadata};
use std::fs;
use std::path::Path;

#[derive(serde::Deserialize)]
struct TestCase {
    test_id: String,
    sender: String,
    subject: String,
    body_preview: String,
    expected_classification: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Load .env if it exists
    dotenvy::dotenv().ok();

    println!("Starting Chotu: Evaluation Framework...");

    let db_path = std::env::var("DATABASE_PATH").unwrap_or_else(|_| "chotu.db".to_string());
    println!("Connecting to SQLite database at: {}", db_path);

    let pool = chotu_common::init_db(&db_path).await?;
    println!("Database initialized and migrations ran successfully.");

    // Load golden dataset from evals/dataset.json
    let dataset_path = Path::new("evals/dataset.json");
    if !dataset_path.exists() {
        return Err(anyhow::anyhow!("Golden dataset not found at {:?}", dataset_path));
    }
    let dataset_str = fs::read_to_string(dataset_path).context("Failed to read golden dataset")?;
    let test_cases: Vec<TestCase> = serde_json::from_str(&dataset_str).context("Failed to deserialize golden dataset")?;
    println!("Loaded {} evaluation test cases.", test_cases.len());

    // Setup local LLM client
    let host = std::env::var("OLLAMA_HOST").unwrap_or_else(|_| "http://localhost".to_string());
    let port = std::env::var("OLLAMA_PORT")
        .unwrap_or_else(|_| "11434".to_string())
        .parse::<u16>()
        .unwrap_or(11434);
    let model = std::env::var("OLLAMA_MODEL").unwrap_or_else(|_| "qwen3.5:9b".to_string());
    println!("Initializing Ollama LLM client for evaluation: {}:{} / model: {}", host, port, model);
    let llm = ChotuLlm::new(&host, port, &model);

    let mut successes = 0;
    let total = test_cases.len();

    println!("\n--- Running Evaluation Run ---");
    for tc in &test_cases {
        let metadata = EmailMetadata {
            sender: tc.sender.clone(),
            subject: tc.subject.clone(),
            body_preview: Some(tc.body_preview.clone()),
        };

        print!("[{}] Expected: {}... ", tc.test_id, tc.expected_classification);
        std::io::Write::flush(&mut std::io::stdout())?;

        match llm.classify_email(&metadata, &[]).await {
            Ok(res) => {
                let actual_str = serde_json::to_value(&res.classification)?
                    .as_str()
                    .unwrap_or("")
                    .to_string();
                
                if actual_str == tc.expected_classification {
                    println!("MATCH (Reason: {})", res.reason);
                    successes += 1;
                } else {
                    println!("MISMATCH (Actual: {}, Reason: {})", actual_str, res.reason);
                }
            }
            Err(e) => {
                println!("ERROR ({:?})", e);
            }
        }
    }

    let triage_accuracy = if total > 0 {
        successes as f64 / total as f64
    } else {
        0.0
    };
    println!("\nEvaluation Run Completed.");
    println!("Triage Accuracy: {:.2}% ({}/{})", triage_accuracy * 100.0, successes, total);

    // Save validation log to database
    let eval_id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO evaluation_log (eval_id, test_timestamp, prompt_version, model_name, triage_accuracy, extraction_faithfulness) VALUES (?, ?, ?, ?, ?, ?)"
    )
    .bind(&eval_id)
    .bind(chrono::Utc::now())
    .bind("1.0.0")
    .bind(&model)
    .bind(triage_accuracy)
    .bind(1.0)
    .execute(&pool)
    .await?;
    println!("Evaluation run logged to database (ID: {}).", eval_id);

    // Enforce 85% accuracy baseline
    let threshold = 0.85;
    if triage_accuracy < threshold {
        return Err(anyhow::anyhow!(
            "REGRESSION BLOCKED: Triage accuracy ({:.2}%) fell below the baseline threshold ({:.2}%)!",
            triage_accuracy * 100.0,
            threshold * 100.0
        ));
    }
    
    println!("Triage accuracy meets requirements. Build passes!");

    Ok(())
}
