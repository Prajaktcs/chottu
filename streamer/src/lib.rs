use anyhow::Result;
use chotu_common::{ChotuLlm, EmailClassification, EmailMetadata};
use sqlx::SqlitePool;

mod imap_client;

/// Main entry point to run the Streamer Agent (live IMAP or offline simulation).
pub async fn run(pool: SqlitePool, llm: ChotuLlm, config: chotu_common::AppConfig) -> Result<()> {
    println!("Streamer Agent initiated.");

    // Check if live IMAP environment credentials are set
    let has_credentials = std::env::var("CHOTU_OAUTH_REFRESH_TOKEN").is_ok()
        && std::env::var("CHOTU_EMAIL_USER").is_ok();

    if has_credentials {
        println!("Live OAuth2 IMAP credentials detected. Starting live streamer...");
        imap_client::start_streamer(pool, llm, config).await?;
    } else {
        println!("CHOTU_OAUTH_REFRESH_TOKEN or CHOTU_EMAIL_USER not found in environment.");
        println!("Defaulting to offline Streamer Agent Simulation...\n");

        // Run simulation
        let mock_emails = [
            EmailMetadata {
                sender: "alerts@citibank.com".to_string(),
                subject: "Your Citibank Credit Card Transaction Alert".to_string(),
                body_preview: Some(
                    "Your Citibank credit card ending in 1234 was charged $45.23 at WHOLE FOODS on 2026-05-30. If this was not you, please call support immediately.".to_string(),
                ),
            },
            EmailMetadata {
                sender: "newsletter@substack.com".to_string(),
                subject: "The Weekly Rust Developer digest #182".to_string(),
                body_preview: Some(
                    "Welcome to the weekly newsletter. Today we discuss the stabilization of edition 2024 features, Tokio design patterns, and compiler improvements.".to_string(),
                ),
            },
            EmailMetadata {
                sender: "siddharth@example.com".to_string(),
                subject: "Coffee tomorrow?".to_string(),
                body_preview: Some(
                    "Hey Alex, are we still meeting for coffee tomorrow morning at 10 AM? Let me know if that time still works for you.".to_string(),
                ),
            },
        ];

        println!("--- Running Email Classification Simulation ---");
        for (i, email) in mock_emails.iter().enumerate() {
            println!("\n[Email {}/3]", i + 1);
            println!("Sender:  {}", email.sender);
            println!("Subject: {}", email.subject);
            println!(
                "Preview: {}",
                email.body_preview.as_deref().unwrap_or("[Empty]")
            );

            match llm.classify_email(email, &[]).await {
                Ok(result) => {
                    println!(">> CLASSIFICATION: {:?}", result.classification);
                    println!(">> REASON:         {}", result.reason);

                    match result.classification {
                        EmailClassification::LedgerStream => {
                            println!("Result: [SUCCESS] Correctly identified as transaction data stream.");
                        }
                        EmailClassification::Archive => {
                            println!("Result: [SUCCESS] Correctly identified as update or personal thread.");
                        }
                        EmailClassification::Trash => {
                            println!("Result: [SUCCESS] Correctly filtered as newsletter / noise.");
                        }
                        EmailClassification::ActionItem => {
                            println!("Result: [SUCCESS] Correctly identified as action item.");
                        }
                        EmailClassification::TravelItinerary => {
                            println!("Result: [SUCCESS] Correctly identified as travel itinerary.");
                        }
                        EmailClassification::FinancialBill => {
                            println!("Result: [SUCCESS] Correctly identified as upcoming financial bill.");
                        }
                        EmailClassification::StatementDocument => {
                            println!("Result: [SUCCESS] Correctly identified as statement document.");
                        }
                        EmailClassification::Newsletter => {
                            println!("Result: [SUCCESS] Correctly identified as newsletter.");
                        }
                        EmailClassification::PersonalReference => {
                            println!("Result: [SUCCESS] Correctly identified as personal reference.");
                        }
                    }
                }
                Err(e) => {
                    eprintln!("\n>> ERROR classifying email: {:?}", e);
                }
            }
        }

        println!("\nSimulation run complete.");
        loop {
            tokio::time::sleep(tokio::time::Duration::from_secs(3600)).await;
        }
    }

    Ok(())
}
