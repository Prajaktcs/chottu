use std::future::Future;

use chotu_common::SignalRecipient;
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
    pub(crate) recipient: SignalRecipient,
}

fn recipient_parts(recipient: &SignalRecipient) -> (&'static str, &str) {
    match recipient {
        SignalRecipient::Direct { aci } => ("direct", aci),
        SignalRecipient::Group { group_id } => ("group", group_id),
    }
}

fn recipient_from_parts(kind: &str, id: String) -> Result<SignalRecipient, sqlx::Error> {
    match kind {
        "direct" => Ok(SignalRecipient::Direct { aci: id }),
        "group" => Ok(SignalRecipient::Group { group_id: id }),
        other => Err(sqlx::Error::Protocol(format!(
            "invalid scheduled Signal recipient kind: {other}"
        ))),
    }
}

async fn register_recipients(
    pool: &SqlitePool,
    job: ScheduledJob,
    local_date: &str,
    recipients: &[SignalRecipient],
) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query(
        "DELETE FROM scheduled_signal_deliveries \
         WHERE job = ? AND local_date <> ? AND delivered_at IS NULL",
    )
    .bind(job.as_str())
    .bind(local_date)
    .execute(&mut *tx)
    .await?;
    for recipient in recipients {
        let (kind, id) = recipient_parts(recipient);
        sqlx::query(
            "INSERT OR IGNORE INTO scheduled_signal_deliveries \
             (job, local_date, recipient_kind, recipient_id) \
             VALUES (?, ?, ?, ?)",
        )
        .bind(job.as_str())
        .bind(local_date)
        .bind(kind)
        .bind(id)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// Registers the current targets when the schedule fires, then returns only
/// currently authorized recipients whose content is due for delivery.
pub(crate) async fn due_recipients(
    pool: &SqlitePool,
    job: ScheduledJob,
    local_date: &str,
    current_targets: &[SignalRecipient],
    schedule_matches: bool,
    now_epoch: i64,
) -> Result<Vec<PendingDelivery>, sqlx::Error> {
    if schedule_matches && !current_targets.is_empty() {
        register_recipients(pool, job, local_date, current_targets).await?;
    }

    let rows: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT local_date, recipient_kind, recipient_id \
         FROM scheduled_signal_deliveries \
         WHERE job = ? AND delivered_at IS NULL \
           AND (retry_after_epoch IS NULL OR retry_after_epoch <= ?) \
         ORDER BY local_date, recipient_kind, recipient_id",
    )
    .bind(job.as_str())
    .bind(now_epoch)
    .fetch_all(pool)
    .await?;

    rows.into_iter()
        .map(|(local_date, kind, id)| {
            recipient_from_parts(&kind, id).map(|recipient| PendingDelivery {
                local_date,
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
    recipient: &SignalRecipient,
    outcome: DeliveryOutcome,
    attempt_finished_at: i64,
) -> Result<(), sqlx::Error> {
    let (kind, id) = recipient_parts(recipient);
    match outcome {
        DeliveryOutcome::Delivered => {
            sqlx::query(
                "UPDATE scheduled_signal_deliveries \
                 SET delivered_at = CURRENT_TIMESTAMP, retry_after_epoch = NULL \
                 WHERE job = ? AND local_date = ? \
                   AND recipient_kind = ? AND recipient_id = ?",
            )
            .bind(job.as_str())
            .bind(local_date)
            .bind(kind)
            .bind(id)
            .execute(pool)
            .await?;
        }
        DeliveryOutcome::Retry => {
            sqlx::query(
                "UPDATE scheduled_signal_deliveries \
                 SET retry_after_epoch = ? \
                 WHERE job = ? AND local_date = ? \
                   AND recipient_kind = ? AND recipient_id = ? \
                   AND delivered_at IS NULL",
            )
            .bind(attempt_finished_at + RETRY_DELAY_SECONDS)
            .bind(job.as_str())
            .bind(local_date)
            .bind(kind)
            .bind(id)
            .execute(pool)
            .await?;
        }
    }
    Ok(())
}

/// Applies one attempt to each due recipient. Only delivery of the scheduled
/// content records success; failures remain pending behind a bounded backoff.
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
    use std::sync::Mutex;

    async fn delivery_pool() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::query(
            "CREATE TABLE scheduled_signal_deliveries (
                job TEXT NOT NULL,
                local_date TEXT NOT NULL,
                recipient_kind TEXT NOT NULL,
                recipient_id TEXT NOT NULL,
                delivered_at TEXT,
                retry_after_epoch INTEGER,
                PRIMARY KEY (job, local_date, recipient_kind, recipient_id)
            )",
        )
        .execute(&pool)
        .await
        .unwrap();
        pool
    }

    async fn assert_partial_failure_retries_only_failed_recipient(job: ScheduledJob) {
        let pool = delivery_pool().await;
        let alex = SignalRecipient::Direct {
            aci: "aci-alex".to_string(),
        };
        let jordan = SignalRecipient::Direct {
            aci: "aci-jordan".to_string(),
        };
        let targets = vec![alex.clone(), jordan.clone()];
        let first_due = due_recipients(&pool, job, "2026-09-13", &targets, true, 1_000)
            .await
            .unwrap();
        let sent = Mutex::new(Vec::new());

        deliver_recipients(&pool, job, first_due, |delivery| {
            sent.lock().unwrap().push(delivery.recipient.clone());
            let outcome = if delivery.recipient == alex {
                DeliveryOutcome::Delivered
            } else {
                DeliveryOutcome::Retry
            };
            async move { outcome }
        })
        .await
        .unwrap();
        assert_eq!(*sent.lock().unwrap(), targets);

        let delivered: Vec<String> = sqlx::query_scalar(
            "SELECT recipient_id FROM scheduled_signal_deliveries \
             WHERE delivered_at IS NOT NULL ORDER BY recipient_id",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(delivered, vec!["aci-alex"]);

        let retry_after: i64 = sqlx::query_scalar(
            "SELECT retry_after_epoch FROM scheduled_signal_deliveries \
             WHERE recipient_id = 'aci-jordan'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(
            due_recipients(&pool, job, "2026-09-14", &targets, false, retry_after - 1,)
                .await
                .unwrap()
                .is_empty()
        );

        sent.lock().unwrap().clear();
        let retry_due = due_recipients(&pool, job, "2026-09-14", &targets, false, retry_after)
            .await
            .unwrap();
        assert_eq!(retry_due[0].local_date, "2026-09-13");
        deliver_recipients(&pool, job, retry_due, |delivery| {
            sent.lock().unwrap().push(delivery.recipient);
            async { DeliveryOutcome::Delivered }
        })
        .await
        .unwrap();

        assert_eq!(*sent.lock().unwrap(), vec![jordan]);
        assert!(due_recipients(
            &pool,
            job,
            "2026-09-14",
            &targets,
            false,
            retry_after + RETRY_DELAY_SECONDS,
        )
        .await
        .unwrap()
        .is_empty());
    }

    #[tokio::test]
    async fn morning_brief_partial_failure_retries_only_failed_recipient() {
        assert_partial_failure_retries_only_failed_recipient(ScheduledJob::MorningBrief).await;
    }

    #[tokio::test]
    async fn portfolio_partial_failure_retries_only_failed_recipient() {
        assert_partial_failure_retries_only_failed_recipient(ScheduledJob::Portfolio).await;
    }

    #[tokio::test]
    async fn reflection_partial_failure_retries_only_failed_recipient() {
        assert_partial_failure_retries_only_failed_recipient(ScheduledJob::Reflection).await;
    }
    #[tokio::test]
    async fn next_schedule_supersedes_an_older_pending_run() {
        let pool = delivery_pool().await;
        let target = SignalRecipient::Direct {
            aci: "aci-alex".to_string(),
        };
        let targets = vec![target];
        let first = due_recipients(
            &pool,
            ScheduledJob::MorningBrief,
            "2026-09-13",
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
            &targets,
            true,
            2_000,
        )
        .await
        .unwrap();

        assert_eq!(next.len(), 1);
        assert_eq!(next[0].local_date, "2026-09-14");
        let old_pending: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM scheduled_signal_deliveries \
             WHERE local_date = '2026-09-13' AND delivered_at IS NULL",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(old_pending, 0);
    }
}
