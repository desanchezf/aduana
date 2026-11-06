use std::str::FromStr;

use anyhow::{Context, Result};
use sqlx::{
    FromRow, SqlitePool,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions},
};

use crate::metrics::OllamaMetrics;

#[derive(FromRow)]
pub struct RequestRow {
    pub id: i64,
    pub created_at: i64,
    pub client: String,
    pub endpoint: String,
    pub model: String,
    pub status: String,
    pub http_status: Option<i64>,
    pub error: Option<String>,
    pub request_enc: Vec<u8>,
    pub response_enc: Option<Vec<u8>>,
    pub request_bytes: i64,
    pub response_bytes: Option<i64>,
    pub latency_ms: Option<i64>,
    pub ollama_total_ms: Option<i64>,
    pub ollama_load_ms: Option<i64>,
    pub prompt_tokens: Option<i64>,
    pub completion_tokens: Option<i64>,
    pub tokens_per_second: Option<f64>,
}

pub struct NewRequest<'a> {
    pub created_at: i64,
    pub client: &'a str,
    pub endpoint: &'a str,
    pub model: &'a str,
    pub request_enc: &'a [u8],
    pub request_bytes: i64,
}

pub struct Completion {
    pub status: &'static str,
    pub http_status: Option<i64>,
    pub error: Option<&'static str>,
    pub response_enc: Option<Vec<u8>>,
    pub response_bytes: Option<i64>,
    pub latency_ms: i64,
    pub ollama: OllamaMetrics,
}

#[derive(Clone)]
pub struct Db(SqlitePool);

impl Db {
    pub async fn connect(url: &str) -> Result<Self> {
        let options = SqliteConnectOptions::from_str(url)
            .context("invalid database URL")?
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal);
        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect_with(options)
            .await
            .context("could not open the database")?;
        sqlx::migrate!()
            .run(&pool)
            .await
            .context("could not apply migrations")?;
        Ok(Self(pool))
    }

    /// Rows still pending at startup lost their handler in a crash or restart.
    pub async fn mark_interrupted(&self) -> Result<u64> {
        let result =
            sqlx::query("UPDATE requests SET status = 'interrupted' WHERE status = 'pending'")
                .execute(&self.0)
                .await?;
        Ok(result.rows_affected())
    }

    pub async fn insert(&self, request: NewRequest<'_>) -> Result<i64> {
        let result = sqlx::query(
            "INSERT INTO requests
                (created_at, client, endpoint, model, status, request_enc, request_bytes)
             VALUES (?, ?, ?, ?, 'pending', ?, ?)",
        )
        .bind(request.created_at)
        .bind(request.client)
        .bind(request.endpoint)
        .bind(request.model)
        .bind(request.request_enc)
        .bind(request.request_bytes)
        .execute(&self.0)
        .await?;
        Ok(result.last_insert_rowid())
    }

    pub async fn complete(&self, id: i64, completion: Completion) -> Result<()> {
        sqlx::query(
            "UPDATE requests SET
                status = ?, http_status = ?, error = ?, response_enc = ?, response_bytes = ?,
                latency_ms = ?, ollama_total_ms = ?, ollama_load_ms = ?, prompt_tokens = ?,
                completion_tokens = ?, tokens_per_second = ?
             WHERE id = ?",
        )
        .bind(completion.status)
        .bind(completion.http_status)
        .bind(completion.error)
        .bind(completion.response_enc)
        .bind(completion.response_bytes)
        .bind(completion.latency_ms)
        .bind(completion.ollama.total_ms)
        .bind(completion.ollama.load_ms)
        .bind(completion.ollama.prompt_tokens)
        .bind(completion.ollama.completion_tokens)
        .bind(completion.ollama.tokens_per_second)
        .bind(id)
        .execute(&self.0)
        .await?;
        Ok(())
    }

    pub async fn count(&self) -> Result<i64> {
        Ok(sqlx::query_scalar("SELECT COUNT(*) FROM requests")
            .fetch_one(&self.0)
            .await?)
    }

    pub async fn list(&self, limit: i64, offset: i64) -> Result<Vec<RequestRow>> {
        Ok(
            sqlx::query_as("SELECT * FROM requests ORDER BY id DESC LIMIT ? OFFSET ?")
                .bind(limit)
                .bind(offset)
                .fetch_all(&self.0)
                .await?,
        )
    }

    pub async fn get(&self, id: i64) -> Result<Option<RequestRow>> {
        Ok(sqlx::query_as("SELECT * FROM requests WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.0)
            .await?)
    }
}
