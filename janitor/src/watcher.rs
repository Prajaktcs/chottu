use anyhow::{Context, Result};
use notify::{Config, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::sync::mpsc::{channel, Receiver};

/// Initializes the directory watcher and returns the watcher handler and event receiver channel.
pub fn watch_directory(
    path: &Path,
) -> Result<(RecommendedWatcher, Receiver<notify::Result<Event>>)> {
    let (tx, rx) = channel(100);

    // Bridges the synchronous notify callback into the Tokio async channel
    let mut watcher = RecommendedWatcher::new(
        move |res| {
            if let Err(e) = tx.blocking_send(res) {
                eprintln!("Failed to forward watcher event: {:?}", e);
            }
        },
        Config::default(),
    )
    .context("Failed to create RecommendedWatcher")?;

    watcher
        .watch(path, RecursiveMode::NonRecursive)
        .context("Failed to register watched path")?;

    Ok((watcher, rx))
}

/// Dynamic debounce helper that waits for a file's size to stabilize to confirm the write is completed.
pub async fn wait_for_file_write_completion(path: &PathBuf) -> Result<()> {
    let mut last_size = 0;

    // Check the file size every 500ms. If it stays identical and non-zero, the write is done.
    for _ in 0..20 {
        // Max 10 seconds timeout
        if let Ok(metadata) = tokio::fs::metadata(path).await {
            let current_size = metadata.len();
            if current_size > 0 && current_size == last_size {
                return Ok(());
            }
            last_size = current_size;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    Err(anyhow::anyhow!(
        "File write timed out or file is empty: {:?}",
        path
    ))
}

/// Safe move helper that copies then deletes to handle mount boundaries safely.
pub async fn safe_archive_file(source: &Path, archive_dir: &Path) -> Result<()> {
    let filename = source
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("Invalid file name: {:?}", source))?;
    let destination = archive_dir.join(filename);

    // Try standard renaming (fastest, keeps metadata)
    if tokio::fs::rename(source, &destination).await.is_err() {
        // Fallback to copy and delete if crossing device mount boundaries
        tokio::fs::copy(source, &destination)
            .await
            .with_context(|| {
                format!("Failed to copy file from {:?} to {:?}", source, destination)
            })?;
        tokio::fs::remove_file(source)
            .await
            .with_context(|| format!("Failed to delete original file after copy: {:?}", source))?;
    }

    Ok(())
}

pub fn is_file_write_event(event: &Event) -> bool {
    matches!(
        event.kind,
        EventKind::Create(_)
            | EventKind::Modify(notify::event::ModifyKind::Data(_))
            | EventKind::Modify(notify::event::ModifyKind::Name(_))
    )
}
