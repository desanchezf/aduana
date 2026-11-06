use std::{fmt::Display, time::Duration};

use axum::{
    Form,
    extract::{Path, Query, Request, State},
    http::{HeaderValue, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Redirect, Response},
};
use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};
use maud::{DOCTYPE, Markup, html};
use serde::Deserialize;
use serde_json::Value;
use time::{OffsetDateTime, macros::format_description};

use crate::{AppState, db::RequestRow};

const SESSION_COOKIE: &str = "aduana_session";
const PAGE_SIZE: i64 = 50;
const PREVIEW_CHARS: usize = 120;
const FAILED_LOGIN_DELAY: Duration = Duration::from_millis(500);
const CSP: &str = "default-src 'none'; style-src 'self'; form-action 'self'; base-uri 'none'; frame-ancestors 'none'";

/// Pages hold decrypted prompts: they must not be cached, framed or sniffed.
/// Maud escapes all interpolated content; the CSP is the second line of defence.
pub async fn security_headers(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CSP),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    if !headers.contains_key(header::CACHE_CONTROL) {
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
    response
}

pub async fn require_session(
    State(state): State<AppState>,
    jar: CookieJar,
    request: Request,
    next: Next,
) -> Response {
    let authenticated = jar
        .get(SESSION_COOKIE)
        .is_some_and(|cookie| state.sessions.is_valid(cookie.value()));
    if authenticated {
        next.run(request).await
    } else {
        Redirect::to("/login").into_response()
    }
}

pub async fn stylesheet() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/css; charset=utf-8"),
            (header::CACHE_CONTROL, "public, max-age=3600"),
        ],
        include_str!("../assets/style.css"),
    )
}

fn page(title: &str, logged_in: bool, content: Markup) -> Markup {
    html! {
        (DOCTYPE)
        html lang="es" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { (title) " · Aduana" }
                link rel="stylesheet" href="/static/style.css";
            }
            body {
                header.top {
                    a.brand href="/" { "Aduana" }
                    @if logged_in {
                        form method="post" action="/logout" {
                            button.link type="submit" { "Cerrar sesión" }
                        }
                    }
                }
                main { (content) }
            }
        }
    }
}

fn login_page(failed: bool) -> Markup {
    page(
        "Iniciar sesión",
        false,
        html! {
            form.login method="post" action="/login" {
                h1 { "Iniciar sesión" }
                @if failed {
                    p.notice role="alert" { "Usuario o contraseña incorrectos." }
                }
                label { "Usuario" input name="user" autocomplete="username" required autofocus; }
                label { "Contraseña" input name="password" type="password" autocomplete="current-password" required; }
                button type="submit" { "Entrar" }
            }
        },
    )
}

pub async fn login_form() -> Markup {
    login_page(false)
}

#[derive(Deserialize)]
pub struct LoginForm {
    user: String,
    password: String,
}

pub async fn login(
    State(state): State<AppState>,
    jar: CookieJar,
    Form(form): Form<LoginForm>,
) -> Response {
    // Attempts run one at a time and failures hold the gate, which caps
    // password guessing at about two tries per second across all clients.
    let gate = state.login_gate.lock().await;
    if !state.admin.verify(&form.user, &form.password) {
        tokio::time::sleep(FAILED_LOGIN_DELAY).await;
        drop(gate);
        tracing::warn!("failed panel login");
        return (StatusCode::UNAUTHORIZED, login_page(true)).into_response();
    }
    drop(gate);

    let token = match state.sessions.create() {
        Ok(token) => token,
        Err(error) => return internal_error(error),
    };
    let cookie = Cookie::build((SESSION_COOKIE, token))
        .http_only(true)
        .same_site(SameSite::Strict)
        .secure(state.cookie_secure)
        .path("/")
        .build();
    (jar.add(cookie), Redirect::to("/")).into_response()
}

pub async fn logout(State(state): State<AppState>, jar: CookieJar) -> Response {
    if let Some(cookie) = jar.get(SESSION_COOKIE) {
        state.sessions.remove(cookie.value());
    }
    let removal = Cookie::build((SESSION_COOKIE, "")).path("/").build();
    (jar.remove(removal), Redirect::to("/login")).into_response()
}

fn internal_error(error: anyhow::Error) -> Response {
    tracing::error!(%error, "panel request failed");
    (StatusCode::INTERNAL_SERVER_ERROR, "Error interno").into_response()
}

#[derive(Deserialize)]
pub struct ListQuery {
    page: Option<i64>,
}

pub async fn list(State(state): State<AppState>, Query(query): Query<ListQuery>) -> Response {
    let page_number = query.page.unwrap_or(1).clamp(1, i64::MAX / PAGE_SIZE);
    let loaded = async {
        let total = state.db.count().await?;
        let rows = state
            .db
            .list(PAGE_SIZE, (page_number - 1) * PAGE_SIZE)
            .await?;
        anyhow::Ok((total, rows))
    }
    .await;
    let (total, rows) = match loaded {
        Ok(loaded) => loaded,
        Err(error) => return internal_error(error),
    };

    let mut entries = Vec::with_capacity(rows.len());
    for row in rows {
        let preview = decrypt_json(&state, row.request_enc.clone())
            .await
            .map(|request| request_preview(&request));
        entries.push((row, preview));
    }

    page("Peticiones", true, html! {
        h1 { "Peticiones" }
        p.muted { (total) " en total. Hora en UTC." }
        @if entries.is_empty() {
            p.empty { "Aún no hay peticiones registradas." }
        } @else {
            div.table-wrap {
                table {
                    thead { tr {
                        th { "#" } th { "Fecha" } th { "Cliente" } th { "Modelo" } th { "Petición" }
                        th { "Estado" } th.num { "Latencia" } th.num { "Tokens" }
                    } }
                    tbody {
                        @for (row, preview) in &entries {
                            tr {
                                td { a href={ "/requests/" (row.id) } { (row.id) } }
                                td.nowrap { (format_time(row.created_at)) }
                                td { (row.client) }
                                td { (row.model) }
                                td.preview {
                                    @match preview {
                                        Some(text) => { (text) }
                                        None => { span.muted { "No se pudo descifrar" } }
                                    }
                                }
                                td { (status_badge(&row.status)) }
                                td.num { (or_dash(row.latency_ms.map(format_ms))) }
                                td.num { (or_dash(total_tokens(row))) }
                            }
                        }
                    }
                }
            }
            nav.pager {
                @if page_number > 1 {
                    a href={ "/?page=" (page_number - 1) } { "← Más recientes" }
                }
                @if page_number.saturating_mul(PAGE_SIZE) < total {
                    a href={ "/?page=" (page_number + 1) } { "Más antiguas →" }
                }
            }
        }
    })
    .into_response()
}

pub async fn detail(State(state): State<AppState>, Path(id): Path<i64>) -> Response {
    let row = match state.db.get(id).await {
        Ok(Some(row)) => row,
        Ok(None) => {
            let content = html! {
                h1 { "Petición no encontrada" }
                p { a href="/" { "Volver a la lista" } }
            };
            return (StatusCode::NOT_FOUND, page("No encontrada", true, content)).into_response();
        }
        Err(error) => return internal_error(error),
    };

    let request = decrypt_bytes(&state, row.request_enc.clone()).await;
    let response = match row.response_enc.clone() {
        Some(sealed) => Some(decrypt_bytes(&state, sealed).await),
        None => None,
    };

    page(
        &format!("Petición {id}"),
        true,
        html! {
            p { a href="/" { "← Volver a la lista" } }
            h1 { "Petición " (id) " " (status_badge(&row.status)) }
            dl.facts {
                (fact("Fecha (UTC)", format_time(row.created_at)))
                (fact("Cliente", &row.client))
                (fact("Endpoint", &row.endpoint))
                (fact("Modelo", &row.model))
                (fact("HTTP", or_dash(row.http_status)))
                @if let Some(error) = &row.error { (fact("Error", error)) }
            }
            h2 { "Métricas" }
            dl.facts {
                (fact("Latencia", or_dash(row.latency_ms.map(format_ms))))
                (fact("Tiempo en Ollama", or_dash(row.ollama_total_ms.map(format_ms))))
                (fact("Carga del modelo", or_dash(row.ollama_load_ms.map(format_ms))))
                (fact("Tokens de entrada", or_dash(row.prompt_tokens)))
                (fact("Tokens de salida", or_dash(row.completion_tokens)))
                (fact("Velocidad", or_dash(row.tokens_per_second.map(|v| format!("{v:.1} tok/s")))))
                (fact("Tamaño petición", format_bytes(row.request_bytes)))
                (fact("Tamaño respuesta", or_dash(row.response_bytes.map(format_bytes))))
            }
            h2 { "Petición" }
            (body_view(&request, request_messages))
            h2 { "Respuesta" }
            @match &response {
                Some(body) => { (body_view(body, response_messages)) }
                None => { p.muted { "Sin respuesta almacenada." } }
            }
        },
    )
    .into_response()
}

async fn decrypt_bytes(state: &AppState, sealed: Vec<u8>) -> Option<Vec<u8>> {
    match state.crypto.decrypt(sealed).await {
        Ok(plain) => Some(plain),
        Err(error) => {
            tracing::error!(%error, "could not decrypt a stored body");
            None
        }
    }
}

/// `None` only when decryption fails; undecodable JSON becomes a string.
async fn decrypt_json(state: &AppState, sealed: Vec<u8>) -> Option<Value> {
    let plain = decrypt_bytes(state, sealed).await?;
    Some(
        serde_json::from_slice(&plain)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&plain).into_owned())),
    )
}

type Message = (String, String);

/// Readable `(role, text)` pairs, followed by the exact stored body.
fn body_view(body: &Option<Vec<u8>>, extract: fn(&Value) -> Vec<Message>) -> Markup {
    let Some(body) = body else {
        return html! { p.notice { "No se pudo descifrar con la clave configurada." } };
    };
    let parsed = serde_json::from_slice::<Value>(body).ok();
    let messages = parsed.as_ref().map(extract).unwrap_or_default();
    let raw = parsed
        .as_ref()
        .and_then(|value| serde_json::to_string_pretty(value).ok())
        .unwrap_or_else(|| String::from_utf8_lossy(body).into_owned());
    html! {
        @for (role, text) in &messages {
            div.message {
                div.role { (role) }
                div.text { (text) }
            }
        }
        details open[messages.is_empty()] {
            summary { "JSON completo" }
            pre { (raw) }
        }
    }
}

fn request_messages(value: &Value) -> Vec<Message> {
    let mut messages = Vec::new();
    if let Some(system) = value.get("system").and_then(Value::as_str) {
        messages.push(("system".to_owned(), system.to_owned()));
    }
    if let Some(prompt) = value.get("prompt").and_then(Value::as_str) {
        messages.push(("prompt".to_owned(), prompt.to_owned()));
    }
    if let Some(list) = value.get("messages").and_then(Value::as_array) {
        messages.extend(list.iter().filter_map(message_parts));
    }
    messages
}

fn response_messages(value: &Value) -> Vec<Message> {
    if let Some(error) = value.get("error").and_then(Value::as_str) {
        return vec![("error".to_owned(), error.to_owned())];
    }
    if let Some(text) = value.get("response").and_then(Value::as_str) {
        return vec![("respuesta".to_owned(), text.to_owned())];
    }
    value
        .get("message")
        .and_then(message_parts)
        .into_iter()
        .collect()
}

fn message_parts(message: &Value) -> Option<Message> {
    Some((
        message.get("role")?.as_str()?.to_owned(),
        message.get("content")?.as_str()?.to_owned(),
    ))
}

fn request_preview(request: &Value) -> String {
    let text = request
        .get("messages")
        .and_then(Value::as_array)
        .and_then(|messages| {
            messages
                .iter()
                .rev()
                .find(|m| m.get("role").and_then(Value::as_str) == Some("user"))
        })
        .and_then(|message| message.get("content"))
        .or_else(|| request.get("prompt"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    truncate(text, PREVIEW_CHARS)
}

fn truncate(text: &str, max_chars: usize) -> String {
    let mut chars = text.chars();
    let head: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}

fn fact(label: &str, value: impl Display) -> Markup {
    html! { div { dt { (label) } dd { (value.to_string()) } } }
}

fn status_badge(status: &str) -> Markup {
    let label = match status {
        "pending" => "En curso",
        "completed" => "Completada",
        "failed" => "Fallida",
        "interrupted" => "Interrumpida",
        other => other,
    };
    html! { span class={ "status status-" (status) } { (label) } }
}

fn or_dash(value: Option<impl Display>) -> String {
    value.map_or_else(|| "—".to_owned(), |value| value.to_string())
}

fn total_tokens(row: &RequestRow) -> Option<i64> {
    match (row.prompt_tokens, row.completion_tokens) {
        (None, None) => None,
        (prompt, completion) => Some(prompt.unwrap_or(0) + completion.unwrap_or(0)),
    }
}

fn format_ms(ms: i64) -> String {
    if ms < 1000 {
        format!("{ms} ms")
    } else {
        format!("{:.1} s", ms as f64 / 1000.0)
    }
}

fn format_bytes(bytes: i64) -> String {
    match bytes {
        b if b < 1024 => format!("{b} B"),
        b if b < 1024 * 1024 => format!("{:.1} KB", b as f64 / 1024.0),
        b => format!("{:.1} MB", b as f64 / (1024.0 * 1024.0)),
    }
}

fn format_time(unix_ms: i64) -> String {
    let format = format_description!("[year]-[month]-[day] [hour]:[minute]:[second]");
    OffsetDateTime::from_unix_timestamp(unix_ms / 1000)
        .ok()
        .and_then(|time| time.format(&format).ok())
        .unwrap_or_else(|| "—".to_owned())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn preview_uses_last_user_message() {
        let request = json!({"messages":[
            {"role":"user","content":"first"},
            {"role":"assistant","content":"reply"},
            {"role":"user","content":"second"},
        ]});
        assert_eq!(request_preview(&request), "second");
        assert_eq!(request_preview(&json!({"prompt":"hi"})), "hi");
        assert_eq!(request_preview(&json!({})), "");
    }

    #[test]
    fn truncate_respects_multibyte_characters() {
        assert_eq!(truncate("ñandú", 3), "ñan…");
        assert_eq!(truncate("ñandú", 5), "ñandú");
    }

    #[test]
    fn extracts_both_response_shapes() {
        let chat = json!({"message":{"role":"assistant","content":"hola"}});
        assert_eq!(
            response_messages(&chat),
            [("assistant".to_owned(), "hola".to_owned())]
        );
        let generate = json!({"response":"hola"});
        assert_eq!(response_messages(&generate)[0].1, "hola");
        let error = json!({"error":"model not found"});
        assert_eq!(response_messages(&error)[0].0, "error");
    }

    #[test]
    fn formats_values() {
        assert_eq!(format_ms(999), "999 ms");
        assert_eq!(format_ms(1500), "1.5 s");
        assert_eq!(format_bytes(2048), "2.0 KB");
        assert_eq!(format_time(0), "1970-01-01 00:00:00");
    }
}
