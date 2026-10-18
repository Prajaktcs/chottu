#![cfg(unix)]

use sqlx::sqlite::SqliteConnectOptions;
use sqlx::SqlitePool;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const OPERATION: &str = "users/me/dataTypes/nutrition-log/operations/test-create";
const DATA_POINT: &str = "users/me/dataTypes/nutrition-log/dataPoints/retained-meal";

async fn recovery_fixture() -> PathBuf {
    let root = std::env::temp_dir().join(format!("chotu-backup-privacy-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&root).unwrap();
    let database = root.join("source.db");
    let pool = chotu_common::init_db(database.to_str().unwrap())
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO food_log (id, timestamp, family_member_id, raw_text_description, \
         estimated_calories, google_data_point_id) \
         VALUES ('meal', '2026-09-30T02:00:00Z', 'alex', 'synthetic test meal', 500, ?)",
    )
    .bind(OPERATION)
    .execute(&pool)
    .await
    .unwrap();
    pool.close().await;
    std::fs::set_permissions(&database, std::fs::Permissions::from_mode(0o600)).unwrap();
    let response = serde_json::json!({
        "name": OPERATION,
        "done": true,
        "response": {
            "@type": "type.googleapis.com/google.devicesandservices.health.v4.DataPoint",
            "name": DATA_POINT,
            "nutritionLog": {"foodDisplayName": "synthetic test meal", "energy": {"kcal": 500}}
        }
    });
    std::fs::write(
        root.join("response.json"),
        serde_json::to_vec(&response).unwrap(),
    )
    .unwrap();
    root
}

fn recover_under_umask_022(root: &Path) -> Output {
    recover_under_umask_022_to(root, &root.join("backup.db"))
}

fn recover_under_umask_022_to(root: &Path, backup: &Path) -> Output {
    // Change umask only in the child, never the shared test process.
    Command::new("sh")
        .current_dir(root)
        .args(["-c", "umask 022; exec \"$@\"", "recover-health-operation"])
        .arg(env!("CARGO_BIN_EXE_recover-health-operation"))
        .args(["--apply", "--database"])
        .arg(root.join("source.db"))
        .arg("--backup")
        .arg(backup)
        .args(["--member", "alex", "--meal", "meal", "--response"])
        .arg(root.join("response.json"))
        .output()
        .unwrap()
}

async fn open_database(path: &Path) -> SqlitePool {
    SqlitePool::connect_with(
        SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(false),
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn recovery_backup_is_private_and_preserves_original_state_under_umask_022() {
    let root = recovery_fixture().await;
    let output = recover_under_umask_022(&root);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        std::fs::metadata(root.join("backup.db"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    let backup = open_database(&root.join("backup.db")).await;
    let original_reference: String =
        sqlx::query_scalar("SELECT google_data_point_id FROM food_log WHERE id = 'meal'")
            .fetch_one(&backup)
            .await
            .unwrap();
    assert_eq!(original_reference, OPERATION);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM food_log_operation_recoveries")
            .fetch_one(&backup)
            .await
            .unwrap(),
        0
    );
    backup.close().await;
    let source = open_database(&root.join("source.db")).await;
    let recovered_reference: String =
        sqlx::query_scalar("SELECT google_data_point_id FROM food_log WHERE id = 'meal'")
            .fetch_one(&source)
            .await
            .unwrap();
    assert_eq!(recovered_reference, DATA_POINT);
    source.close().await;
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn recovery_refuses_an_existing_empty_backup_without_changing_the_meal() {
    let root = recovery_fixture().await;
    std::fs::write(root.join("backup.db"), []).unwrap();
    let output = recover_under_umask_022(&root);
    assert!(!output.status.success());
    assert_eq!(std::fs::metadata(root.join("backup.db")).unwrap().len(), 0);
    let source = open_database(&root.join("source.db")).await;
    let reference: String =
        sqlx::query_scalar("SELECT google_data_point_id FROM food_log WHERE id = 'meal'")
            .fetch_one(&source)
            .await
            .unwrap();
    assert_eq!(reference, OPERATION);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM food_log_operation_recoveries")
            .fetch_one(&source)
            .await
            .unwrap(),
        0
    );
    source.close().await;
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn relative_file_uri_backup_name_writes_only_the_reserved_private_file() {
    let root = recovery_fixture().await;
    // SQLite would read a relative `file:` name as a URI for `backup.db`.
    let output = recover_under_umask_022_to(&root, Path::new("file:backup.db"));
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!root.join("backup.db").exists());
    let reserved = std::fs::metadata(root.join("file:backup.db")).unwrap();
    assert_eq!(reserved.permissions().mode() & 0o777, 0o600);
    assert!(reserved.len() > 0);
    let backup = open_database(&root.join("file:backup.db")).await;
    let original_reference: String =
        sqlx::query_scalar("SELECT google_data_point_id FROM food_log WHERE id = 'meal'")
            .fetch_one(&backup)
            .await
            .unwrap();
    assert_eq!(original_reference, OPERATION);
    backup.close().await;
    std::fs::remove_dir_all(root).unwrap();
}
