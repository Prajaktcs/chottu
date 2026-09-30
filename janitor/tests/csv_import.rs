use janitor::csv_import::{import_csv_file, rebuild_archived_csvs};
use sqlx::{sqlite::SqlitePoolOptions, SqlitePool};
use std::path::Path;
use tempfile::TempDir;

async fn database() -> SqlitePool {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    sqlx::raw_sql("CREATE TABLE financial_ledger (id TEXT PRIMARY KEY, timestamp TEXT NOT NULL, amount REAL NOT NULL, currency TEXT NOT NULL, institution TEXT NOT NULL, merchant TEXT NOT NULL, category TEXT NOT NULL, source_type TEXT NOT NULL, message_id TEXT UNIQUE)")
        .execute(&pool).await.unwrap();
    pool
}

fn source(dir: &Path, filename: &str, content: &str) -> std::path::PathBuf {
    let path = dir.join(filename);
    std::fs::write(&path, content).unwrap();
    path
}

#[tokio::test]
async fn overlapping_card_formats_keep_repeats_refunds_payments_and_real_categories() {
    let dir = TempDir::new().unwrap();
    let pool = database().await;
    let statement = source(dir.path(), "credit-card-statement-transactions-2026-05-01.csv",
        "transaction_date,post_date,type,details,amount,currency\n2026-05-05,2026-05-06,Purchase,TST-YALETOWN,11.88,CAD\n2026-04-18,2026-04-19,Purchase,CITY PARKING,2,CAD\n2026-04-18,2026-04-19,Purchase,CITY PARKING,2,CAD\n");
    let activities = source(dir.path(), "credit-card-activities-2026-09-29.csv",
        "transaction_date,transaction_type,status,merchant,amount,currency,notes,category\n2026-05-06,Purchase,Completed,Tst-Yaletown,-11.88,CAD,,Restaurants\n2026-04-18,Purchase,Completed,City  Parking,-2,CAD,,Parking\n2026-04-18,Purchase,Completed,City Parking,-2,CAD,,Parking\n2026-09-15,Payment,Completed,,100,CAD,,Uncategorized\n2026-09-16,Refund,Completed,Tst-Yaletown,5,CAD,,Restaurants\n");
    import_csv_file(&statement, &pool, "USD").await.unwrap();
    let result = import_csv_file(&activities, &pool, "USD").await.unwrap();
    assert_eq!((result.inserted, result.matched), (2, 3));
    let rows: Vec<(f64, String, String)> =
        sqlx::query_as("SELECT amount, category, merchant FROM financial_ledger ORDER BY amount")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(
        rows,
        vec![
            (-11.88, "Restaurants".into(), "TST-YALETOWN".into()),
            (-2.0, "Parking".into(), "CITY PARKING".into()),
            (-2.0, "Parking".into(), "CITY PARKING".into()),
            (5.0, "Restaurants".into(), "Tst-Yaletown".into()),
            (100.0, "Transfer".into(), "Credit card payment".into())
        ]
    );
    let again = import_csv_file(&activities, &pool, "USD").await.unwrap();
    assert_eq!((again.inserted, again.matched, again.updated), (0, 5, 0));
    let older = import_csv_file(&statement, &pool, "USD").await.unwrap();
    assert_eq!((older.inserted, older.updated), (0, 0));
    let dates: Vec<String> =
        sqlx::query_scalar("SELECT timestamp FROM financial_ledger WHERE amount = -11.88")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(dates, vec!["2026-05-06T00:00:00+00:00"]);
}

#[tokio::test]
async fn account_and_effective_time_preserve_distinct_transfers_across_exports() {
    let dir = TempDir::new().unwrap();
    let pool = database().await;
    let statement = source(dir.path(), "Chequing-monthly-statement-transactions-WK0000001CAD-2026-04-01.csv",
        "date,transaction,description,amount,balance,currency\n2026-04-15,TRFOUT,Transfer to savings,-1000,2000,CAD\n2026-04-15,TRFOUT,Transfer to brokerage,-1000,1000,CAD\n");
    let header = "effective_date,effective_time,account_id,account_type,activity_type,activity_sub_type,description,net_cash_amount,currency\n";
    let timed = source(dir.path(), "activities-export.csv", &format!("{header}2026-04-15,06:48:36,WK0000001CAD,Chequing,MoneyMovement,TRANSFER,Money transfer out of the account,-1000,CAD\n2026-04-15,16:08:46,WK0000001CAD,Chequing,MoneyMovement,TRANSFER,Money transfer out of the account,-1000,CAD\n2026-04-15,06:48:36,WK0000002CAD,Chequing,MoneyMovement,TRANSFER,Money transfer out of the account,-1000,CAD\n"));
    import_csv_file(&statement, &pool, "CAD").await.unwrap();
    let imported = import_csv_file(&timed, &pool, "CAD").await.unwrap();
    assert_eq!((imported.inserted, imported.matched), (1, 2));
    let rows: Vec<(String, String, f64)> = sqlx::query_as("SELECT institution, timestamp, amount FROM financial_ledger ORDER BY institution, timestamp")
        .fetch_all(&pool).await.unwrap();
    assert_eq!(
        rows,
        vec![
            (
                "Wealthsimple:WK0000001CAD".into(),
                "2026-04-15T06:48:36+00:00".into(),
                -1000.0
            ),
            (
                "Wealthsimple:WK0000001CAD".into(),
                "2026-04-15T16:08:46+00:00".into(),
                -1000.0
            ),
            (
                "Wealthsimple:WK0000002CAD".into(),
                "2026-04-15T06:48:36+00:00".into(),
                -1000.0
            )
        ]
    );
    let subset = source(dir.path(), "subset.csv", &format!("{header}2026-04-15,16:08:46,WK0000001CAD,Chequing,MoneyMovement,TRANSFER,Money transfer out of the account,-1000,CAD\n"));
    let imported = import_csv_file(&subset, &pool, "CAD").await.unwrap();
    assert_eq!((imported.inserted, imported.matched), (0, 1));
}

#[tokio::test]
async fn invalid_batch_does_not_partially_commit() {
    let dir = TempDir::new().unwrap();
    let pool = database().await;
    let bad = source(
        dir.path(),
        "bank.csv",
        "Date,Amount,Merchant\n2026-05-01,-10,Shop\n2026-05-02,0,Invalid\n",
    );
    assert!(import_csv_file(&bad, &pool, "CAD").await.is_err());
    let sum: f64 = sqlx::query_scalar("SELECT COALESCE(SUM(amount), 0.0) FROM financial_ledger")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(sum, 0.0);
}

#[tokio::test]
async fn repair_preserves_non_csv_rows_and_repeated_source_occurrences() {
    let dir = TempDir::new().unwrap();
    let pool = database().await;
    sqlx::raw_sql("INSERT INTO financial_ledger VALUES ('email', '2026-05-01T00:00:00+00:00', 12, 'CAD', 'Bank', 'Shop', 'Food', 'EMAIL_STREAM', 'mail'); INSERT INTO financial_ledger VALUES ('receipt', '2026-05-01T00:00:00+00:00', 30, 'CAD', 'BATCH_DROP', 'Receipt', 'Food', 'BATCH_DROP', NULL)")
        .execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO financial_ledger VALUES ('e96f3f9736e9910f38296039d1e2284c8e074de8426961ca9233826aab9bf890', '2026-05-01T00:00:00+00:00', -10, 'CAD', 'Dropped CSV', 'Shop', 'Uncategorized', 'BATCH_DROP', NULL)")
        .execute(&pool).await.unwrap();
    source(
        dir.path(),
        "bank.csv",
        "Date,Amount,Merchant\n2026-05-01,-10,Shop\n2026-05-01,-10,Shop\n",
    );
    let result = rebuild_archived_csvs(&pool, dir.path(), "CAD")
        .await
        .unwrap();
    assert_eq!(result.inserted, 2);
    assert_eq!(result.legacy_removed, 1);
    let legacy: Vec<(String, f64, String)> = sqlx::query_as("SELECT id, amount, source_type FROM financial_ledger WHERE source_type != 'CSV_IMPORT' ORDER BY id")
        .fetch_all(&pool).await.unwrap();
    assert_eq!(
        legacy,
        vec![
            ("email".into(), 12.0, "EMAIL_STREAM".into()),
            ("receipt".into(), 30.0, "BATCH_DROP".into())
        ]
    );
    let second = rebuild_archived_csvs(&pool, dir.path(), "CAD")
        .await
        .unwrap();
    assert_eq!((second.inserted, second.matched, second.updated), (0, 2, 0));
}

#[tokio::test]
async fn storage_failure_rolls_back_legacy_removal_and_new_rows() {
    let dir = TempDir::new().unwrap();
    let pool = database().await;
    sqlx::query("INSERT INTO financial_ledger VALUES ('e96f3f9736e9910f38296039d1e2284c8e074de8426961ca9233826aab9bf890', '2026-05-01T00:00:00+00:00', -10, 'CAD', 'Dropped CSV', 'Shop', 'Uncategorized', 'BATCH_DROP', NULL)")
        .execute(&pool).await.unwrap();
    sqlx::raw_sql("CREATE TRIGGER reject_import BEFORE INSERT ON financial_ledger WHEN NEW.merchant = 'Reject' BEGIN SELECT RAISE(ABORT, 'storage rejected row'); END")
        .execute(&pool).await.unwrap();
    let path = source(
        dir.path(),
        "bank.csv",
        "Date,Amount,Merchant\n2026-05-01,-10,Shop\n2026-05-02,-20,Reject\n",
    );
    let error = import_csv_file(&path, &pool, "CAD").await.unwrap_err();
    assert!(error.to_string().contains("storage rejected row"));
    let rows: Vec<(String, f64, String)> =
        sqlx::query_as("SELECT merchant, amount, source_type FROM financial_ledger")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(rows, vec![("Shop".into(), -10.0, "BATCH_DROP".into())]);
}
