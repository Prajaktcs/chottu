//! Explicit, sender-scoped meal updates. Analysis remains serialized per conversation.
use super::{handle_food_photo, reject_foreign_food_mutation, send_signal, Bot, ChatId};
use chotu_common::{
    AppConfig, ChotuLlm, FoodLog, GeminiClient, NutritionEstimation, SignalAttachment, SignalError,
    SignalInbound, SignalRecipient,
};
use chrono::{Duration, Utc};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

const CONTEXT_MINUTES: i64 = 20;
const CHOICE_MINUTES: i64 = 15;

pub(super) struct FoodUpdateTarget {
    pub log: FoodLog,
    pub revision: i64,
    pub user_facts: String,
}

#[derive(Serialize, Deserialize)]
struct Candidate {
    id: String,
    revision: i64,
    description: String,
}

#[derive(sqlx::FromRow)]
struct PhotoChoice {
    prompt_timestamp: i64,
    attachment_id: String,
    content_type: String,
    caption: String,
    candidates_json: String,
    expires_at: chrono::DateTime<Utc>,
}

fn recipient(chat: &ChatId) -> (&'static str, &str) {
    match chat {
        SignalRecipient::Direct { aci } => ("direct", aci),
        SignalRecipient::Group { group_id } => ("group", group_id),
    }
}

fn failure(error: impl std::fmt::Display) -> SignalError {
    SignalError::Protocol(format!("Food steering: {error}"))
}

pub(super) async fn record_context(
    pool: &SqlitePool,
    chat: &ChatId,
    sender: &str,
    log_id: &str,
    user_facts: &str,
) -> Result<(), SignalError> {
    let (kind, recipient_id) = recipient(chat);
    sqlx::query("INSERT INTO food_signal_context (food_log_id, recipient_kind, recipient_id, sender_aci, user_facts, logged_at) VALUES (?, ?, ?, ?, ?, ?) ON CONFLICT(food_log_id) DO UPDATE SET user_facts = excluded.user_facts, logged_at = excluded.logged_at")
        .bind(log_id).bind(kind).bind(recipient_id).bind(sender).bind(user_facts).bind(Utc::now())
        .execute(pool).await.map_err(failure)?;
    Ok(())
}

pub(super) async fn record_confirmation(
    pool: &SqlitePool,
    chat: &ChatId,
    log_id: &str,
    timestamp: i64,
) -> Result<(), SignalError> {
    let (kind, recipient_id) = recipient(chat);
    sqlx::query("INSERT INTO food_signal_messages (recipient_kind, recipient_id, message_timestamp, food_log_id) VALUES (?, ?, ?, ?)")
        .bind(kind).bind(recipient_id).bind(timestamp).bind(log_id).execute(pool).await.map_err(failure)?;
    Ok(())
}

async fn target(
    pool: &SqlitePool,
    chat: &ChatId,
    sender: &str,
    id: &str,
    config: &AppConfig,
) -> Result<Option<FoodUpdateTarget>, SignalError> {
    let (kind, recipient_id) = recipient(chat);
    let context: Option<(String, String, String, String)> = sqlx::query_as("SELECT recipient_kind, recipient_id, sender_aci, user_facts FROM food_signal_context WHERE food_log_id = ?")
        .bind(id).fetch_optional(pool).await.map_err(failure)?;
    if let Some((saved_kind, saved_recipient, saved_sender, _)) = &context {
        if saved_kind != kind || saved_recipient != recipient_id || saved_sender != sender {
            return Ok(None);
        }
    } else if chat.group_id().is_some() {
        // Historical group entries have no trustworthy originating sender.
        return Ok(None);
    }
    let log: Option<FoodLog> = sqlx::query_as("SELECT * FROM food_log WHERE id = ?")
        .bind(id)
        .fetch_optional(pool)
        .await
        .map_err(failure)?;
    let Some(log) = log else { return Ok(None) };
    if chotu_common::ensure_food_mutation_allowed(config, chat.lookup_aci(), &log.family_member_id)
        .is_err()
    {
        return Ok(None);
    }
    let revision = health_coach::food_log_revision(pool, id)
        .await
        .map_err(failure)?;
    Ok(Some(FoodUpdateTarget {
        log,
        revision,
        user_facts: context.map(|c| c.3).unwrap_or_default(),
    }))
}

async fn quoted_id(
    pool: &SqlitePool,
    chat: &ChatId,
    timestamp: i64,
) -> Result<Option<String>, SignalError> {
    let (kind, recipient_id) = recipient(chat);
    sqlx::query_scalar("SELECT food_log_id FROM food_signal_messages WHERE recipient_kind = ? AND recipient_id = ? AND message_timestamp = ?")
        .bind(kind).bind(recipient_id).bind(timestamp).fetch_optional(pool).await.map_err(failure)
}

async fn recent_candidates(
    pool: &SqlitePool,
    inbound: &SignalInbound,
) -> Result<Vec<Candidate>, SignalError> {
    let (kind, recipient_id) = recipient(&inbound.recipient);
    let rows: Vec<(String, String)> = sqlx::query_as("SELECT f.id, f.raw_text_description FROM food_signal_context c JOIN food_log f ON f.id = c.food_log_id WHERE c.recipient_kind = ? AND c.recipient_id = ? AND c.sender_aci = ? AND julianday(c.logged_at) >= julianday(?) ORDER BY c.logged_at DESC LIMIT 4")
        .bind(kind).bind(recipient_id).bind(&inbound.sender_aci).bind(Utc::now() - Duration::minutes(CONTEXT_MINUTES))
        .fetch_all(pool).await.map_err(failure)?;
    let mut candidates = Vec::with_capacity(rows.len());
    for (id, description) in rows {
        let revision = health_coach::food_log_revision(pool, &id)
            .await
            .map_err(failure)?;
        candidates.push(Candidate {
            id,
            revision,
            description,
        });
    }
    Ok(candidates)
}

fn candidate_list(candidates: &[Candidate]) -> String {
    candidates
        .iter()
        .map(|c| format!("{}: {}", &c.id[..8.min(c.id.len())], c.description))
        .collect::<Vec<_>>()
        .join("\n")
}

fn caption(inbound: &SignalInbound) -> &str {
    inbound
        .text
        .as_deref()
        .filter(|text| !text.trim().is_empty())
        .or_else(|| {
            inbound
                .attachments
                .first()
                .and_then(|a| a.caption.as_deref())
        })
        .unwrap_or("")
        .trim()
}

fn correction_cue(text: &str) -> bool {
    let text = text.trim().to_ascii_lowercase().replace('’', "'");
    [
        "correction:",
        "actually,",
        "actually ",
        "it's not ",
        "it is not ",
        "that's not ",
        "that is not ",
        "it was not ",
        "that was not ",
        "not egg",
        "no, ",
        "correct that ",
    ]
    .iter()
    .any(|prefix| text.starts_with(prefix))
}

fn acknowledgment(text: &str) -> bool {
    matches!(
        text.trim().to_ascii_lowercase().as_str(),
        "thanks" | "thank you" | "ok" | "okay" | "got it" | "great"
    )
}

fn selected_candidate<'a>(candidates: &'a [Candidate], prefix: &str) -> Option<&'a Candidate> {
    if prefix.is_empty() {
        return (candidates.len() == 1).then(|| &candidates[0]);
    }
    if prefix.len() < 8 {
        return None;
    }
    let mut matches = candidates
        .iter()
        .filter(|candidate| candidate.id.starts_with(prefix));
    let first = matches.next()?;
    matches.next().is_none().then_some(first)
}

async fn remove_choice(pool: &SqlitePool, inbound: &SignalInbound) -> Result<(), SignalError> {
    let (kind, recipient_id) = recipient(&inbound.recipient);
    sqlx::query("DELETE FROM food_photo_choices WHERE recipient_kind = ? AND recipient_id = ? AND sender_aci = ?")
        .bind(kind).bind(recipient_id).bind(&inbound.sender_aci).execute(pool).await.map_err(failure)?;
    Ok(())
}

/// Return true only when this message belongs to an explicit steering flow.
pub(super) async fn handle(
    bot: &Bot,
    inbound: &SignalInbound,
    pool: &SqlitePool,
    llm: &ChotuLlm,
    gemini: &GeminiClient,
    config: &AppConfig,
    allow_unquoted_correction: bool,
) -> Result<bool, SignalError> {
    let chat = &inbound.recipient;
    let text = caption(inbound);
    let image_count = inbound
        .attachments
        .iter()
        .filter(|a| a.content_type.starts_with("image/"))
        .count();
    if image_count > 1 {
        send_signal(
            bot,
            chat,
            "Send one meal photo at a time so I don't silently omit part of your meal.",
        )
        .await?;
        return Ok(true);
    }
    let quoted = match inbound.quote_timestamp {
        Some(timestamp) => quoted_id(pool, chat, timestamp).await?,
        None => None,
    };
    if image_count == 1 {
        remove_choice(pool, inbound).await?;
        // /food explicitly starts a new meal, even when sent as a reply.
        if super::strip_leading_food_command(text) != text {
            return Ok(false);
        }
        if let Some(id) = quoted {
            let Some(target) = target(pool, chat, &inbound.sender_aci, &id, config).await? else {
                send_signal(bot, chat, "That meal is deleted or isn't available to you in this conversation. Nothing was logged.").await?;
                return Ok(true);
            };
            remove_choice(pool, inbound).await?;
            handle_food_photo(bot, chat, inbound, pool, llm, gemini, config, Some(target)).await?;
            return Ok(true);
        }
        if inbound.quote_timestamp.is_some() {
            send_signal(bot, chat, "That isn't a meal confirmation. Reply to a meal confirmation to update it, or caption the photo with `/food ...` to log a new meal.").await?;
            return Ok(true);
        }
        let candidates = recent_candidates(pool, inbound).await?;
        if candidates.is_empty() {
            return Ok(false);
        }
        let list = candidate_list(&candidates);
        let prompt = if candidates.len() == 1 {
            format!("Use this photo to update your recently logged meal, or log a new meal?\n{list}\nReply `update`, `new`, or `cancel`. Nothing from this photo is logged yet.")
        } else {
            format!("Which meal does this photo belong to?\n{list}\nReply `update <meal id>`, `new`, or `cancel`. Nothing from this photo is logged yet.")
        };
        let timestamp = send_signal(bot, chat, prompt).await?;
        let attachment = &inbound.attachments[0];
        let (kind, recipient_id) = recipient(chat);
        sqlx::query("INSERT INTO food_photo_choices (recipient_kind, recipient_id, sender_aci, prompt_timestamp, attachment_id, content_type, caption, candidates_json, expires_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?) ON CONFLICT(recipient_kind, recipient_id, sender_aci) DO UPDATE SET prompt_timestamp = excluded.prompt_timestamp, attachment_id = excluded.attachment_id, content_type = excluded.content_type, caption = excluded.caption, candidates_json = excluded.candidates_json, expires_at = excluded.expires_at")
            .bind(kind).bind(recipient_id).bind(&inbound.sender_aci).bind(timestamp).bind(&attachment.id)
            .bind(&attachment.content_type).bind(text).bind(serde_json::to_string(&candidates).map_err(failure)?)
            .bind(Utc::now() + Duration::minutes(CHOICE_MINUTES)).execute(pool).await.map_err(failure)?;
        return Ok(true);
    }
    let (action, argument) = text.split_once(char::is_whitespace).unwrap_or((text, ""));
    if matches!(
        action.to_ascii_lowercase().as_str(),
        "update" | "new" | "cancel"
    ) {
        let (kind, recipient_id) = recipient(chat);
        let pending: Option<PhotoChoice> = sqlx::query_as("SELECT prompt_timestamp, attachment_id, content_type, caption, candidates_json, expires_at FROM food_photo_choices WHERE recipient_kind = ? AND recipient_id = ? AND sender_aci = ?")
            .bind(kind).bind(recipient_id).bind(&inbound.sender_aci).fetch_optional(pool).await.map_err(failure)?;
        if let Some(pending) = pending {
            if inbound
                .quote_timestamp
                .is_some_and(|ts| ts != pending.prompt_timestamp)
            {
                send_signal(bot, chat, "That reply is for an older question. Reply to the latest photo question; nothing was changed.").await?;
                return Ok(true);
            }
            if pending.expires_at < Utc::now() {
                remove_choice(pool, inbound).await?;
                send_signal(bot, chat, "That photo choice expired. Resend the photo with a caption or reply to the meal confirmation.").await?;
                return Ok(true);
            }
            if action.eq_ignore_ascii_case("cancel") {
                remove_choice(pool, inbound).await?;
                send_signal(
                    bot,
                    chat,
                    "Cancelled that photo. Your existing meals are unchanged.",
                )
                .await?;
                return Ok(true);
            }
            if action.eq_ignore_ascii_case("new") && !argument.trim().is_empty() {
                send_signal(
                    bot,
                    chat,
                    "Reply `new` to log the captured photo, or `cancel` to discard it.",
                )
                .await?;
                return Ok(true);
            }
            let update_target = if action.eq_ignore_ascii_case("update") {
                let candidates: Vec<Candidate> =
                    serde_json::from_str(&pending.candidates_json).map_err(failure)?;
                let Some(candidate) = selected_candidate(&candidates, argument.trim()) else {
                    send_signal(
                        bot,
                        chat,
                        format!(
                            "Choose one meal with `update <meal id>`:\n{}",
                            candidate_list(&candidates)
                        ),
                    )
                    .await?;
                    return Ok(true);
                };
                let Some(target) =
                    target(pool, chat, &inbound.sender_aci, &candidate.id, config).await?
                else {
                    remove_choice(pool, inbound).await?;
                    send_signal(
                        bot,
                        chat,
                        "That meal is no longer available. Resend the photo; nothing was logged.",
                    )
                    .await?;
                    return Ok(true);
                };
                if target.revision != candidate.revision {
                    remove_choice(pool, inbound).await?;
                    send_signal(bot, chat, "That meal changed since I asked. Reply to its latest confirmation with the photo instead.").await?;
                    return Ok(true);
                }
                Some(target)
            } else {
                None
            };
            // Consume the choice before any analysis; repeated replies cannot log twice.
            remove_choice(pool, inbound).await?;
            let photo = SignalInbound {
                sender_aci: inbound.sender_aci.clone(),
                recipient: chat.clone(),
                text: Some(pending.caption),
                quote_timestamp: None,
                attachments: vec![SignalAttachment {
                    id: pending.attachment_id,
                    content_type: pending.content_type,
                    size: None,
                    caption: None,
                }],
            };
            handle_food_photo(bot, chat, &photo, pool, llm, gemini, config, update_target).await?;
            return Ok(true);
        }
        if action.eq_ignore_ascii_case("update")
            || action.eq_ignore_ascii_case("cancel")
            || text.eq_ignore_ascii_case("new")
        {
            send_signal(
                bot,
                chat,
                "No pending photo choice. Resend the photo or reply to a meal confirmation.",
            )
            .await?;
            return Ok(true);
        }
    }
    if quoted.is_some() && acknowledgment(text) {
        return Ok(true);
    }
    if quoted.is_some() && text.ends_with('?') {
        send_signal(bot, chat, "Nothing changed. To correct this meal, reply with the ingredient or portion correction; use /status to review totals.").await?;
        return Ok(true);
    }
    let command_args = action
        .eq_ignore_ascii_case("/correctfood")
        .then_some(argument);
    if quoted.is_none() && !allow_unquoted_correction && command_args.is_none() {
        return Ok(false);
    }
    if command_args.is_none()
        && !correction_cue(text)
        && !(quoted.is_some() && !acknowledgment(text) && !text.starts_with('/'))
    {
        return Ok(false);
    }
    let correction = command_args.unwrap_or(text).trim();
    let (first, rest) = correction
        .split_once(char::is_whitespace)
        .unwrap_or((correction, ""));
    let explicit_id = first.len() >= 8 && first.chars().all(|c| c.is_ascii_hexdigit() || c == '-');
    let correction = if explicit_id { rest.trim() } else { correction };
    if correction.is_empty() {
        send_signal(bot, chat, "Reply to a meal confirmation with the correction, or send `/correctfood <meal id> <correction>`.").await?;
        return Ok(true);
    }
    if explicit_id && quoted.as_ref().is_some_and(|id| !id.starts_with(first)) {
        send_signal(bot, chat, "The meal id and replied-to confirmation identify different meals. Choose one target; nothing changed.").await?;
        return Ok(true);
    }
    let id = if explicit_id {
        let (kind, recipient_id) = recipient(chat);
        let ids: Vec<String> = if chat.group_id().is_none() {
            let member = chotu_common::default_member_id(config, chat.lookup_aci());
            sqlx::query_scalar("SELECT id FROM food_log WHERE family_member_id = ? AND substr(id, 1, length(?)) = ? LIMIT 2")
                .bind(member).bind(first).bind(first).fetch_all(pool).await.map_err(failure)?
        } else {
            sqlx::query_scalar("SELECT food_log_id FROM food_signal_context WHERE recipient_kind = ? AND recipient_id = ? AND sender_aci = ? AND substr(food_log_id, 1, length(?)) = ? LIMIT 2")
                .bind(kind).bind(recipient_id).bind(&inbound.sender_aci).bind(first).bind(first).fetch_all(pool).await.map_err(failure)?
        };
        if ids.len() != 1 {
            send_signal(bot, chat, "That meal id is missing, ambiguous, or unavailable to you. Reply to its confirmation instead.").await?;
            return Ok(true);
        }
        ids.into_iter().next().unwrap()
    } else if let Some(id) = quoted {
        id
    } else {
        let candidates = recent_candidates(pool, inbound).await?;
        if candidates.len() != 1 {
            send_signal(bot, chat, format!("Which meal should I correct? Reply to its confirmation, or use `/correctfood <meal id> <correction>`.\n{}", candidate_list(&candidates))).await?;
            return Ok(true);
        }
        candidates.into_iter().next().unwrap().id
    };
    let Some(target) = target(pool, chat, &inbound.sender_aci, &id, config).await? else {
        send_signal(
            bot,
            chat,
            "That meal is deleted or unavailable to you. Nothing was changed.",
        )
        .await?;
        return Ok(true);
    };
    send_signal(
        bot,
        chat,
        format!(
            "Updating meal {} — not adding another entry…",
            &id[..8.min(id.len())]
        ),
    )
    .await?;
    let original = format!(
        "Current meal: {}\nKnown user facts (later corrections take precedence): {}",
        target.log.raw_text_description, target.user_facts
    );
    let analysis = match gemini.correct_food_estimation(&original, correction).await {
        Ok(analysis) => analysis,
        Err(error) => {
            send_signal(
                bot,
                chat,
                format!("Could not estimate that correction: {error}. Your meal is unchanged."),
            )
            .await?;
            return Ok(true);
        }
    };
    if let Some(question) = analysis.clarification {
        let question = if question.trim().is_empty() {
            "Please clarify which ingredient or portion should change."
        } else {
            &question
        };
        send_signal(bot, chat, format!("{question}\nNothing changed. Reply to the meal confirmation with the clarified correction.")).await?;
        return Ok(true);
    }
    if analysis.description.trim().is_empty() {
        send_signal(
            bot,
            chat,
            "The correction produced no meal description. Your meal is unchanged.",
        )
        .await?;
        return Ok(true);
    }
    let facts = format!("{}\nCorrection: {}", target.user_facts, correction);
    let tag_context = format!("{}\nCorrection: {}", analysis.description, correction);
    save_update(
        bot,
        chat,
        &inbound.sender_aci,
        pool,
        config,
        target,
        &analysis.description,
        &analysis.nutrition,
        &facts,
        &tag_context,
    )
    .await?;
    Ok(true)
}

pub(super) async fn save_update(
    bot: &Bot,
    chat: &ChatId,
    sender: &str,
    pool: &SqlitePool,
    config: &AppConfig,
    target: FoodUpdateTarget,
    description: &str,
    nutrition: &NutritionEstimation,
    facts: &str,
    tag_context: &str,
) -> Result<(), SignalError> {
    if reject_foreign_food_mutation(bot, chat, config, &target.log.family_member_id).await? {
        return Ok(());
    }
    if let Err(error) = health_coach::revise_food_log(
        pool,
        config,
        &target.log,
        target.revision,
        description,
        nutrition,
        tag_context,
    )
    .await
    {
        send_signal(bot, chat, format!("Could not update that meal: {error}. No correction was saved; reply to its latest confirmation to retry.")).await?;
        return Ok(());
    }
    record_context(pool, chat, sender, &target.log.id, facts).await?;
    let sync_note =
        if health_coach::member_health_credentials_configured(&target.log.family_member_id, config)
        {
            match health_coach::sync_corrected_food_log(pool, config, &target.log.id).await {
                Ok(()) => "Synced to Google Health.",
                Err(error) => {
                    eprintln!("Corrected meal sync pending: {error:?}");
                    "Updated locally; Google Health replacement pending. Retry with /sync."
                }
            }
        } else {
            "Saved locally."
        };
    let date = target
        .log
        .timestamp
        .with_timezone(&config.resolved_tz())
        .format("%Y-%m-%d")
        .to_string();
    let totals: Option<(i32, f64, f64, f64)> = sqlx::query_as("SELECT total_calories_ingested, protein_grams, carbs_grams, fats_grams FROM health_family_summary WHERE family_member_id = ? AND date = ?")
        .bind(&target.log.family_member_id).bind(&date).fetch_optional(pool).await.map_err(failure)?;
    let totals_note = totals
        .map(|(kcal, p, c, f)| format!("\n{date}: {kcal} kcal · {p:.0}g P / {c:.0}g C / {f:.0}g F"))
        .unwrap_or_default();
    let flags = match health_coach::conditions::pending_food_flags(
        pool,
        config,
        super::private_health_member_id(config, chat),
        &target.log.family_member_id,
        &date,
        &target.log.id,
    )
    .await
    {
        Ok(flags) => flags,
        Err(error) => {
            eprintln!("Corrected food watchlist flags unavailable: {error:?}");
            Vec::new()
        }
    };
    let mut message = format!("Updated meal {} for {}: {}\n{} kcal · {:.1}g P / {:.1}g C / {:.1}g F{}\nOriginal meal time and unaffected ingredients/portions retained; no extra entry.\n{}", &target.log.id[..8.min(target.log.id.len())], target.log.family_member_id, description, nutrition.total_calories, nutrition.protein_grams, nutrition.carbs_grams, nutrition.fats_grams, totals_note, sync_note);
    for flag in &flags {
        message.push_str(&flag.confirmation_line());
    }
    let timestamp = send_signal(bot, chat, message).await?;
    record_confirmation(pool, chat, &target.log.id, timestamp).await?;
    if !flags.is_empty() {
        if let Err(error) = health_coach::conditions::mark_food_flags_sent(
            pool,
            &target.log.family_member_id,
            &date,
            &flags,
        )
        .await
        {
            eprintln!("Could not record corrected food flag delivery: {error:?}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn pool() -> (tempfile::TempDir, SqlitePool) {
        let dir = tempfile::tempdir().unwrap();
        let pool = chotu_common::init_db(dir.path().join("steering.sqlite").to_str().unwrap())
            .await
            .unwrap();
        (dir, pool)
    }

    async fn seed(pool: &SqlitePool, id: &str, member: &str) {
        sqlx::query("INSERT INTO food_log (id, timestamp, family_member_id, raw_text_description, estimated_calories) VALUES (?, '2026-09-01T18:00:00Z', ?, 'paneer bhurji with cream, half bowl', 400)")
            .bind(id).bind(member).execute(pool).await.unwrap();
    }

    fn group_message(sender: &str, text: &str) -> SignalInbound {
        SignalInbound {
            sender_aci: sender.into(),
            recipient: SignalRecipient::Group {
                group_id: "household".into(),
            },
            text: Some(text.into()),
            quote_timestamp: None,
            attachments: Vec::new(),
        }
    }

    #[tokio::test]
    async fn recent_context_uses_sender_and_interaction_time_not_meal_time() {
        let (_dir, pool) = pool().await;
        seed(&pool, "aaaaaaaa-one", "alex").await;
        seed(&pool, "bbbbbbbb-two", "jordan").await;
        let alice = group_message("aci-alex", "");
        record_context(
            &pool,
            &alice.recipient,
            "aci-alex",
            "aaaaaaaa-one",
            "half bowl",
        )
        .await
        .unwrap();
        record_context(
            &pool,
            &alice.recipient,
            "aci-jordan",
            "bbbbbbbb-two",
            "full bowl",
        )
        .await
        .unwrap();
        let candidates = recent_candidates(&pool, &alice).await.unwrap();
        assert_eq!(
            candidates.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
            ["aaaaaaaa-one"]
        );
        assert!(target(
            &pool,
            &alice.recipient,
            "aci-jordan",
            "aaaaaaaa-one",
            &AppConfig::default()
        )
        .await
        .unwrap()
        .is_none());
        sqlx::query(
            "UPDATE food_signal_context SET logged_at = ? WHERE food_log_id = 'aaaaaaaa-one'",
        )
        .bind(Utc::now() - Duration::minutes(CONTEXT_MINUTES + 1))
        .execute(&pool)
        .await
        .unwrap();
        assert!(recent_candidates(&pool, &alice).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn deleted_confirmation_keeps_a_non_resurrecting_target() {
        let (_dir, pool) = pool().await;
        seed(&pool, "aaaaaaaa-one", "alex").await;
        let message = group_message("aci-alex", "");
        record_context(
            &pool,
            &message.recipient,
            "aci-alex",
            "aaaaaaaa-one",
            "half bowl",
        )
        .await
        .unwrap();
        record_confirmation(&pool, &message.recipient, "aaaaaaaa-one", 42)
            .await
            .unwrap();
        sqlx::query("DELETE FROM food_log WHERE id = 'aaaaaaaa-one'")
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(
            quoted_id(&pool, &message.recipient, 42)
                .await
                .unwrap()
                .as_deref(),
            Some("aaaaaaaa-one")
        );
        assert!(target(
            &pool,
            &message.recipient,
            "aci-alex",
            "aaaaaaaa-one",
            &AppConfig::default()
        )
        .await
        .unwrap()
        .is_none());
    }

    #[tokio::test]
    async fn photo_choice_requires_its_sender_and_cancel_preserves_existing_meal() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let (dir, pool) = pool().await;
        seed(&pool, "aaaaaaaa-one", "alex").await;
        let mut photo = group_message("aci-alex", "");
        record_context(
            &pool,
            &photo.recipient,
            "aci-alex",
            "aaaaaaaa-one",
            "half bowl",
        )
        .await
        .unwrap();
        photo.attachments.push(SignalAttachment {
            id: "photo-1".into(),
            content_type: "image/jpeg".into(),
            size: None,
            caption: None,
        });
        let socket = dir.path().join("signal.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (reader, mut writer) = stream.into_split();
            let mut lines = BufReader::new(reader).lines();
            let mut timestamp = 100;
            while let Some(line) = lines.next_line().await.unwrap() {
                let request: serde_json::Value = serde_json::from_str(&line).unwrap();
                assert_eq!(
                    request["method"], "send",
                    "An unconfirmed choice must never download/analyze the photo"
                );
                timestamp += 1;
                writer.write_all(format!("{}\n", serde_json::json!({"jsonrpc":"2.0", "id":request["id"], "result":{"timestamp":timestamp}})).as_bytes()).await.unwrap();
            }
        });
        let bot = chotu_common::SignalClient::connect(&socket).await.unwrap();
        let llm = ChotuLlm::new("http://127.0.0.1", 1, "unused");
        let gemini = GeminiClient::new("unused".into());
        let config = AppConfig::default();
        assert!(handle(&bot, &photo, &pool, &llm, &gemini, &config, true)
            .await
            .unwrap());
        let choice_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM food_photo_choices")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(choice_count, 1);
        assert!(handle(
            &bot,
            &group_message("aci-jordan", "update"),
            &pool,
            &llm,
            &gemini,
            &config,
            true
        )
        .await
        .unwrap());
        let choice_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM food_photo_choices")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(choice_count, 1);
        assert!(handle(
            &bot,
            &group_message("aci-alex", "cancel"),
            &pool,
            &llm,
            &gemini,
            &config,
            true
        )
        .await
        .unwrap());
        let choice_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM food_photo_choices")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(choice_count, 0);
        assert!(handle(&bot, &photo, &pool, &llm, &gemini, &config, true)
            .await
            .unwrap());
        sqlx::query("UPDATE food_photo_choices SET expires_at = ?")
            .bind(Utc::now() - Duration::seconds(1))
            .execute(&pool)
            .await
            .unwrap();
        assert!(handle(
            &bot,
            &group_message("aci-alex", "update"),
            &pool,
            &llm,
            &gemini,
            &config,
            true
        )
        .await
        .unwrap());
        let choice_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM food_photo_choices")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(choice_count, 0);
        assert!(handle(&bot, &photo, &pool, &llm, &gemini, &config, true)
            .await
            .unwrap());
        let mut old_reply = group_message("aci-alex", "cancel");
        old_reply.quote_timestamp = Some(101);
        assert!(
            handle(&bot, &old_reply, &pool, &llm, &gemini, &config, true)
                .await
                .unwrap()
        );
        let choice_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM food_photo_choices")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(choice_count, 1);
        assert!(handle(
            &bot,
            &group_message("aci-alex", "cancel"),
            &pool,
            &llm,
            &gemini,
            &config,
            true
        )
        .await
        .unwrap());
        let meals: Vec<(String, String, i32)> =
            sqlx::query_as("SELECT id, raw_text_description, estimated_calories FROM food_log")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(
            meals,
            [(
                "aaaaaaaa-one".into(),
                "paneer bhurji with cream, half bowl".into(),
                400
            )]
        );
        server.abort();
    }

    #[test]
    fn ambiguous_selection_never_defaults_to_last_meal() {
        let candidates = vec![
            Candidate {
                id: "12345678-one".into(),
                revision: 0,
                description: "dinner".into(),
            },
            Candidate {
                id: "12345678-two".into(),
                revision: 0,
                description: "breakfast".into(),
            },
        ];
        assert!(selected_candidate(&candidates, "").is_none());
        assert!(selected_candidate(&candidates, "12345678").is_none());
        assert_eq!(
            selected_candidate(&candidates, "12345678-one")
                .unwrap()
                .description,
            "dinner"
        );
    }

    #[test]
    fn corrections_are_not_generic_food_descriptions_or_acknowledgments() {
        assert!(correction_cue("it's not egg, it is paneer"));
        assert!(correction_cue("Actually, I ate half"));
        assert!(!correction_cue("paneer bhurji with cream"));
        assert!(!correction_cue("I had eggs for breakfast"));
        assert!(acknowledgment("Thanks"));
    }
}
