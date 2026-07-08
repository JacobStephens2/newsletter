//! Newsletter service for stephens.page/blog (Rust/Axum).
//!
//! Public endpoints (behind Apache at https://newsletter.stephens.page):
//!   POST /subscribe          double opt-in: Turnstile + honeypot + rate-limit, mails a confirm link
//!   GET  /confirm?token=      confirm a pending subscriber
//!   GET  /unsubscribe?token=  opt-out landing page
//!   POST /unsubscribe?token=  RFC 8058 one-click unsubscribe
//!
//! Admin endpoints (require the NEWSLETTER_ADMIN_TOKEN bearer/header; used by the dashboard):
//!   GET  /admin/subscribers   stats + recent subscribers + send history (JSON)
//!   POST /admin/unsubscribe   {email}
//!   POST /admin/delete        {email}
//!   POST /admin/send          {slug, force?}  -> sends a post to confirmed subscribers
//!
//! CLI: `newsletter send <slug> [--force]` runs a send from the shell.
//!
//! The Resend API key comes from the environment (RESEND_API_KEY, or SMTP_PASS injected
//! by secret-env from the shared fleet secret) - no private copy of the key.

mod db;
mod mail;
mod send;

use axum::{
    extract::{DefaultBodyLimit, Form, Query, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use tower_http::cors::CorsLayer;
use rand::RngCore;
use serde::Deserialize;
use serde_json::json;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub struct Config {
    pub addr: String,
    pub db_path: String,
    pub public_url: String,
    pub blog_url: String,
    pub blog_dir: String,
    pub resend_key: String,
    pub turnstile_secret: String,
    pub admin_token: String,
    pub from_email: String,
    pub from_name: String,
}

#[derive(Clone)]
pub struct AppState {
    pub db: Arc<Mutex<rusqlite::Connection>>,
    pub cfg: Arc<Config>,
    pub http: reqwest::Client,
    pub rl: Arc<Mutex<HashMap<String, Vec<i64>>>>,
}

impl AppState {
    fn from(&self) -> String {
        format!("{} <{}>", self.cfg.from_name, self.cfg.from_email)
    }
}

/// Seconds since the Unix epoch.
pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn token() -> String {
    let mut b = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut b);
    hex::encode(b)
}

fn valid_email(e: &str) -> bool {
    let e = e.trim();
    if e.contains(' ') || e.len() > 254 {
        return false;
    }
    match e.find('@') {
        Some(at) => {
            let local = &e[..at];
            let domain = &e[at + 1..];
            !local.is_empty() && domain.contains('.') && !domain.starts_with('.') && !domain.ends_with('.')
        }
        None => false,
    }
}

fn client_ip(h: &HeaderMap) -> String {
    h.get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.split(',').next())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

fn hexish(s: &str) -> bool {
    !s.is_empty() && s.len() <= 128 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

async fn turnstile_ok(http: &reqwest::Client, secret: &str, token: &str, ip: &str) -> bool {
    let params = [("secret", secret), ("response", token), ("remoteip", ip)];
    match http
        .post("https://challenges.cloudflare.com/turnstile/v0/siteverify")
        .form(&params)
        .send()
        .await
    {
        Ok(r) => {
            let j: serde_json::Value = r.json().await.unwrap_or_default();
            j.get("success").and_then(|v| v.as_bool()).unwrap_or(false)
        }
        Err(_) => false,
    }
}

/// Per-IP: at most 5 subscribe attempts per hour.
fn rate_ok(state: &AppState, ip: &str) -> bool {
    let mut rl = state.rl.lock().unwrap();
    let n = now();
    let v = rl.entry(ip.to_string()).or_default();
    v.retain(|t| *t > n - 3600);
    if v.len() >= 5 {
        false
    } else {
        v.push(n);
        true
    }
}

fn jok(msg: &str) -> Response {
    (StatusCode::OK, Json(json!({"ok": true, "message": msg}))).into_response()
}

fn jerr(code: StatusCode, msg: &str) -> Response {
    (code, Json(json!({"ok": false, "message": msg}))).into_response()
}

fn admin_ok(headers: &HeaderMap, token: &str) -> bool {
    if token.is_empty() {
        return false;
    }
    let bearer = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .map(str::trim);
    let x = headers.get("x-admin-token").and_then(|v| v.to_str().ok());
    bearer == Some(token) || x == Some(token)
}

// ---------- Public handlers ----------

#[derive(Deserialize)]
struct SubscribeForm {
    #[serde(default)]
    email: String,
    #[serde(default)]
    website_url: String,
    #[serde(default, rename = "cf-turnstile-response")]
    turnstile: String,
}

async fn subscribe(State(state): State<AppState>, headers: HeaderMap, Form(form): Form<SubscribeForm>) -> Response {
    let ip = client_ip(&headers);

    // Honeypot: pretend success, do nothing.
    if !form.website_url.trim().is_empty() {
        return jok("Thanks! Please check your email to confirm.");
    }

    let email = form.email.trim().to_string();
    if email.is_empty() {
        return jerr(StatusCode::OK, "Please enter your email address.");
    }
    if !valid_email(&email) {
        return jerr(StatusCode::OK, "Please enter a valid email address.");
    }
    if !rate_ok(&state, &ip) {
        return jerr(StatusCode::TOO_MANY_REQUESTS, "Too many attempts from this connection. Please wait a bit and try again.");
    }
    if state.cfg.turnstile_secret.is_empty() {
        return jerr(StatusCode::INTERNAL_SERVER_ERROR, "The form is not fully configured yet. Please email jacob@stephens.page.");
    }
    if form.turnstile.is_empty() {
        return jerr(StatusCode::OK, "Please complete the verification challenge and try again.");
    }
    if !turnstile_ok(&state.http, &state.cfg.turnstile_secret, &form.turnstile, &ip).await {
        return jerr(StatusCode::OK, "Verification failed. Please try the challenge again.");
    }

    let confirm_token = token();
    {
        let conn = state.db.lock().unwrap();
        match db::find_by_email(&conn, &email) {
            Ok(Some((_, status))) if status == "confirmed" => {
                return jok("You're already subscribed - thanks for reading.");
            }
            Ok(Some((id, _))) => {
                if db::set_pending(&conn, id, &confirm_token, &ip).is_err() {
                    return jerr(StatusCode::INTERNAL_SERVER_ERROR, "Something went wrong on our end. Please try again later.");
                }
            }
            Ok(None) => {
                if db::insert_pending(&conn, &email, &confirm_token, &token(), now(), &ip).is_err() {
                    return jerr(StatusCode::INTERNAL_SERVER_ERROR, "Something went wrong on our end. Please try again later.");
                }
            }
            Err(e) => {
                tracing::error!("subscribe db error: {e}");
                return jerr(StatusCode::INTERNAL_SERVER_ERROR, "Something went wrong on our end. Please try again later.");
            }
        }
    }

    let confirm_url = format!("{}/confirm?token={}", state.cfg.public_url.trim_end_matches('/'), confirm_token);
    let (html, text) = mail::confirm_email(&confirm_url);
    let from = state.from();
    if let Err(e) = mail::send(
        &state.http,
        &state.cfg.resend_key,
        &from,
        &email,
        "Confirm your subscription to Jacob Stephens' blog",
        &html,
        &text,
        &HashMap::new(),
    )
    .await
    {
        tracing::error!("confirm send failed: {e}");
        return jerr(StatusCode::BAD_GATEWAY, "We could not send the confirmation email. Please try again later.");
    }

    jok("Almost there - check your email and click the confirmation link.")
}

#[derive(Deserialize)]
struct TokenQuery {
    #[serde(default)]
    token: String,
}

async fn confirm(State(state): State<AppState>, Query(q): Query<TokenQuery>) -> Response {
    if !hexish(&q.token) {
        return (
            StatusCode::BAD_REQUEST,
            Html(mail::landing_page(
                "Invalid link",
                "That link doesn't look right",
                "<p>The confirmation link is missing or malformed. Try subscribing again from the blog.</p>",
            )),
        )
            .into_response();
    }
    let res = {
        let conn = state.db.lock().unwrap();
        db::confirm(&conn, &q.token, now())
    };
    match res {
        Ok(true) => Html(mail::landing_page(
            "Subscription confirmed",
            "You're subscribed",
            "<p>Thanks for confirming. You'll get an email when I publish a new post - nothing else.</p>\
             <p>Every email includes a one-click unsubscribe link if you ever change your mind.</p>",
        ))
        .into_response(),
        Ok(false) => Html(mail::landing_page(
            "Already confirmed",
            "You're all set",
            "<p>This link has already been used. If you've confirmed once, you're subscribed - nothing more to do.</p>",
        ))
        .into_response(),
        Err(e) => {
            tracing::error!("confirm error: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Html(mail::landing_page("Something went wrong", "Something went wrong", "<p>We couldn't confirm your subscription just now. Please try the link again shortly.</p>")),
            )
                .into_response()
        }
    }
}

async fn unsubscribe_get(State(state): State<AppState>, Query(q): Query<TokenQuery>) -> Response {
    if !hexish(&q.token) {
        return (
            StatusCode::BAD_REQUEST,
            Html(mail::landing_page(
                "Invalid link",
                "That link doesn't look right",
                "<p>The unsubscribe link is missing or malformed. Email <a href=\"mailto:jacob@stephens.page\">jacob@stephens.page</a> and I'll remove you.</p>",
            )),
        )
            .into_response();
    }
    let outcome = {
        let conn = state.db.lock().unwrap();
        db::unsubscribe_by_token(&conn, &q.token, now())
    };
    match outcome {
        Ok(db::UnsubOutcome::NotFound) => Html(mail::landing_page(
            "Not found",
            "We couldn't find that subscription",
            "<p>This link doesn't match anyone on the list. You may already be removed.</p>",
        ))
        .into_response(),
        Ok(_) => Html(mail::landing_page(
            "Unsubscribed",
            "You've been unsubscribed",
            "<p>You won't receive any more newsletter emails. Sorry to see you go - you're welcome back anytime from the blog.</p>",
        ))
        .into_response(),
        Err(e) => {
            tracing::error!("unsubscribe error: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Html(mail::landing_page("Something went wrong", "Something went wrong", "<p>We couldn't process that just now. Please try again shortly.</p>")),
            )
                .into_response()
        }
    }
}

async fn unsubscribe_post(State(state): State<AppState>, Query(q): Query<TokenQuery>) -> Response {
    if hexish(&q.token) {
        let conn = state.db.lock().unwrap();
        let _ = db::unsubscribe_by_token(&conn, &q.token, now());
    }
    (StatusCode::OK, "OK").into_response()
}

// ---------- Admin handlers ----------

async fn admin_data(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if !admin_ok(&headers, &state.cfg.admin_token) {
        return (StatusCode::UNAUTHORIZED, Json(json!({"error": "unauthorized"}))).into_response();
    }
    let conn = state.db.lock().unwrap();
    let stats = match db::stats(&conn) {
        Ok(s) => s,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))).into_response(),
    };
    let subscribers = db::recent_subscribers(&conn, 1000).unwrap_or_default();
    let sends = db::recent_sends(&conn, 50).unwrap_or_default();
    Json(json!({"stats": stats, "subscribers": subscribers, "sends": sends})).into_response()
}

#[derive(Deserialize)]
struct EmailBody {
    #[serde(default)]
    email: String,
}

async fn admin_unsub(State(state): State<AppState>, headers: HeaderMap, Json(body): Json<EmailBody>) -> Response {
    if !admin_ok(&headers, &state.cfg.admin_token) {
        return (StatusCode::UNAUTHORIZED, Json(json!({"error": "unauthorized"}))).into_response();
    }
    let n = {
        let conn = state.db.lock().unwrap();
        db::unsubscribe_by_email(&conn, body.email.trim(), now()).unwrap_or(0)
    };
    Json(json!({"ok": true, "affected": n})).into_response()
}

async fn admin_delete(State(state): State<AppState>, headers: HeaderMap, Json(body): Json<EmailBody>) -> Response {
    if !admin_ok(&headers, &state.cfg.admin_token) {
        return (StatusCode::UNAUTHORIZED, Json(json!({"error": "unauthorized"}))).into_response();
    }
    let n = {
        let conn = state.db.lock().unwrap();
        db::delete_by_email(&conn, body.email.trim()).unwrap_or(0)
    };
    Json(json!({"ok": true, "affected": n})).into_response()
}

#[derive(Deserialize)]
struct SlugBody {
    #[serde(default)]
    slug: String,
    #[serde(default)]
    force: bool,
}

async fn admin_send(State(state): State<AppState>, headers: HeaderMap, Json(body): Json<SlugBody>) -> Response {
    if !admin_ok(&headers, &state.cfg.admin_token) {
        return (StatusCode::UNAUTHORIZED, Json(json!({"error": "unauthorized"}))).into_response();
    }
    let slug = body.slug.trim().to_string();
    if slug.is_empty() {
        return jerr(StatusCode::BAD_REQUEST, "Missing slug.");
    }
    let st = state.clone();
    let force = body.force;
    let slug2 = slug.clone();
    tokio::spawn(async move {
        match send::send_post(&st, &slug2, force).await {
            Ok(r) => tracing::info!("send {slug2}: sent {}, failed {} of {}", r.sent, r.failed, r.recipients),
            Err(e) => tracing::error!("send {slug2} failed: {e}"),
        }
    });
    Json(json!({"ok": true, "started": true, "message": format!("Sending \"{slug}\" to confirmed subscribers.")})).into_response()
}

#[derive(Deserialize)]
struct ComposeSeedBody {
    #[serde(default)]
    slug: String,
}

async fn admin_compose(State(state): State<AppState>, headers: HeaderMap, Json(body): Json<ComposeSeedBody>) -> Response {
    if !admin_ok(&headers, &state.cfg.admin_token) {
        return (StatusCode::UNAUTHORIZED, Json(json!({"error": "unauthorized"}))).into_response();
    }
    match send::read_post(&state.cfg, &body.slug) {
        Ok((_slug, title, desc, post_url)) => {
            let seed = send::seed_body(&title, &desc, &post_url);
            Json(json!({"ok": true, "subject": title, "body_html": seed})).into_response()
        }
        Err(e) => jerr(StatusCode::BAD_REQUEST, &e.to_string()),
    }
}

#[derive(Deserialize)]
struct PreviewBody {
    #[serde(default)]
    body_html: String,
}

async fn admin_preview(State(state): State<AppState>, headers: HeaderMap, Json(body): Json<PreviewBody>) -> Response {
    if !admin_ok(&headers, &state.cfg.admin_token) {
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    }
    let unsub = format!("{}/unsubscribe?token=preview", state.cfg.public_url.trim_end_matches('/'));
    let (html, _text) = mail::wrap_custom(&body.body_html, &unsub);
    Html(html).into_response()
}

#[derive(Deserialize)]
struct SendHtmlBody {
    #[serde(default)]
    subject: String,
    #[serde(default)]
    body_html: String,
    #[serde(default)]
    test_email: String,
}

async fn admin_send_html(State(state): State<AppState>, headers: HeaderMap, Json(body): Json<SendHtmlBody>) -> Response {
    if !admin_ok(&headers, &state.cfg.admin_token) {
        return (StatusCode::UNAUTHORIZED, Json(json!({"error": "unauthorized"}))).into_response();
    }
    if body.subject.trim().is_empty() {
        return jerr(StatusCode::BAD_REQUEST, "Subject is required.");
    }
    if body.body_html.trim().is_empty() {
        return jerr(StatusCode::BAD_REQUEST, "The email body is empty.");
    }
    let test = body.test_email.trim().to_string();
    if !test.is_empty() {
        if !valid_email(&test) {
            return jerr(StatusCode::BAD_REQUEST, "Invalid test address.");
        }
        return match send::send_test(&state, &body.subject, &body.body_html, &test).await {
            Ok(_) => Json(json!({"ok": true, "message": format!("Test email sent to {test}.")})).into_response(),
            Err(e) => jerr(StatusCode::BAD_GATEWAY, &format!("Test send failed: {e}")),
        };
    }
    let st = state.clone();
    let subject = body.subject.clone();
    let bh = body.body_html.clone();
    tokio::spawn(async move {
        match send::send_custom(&st, &subject, &bh).await {
            Ok(r) => tracing::info!("compose send '{}': sent {}, failed {} of {}", subject, r.sent, r.failed, r.recipients),
            Err(e) => tracing::error!("compose send failed: {e}"),
        }
    });
    Json(json!({"ok": true, "started": true, "message": "Sending to confirmed subscribers."})).into_response()
}

fn config_from_env() -> Config {
    let get = |k: &str, d: &str| std::env::var(k).unwrap_or_else(|_| d.to_string());
    let resend = std::env::var("RESEND_API_KEY")
        .or_else(|_| std::env::var("SMTP_PASS"))
        .unwrap_or_default();
    Config {
        addr: get("NEWSLETTER_ADDR", "127.0.0.1:3462"),
        db_path: get("NEWSLETTER_DB", "/var/lib/stephens-newsletter/newsletter.sqlite"),
        public_url: get("NEWSLETTER_PUBLIC_URL", "https://newsletter.stephens.page"),
        blog_url: get("NEWSLETTER_BLOG_URL", "https://stephens.page/blog"),
        blog_dir: get("NEWSLETTER_BLOG_DIR", "/var/www/stephens.page/blog"),
        resend_key: resend,
        turnstile_secret: get("TURNSTILE_SECRET", ""),
        admin_token: get("NEWSLETTER_ADMIN_TOKEN", ""),
        from_email: get("NEWSLETTER_FROM_EMAIL", "jacob@stephens.page"),
        from_name: get("NEWSLETTER_FROM_NAME", "Jacob Stephens"),
    }
}

fn build_state(cfg: Config) -> anyhow::Result<AppState> {
    let conn = db::open(&cfg.db_path)?;
    let http = reqwest::Client::builder().timeout(Duration::from_secs(20)).build()?;
    Ok(AppState {
        db: Arc::new(Mutex::new(conn)),
        cfg: Arc::new(cfg),
        http,
        rl: Arc::new(Mutex::new(HashMap::new())),
    })
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let args: Vec<String> = std::env::args().collect();

    // CLI: `newsletter send <slug> [--force]`
    if args.get(1).map(String::as_str) == Some("send") {
        let cfg = config_from_env();
        let force = args.iter().any(|a| a == "--force");
        let slug = args.iter().skip(2).find(|a| !a.starts_with("--"));
        let Some(slug) = slug else {
            eprintln!("usage: newsletter send <slug> [--force]");
            std::process::exit(2);
        };
        let state = build_state(cfg)?;
        match send::send_post(&state, slug, force).await {
            Ok(r) => {
                println!("Subject: {}", r.subject);
                println!("Recipients: {}", r.recipients);
                println!("Sent {}, failed {}.", r.sent, r.failed);
                std::process::exit(if r.failed > 0 { 1 } else { 0 });
            }
            Err(e) => {
                eprintln!("send failed: {e}");
                std::process::exit(1);
            }
        }
    }

    let cfg = config_from_env();
    let addr = cfg.addr.clone();
    if cfg.resend_key.is_empty() {
        tracing::warn!("no RESEND_API_KEY/SMTP_PASS in env - email sending will fail");
    }
    if cfg.admin_token.is_empty() {
        tracing::warn!("no NEWSLETTER_ADMIN_TOKEN - admin API is disabled");
    }
    let state = build_state(cfg)?;

    // The subscribe form lives on stephens.page and fetch()es this service on a
    // different subdomain, so the browser needs CORS to read the JSON response.
    let cors = CorsLayer::new()
        .allow_origin("https://stephens.page".parse::<HeaderValue>().unwrap())
        .allow_methods([Method::GET, Method::POST]);

    let app = Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/subscribe", post(subscribe))
        .route("/confirm", get(confirm))
        .route("/unsubscribe", get(unsubscribe_get).post(unsubscribe_post))
        .route("/admin/subscribers", get(admin_data))
        .route("/admin/unsubscribe", post(admin_unsub))
        .route("/admin/delete", post(admin_delete))
        .route("/admin/send", post(admin_send))
        .route("/admin/compose", post(admin_compose))
        .route("/admin/preview", post(admin_preview))
        .route("/admin/send_html", post(admin_send_html))
        .layer(DefaultBodyLimit::max(512 * 1024))
        .layer(cors)
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("newsletter service listening on {addr}");
    axum::serve(listener, app).await?;
    Ok(())
}
