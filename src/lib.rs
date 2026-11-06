pub mod auth;
pub mod config;
pub mod crypto;
pub mod dashboard;
pub mod db;
pub mod metrics;
pub mod proxy;

use std::sync::Arc;

use anyhow::{Context, Result};
use axum::{
    Router,
    extract::DefaultBodyLimit,
    middleware,
    routing::{get, post},
};

use crate::{
    auth::{AdminCredentials, ApiKeys, Sessions},
    config::Config,
    crypto::Crypto,
    db::Db,
};

#[derive(Clone)]
pub struct AppState {
    api_keys: Arc<ApiKeys>,
    admin: Arc<AdminCredentials>,
    sessions: Arc<Sessions>,
    login_gate: Arc<tokio::sync::Mutex<()>>,
    crypto: Crypto,
    db: Db,
    http: reqwest::Client,
    ollama_url: Arc<str>,
    cookie_secure: bool,
    max_body_bytes: usize,
}

impl AppState {
    pub async fn new(config: Config) -> Result<Self> {
        let db = Db::connect(&config.database_url).await?;
        let interrupted = db.mark_interrupted().await?;
        if interrupted > 0 {
            tracing::warn!(
                interrupted,
                "marked requests left pending by a previous run"
            );
        }
        Ok(Self {
            api_keys: Arc::new(ApiKeys::new(&config.api_keys)),
            admin: Arc::new(AdminCredentials::new(
                &config.admin_user,
                &config.admin_password,
            )),
            sessions: Arc::default(),
            login_gate: Arc::default(),
            crypto: Crypto::new(config.identity),
            db,
            http: reqwest::Client::builder()
                .timeout(config.ollama_timeout)
                .build()
                .context("could not build the HTTP client")?,
            ollama_url: config.ollama_url.trim_end_matches('/').into(),
            cookie_secure: config.cookie_secure,
            max_body_bytes: config.max_body_bytes,
        })
    }
}

pub fn app(state: AppState) -> Router {
    let api = Router::new()
        .route("/api/chat", post(proxy::chat))
        .route("/api/generate", post(proxy::generate))
        .layer(DefaultBodyLimit::max(state.max_body_bytes))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            proxy::require_api_key,
        ));

    let panel = Router::new()
        .route("/", get(dashboard::list))
        .route("/requests/{id}", get(dashboard::detail))
        .route("/logout", post(dashboard::logout))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            dashboard::require_session,
        ));

    let public = Router::new()
        .route("/login", get(dashboard::login_form).post(dashboard::login))
        .route("/static/style.css", get(dashboard::stylesheet));

    Router::new()
        .merge(panel)
        .merge(public)
        .layer(middleware::from_fn(dashboard::security_headers))
        .merge(api)
        .with_state(state)
}
