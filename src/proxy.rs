use std::time::{Instant, SystemTime, UNIX_EPOCH};

use axum::{
    Extension, Json,
    body::{Body, Bytes},
    extract::{Request, State},
    http::{HeaderValue, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use serde_json::{Value, json};

use crate::{
    AppState,
    db::{Completion, NewRequest},
    metrics::OllamaMetrics,
};

const MAX_MODEL_NAME_LEN: usize = 256;

/// Name of the API key that authenticated the current request.
#[derive(Clone)]
pub struct ClientName(String);

fn api_error(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "error": message }))).into_response()
}

pub async fn require_api_key(
    State(state): State<AppState>,
    mut request: Request,
    next: Next,
) -> Response {
    let client = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .and_then(|token| state.api_keys.authenticate(token))
        .map(str::to_owned);

    let Some(client) = client else {
        tracing::warn!(
            path = request.uri().path(),
            "rejected request: invalid API key"
        );
        let mut response = api_error(StatusCode::UNAUTHORIZED, "invalid or missing API key");
        response
            .headers_mut()
            .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
        return response;
    };
    request.extensions_mut().insert(ClientName(client));
    next.run(request).await
}

pub async fn chat(state: State<AppState>, client: Extension<ClientName>, body: Bytes) -> Response {
    forward(state, client, "/api/chat", body).await
}

pub async fn generate(
    state: State<AppState>,
    client: Extension<ClientName>,
    body: Bytes,
) -> Response {
    forward(state, client, "/api/generate", body).await
}

struct Prepared {
    model: String,
    upstream_body: Vec<u8>,
}

/// Validates the body and builds what is sent upstream. Streaming is refused
/// rather than silently downgraded, and turned off when the client left it
/// unset, because Ollama streams by default.
fn prepare(body: &[u8]) -> Result<Prepared, &'static str> {
    let mut value: Value = serde_json::from_slice(body).map_err(|_| "body must be valid JSON")?;
    let object = value.as_object_mut().ok_or("body must be a JSON object")?;
    let model = object
        .get("model")
        .and_then(Value::as_str)
        .filter(|model| !model.is_empty() && model.len() <= MAX_MODEL_NAME_LEN)
        .ok_or("'model' must be a non-empty model name")?
        .to_owned();
    if object.get("stream").and_then(Value::as_bool) == Some(true) {
        return Err("streaming is not supported; omit 'stream' or set it to false");
    }
    object.insert("stream".into(), Value::Bool(false));

    let upstream_body = serde_json::to_vec(&value).expect("a JSON value always serializes");
    Ok(Prepared {
        model,
        upstream_body,
    })
}

async fn forward(
    State(state): State<AppState>,
    Extension(ClientName(client)): Extension<ClientName>,
    endpoint: &'static str,
    body: Bytes,
) -> Response {
    let prepared = match prepare(&body) {
        Ok(prepared) => prepared,
        Err(message) => return api_error(StatusCode::BAD_REQUEST, message),
    };
    // Axum drops the handler if the client disconnects. A separate task lets
    // the exchange finish, so the row never stays pending and the response
    // Ollama already computed still gets stored.
    let task = tokio::spawn(process(state, client, endpoint, body, prepared));
    match task.await {
        Ok(response) => response,
        Err(error) => {
            tracing::error!(%error, "request task failed");
            api_error(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
        }
    }
}

async fn process(
    state: AppState,
    client: String,
    endpoint: &'static str,
    body: Bytes,
    prepared: Prepared,
) -> Response {
    let created_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as i64);
    let request_bytes = body.len() as i64;

    // Fail closed: nothing reaches Ollama unless the request is on record.
    let stored = async {
        let request_enc = state.crypto.encrypt(body.to_vec()).await?;
        state
            .db
            .insert(NewRequest {
                created_at,
                client: &client,
                endpoint,
                model: &prepared.model,
                request_enc: &request_enc,
                request_bytes,
            })
            .await
    }
    .await;
    let id = match stored {
        Ok(id) => id,
        Err(error) => {
            tracing::error!(%error, "could not store request; not forwarding it");
            return api_error(StatusCode::INTERNAL_SERVER_ERROR, "could not store request");
        }
    };

    let started = Instant::now();
    let upstream = call_ollama(&state, endpoint, prepared.upstream_body).await;
    let latency_ms = started.elapsed().as_millis() as i64;

    let (completion, response) = match upstream {
        Ok((status, upstream_body)) => {
            let response_enc = match state.crypto.encrypt(upstream_body.to_vec()).await {
                Ok(sealed) => Some(sealed),
                Err(error) => {
                    tracing::error!(request_id = id, %error, "could not encrypt response");
                    None
                }
            };
            let error = match (response_enc.is_some(), status.is_success()) {
                (false, _) => Some("response_not_stored"),
                (true, false) => Some("upstream_error"),
                (true, true) => None,
            };
            let completion = Completion {
                status: if error.is_none() {
                    "completed"
                } else {
                    "failed"
                },
                http_status: Some(i64::from(status.as_u16())),
                error,
                response_enc,
                response_bytes: Some(upstream_body.len() as i64),
                latency_ms,
                ollama: OllamaMetrics::from_response(&upstream_body),
            };
            let response = Response::builder()
                .status(status)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(upstream_body))
                .expect("status and static header are valid");
            (completion, response)
        }
        Err(failure) => {
            tracing::error!(
                request_id = id,
                error = failure.category,
                "Ollama call failed"
            );
            let completion = Completion {
                status: "failed",
                http_status: Some(i64::from(failure.status.as_u16())),
                error: Some(failure.category),
                response_enc: None,
                response_bytes: None,
                latency_ms,
                ollama: OllamaMetrics::default(),
            };
            (completion, api_error(failure.status, failure.message))
        }
    };

    // The inference already happened and the request is on record, so a failure
    // here is logged but does not cost the client its answer.
    if let Err(error) = state.db.complete(id, completion).await {
        tracing::error!(request_id = id, %error, "could not store response");
    }
    tracing::info!(
        request_id = id,
        client,
        endpoint,
        model = prepared.model,
        latency_ms,
        "request handled"
    );
    response
}

struct UpstreamFailure {
    status: StatusCode,
    category: &'static str,
    message: &'static str,
}

async fn call_ollama(
    state: &AppState,
    endpoint: &str,
    body: Vec<u8>,
) -> Result<(StatusCode, Bytes), UpstreamFailure> {
    let url = format!("{}{endpoint}", state.ollama_url);
    let classify = |error: reqwest::Error| {
        if error.is_timeout() {
            UpstreamFailure {
                status: StatusCode::GATEWAY_TIMEOUT,
                category: "upstream_timeout",
                message: "Ollama did not respond in time",
            }
        } else {
            UpstreamFailure {
                status: StatusCode::BAD_GATEWAY,
                category: "upstream_unavailable",
                message: "Ollama is unavailable",
            }
        }
    };
    let response = state
        .http
        .post(url)
        .header(header::CONTENT_TYPE, "application/json")
        .body(body)
        .send()
        .await
        .map_err(classify)?;
    let status = response.status();
    let bytes = response.bytes().await.map_err(classify)?;
    Ok((status, bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn upstream(body: &str) -> Value {
        let prepared = prepare(body.as_bytes()).ok().unwrap();
        serde_json::from_slice(&prepared.upstream_body).unwrap()
    }

    #[test]
    fn forces_stream_off_and_keeps_other_fields() {
        let value = upstream(r#"{"model":"m","prompt":"hi","options":{"seed":1}}"#);
        assert_eq!(value["stream"], false);
        assert_eq!(value["prompt"], "hi");
        assert_eq!(value["options"]["seed"], 1);
    }

    #[test]
    fn accepts_any_model_name() {
        let prepared = prepare(br#"{"model":"hf.co/org/any-model:Q4_K_M"}"#)
            .ok()
            .unwrap();
        assert_eq!(prepared.model, "hf.co/org/any-model:Q4_K_M");
    }

    #[test]
    fn rejects_invalid_bodies() {
        for body in [
            "not json",
            "[]",
            "{}",
            r#"{"model":""}"#,
            r#"{"model":1}"#,
            r#"{"model":"m","stream":true}"#,
        ] {
            assert!(prepare(body.as_bytes()).is_err(), "{body} accepted");
        }
    }
}
