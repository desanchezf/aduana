//! Exercises the full router against a fake Ollama listening on a real socket.

use std::{sync::Arc, time::Duration};

use aduana::{AppState, app, config::Config};
use age::x25519::Identity;
use axum::{
    Json, Router,
    body::{Body, to_bytes},
    extract::State,
    http::{Request, Response, StatusCode, header},
    routing::post,
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tokio::{net::TcpListener, sync::Mutex};
use tower::ServiceExt;

const API_KEY: &str = "test-key-0123456789abcdef0123456789";
const PASSWORD: &str = "correct horse battery";
const SECRET_PROMPT: &str = "PROMPT-SENTINEL-7f3a";
const SECRET_ANSWER: &str = "ANSWER-SENTINEL-91bc";

type Received = Arc<Mutex<Vec<Value>>>;

async fn fake_ollama(State(received): State<Received>, Json(body): Json<Value>) -> Response<Body> {
    let model = body["model"].as_str().unwrap_or_default().to_owned();
    received.lock().await.push(body);
    if model == "missing" {
        return Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Body::from(r#"{"error":"model 'missing' not found"}"#))
            .unwrap();
    }
    let reply = json!({
        "model": model,
        "message": {"role": "assistant", "content": format!("{SECRET_ANSWER} <script>alert(1)</script>")},
        "done": true,
        "total_duration": 2_000_000_000u64,
        "prompt_eval_count": 5,
        "eval_count": 10,
        "eval_duration": 1_000_000_000u64,
    });
    Response::new(Body::from(reply.to_string()))
}

struct Harness {
    router: Router,
    received: Received,
    db_dir: tempfile::TempDir,
}

impl Harness {
    async fn start() -> Self {
        let received = Received::default();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let ollama_url = format!("http://{}", listener.local_addr().unwrap());
        let ollama = Router::new()
            .route("/api/chat", post(fake_ollama))
            .route("/api/generate", post(fake_ollama))
            .with_state(received.clone());
        tokio::spawn(async move { axum::serve(listener, ollama).await.unwrap() });
        Self::with_ollama(ollama_url, received).await
    }

    async fn with_ollama(ollama_url: String, received: Received) -> Self {
        let db_dir = tempfile::tempdir().unwrap();
        let config = Config {
            listen_addr: "127.0.0.1:0".parse().unwrap(),
            ollama_url,
            database_url: format!("sqlite://{}/test.db", db_dir.path().display()),
            api_keys: vec![("tester".into(), API_KEY.into())],
            admin_user: "admin".into(),
            admin_password: PASSWORD.into(),
            identity: Identity::generate(),
            cookie_secure: false,
            ollama_timeout: Duration::from_secs(5),
            max_body_bytes: 1024 * 1024,
        };
        let router = app(AppState::new(config).await.unwrap());
        Self {
            router,
            received,
            db_dir,
        }
    }

    async fn send(&self, request: Request<Body>) -> Response<Body> {
        self.router.clone().oneshot(request).await.unwrap()
    }

    async fn chat(&self, key: Option<&str>, body: Value) -> Response<Body> {
        let mut request =
            Request::post("/api/chat").header(header::CONTENT_TYPE, "application/json");
        if let Some(key) = key {
            request = request.header(header::AUTHORIZATION, format!("Bearer {key}"));
        }
        self.send(request.body(Body::from(body.to_string())).unwrap())
            .await
    }

    async fn login(&self) -> String {
        let form = format!("user=admin&password={}", PASSWORD.replace(' ', "+"));
        let response = self
            .send(
                Request::post("/login")
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(form))
                    .unwrap(),
            )
            .await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        let cookie = response.headers()[header::SET_COOKIE].to_str().unwrap();
        assert!(cookie.contains("HttpOnly"), "{cookie}");
        assert!(cookie.contains("SameSite=Strict"), "{cookie}");
        cookie.split(';').next().unwrap().to_owned()
    }

    async fn page(&self, path: &str, cookie: &str) -> (StatusCode, String) {
        let response = self
            .send(
                Request::get(path)
                    .header(header::COOKIE, cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await;
        let status = response.status();
        (status, body_text(response).await)
    }

    /// Raw bytes of everything SQLite wrote, including the WAL.
    fn database_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        for entry in std::fs::read_dir(self.db_dir.path()).unwrap() {
            bytes.extend(std::fs::read(entry.unwrap().path()).unwrap());
        }
        bytes
    }
}

async fn body_text(response: Response<Body>) -> String {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8(bytes.to_vec()).unwrap()
}

fn chat_body(model: &str) -> Value {
    json!({"model": model, "messages": [{"role": "user", "content": SECRET_PROMPT}]})
}

fn contains(haystack: &[u8], needle: &str) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle.as_bytes())
}

#[tokio::test]
async fn rejects_missing_wrong_and_malformed_credentials() {
    let harness = Harness::start().await;

    let response = harness.chat(None, chat_body("m")).await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(response.headers()[header::WWW_AUTHENTICATE], "Bearer");

    let response = harness.chat(Some("wrong-key"), chat_body("m")).await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    let basic = Request::post("/api/chat")
        .header(header::AUTHORIZATION, format!("Basic {API_KEY}"))
        .body(Body::from(chat_body("m").to_string()))
        .unwrap();
    assert_eq!(harness.send(basic).await.status(), StatusCode::UNAUTHORIZED);

    assert!(
        harness.received.lock().await.is_empty(),
        "nothing may reach Ollama"
    );
    let (_, list) = harness.page("/", &harness.login().await).await;
    assert!(
        list.contains("Aún no hay peticiones"),
        "rejected calls are not stored"
    );
}

#[tokio::test]
async fn forwards_in_clear_and_returns_the_answer_in_clear() {
    let harness = Harness::start().await;
    let response = harness
        .chat(Some(API_KEY), chat_body("any/model:tag"))
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let answer: Value = serde_json::from_str(&body_text(response).await).unwrap();
    assert!(
        answer["message"]["content"]
            .as_str()
            .unwrap()
            .contains(SECRET_ANSWER)
    );

    let received = harness.received.lock().await;
    assert_eq!(received[0]["model"], "any/model:tag");
    assert_eq!(received[0]["stream"], false);
    assert_eq!(received[0]["messages"][0]["content"], SECRET_PROMPT);
}

#[tokio::test]
async fn database_never_contains_plaintext() {
    let harness = Harness::start().await;
    harness.chat(Some(API_KEY), chat_body("m")).await;

    let bytes = harness.database_bytes();
    assert!(
        contains(&bytes, "age-encryption.org"),
        "expected age ciphertext on disk"
    );
    assert!(!contains(&bytes, SECRET_PROMPT), "prompt leaked to disk");
    assert!(!contains(&bytes, SECRET_ANSWER), "answer leaked to disk");
}

#[tokio::test]
async fn panel_requires_login() {
    let harness = Harness::start().await;
    harness.chat(Some(API_KEY), chat_body("m")).await;

    for path in ["/", "/requests/1"] {
        let response = harness
            .send(Request::get(path).body(Body::empty()).unwrap())
            .await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER, "{path}");
        assert_eq!(response.headers()[header::LOCATION], "/login");
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert!(!contains(&body, SECRET_PROMPT));
    }
    let (status, _) = harness.page("/", "aduana_session=forged").await;
    assert_eq!(status, StatusCode::SEE_OTHER);
}

#[tokio::test]
async fn wrong_password_is_refused() {
    let harness = Harness::start().await;
    let response = harness
        .send(
            Request::post("/login")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from("user=admin&password=wrong"))
                .unwrap(),
        )
        .await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert!(response.headers().get(header::SET_COOKIE).is_none());
}

#[tokio::test]
async fn after_login_shows_plaintext_escaped_with_metrics() {
    let harness = Harness::start().await;
    harness.chat(Some(API_KEY), chat_body("m")).await;
    let cookie = harness.login().await;

    let (status, list) = harness.page("/", &cookie).await;
    assert_eq!(status, StatusCode::OK);
    assert!(list.contains(SECRET_PROMPT));
    assert!(list.contains("tester"));
    assert!(!list.contains(API_KEY));

    let (status, detail) = harness.page("/requests/1", &cookie).await;
    assert_eq!(status, StatusCode::OK);
    assert!(detail.contains(SECRET_PROMPT));
    assert!(detail.contains(SECRET_ANSWER));
    assert!(detail.contains("10.0 tok/s"));
    assert!(
        !detail.contains("<script>alert"),
        "model output must be escaped"
    );
    assert!(detail.contains("&lt;script&gt;"));

    let (status, _) = harness.page("/requests/999", &cookie).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn panel_pages_are_not_cacheable_and_set_csp() {
    let harness = Harness::start().await;
    let cookie = harness.login().await;
    let response = harness
        .send(
            Request::get("/")
                .header(header::COOKIE, cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    let csp = response.headers()[header::CONTENT_SECURITY_POLICY]
        .to_str()
        .unwrap();
    assert!(csp.contains("default-src 'none'"));
}

#[tokio::test]
async fn logout_invalidates_the_session() {
    let harness = Harness::start().await;
    let cookie = harness.login().await;
    let response = harness
        .send(
            Request::post("/logout")
                .header(header::COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let (status, _) = harness.page("/", &cookie).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
}

#[tokio::test]
async fn missing_model_returns_ollama_404_and_is_recorded() {
    let harness = Harness::start().await;
    let response = harness.chat(Some(API_KEY), chat_body("missing")).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert!(body_text(response).await.contains("not found"));

    let (_, detail) = harness.page("/requests/1", &harness.login().await).await;
    assert!(detail.contains("Fallida"));
    assert!(detail.contains("upstream_error"));
}

#[tokio::test]
async fn unreachable_ollama_is_a_502_and_is_recorded() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let closed_port = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);

    let harness = Harness::with_ollama(closed_port, Received::default()).await;
    let response = harness.chat(Some(API_KEY), chat_body("m")).await;
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);

    let (_, detail) = harness.page("/requests/1", &harness.login().await).await;
    assert!(
        detail.contains(SECRET_PROMPT),
        "the request is still on record"
    );
    assert!(detail.contains("upstream_unavailable"));
}

#[tokio::test]
async fn invalid_and_oversized_bodies_are_rejected_before_ollama() {
    let harness = Harness::start().await;
    let response = harness
        .chat(Some(API_KEY), json!({"model": "m", "stream": true}))
        .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    let huge = json!({"model": "m", "prompt": "x".repeat(2 * 1024 * 1024)});
    let response = harness.chat(Some(API_KEY), huge).await;
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);

    assert!(harness.received.lock().await.is_empty());
}
