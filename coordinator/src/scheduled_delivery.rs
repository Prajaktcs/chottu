use std::future::Future;

use chotu_common::{ChatAddress, ChatProvider, ConversationKind};
use sqlx::SqlitePool;

const RETRY_DELAY_SECONDS: i64 = 5 * 60;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum ScheduledJob {
    MorningBrief,
    Portfolio,
    Reflection,
}

impl ScheduledJob {
    fn as_str(self) -> &'static str {
        match self {
            Self::MorningBrief => "morning_brief",
            Self::Portfolio => "portfolio",
            Self::Reflection => "reflection",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DeliveryOutcome {
    Delivered,
    Retry,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PendingDelivery {
    pub(crate) local_date: String,
    pub(crate) recipient: ChatAddress,
}

fn recipient_from_parts(
    provider: ChatProvider,
    kind: &str,
    id: String,
) -> Result<ChatAddress, sqlx::Error> {
    let kind = kind.parse::<ConversationKind>().map_err(|error| {
        sqlx::Error::Protocol(format!("invalid scheduled chat recipient kind: {error}"))
    })?;
    Ok(ChatAddress { provider, kind, id })
}

async fn register_recipients(
    pool: &SqlitePool,
    job: ScheduledJob,
    local_date: &str,
    provider: ChatProvider,
    recipients: &[ChatAddress],
) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query(
        "DELETE FROM scheduled_chat_deliveries \
         WHERE job = ? AND provider = ? AND local_date <> ? AND delivered_at IS NULL",
    )
    .bind(job.as_str())
    .bind(provider.as_str())
    .bind(local_date)
    .execute(&mut *tx)
    .await?;
    if provider == ChatProvider::Signal {
        sqlx::query(
            "DELETE FROM scheduled_signal_deliveries \
             WHERE job = ? AND local_date <> ? AND delivered_at IS NULL",
        )
        .bind(job.as_str())
        .bind(local_date)
        .execute(&mut *tx)
        .await?;
    }
    for recipient in recipients {
        sqlx::query(
            "INSERT OR IGNORE INTO scheduled_chat_deliveries \
             (job, local_date, provider, conversation_kind, conversation_id) \
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(job.as_str())
        .bind(local_date)
        .bind(recipient.provider.as_str())
        .bind(recipient.kind.as_str())
        .bind(&recipient.id)
        .execute(&mut *tx)
        .await?;
        if provider == ChatProvider::Signal {
            sqlx::query(
                "INSERT OR IGNORE INTO scheduled_signal_deliveries \
                 (job, local_date, recipient_kind, recipient_id) VALUES (?, ?, ?, ?)",
            )
            .bind(job.as_str())
            .bind(local_date)
            .bind(recipient.kind.as_str())
            .bind(&recipient.id)
            .execute(&mut *tx)
            .await?;
        }
    }
    tx.commit().await?;
    Ok(())
}

/// Registers current targets when the schedule fires, then returns authorized due recipients.
pub(crate) async fn due_recipients(
    pool: &SqlitePool,
    job: ScheduledJob,
    local_date: &str,
    provider: ChatProvider,
    current_targets: &[ChatAddress],
    schedule_matches: bool,
    now_epoch: i64,
) -> Result<Vec<PendingDelivery>, sqlx::Error> {
    if schedule_matches && !current_targets.is_empty() {
        register_recipients(pool, job, local_date, provider, current_targets).await?;
    }

    let rows: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT local_date, conversation_kind, conversation_id \
         FROM scheduled_chat_deliveries \
         WHERE job = ? AND provider = ? AND delivered_at IS NULL \
           AND (retry_after_epoch IS NULL OR retry_after_epoch <= ?) \
         ORDER BY local_date, conversation_kind, conversation_id",
    )
    .bind(job.as_str())
    .bind(provider.as_str())
    .bind(now_epoch)
    .fetch_all(pool)
    .await?;

    rows.into_iter()
        .map(|(date, kind, id)| {
            recipient_from_parts(provider, &kind, id).map(|recipient| PendingDelivery {
                local_date: date,
                recipient,
            })
        })
        .filter_map(|delivery| match delivery {
            Ok(delivery) if current_targets.contains(&delivery.recipient) => Some(Ok(delivery)),
            Ok(_) => None,
            Err(error) => Some(Err(error)),
        })
        .collect()
}

async fn record_outcome(
    pool: &SqlitePool,
    job: ScheduledJob,
    local_date: &str,
    recipient: &ChatAddress,
    outcome: DeliveryOutcome,
    attempt_finished_at: i64,
) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    match outcome {
        DeliveryOutcome::Delivered => {
            sqlx::query(
                "UPDATE scheduled_chat_deliveries \
                 SET delivered_at = CURRENT_TIMESTAMP, retry_after_epoch = NULL \
                 WHERE job = ? AND local_date = ? AND provider = ? \
                   AND conversation_kind = ? AND conversation_id = ?",
            )
            .bind(job.as_str())
            .bind(local_date)
            .bind(recipient.provider.as_str())
            .bind(recipient.kind.as_str())
            .bind(&recipient.id)
            .execute(&mut *tx)
            .await?;
            if recipient.provider == ChatProvider::Signal {
                sqlx::query(
                    "UPDATE scheduled_signal_deliveries \
                     SET delivered_at = CURRENT_TIMESTAMP, retry_after_epoch = NULL \
                     WHERE job = ? AND local_date = ? \
                       AND recipient_kind = ? AND recipient_id = ?",
                )
                .bind(job.as_str())
                .bind(local_date)
                .bind(recipient.kind.as_str())
                .bind(&recipient.id)
                .execute(&mut *tx)
                .await?;
            }
        }
        DeliveryOutcome::Retry => {
            let retry_after = attempt_finished_at + RETRY_DELAY_SECONDS;
            sqlx::query(
                "UPDATE scheduled_chat_deliveries SET retry_after_epoch = ? \
                 WHERE job = ? AND local_date = ? AND provider = ? \
                   AND conversation_kind = ? AND conversation_id = ? \
                   AND delivered_at IS NULL",
            )
            .bind(retry_after)
            .bind(job.as_str())
            .bind(local_date)
            .bind(recipient.provider.as_str())
            .bind(recipient.kind.as_str())
            .bind(&recipient.id)
            .execute(&mut *tx)
            .await?;
            if recipient.provider == ChatProvider::Signal {
                sqlx::query(
                    "UPDATE scheduled_signal_deliveries SET retry_after_epoch = ? \
                     WHERE job = ? AND local_date = ? \
                       AND recipient_kind = ? AND recipient_id = ? AND delivered_at IS NULL",
                )
                .bind(retry_after)
                .bind(job.as_str())
                .bind(local_date)
                .bind(recipient.kind.as_str())
                .bind(&recipient.id)
                .execute(&mut *tx)
                .await?;
            }
        }
    }
    tx.commit().await?;
    Ok(())
}

pub(crate) async fn deliver_recipients<F, Fut>(
    pool: &SqlitePool,
    job: ScheduledJob,
    recipients: Vec<PendingDelivery>,
    mut deliver: F,
) -> Result<(), sqlx::Error>
where
    F: FnMut(PendingDelivery) -> Fut,
    Fut: Future<Output = DeliveryOutcome>,
{
    for delivery in recipients {
        let outcome = deliver(delivery.clone()).await;
        record_outcome(
            pool,
            job,
            &delivery.local_date,
            &delivery.recipient,
            outcome,
            chrono::Utc::now().timestamp(),
        )
        .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;
    use std::cell::RefCell;

    async fn delivery_pool() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::query(
            "CREATE TABLE scheduled_chat_deliveries (
                job TEXT NOT NULL,
                local_date TEXT NOT NULL,
                provider TEXT NOT NULL,
                conversation_kind TEXT NOT NULL,
                conversation_id TEXT NOT NULL,
                delivered_at TEXT,
                retry_after_epoch INTEGER,
                PRIMARY KEY (job, local_date, provider, conversation_kind, conversation_id)
            )",
        )
        .execute(&pool)
        .await
        .unwrap();
        pool
    }

    async fn assert_partial_failure_retries_only_failed_recipient(job: ScheduledJob) {
        let pool = delivery_pool().await;
        let alex = ChatAddress::direct(ChatProvider::Telegram, "101");
        let jordan = ChatAddress::direct(ChatProvider::Telegram, "202");
        let targets = vec![alex.clone(), jordan.clone()];
        let first_due = due_recipients(
            &pool,
            job,
            "2026-09-13",
            ChatProvider::Telegram,
            &targets,
            true,
            1_000,
        )
        .await
        .unwrap();
        let sent = RefCell::new(Vec::new());

        deliver_recipients(&pool, job, first_due, |delivery| {
            sent.borrow_mut().push(delivery.recipient.clone());
            let outcome = if delivery.recipient == alex {
                DeliveryOutcome::Delivered
            } else {
                DeliveryOutcome::Retry
            };
            async move { outcome }
        })
        .await
        .unwrap();
        assert_eq!(*sent.borrow(), targets);

        let delivered: Vec<String> = sqlx::query_scalar(
            "SELECT conversation_id FROM scheduled_chat_deliveries
             WHERE delivered_at IS NOT NULL ORDER BY conversation_id",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(delivered, vec!["101"]);

        let retry_after: i64 = sqlx::query_scalar(
            "SELECT retry_after_epoch FROM scheduled_chat_deliveries
             WHERE provider = 'telegram' AND conversation_id = '202'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(due_recipients(
            &pool,
            job,
            "2026-09-14",
            ChatProvider::Telegram,
            &targets,
            false,
            retry_after - 1,
        )
        .await
        .unwrap()
        .is_empty());

        sent.borrow_mut().clear();
        let retry_due = due_recipients(
            &pool,
            job,
            "2026-09-14",
            ChatProvider::Telegram,
            &targets,
            false,
            retry_after,
        )
        .await
        .unwrap();
        assert_eq!(retry_due[0].local_date, "2026-09-13");
        deliver_recipients(&pool, job, retry_due, |delivery| {
            sent.borrow_mut().push(delivery.recipient);
            async { DeliveryOutcome::Delivered }
        })
        .await
        .unwrap();

        assert_eq!(*sent.borrow(), vec![jordan]);
        assert!(due_recipients(
            &pool,
            job,
            "2026-09-14",
            ChatProvider::Telegram,
            &targets,
            false,
            retry_after + RETRY_DELAY_SECONDS,
        )
        .await
        .unwrap()
        .is_empty());
    }

    #[tokio::test]
    async fn scheduled_jobs_retry_only_failed_recipients() {
        for job in [
            ScheduledJob::MorningBrief,
            ScheduledJob::Portfolio,
            ScheduledJob::Reflection,
        ] {
            assert_partial_failure_retries_only_failed_recipient(job).await;
        }
    }

    #[tokio::test]
    async fn next_schedule_supersedes_an_older_pending_run() {
        let pool = delivery_pool().await;
        let targets = vec![ChatAddress::direct(ChatProvider::Telegram, "101")];
        let first = due_recipients(
            &pool,
            ScheduledJob::MorningBrief,
            "2026-09-13",
            ChatProvider::Telegram,
            &targets,
            true,
            1_000,
        )
        .await
        .unwrap();
        deliver_recipients(&pool, ScheduledJob::MorningBrief, first, |_| async {
            DeliveryOutcome::Retry
        })
        .await
        .unwrap();

        let next = due_recipients(
            &pool,
            ScheduledJob::MorningBrief,
            "2026-09-14",
            ChatProvider::Telegram,
            &targets,
            true,
            2_000,
        )
        .await
        .unwrap();

        assert_eq!(next.len(), 1);
        assert_eq!(next[0].local_date, "2026-09-14");
        let old_pending: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM scheduled_chat_deliveries
             WHERE local_date = '2026-09-13' AND delivered_at IS NULL",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(old_pending, 0);
    }
}
