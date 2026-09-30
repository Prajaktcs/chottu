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
async fn structural_categories_override_export_labels_and_repair_prior_classification() {
    let dir = TempDir::new().unwrap();
    let pool = database().await;
    let statement = source(dir.path(), "monthly-WK0000001CAD.csv",
        "date,transaction,description,amount,balance,currency,category\n2026-05-01,TRFOUT,Move to savings,-1000,0,CAD,Savings\n2026-05-01,BUY,Buy shares,-200,0,CAD,Savings\n2026-05-01,SPEND,Shop,-12,0,CAD,Groceries\n2026-05-01,DIV,Dividend,5,0,CAD,Savings\n2026-05-01,FEE,Service fee,-2,0,CAD,Savings\n2026-05-01,TAX,Withholding,-3,0,CAD,Savings\n");
    import_csv_file(&statement, &pool, "CAD").await.unwrap();
    let expected = vec![
        (-1000.0, "Transfer".to_string()),
        (-200.0, "Investment".to_string()),
        (-12.0, "Groceries".to_string()),
        (-3.0, "Taxes".to_string()),
        (-2.0, "Fees".to_string()),
        (5.0, "Income".to_string()),
    ];
    let rows: Vec<(f64, String)> =
        sqlx::query_as("SELECT amount, category FROM financial_ledger ORDER BY amount")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(rows, expected);

    // Reproduce metadata written when real export labels outranked structural kinds.
    sqlx::query("UPDATE financial_ledger SET category = 'Savings' WHERE category != 'Groceries'")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE csv_import_records SET category_quality = 2")
        .execute(&pool)
        .await
        .unwrap();
    let repaired = import_csv_file(&statement, &pool, "CAD").await.unwrap();
    assert_eq!(
        (repaired.inserted, repaired.matched, repaired.updated),
        (0, 6, 5)
    );
    let rows: Vec<(f64, String)> =
        sqlx::query_as("SELECT amount, category FROM financial_ledger ORDER BY amount")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(rows, expected);
    let spend: f64 = rows
        .iter()
        .map(|(amount, category)| {
            chotu_common::ledger::expense_contribution(*amount, "CSV_IMPORT", category)
        })
        .sum();
    assert_eq!(spend, 17.0);
    let again = import_csv_file(&statement, &pool, "CAD").await.unwrap();
    assert_eq!((again.inserted, again.updated), (0, 0));
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
        "Date,Amount,Merchant,account\n2026-05-01,-10,Shop,test-account\n2026-05-02,0,Invalid,test-account\n",
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
        "Date,Amount,Merchant,account\n2026-05-01,-10,Shop,test-account\n2026-05-01,-10,Shop,test-account\n",
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
        "Date,Amount,Merchant,account\n2026-05-01,-10,Shop,test-account\n2026-05-02,-20,Reject,test-account\n",
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

#[tokio::test]
async fn generic_accounts_are_required_and_isolated_across_sources() {
    let dir = TempDir::new().unwrap();
    let pool = database().await;
    for (filename, account) in [("alpha.csv", "account-a"), ("beta.csv", "account-b")] {
        let path = source(
            dir.path(),
            filename,
            &format!("date,amount,merchant,account\n2026-05-01,-10,Shop,{account}\n"),
        );
        assert_eq!(
            import_csv_file(&path, &pool, "CAD").await.unwrap().inserted,
            1
        );
    }
    let unknown = source(
        dir.path(),
        "unknown.csv",
        "date,amount,merchant\n2026-05-01,-10,Shop\n",
    );
    let error = import_csv_file(&unknown, &pool, "CAD").await.unwrap_err();
    assert!(format!("{error:#}").contains("account"));
    let total: (i64, f64) = sqlx::query_as("SELECT COUNT(*), SUM(amount) FROM financial_ledger")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(total, (2, -20.0));
    let overlap = source(
        dir.path(),
        "alpha-again.csv",
        "date,amount,merchant,account\n2026-05-01,-10,Shop,account-a\n",
    );
    let stats = import_csv_file(&overlap, &pool, "CAD").await.unwrap();
    assert_eq!((stats.inserted, stats.matched), (0, 1));
}

#[tokio::test]
async fn authoritative_activity_date_is_independent_of_category_and_import_order() {
    for category in ["", "Uncategorized", "Food"] {
        for activities_first in [false, true] {
            let dir = TempDir::new().unwrap();
            let pool = database().await;
            let statement = source(dir.path(), "statement.csv",
                "transaction_date,post_date,type,details,amount,currency\n2026-05-01,2026-05-02,Purchase,Shop,12,CAD\n");
            let activities = source(dir.path(), "activities.csv",
                &format!("transaction_date,transaction_type,status,merchant,amount,currency,category\n2026-05-02,Purchase,Completed,Shop,-12,CAD,{category}\n"));
            let order = if activities_first {
                [&activities, &statement]
            } else {
                [&statement, &activities]
            };
            for path in order {
                import_csv_file(path, &pool, "CAD").await.unwrap();
            }
            let row: (String, f64) =
                sqlx::query_as("SELECT timestamp, amount FROM financial_ledger")
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            assert_eq!(row, ("2026-05-02T00:00:00+00:00".into(), -12.0));
            std::fs::write(&activities,
                "transaction_date,transaction_type,status,merchant,amount,currency,category\n2026-05-02,Purchase,Completed,Shop,-12,CAD,Food\n").unwrap();
            import_csv_file(&activities, &pool, "CAD").await.unwrap();
            let row: (String, String) =
                sqlx::query_as("SELECT timestamp, category FROM financial_ledger")
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            assert_eq!(row, ("2026-05-02T00:00:00+00:00".into(), "Food".into()));
        }
    }
}

#[tokio::test]
async fn statement_category_enrichment_does_not_replace_effective_time() {
    let dir = TempDir::new().unwrap();
    let pool = database().await;
    let activities = source(dir.path(), "activities.csv",
        "effective_date,effective_time,account_id,activity_type,activity_sub_type,description,net_cash_amount,currency\n2026-05-01,10:30:01,WK0000001CAD,MoneyMovement,SPEND,Spend,-12,CAD\n");
    let statement = source(dir.path(), "monthly-WK0000001CAD.csv",
        "date,transaction,description,amount,balance,currency,category\n2026-05-01,SPEND,Shop,-12,100,CAD,Groceries\n");
    import_csv_file(&activities, &pool, "CAD").await.unwrap();
    import_csv_file(&statement, &pool, "CAD").await.unwrap();
    let row: (String, String, String) =
        sqlx::query_as("SELECT timestamp, merchant, category FROM financial_ledger")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        row,
        (
            "2026-05-01T10:30:01+00:00".into(),
            "Shop".into(),
            "Groceries".into()
        )
    );
}

#[tokio::test]
async fn card_sources_keep_explicit_accounts_distinct_across_formats_and_import_orders() {
    for account_column in ["account_id", "account"] {
        for activities_first in [false, true] {
            let dir = TempDir::new().unwrap();
            let pool = database().await;
            let mut statements = Vec::new();
            let mut activities = Vec::new();
            for account in ["card-a", "card-b"] {
                statements.push(source(dir.path(), &format!("statement-{account}.csv"),
                    &format!("transaction_date,post_date,type,details,amount,currency,{account_column}\n2026-05-01,2026-05-01,Purchase,Shop,12,CAD,{account}\n")));
                activities.push(source(dir.path(), &format!("activities-{account}.csv"),
                    &format!("transaction_date,transaction_type,status,merchant,amount,currency,category,{account_column}\n2026-05-01,Purchase,Completed,Shop,-12,CAD,Food,{account}\n")));
            }
            let order = if activities_first {
                [&activities, &statements]
            } else {
                [&statements, &activities]
            };
            let mut inserted = 0;
            let mut matched = 0;
            for paths in order {
                for path in paths {
                    let stats = import_csv_file(path, &pool, "CAD").await.unwrap();
                    inserted += stats.inserted;
                    matched += stats.matched;
                }
            }
            assert_eq!((inserted, matched), (2, 2));
            let rows: Vec<(String, f64)> = sqlx::query_as(
                "SELECT institution, amount FROM financial_ledger ORDER BY institution",
            )
            .fetch_all(&pool)
            .await
            .unwrap();
            assert_eq!(
                rows,
                vec![
                    ("Wealthsimple:card-a".to_string(), -12.0),
                    ("Wealthsimple:card-b".to_string(), -12.0),
                ]
            );
            let rerun = import_csv_file(&activities[0], &pool, "CAD").await.unwrap();
            assert_eq!((rerun.inserted, rerun.matched, rerun.updated), (0, 1, 0));
        }
    }
}

#[tokio::test]
async fn preview_metadata_upgrades_preserve_identity_and_cleanup_orphans_without_fks() {
    for foreign_keys in [false, true] {
        let dir = TempDir::new().unwrap();
        let pool = database().await;
        sqlx::query(if foreign_keys {
            "PRAGMA foreign_keys = ON"
        } else {
            "PRAGMA foreign_keys = OFF"
        })
        .execute(&pool)
        .await
        .unwrap();
        let path = source(dir.path(), "activities.csv",
            "transaction_date,transaction_type,status,merchant,amount,currency,category\n2026-05-01,Purchase,Completed,Shop,-12,CAD,Food\n");
        import_csv_file(&path, &pool, "CAD").await.unwrap();
        let before: (String, String, f64) =
            sqlx::query_as("SELECT id, timestamp, amount FROM financial_ledger")
                .fetch_one(&pool)
                .await
                .unwrap();
        sqlx::raw_sql(
            "CREATE TEMP TABLE saved_records AS SELECT ledger_id, has_time, merchant_quality, category_quality FROM csv_import_records;
             CREATE TEMP TABLE saved_keys AS SELECT * FROM csv_import_keys;
             DROP TABLE csv_import_keys;
             DROP TABLE csv_import_records;
             CREATE TABLE csv_import_records (
                 ledger_id TEXT PRIMARY KEY REFERENCES financial_ledger(id) ON DELETE CASCADE,
                 has_time INTEGER NOT NULL, merchant_quality INTEGER NOT NULL, category_quality INTEGER NOT NULL);
             CREATE TABLE csv_import_keys (
                 match_key TEXT NOT NULL,
                 ledger_id TEXT NOT NULL REFERENCES csv_import_records(ledger_id) ON DELETE CASCADE,
                 PRIMARY KEY (match_key, ledger_id));
             INSERT INTO csv_import_records SELECT * FROM saved_records;
             INSERT INTO csv_import_keys SELECT * FROM saved_keys;
             DROP TABLE saved_keys;
             DROP TABLE saved_records;",
        ).execute(&pool).await.unwrap();
        let statement = source(dir.path(), "statement.csv",
            "transaction_date,post_date,type,details,amount,currency\n2026-04-30,2026-05-01,Purchase,Shop,12,CAD\n");
        let overlapping = import_csv_file(&statement, &pool, "CAD").await.unwrap();
        assert_eq!(
            (
                overlapping.inserted,
                overlapping.matched,
                overlapping.updated
            ),
            (0, 1, 0)
        );
        let after_statement: (String, String, f64) =
            sqlx::query_as("SELECT id, timestamp, amount FROM financial_ledger")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(before, after_statement);
        let upgraded = import_csv_file(&path, &pool, "CAD").await.unwrap();
        assert_eq!(
            (upgraded.inserted, upgraded.matched, upgraded.updated),
            (0, 1, 0)
        );
        let after: (String, String, f64) =
            sqlx::query_as("SELECT id, timestamp, amount FROM financial_ledger")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(before, after);
        sqlx::query("DELETE FROM financial_ledger")
            .execute(&pool)
            .await
            .unwrap();
        let dangling: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM csv_import_records")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            dangling, 1,
            "logical references must not depend on FK cascades"
        );
        let restored = import_csv_file(&path, &pool, "CAD").await.unwrap();
        assert_eq!((restored.inserted, restored.matched), (1, 0));
        let after: (String, String, f64) =
            sqlx::query_as("SELECT id, timestamp, amount FROM financial_ledger")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(before, after);
        let stale_keys: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM csv_import_keys k LEFT JOIN csv_import_records r ON r.ledger_id = k.ledger_id WHERE r.ledger_id IS NULL",
        ).fetch_one(&pool).await.unwrap();
        assert_eq!(stale_keys, 0);
    }
}
