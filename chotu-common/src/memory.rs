//! Queryable memory: local RAG over journals, digests, personal references, and tasks.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::Result;
use chrono::Utc;
use sha2::{Digest, Sha256};
use sqlx::{QueryBuilder, SqlitePool};
use thiserror::Error;

use crate::llm::{ChotuLlm, GeminiClient, LlmError};

pub const DEFAULT_EMBED_MODEL: &str = "nomic-embed-text";
const DEFAULT_TOP_K: usize = 8;
const CHUNK_TARGET_CHARS: usize = 1000;
const CHUNK_OVERLAP_CHARS: usize = 120;

#[derive(Debug, Error)]
pub enum MemoryError {
    #[error("Embedding error: {0}")]
    Embed(String),
    #[error("Database error: {0}")]
    Db(#[from] sqlx::Error),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Other(String),
}

impl From<anyhow::Error> for MemoryError {
    fn from(e: anyhow::Error) -> Self {
        MemoryError::Other(e.to_string())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SourceType {
    Journal,
    Digest,
    PersonalReference,
    Task,
}

impl SourceType {
    pub fn as_str(self) -> &'static str {
        match self {
            SourceType::Journal => "journal",
            SourceType::Digest => "digest",
            SourceType::PersonalReference => "personal_reference",
            SourceType::Task => "task",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "journal" => Some(SourceType::Journal),
            "digest" => Some(SourceType::Digest),
            "personal_reference" => Some(SourceType::PersonalReference),
            "task" => Some(SourceType::Task),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct MemoryHit {
    pub source_type: SourceType,
    pub source_id: String,
    pub title: String,
    pub body: String,
    pub url: Option<String>,
    pub occurred_at: Option<String>,
    pub score: f32,
}

#[derive(Debug, Clone, Default)]
pub struct ReindexStats {
    pub upserted: usize,
    pub skipped: usize,
    pub deleted: usize,
    pub errors: usize,
}

#[derive(Debug, Clone)]
struct PendingChunk {
    source_type: SourceType,
    source_id: String,
    title: String,
    body: String,
    url: Option<String>,
    occurred_at: Option<String>,
    owner_member_id: Option<String>,
}

/// Resolve `$CHOTU_BRAIN_DIR` (default `~/chotu_brain`).
pub fn brain_dir() -> PathBuf {
    let brain_dir_str =
        std::env::var("CHOTU_BRAIN_DIR").unwrap_or_else(|_| "~/chotu_brain".to_string());
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(brain_dir_str.replace('~', &home))
}

/// Ollama embedding client + index helpers.
#[derive(Debug, Clone)]
pub struct MemoryIndex {
    embed_base_url: String,
    embed_model: String,
}

impl MemoryIndex {
    pub fn from_env() -> Self {
        let host = std::env::var("OLLAMA_HOST").unwrap_or_else(|_| "http://localhost".to_string());
        let port = std::env::var("OLLAMA_PORT").unwrap_or_else(|_| "11434".to_string());
        let base = if host.contains("://") {
            format!("{}:{}", host.trim_end_matches('/'), port)
        } else {
            format!("http://{}:{}", host, port)
        };
        // Prefer explicit base URL if set (matches ChotuLlm host:port patterns).
        let embed_base_url = std::env::var("OLLAMA_BASE_URL").unwrap_or(base);
        let embed_model =
            std::env::var("OLLAMA_EMBED_MODEL").unwrap_or_else(|_| DEFAULT_EMBED_MODEL.to_string());
        Self {
            embed_base_url: embed_base_url.trim_end_matches('/').to_string(),
            embed_model,
        }
    }

    pub fn with_base_url(base_url: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            embed_base_url: base_url.into().trim_end_matches('/').to_string(),
            embed_model: model.into(),
        }
    }

    pub async fn embed_texts(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, MemoryError> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let url = format!("{}/api/embed", self.embed_base_url);
        let body = serde_json::json!({
            "model": self.embed_model,
            "input": texts,
        });
        let resp = reqwest::Client::new()
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| MemoryError::Embed(format!("Ollama embed request failed: {e}")))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(MemoryError::Embed(format!(
                "Ollama embed returned {status}: {text}"
            )));
        }
        let json: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| MemoryError::Embed(format!("Bad embed JSON: {e}")))?;
        let embeddings = json["embeddings"]
            .as_array()
            .ok_or_else(|| MemoryError::Embed("Missing embeddings array".into()))?;
        if embeddings.len() != texts.len() {
            return Err(MemoryError::Embed(format!(
                "Expected {} embeddings, got {}",
                texts.len(),
                embeddings.len()
            )));
        }
        let mut out = Vec::with_capacity(embeddings.len());
        for emb in embeddings {
            let arr = emb
                .as_array()
                .ok_or_else(|| MemoryError::Embed("Embedding not an array".into()))?;
            out.push(
                arr.iter()
                    .map(|v| v.as_f64().unwrap_or(0.0) as f32)
                    .collect(),
            );
        }
        Ok(out)
    }

    pub async fn embed_one(&self, text: &str) -> Result<Vec<f32>, MemoryError> {
        let mut v = self.embed_texts(&[text.to_string()]).await?;
        v.pop()
            .ok_or_else(|| MemoryError::Embed("Empty embed response".into()))
    }

    /// Full catch-up reindex (hash-skips unchanged chunks unless `force`).
    pub async fn reindex_all(
        &self,
        pool: &SqlitePool,
        force: bool,
    ) -> Result<ReindexStats, MemoryError> {
        // Fail fast if the embedding model is missing — avoid per-chunk 404 spam.
        self.embed_one("chotu memory probe")
            .await
            .map_err(|e| {
                MemoryError::Embed(format!(
                    "{e}. Pull it with: ollama pull {}",
                    self.embed_model
                ))
            })?;

        let mut stats = ReindexStats::default();
        let mut seen: HashSet<(String, String)> = HashSet::new();

        match self.index_personal_references(pool, force, &mut seen, &mut stats).await {
            Ok(()) => {}
            Err(e) => {
                eprintln!("Memory reindex: personal_references failed: {e}");
                stats.errors += 1;
            }
        }
        match self.index_tasks(pool, force, &mut seen, &mut stats).await {
            Ok(()) => {}
            Err(e) => {
                eprintln!("Memory reindex: tasks failed: {e}");
                stats.errors += 1;
            }
        }
        match self
            .index_markdown_tree(pool, force, &mut seen, &mut stats)
            .await
        {
            Ok(()) => {}
            Err(e) => {
                eprintln!("Memory reindex: brain files failed: {e}");
                stats.errors += 1;
            }
        }

        // Drop orphaned chunks.
        let existing: Vec<(String, String, String)> = sqlx::query_as(
            "SELECT id, source_type, source_id FROM memory_chunks",
        )
        .fetch_all(pool)
        .await?;
        for (id, st, sid) in existing {
            if !seen.contains(&(st, sid)) {
                sqlx::query("DELETE FROM memory_chunks WHERE id = ?")
                    .bind(&id)
                    .execute(pool)
                    .await?;
                stats.deleted += 1;
            }
        }

        Ok(stats)
    }

    async fn index_personal_references(
        &self,
        pool: &SqlitePool,
        force: bool,
        seen: &mut HashSet<(String, String)>,
        stats: &mut ReindexStats,
    ) -> Result<(), MemoryError> {
        let rows: Vec<(String, String, Option<String>, String, Option<String>)> = sqlx::query_as(
            "SELECT id, title, url, notes, timestamp FROM personal_references",
        )
        .fetch_all(pool)
        .await?;

        for (id, title, url, notes, timestamp) in rows {
            let body = format_personal_ref_body(&title, url.as_deref(), &notes);
            let chunk = PendingChunk {
                source_type: SourceType::PersonalReference,
                source_id: id.clone(),
                title,
                body,
                url,
                occurred_at: timestamp,
                owner_member_id: None,
            };
            seen.insert((
                chunk.source_type.as_str().to_string(),
                chunk.source_id.clone(),
            ));
            self.upsert_pending(pool, &chunk, force, stats).await?;
        }
        Ok(())
    }

    async fn index_tasks(
        &self,
        pool: &SqlitePool,
        force: bool,
        seen: &mut HashSet<(String, String)>,
        stats: &mut ReindexStats,
    ) -> Result<(), MemoryError> {
        let rows: Vec<(
            String,
            String,
            Option<String>,
            String,
            Option<String>,
            Option<String>,
            Option<String>,
        )> = sqlx::query_as(
            "SELECT id, title, description, status, due_date, assigned_to, created_at FROM tasks",
        )
        .fetch_all(pool)
        .await?;

        for (id, title, description, status, due_date, assigned_to, created_at) in rows {
            let body = format_task_body(
                &title,
                description.as_deref(),
                &status,
                due_date.as_deref(),
                assigned_to.as_deref(),
            );
            let chunk = PendingChunk {
                source_type: SourceType::Task,
                source_id: id.clone(),
                title,
                body,
                url: None,
                occurred_at: due_date.or(created_at),
                owner_member_id: assigned_to,
            };
            seen.insert((
                chunk.source_type.as_str().to_string(),
                chunk.source_id.clone(),
            ));
            self.upsert_pending(pool, &chunk, force, stats).await?;
        }
        Ok(())
    }

    async fn index_markdown_tree(
        &self,
        pool: &SqlitePool,
        force: bool,
        seen: &mut HashSet<(String, String)>,
        stats: &mut ReindexStats,
    ) -> Result<(), MemoryError> {
        let root = brain_dir();
        let journal_root = root.join("Journal");
        if journal_root.is_dir() {
            let files = collect_md_files(&journal_root)?;
            for path in files {
                let rel = path
                    .strip_prefix(&root)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .to_string();
                let content = match tokio::fs::read_to_string(&path).await {
                    Ok(c) => c,
                    Err(e) => {
                        eprintln!("Memory: skip journal {:?}: {e}", path);
                        stats.errors += 1;
                        continue;
                    }
                };
                let date = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .map(|s| s.to_string());
                let chunks = chunk_journal(&rel, &content, date.as_deref());
                for chunk in chunks {
                    seen.insert((
                        chunk.source_type.as_str().to_string(),
                        chunk.source_id.clone(),
                    ));
                    self.upsert_pending(pool, &chunk, force, stats).await?;
                }
            }
        }

        let readings_root = root.join("Readings");
        if readings_root.is_dir() {
            let files = collect_md_files(&readings_root)?;
            for path in files {
                let name = path
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("")
                    .to_string();
                if !name.starts_with("digest-") {
                    continue;
                }
                let rel = path
                    .strip_prefix(&root)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .to_string();
                let content = match tokio::fs::read_to_string(&path).await {
                    Ok(c) => c,
                    Err(e) => {
                        eprintln!("Memory: skip digest {:?}: {e}", path);
                        stats.errors += 1;
                        continue;
                    }
                };
                let date = name
                    .trim_start_matches("digest-")
                    .trim_end_matches(".md")
                    .to_string();
                let chunks = chunk_digest(&rel, &content, &date);
                for chunk in chunks {
                    seen.insert((
                        chunk.source_type.as_str().to_string(),
                        chunk.source_id.clone(),
                    ));
                    self.upsert_pending(pool, &chunk, force, stats).await?;
                }
            }
        }

        Ok(())
    }

    async fn upsert_pending(
        &self,
        pool: &SqlitePool,
        chunk: &PendingChunk,
        force: bool,
        stats: &mut ReindexStats,
    ) -> Result<(), MemoryError> {
        let hash = content_hash(&chunk.body);
        if !force {
            let existing: Option<(String,)> = sqlx::query_as(
                "SELECT content_hash FROM memory_chunks WHERE source_type = ? AND source_id = ?",
            )
            .bind(chunk.source_type.as_str())
            .bind(&chunk.source_id)
            .fetch_optional(pool)
            .await?;
            if let Some((h,)) = existing {
                if h == hash {
                    // Backfill owner without re-embedding when the body is unchanged.
                    sqlx::query(
                        "UPDATE memory_chunks SET owner_member_id = ? \
                         WHERE source_type = ? AND source_id = ?",
                    )
                    .bind(chunk.owner_member_id.as_deref())
                    .bind(chunk.source_type.as_str())
                    .bind(&chunk.source_id)
                    .execute(pool)
                    .await?;
                    stats.skipped += 1;
                    return Ok(());
                }
            }
        }

        let embedding = match self.embed_one(&chunk.body).await {
            Ok(v) => v,
            Err(e) => {
                let msg = e.to_string();
                // Model missing / unreachable: abort the whole reindex instead of spamming.
                if msg.contains("404") || msg.contains("not found") || msg.contains("Connection") {
                    return Err(e);
                }
                eprintln!(
                    "Memory: embed failed for {}/{}: {e}",
                    chunk.source_type.as_str(),
                    chunk.source_id
                );
                stats.errors += 1;
                return Ok(());
            }
        };

        self.upsert_chunk(
            pool,
            chunk.source_type,
            &chunk.source_id,
            &chunk.title,
            &chunk.body,
            chunk.url.as_deref(),
            chunk.occurred_at.as_deref(),
            chunk.owner_member_id.as_deref(),
            &hash,
            &embedding,
        )
        .await?;
        stats.upserted += 1;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn upsert_chunk(
        &self,
        pool: &SqlitePool,
        source_type: SourceType,
        source_id: &str,
        title: &str,
        body: &str,
        url: Option<&str>,
        occurred_at: Option<&str>,
        owner_member_id: Option<&str>,
        content_hash: &str,
        embedding: &[f32],
    ) -> Result<(), MemoryError> {
        let id = uuid::Uuid::new_v4().to_string();
        let now = Utc::now().to_rfc3339();
        let blob = pack_f32(embedding);
        sqlx::query(
            "INSERT INTO memory_chunks \
             (id, source_type, source_id, title, body, url, occurred_at, owner_member_id, content_hash, embedding, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(source_type, source_id) DO UPDATE SET \
               title = excluded.title, \
               body = excluded.body, \
               url = excluded.url, \
               occurred_at = excluded.occurred_at, \
               owner_member_id = excluded.owner_member_id, \
               content_hash = excluded.content_hash, \
               embedding = excluded.embedding, \
               updated_at = excluded.updated_at",
        )
        .bind(&id)
        .bind(source_type.as_str())
        .bind(source_id)
        .bind(title)
        .bind(body)
        .bind(url)
        .bind(occurred_at)
        .bind(owner_member_id)
        .bind(content_hash)
        .bind(&blob)
        .bind(&now)
        .execute(pool)
        .await?;
        Ok(())
    }

    /// Best-effort index of a single personal reference (non-fatal for callers).
    pub async fn index_personal_reference(
        &self,
        pool: &SqlitePool,
        id: &str,
        title: &str,
        url: Option<&str>,
        notes: &str,
        timestamp: Option<&str>,
    ) -> Result<(), MemoryError> {
        let body = format_personal_ref_body(title, url, notes);
        let hash = content_hash(&body);
        let emb = self.embed_one(&body).await?;
        self.upsert_chunk(
            pool,
            SourceType::PersonalReference,
            id,
            title,
            &body,
            url,
            timestamp,
            None,
            &hash,
            &emb,
        )
        .await
    }

    /// Best-effort index of a single task.
    pub async fn index_task(
        &self,
        pool: &SqlitePool,
        id: &str,
        title: &str,
        description: Option<&str>,
        status: &str,
        due_date: Option<&str>,
        assigned_to: Option<&str>,
        created_at: Option<&str>,
    ) -> Result<(), MemoryError> {
        let body = format_task_body(title, description, status, due_date, assigned_to);
        let hash = content_hash(&body);
        let emb = self.embed_one(&body).await?;
        self.upsert_chunk(
            pool,
            SourceType::Task,
            id,
            title,
            &body,
            None,
            due_date.or(created_at),
            assigned_to,
            &hash,
            &emb,
        )
        .await
    }

    /// Best-effort reindex of one journal markdown file.
    pub async fn index_journal_file(
        &self,
        pool: &SqlitePool,
        path: &Path,
    ) -> Result<(), MemoryError> {
        let root = brain_dir();
        let rel = path
            .strip_prefix(&root)
            .unwrap_or(path)
            .to_string_lossy()
            .to_string();
        let content = tokio::fs::read_to_string(path).await?;
        let date = path
            .file_stem()
            .and_then(|s| s.to_str())
            .map(|s| s.to_string());
        let chunks = chunk_journal(&rel, &content, date.as_deref());
        let mut stats = ReindexStats::default();
        for chunk in chunks {
            self.upsert_pending(pool, &chunk, true, &mut stats).await?;
        }
        Ok(())
    }

    /// Embed query and return top-k cosine hits.
    ///
    /// `for_member_id` scopes a linked personal DM: that member's owned chunks
    /// plus unassigned tasks. Household / unlinked chats pass `None` (all rows).
    pub async fn search(
        &self,
        pool: &SqlitePool,
        query: &str,
        top_k: Option<usize>,
        for_member_id: Option<&str>,
    ) -> Result<Vec<MemoryHit>, MemoryError> {
        let k = top_k.unwrap_or(DEFAULT_TOP_K);
        let q = self.embed_one(query.trim()).await?;
        let rows = fetch_scoped_memory_rows(pool, for_member_id).await?;

        let mut scored: Vec<MemoryHit> = Vec::with_capacity(rows.len());
        for (st, sid, title, body, url, occurred_at, blob) in rows {
            let Some(source_type) = SourceType::parse(&st) else {
                continue;
            };
            let emb = unpack_f32(&blob);
            if emb.is_empty() || emb.len() != q.len() {
                continue;
            }
            let mut score = cosine_similarity(&q, &emb);
            // Light recency boost for items within the last year.
            if let Some(ref d) = occurred_at {
                if let Some(days) = days_ago(d) {
                    if days <= 365.0 {
                        score += 0.02 * (1.0 - (days / 365.0) as f32);
                    }
                }
            }
            scored.push(MemoryHit {
                source_type,
                source_id: sid,
                title,
                body,
                url,
                occurred_at,
                score,
            });
        }
        scored.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        scored.truncate(k);
        Ok(scored)
    }
}

type MemoryRow = (
    String,
    String,
    String,
    String,
    Option<String>,
    Option<String>,
    Vec<u8>,
);

async fn fetch_scoped_memory_rows(
    pool: &SqlitePool,
    for_member_id: Option<&str>,
) -> Result<Vec<MemoryRow>, MemoryError> {
    let mut qb = QueryBuilder::new(
        "SELECT source_type, source_id, title, body, url, occurred_at, embedding \
         FROM memory_chunks",
    );
    if let Some(mid) = for_member_id {
        qb.push(" WHERE (owner_member_id COLLATE NOCASE = ");
        qb.push_bind(mid);
        qb.push(" OR (owner_member_id IS NULL AND source_type = 'task'))");
    }
    Ok(qb.build_query_as::<MemoryRow>().fetch_all(pool).await?)
}

/// Linked DM (`Some`): own chunks or unassigned tasks. Household (`None`): all.
pub fn memory_chunk_in_scope(
    owner_member_id: Option<&str>,
    source_type: SourceType,
    for_member_id: Option<&str>,
) -> bool {
    match for_member_id {
        None => true,
        Some(mid) => match owner_member_id {
            Some(owner) => owner.eq_ignore_ascii_case(mid),
            None => source_type == SourceType::Task,
        },
    }
}

/// Synthesize a grounded answer: local Ollama first, optional Gemini fallback, else hit list.
pub async fn answer_memory_query(
    ollama: Option<&ChotuLlm>,
    gemini: Option<&GeminiClient>,
    query: &str,
    hits: &[MemoryHit],
) -> Result<String, LlmError> {
    if hits.is_empty() {
        return Ok(
            "I couldn't find anything relevant in journals, digests, personal references, or tasks."
                .to_string(),
        );
    }

    // Keep the prompt small — 9B models get slow on long context, especially after embed model swap.
    let context = format_hits_for_prompt(&hits[..hits.len().min(5)]);
    let system_prompt = "\
You are Chotu's personal memory assistant. Answer ONLY from the retrieved snippets. \
If insufficient, say so. Cite briefly like [journal: 2026-06-07] or [personal_reference: Title]. \
Max 120 words. Telegram Markdown sparingly. No preamble.";
    let user_prompt = format!("Question: {query}\n\nMemories:\n{context}");

    if let Some(llm) = ollama {
        let gen = llm.generate_prompt_fast(system_prompt, &user_prompt);
        match tokio::time::timeout(std::time::Duration::from_secs(45), gen).await {
            Ok(Ok(text)) => {
                let cleaned = strip_think_blocks(text.trim());
                if !cleaned.is_empty() {
                    return Ok(cleaned);
                }
            }
            Ok(Err(e)) => {
                eprintln!("Memory: Ollama synthesis failed, trying Gemini/hit list: {e}");
            }
            Err(_) => {
                eprintln!(
                    "Memory: Ollama synthesis timed out after 45s — falling back to hit list"
                );
                return Ok(format!(
                    "⏱️ Local model took too long — here are the top matches instead:\n\n{}",
                    format_hit_list(hits)
                ));
            }
        }
    }

    if let Some(client) = gemini {
        let combined = format!("{system_prompt}\n\n{user_prompt}");
        match client.ask(&combined).await {
            Ok(text) if !text.trim().is_empty() => return Ok(text.trim().to_string()),
            Ok(_) => {}
            Err(e) => {
                eprintln!("Memory: Gemini synthesis failed, falling back to hit list: {e}");
            }
        }
    }

    Ok(format_hit_list(hits))
}

fn format_hits_for_prompt(hits: &[MemoryHit]) -> String {
    hits.iter()
        .enumerate()
        .map(|(i, h)| {
            let when = h.occurred_at.as_deref().unwrap_or("?");
            let url = h
                .url
                .as_ref()
                .map(|u| format!(" | {u}"))
                .unwrap_or_default();
            let snippet: String = h.body.chars().take(350).collect();
            format!(
                "[{}] ({}) {} ({}){}\n{}",
                i + 1,
                h.source_type.as_str(),
                h.title,
                when,
                url,
                snippet
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Strip Qwen-style think tags if present.
fn strip_think_blocks(text: &str) -> String {
    let mut result = text.to_string();
    while let Some(start) = result.find("<think>") {
        if let Some(end_rel) = result[start..].find("</think>") {
            let end = start + end_rel + "</think>".len();
            result.replace_range(start..end, "");
        } else {
            break;
        }
    }
    result.trim().to_string()
}

pub fn format_hit_list(hits: &[MemoryHit]) -> String {
    if hits.is_empty() {
        return "No matching memories.".to_string();
    }
    let mut out = String::from("🔍 *Memory matches*\n");
    for (i, h) in hits.iter().take(8).enumerate() {
        let when = h.occurred_at.as_deref().unwrap_or("?");
        let snippet: String = h
            .body
            .chars()
            .take(140)
            .collect::<String>()
            .replace('\n', " ");
        out.push_str(&format!(
            "\n{}. *{}* _{}_ ({}, score {:.2})\n   {}\n",
            i + 1,
            escape_md(h.title.chars().take(80).collect::<String>().as_str()),
            when,
            h.source_type.as_str(),
            h.score,
            escape_md(&snippet)
        ));
        if let Some(url) = &h.url {
            out.push_str(&format!("   {}\n", url));
        }
    }
    out
}

fn escape_md(s: &str) -> String {
    s.replace('*', "\\*")
        .replace('_', "\\_")
        .replace('`', "\\`")
        .replace('[', "\\[")
}

fn format_personal_ref_body(title: &str, url: Option<&str>, notes: &str) -> String {
    format!(
        "Personal reference: {title}\nURL: {}\nNotes: {notes}",
        url.unwrap_or("N/A")
    )
}

fn format_task_body(
    title: &str,
    description: Option<&str>,
    status: &str,
    due_date: Option<&str>,
    assigned_to: Option<&str>,
) -> String {
    format!(
        "Task ({status}): {title}\nDescription: {}\nDue: {}\nAssigned: {}",
        description.unwrap_or(""),
        due_date.unwrap_or("none"),
        assigned_to.unwrap_or("unassigned")
    )
}

fn content_hash(body: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(body.as_bytes());
    let dig = hasher.finalize();
    dig.iter().map(|b| format!("{b:02x}")).collect()
}

fn pack_f32(v: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len() * 4);
    for f in v {
        out.extend_from_slice(&f.to_le_bytes());
    }
    out
}

fn unpack_f32(b: &[u8]) -> Vec<f32> {
    b.chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0f32;
    let mut na = 0.0f32;
    let mut nb = 0.0f32;
    for i in 0..a.len() {
        dot += a[i] * b[i];
        na += a[i] * a[i];
        nb += b[i] * b[i];
    }
    let denom = na.sqrt() * nb.sqrt();
    if denom == 0.0 {
        0.0
    } else {
        dot / denom
    }
}

fn days_ago(date_str: &str) -> Option<f64> {
    let date_part = date_str.get(0..10)?;
    let d = chrono::NaiveDate::parse_from_str(date_part, "%Y-%m-%d").ok()?;
    let today = chrono::Local::now().date_naive();
    Some((today - d).num_days() as f64)
}

fn collect_md_files(dir: &Path) -> Result<Vec<PathBuf>, MemoryError> {
    let mut out = Vec::new();
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), MemoryError> {
        let entries = std::fs::read_dir(dir)?;
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                walk(&path, out)?;
            } else if path
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| e.eq_ignore_ascii_case("md"))
                .unwrap_or(false)
            {
                out.push(path);
            }
        }
        Ok(())
    }
    walk(dir, &mut out)?;
    out.sort();
    Ok(out)
}

fn strip_yaml_frontmatter(content: &str) -> &str {
    let trimmed = content.trim_start();
    if let Some(rest) = trimmed.strip_prefix("---") {
        if let Some(end) = rest.find("\n---") {
            let after = &rest[end + 4..];
            return after.trim_start_matches('\n');
        }
    }
    content
}

fn extract_journal_response(content: &str) -> String {
    let body = strip_yaml_frontmatter(content);
    if let Some(idx) = body.find("## Response") {
        let after = &body[idx + "## Response".len()..];
        let after = after.trim_start_matches(|c: char| c == '\n' || c == '\r' || c == ' ');
        // Stop at next top-level ## if present
        if let Some(next) = after.find("\n## ") {
            after[..next].trim().to_string()
        } else {
            after.trim().to_string()
        }
    } else {
        body.trim().to_string()
    }
}

fn parse_journal_owner(content: &str) -> Option<String> {
    let trimmed = content.trim_start();
    let rest = trimmed.strip_prefix("---")?;
    let end = rest.find("\n---")?;
    let fm = &rest[..end];
    let mut member = None;
    let mut member_id = None;
    for line in fm.lines() {
        let line = line.trim();
        if let Some(val) = yaml_map_value(line, "member_id") {
            if !val.is_empty() {
                member_id = Some(val);
            }
        } else if let Some(val) = yaml_map_value(line, "member") {
            if !val.is_empty() {
                member = Some(val);
            }
        }
    }
    member_id.or(member)
}

fn yaml_map_value(line: &str, key: &str) -> Option<String> {
    let rest = line.strip_prefix(key)?.strip_prefix(':')?;
    let v = rest
        .trim()
        .trim_matches('"')
        .trim_matches('\'')
        .trim();
    Some(v.to_string())
}

fn chunk_journal(rel_path: &str, content: &str, date: Option<&str>) -> Vec<PendingChunk> {
    let text = extract_journal_response(content);
    if text.is_empty() {
        return Vec::new();
    }
    let owner = parse_journal_owner(content);
    let title = format!("Journal {}", date.unwrap_or(rel_path));
    split_text(&text, CHUNK_TARGET_CHARS, CHUNK_OVERLAP_CHARS)
        .into_iter()
        .enumerate()
        .map(|(i, part)| PendingChunk {
            source_type: SourceType::Journal,
            source_id: format!("{rel_path}#{i}"),
            title: if i == 0 {
                title.clone()
            } else {
                format!("{title} (part {})", i + 1)
            },
            body: format!("{title}\n{part}"),
            url: None,
            occurred_at: date.map(|d| d.to_string()),
            owner_member_id: owner.clone(),
        })
        .collect()
}

fn chunk_digest(rel_path: &str, content: &str, date: &str) -> Vec<PendingChunk> {
    let mut sections: Vec<(String, String)> = Vec::new();
    let mut current_title: Option<String> = None;
    let mut current_body = String::new();

    for line in content.lines() {
        if let Some(rest) = line.strip_prefix("## ") {
            if let Some(title) = current_title.take() {
                if !current_body.trim().is_empty() {
                    sections.push((title, current_body.clone()));
                }
            }
            current_title = Some(rest.trim().to_string());
            current_body.clear();
        } else if current_title.is_some() {
            current_body.push_str(line);
            current_body.push('\n');
        }
    }
    if let Some(title) = current_title {
        if !current_body.trim().is_empty() {
            sections.push((title, current_body));
        }
    }

    sections
        .into_iter()
        .enumerate()
        .filter(|(_, (_, body))| body.trim().len() > 20)
        .map(|(i, (title, body))| PendingChunk {
            source_type: SourceType::Digest,
            source_id: format!("{rel_path}#{i}"),
            title: title.clone(),
            body: format!("Newsletter digest ({date}): {title}\n{}", body.trim()),
            url: None,
            occurred_at: Some(date.to_string()),
            owner_member_id: None,
        })
        .collect()
}

fn split_text(text: &str, target: usize, overlap: usize) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= target {
        return vec![text.to_string()];
    }
    let mut out = Vec::new();
    let mut start = 0;
    while start < chars.len() {
        let end = (start + target).min(chars.len());
        out.push(chars[start..end].iter().collect());
        if end == chars.len() {
            break;
        }
        start = end.saturating_sub(overlap);
        if start >= chars.len() {
            break;
        }
    }
    out
}

/// Spawn a background catch-up reindex (hash-skip). Errors are logged only.
pub fn spawn_background_reindex(pool: SqlitePool) {
    tokio::spawn(async move {
        let index = MemoryIndex::from_env();
        println!(
            "Memory: starting background reindex (model={})...",
            index.embed_model
        );
        match index.reindex_all(&pool, false).await {
            Ok(stats) => {
                println!(
                    "Memory: reindex done — upserted={}, skipped={}, deleted={}, errors={}",
                    stats.upserted, stats.skipped, stats.deleted, stats.errors
                );
            }
            Err(e) => {
                eprintln!("Memory: background reindex failed: {e}");
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cosine_identical() {
        let a = vec![1.0, 0.0, 0.0];
        assert!((cosine_similarity(&a, &a) - 1.0).abs() < 1e-5);
    }

    #[test]
    fn test_pack_unpack() {
        let v = vec![0.1f32, -2.5, 3.0];
        let b = pack_f32(&v);
        let u = unpack_f32(&b);
        assert_eq!(v, u);
    }

    #[test]
    fn test_chunk_digest_sections() {
        let md = "# Daily Newsletter Digest - 2026-06-28\n\n## Rust Weekly\n- **Sender**: a@b.com\n- **Preview**: lots of cool rust crates and compiler news this week\n\n---\n\n## Finimize\n- Preview: markets moved on inflation data and rate expectations today\n";
        let chunks = chunk_digest("Readings/digest-2026-06-28.md", md, "2026-06-28");
        assert!(chunks.len() >= 2, "got {} chunks: {:?}", chunks.len(), chunks.iter().map(|c| &c.title).collect::<Vec<_>>());
        assert!(chunks[0].title.contains("Rust Weekly"));
    }

    #[test]
    fn test_extract_journal_response() {
        let md = "---\ndate: 2026-06-07\n---\n\n# Evening Reflection\n## Prompt\nHow was today?\n## Response\nFelt good about the interview.\n";
        let resp = extract_journal_response(md);
        assert!(resp.contains("interview"));
        assert!(!resp.contains("Evening Reflection"));
    }

    #[test]
    fn test_content_hash_stable() {
        assert_eq!(content_hash("abc"), content_hash("abc"));
        assert_ne!(content_hash("abc"), content_hash("abd"));
    }

    #[test]
    fn memory_chunk_in_scope_household_sees_all() {
        assert!(memory_chunk_in_scope(
            Some("jordan"),
            SourceType::Task,
            None
        ));
        assert!(memory_chunk_in_scope(None, SourceType::Journal, None));
        assert!(memory_chunk_in_scope(
            None,
            SourceType::PersonalReference,
            None
        ));
    }

    #[test]
    fn memory_chunk_in_scope_linked_dm() {
        assert!(memory_chunk_in_scope(
            Some("alex"),
            SourceType::Task,
            Some("alex")
        ));
        assert!(memory_chunk_in_scope(
            Some("ALEX"),
            SourceType::Journal,
            Some("alex")
        ));
        assert!(!memory_chunk_in_scope(
            Some("jordan"),
            SourceType::Task,
            Some("alex")
        ));
        assert!(memory_chunk_in_scope(None, SourceType::Task, Some("alex")));
        assert!(!memory_chunk_in_scope(
            None,
            SourceType::Journal,
            Some("alex")
        ));
        assert!(!memory_chunk_in_scope(
            None,
            SourceType::Digest,
            Some("alex")
        ));
        assert!(!memory_chunk_in_scope(
            None,
            SourceType::PersonalReference,
            Some("alex")
        ));
    }

    #[test]
    fn parse_journal_owner_from_frontmatter() {
        let md = "---\ndate: 2026-06-07\nmember: alex\n---\n\n# Evening Reflection\n## Prompt\nHow was today?\n## Response\nFelt good about the interview.\n";
        assert_eq!(parse_journal_owner(md).as_deref(), Some("alex"));
        let chunks = chunk_journal("Journal/x.md", md, Some("2026-06-07"));
        assert_eq!(chunks[0].owner_member_id.as_deref(), Some("alex"));
    }

    #[test]
    fn parse_journal_owner_prefers_member_id() {
        let md = "---\nmember: alex\nmember_id: jordan\n---\n## Response\nhi\n";
        assert_eq!(parse_journal_owner(md).as_deref(), Some("jordan"));
    }

    #[test]
    fn parse_journal_owner_absent_is_household() {
        let md = "---\ndate: 2026-06-07\n---\n## Response\nhi\n";
        assert_eq!(parse_journal_owner(md), None);
        let chunks = chunk_journal("Journal/x.md", md, Some("2026-06-07"));
        assert_eq!(chunks[0].owner_member_id, None);
    }

    #[test]
    fn chunk_digest_has_no_owner() {
        let md = "# Daily Newsletter Digest - 2026-06-28\n\n## Rust Weekly\n- **Sender**: a@b.com\n- **Preview**: lots of cool rust crates and compiler news this week\n";
        let chunks = chunk_digest("Readings/digest-2026-06-28.md", md, "2026-06-28");
        assert!(chunks.iter().all(|c| c.owner_member_id.is_none()));
    }

    async fn insert_chunk(
        pool: &SqlitePool,
        source_type: &str,
        source_id: &str,
        owner: Option<&str>,
    ) {
        sqlx::query(
            "INSERT INTO memory_chunks \
             (id, source_type, source_id, title, body, url, occurred_at, owner_member_id, content_hash, embedding, updated_at) \
             VALUES (?, ?, ?, ?, ?, NULL, NULL, ?, 'h', ?, 'now')",
        )
        .bind(uuid::Uuid::new_v4().to_string())
        .bind(source_type)
        .bind(source_id)
        .bind(source_id)
        .bind("body")
        .bind(owner)
        .bind(pack_f32(&[1.0]))
        .execute(pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn fetch_scoped_hides_other_member_and_household_journal() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::query(
            "CREATE TABLE memory_chunks (
                id TEXT PRIMARY KEY NOT NULL,
                source_type TEXT NOT NULL,
                source_id TEXT NOT NULL,
                title TEXT NOT NULL,
                body TEXT NOT NULL,
                url TEXT,
                occurred_at TEXT,
                owner_member_id TEXT,
                content_hash TEXT NOT NULL,
                embedding BLOB NOT NULL,
                updated_at TEXT NOT NULL
            )",
        )
        .execute(&pool)
        .await
        .unwrap();

        insert_chunk(&pool, "task", "alex-task", Some("alex")).await;
        insert_chunk(&pool, "task", "jordan-task", Some("jordan")).await;
        insert_chunk(&pool, "task", "unassigned-task", None).await;
        insert_chunk(&pool, "journal", "hh-journal", None).await;

        let alex = fetch_scoped_memory_rows(&pool, Some("alex"))
            .await
            .unwrap();
        let ids: Vec<&str> = alex.iter().map(|r| r.1.as_str()).collect();
        assert!(ids.contains(&"alex-task"));
        assert!(ids.contains(&"unassigned-task"));
        assert!(!ids.contains(&"jordan-task"));
        assert!(!ids.contains(&"hh-journal"));

        let alex_upper = fetch_scoped_memory_rows(&pool, Some("ALEX"))
            .await
            .unwrap();
        let upper_ids: Vec<&str> = alex_upper.iter().map(|r| r.1.as_str()).collect();
        assert!(upper_ids.contains(&"alex-task"));
        assert!(!upper_ids.contains(&"jordan-task"));

        let all = fetch_scoped_memory_rows(&pool, None).await.unwrap();
        assert_eq!(all.len(), 4);
    }
}
