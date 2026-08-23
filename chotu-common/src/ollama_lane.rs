//! Shared Ollama slot for one [`crate::ChotuLlm`] instance (cloned via `Arc`).
//!
//! Telegram (interactive) jumps the queue; email waits. An in-flight email call
//! still finishes — Ollama cannot preempt a running generation — then the next
//! slot goes to any waiting Telegram work. Coordinator constructs one `ChotuLlm`
//! and clones it; a second `ChotuLlm::new` would have its own lane.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use tokio::sync::{Mutex, Notify};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OllamaPriority {
    /// Telegram, memory RAG, coach tips — take the next free slot.
    #[default]
    Interactive,
    /// Email classify/extract — wait while interactive work is queued or running.
    Background,
}

pub(crate) struct OllamaLane {
    lock: Mutex<()>,
    interactive: AtomicUsize,
    notify: Notify,
}

impl OllamaLane {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            lock: Mutex::new(()),
            interactive: AtomicUsize::new(0),
            notify: Notify::new(),
        })
    }

    pub(crate) async fn run<T, F>(&self, priority: OllamaPriority, fut: F) -> T
    where
        F: std::future::Future<Output = T>,
    {
        match priority {
            OllamaPriority::Interactive => self.run_interactive(fut).await,
            OllamaPriority::Background => self.run_background(fut).await,
        }
    }

    async fn run_interactive<T, F>(&self, fut: F) -> T
    where
        F: std::future::Future<Output = T>,
    {
        let _hold = InteractiveHold::acquire(self);
        let _guard = self.lock.lock().await;
        fut.await
    }

    async fn run_background<T, F>(&self, fut: F) -> T
    where
        F: std::future::Future<Output = T>,
    {
        loop {
            self.wait_while_interactive().await;
            let _guard = self.lock.lock().await;
            if self.interactive.load(Ordering::SeqCst) > 0 {
                continue;
            }
            return fut.await;
        }
    }

    async fn wait_while_interactive(&self) {
        loop {
            if self.interactive.load(Ordering::SeqCst) == 0 {
                return;
            }
            let notified = self.notify.notified();
            if self.interactive.load(Ordering::SeqCst) == 0 {
                return;
            }
            notified.await;
        }
    }
}

struct InteractiveHold<'a> {
    lane: &'a OllamaLane,
}

impl<'a> InteractiveHold<'a> {
    fn acquire(lane: &'a OllamaLane) -> Self {
        lane.interactive.fetch_add(1, Ordering::SeqCst);
        lane.notify.notify_waiters();
        Self { lane }
    }
}

impl Drop for InteractiveHold<'_> {
    fn drop(&mut self) {
        self.lane.interactive.fetch_sub(1, Ordering::SeqCst);
        self.lane.notify.notify_waiters();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::time::Duration;

    #[tokio::test]
    async fn background_waits_for_interactive() {
        let lane = OllamaLane::new();
        let bg_ran_during_fg = Arc::new(AtomicBool::new(false));
        let fg_holding = Arc::new(AtomicBool::new(false));

        let lane_fg = lane.clone();
        let holding = fg_holding.clone();
        let fg = tokio::spawn(async move {
            lane_fg
                .run(OllamaPriority::Interactive, async {
                    holding.store(true, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(80)).await;
                    holding.store(false, Ordering::SeqCst);
                })
                .await;
        });

        tokio::time::timeout(Duration::from_secs(1), async {
            while !fg_holding.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("interactive never acquired the lock");

        let lane_bg = lane.clone();
        let flag = bg_ran_during_fg.clone();
        let holding = fg_holding.clone();
        let bg = tokio::spawn(async move {
            lane_bg
                .run(OllamaPriority::Background, async {
                    if holding.load(Ordering::SeqCst) {
                        flag.store(true, Ordering::SeqCst);
                    }
                })
                .await;
        });

        fg.await.unwrap();
        bg.await.unwrap();
        assert!(
            !bg_ran_during_fg.load(Ordering::SeqCst),
            "email-priority work must not run while Telegram holds the Ollama slot"
        );
    }

    #[tokio::test]
    async fn cancelled_interactive_unblocks_background() {
        let lane = OllamaLane::new();
        let lane_fg = lane.clone();
        let fg = tokio::spawn(async move {
            lane_fg
                .run(OllamaPriority::Interactive, std::future::pending::<()>())
                .await;
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        fg.abort();
        let _ = fg.await;

        let lane_bg = lane.clone();
        tokio::time::timeout(
            Duration::from_millis(200),
            lane_bg.run(OllamaPriority::Background, async {}),
        )
        .await
        .expect("background must not hang after cancelled interactive work");
    }
}
