//! Sending posts / composed emails to confirmed subscribers.
//! Shared by the `newsletter send <slug>` CLI, the admin send endpoints, and compose.

use crate::{mail, AppState, Config};
use std::collections::HashMap;

/// Extract text between the first `open` and the following `close`.
fn between<'a>(haystack: &'a str, open: &str, close: &str) -> Option<&'a str> {
    let start = haystack.find(open)? + open.len();
    let rest = &haystack[start..];
    let end = rest.find(close)?;
    Some(&rest[..end])
}

fn unescape(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
}

fn valid_slug(slug: &str) -> bool {
    !slug.is_empty() && slug.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// Read a published post: returns (slug, title, description, post_url).
pub fn read_post(cfg: &Config, slug: &str) -> anyhow::Result<(String, String, String, String)> {
    let slug = slug.rsplit('/').find(|s| !s.is_empty()).unwrap_or(slug).trim();
    if !valid_slug(slug) {
        anyhow::bail!("invalid slug: {slug}");
    }
    let post_file = format!("{}/{}/index.html", cfg.blog_dir, slug);
    let html = std::fs::read_to_string(&post_file)
        .map_err(|e| anyhow::anyhow!("cannot read post {post_file}: {e}"))?;
    let title = between(&html, "<title>", "</title>")
        .map(|t| unescape(t.trim()))
        .map(|t| t.trim_end_matches("| Jacob Stephens").trim().to_string())
        .unwrap_or_else(|| slug.to_string());
    let desc = between(&html, r#"<meta name="description" content=""#, "\"")
        .map(|d| unescape(d.trim()))
        .unwrap_or_default();
    let post_url = format!("{}/{}/", cfg.blog_url.trim_end_matches('/'), slug);
    Ok((slug.to_string(), title, desc, post_url))
}

/// A starting email body (editable in the compose UI) seeded from a post.
pub fn seed_body(title: &str, desc: &str, post_url: &str) -> String {
    format!(
        "<p style=\"color:#625a52;font-size:14px;margin:0 0 16px;\">New post on Jacob Stephens' blog</p>\
         <h1 style=\"font-size:22px;line-height:1.25;margin:0 0 12px;\">{t}</h1>\
         <p>{d}</p>\
         <p style=\"margin:24px 0;\"><a href=\"{u}\">Read the post</a></p>",
        t = mail::esc(title),
        d = mail::esc(desc),
        u = mail::esc(post_url),
    )
}

/// Result of a send: (sent, failed, subject).
pub struct SendResult {
    pub sent: usize,
    pub failed: usize,
    pub subject: String,
    pub recipients: usize,
}

/// Send a published post (auto-built email) to all confirmed subscribers.
pub async fn send_post(state: &AppState, slug: &str, force: bool) -> anyhow::Result<SendResult> {
    let (_slug, title, desc, post_url) = read_post(&state.cfg, slug)?;

    let recipients: Vec<(String, String)> = {
        let conn = state.db.lock().unwrap();
        if !force {
            if let Some(at) = crate::db::last_send_at(&conn, &post_url)? {
                anyhow::bail!("already sent at {at}; pass force to send again");
            }
        }
        crate::db::confirmed_recipients(&conn)?
    };

    let subject = title.clone();
    let from = state.from();
    let mut sent = 0usize;
    let mut failed = 0usize;

    for (email, token) in &recipients {
        let unsub_url = format!("{}/unsubscribe?token={}", state.cfg.public_url.trim_end_matches('/'), token);
        let (body_html, body_text) = mail::post_email(&title, &desc, &post_url, &unsub_url);
        match mail::send(&state.http, &state.cfg.resend_key, &from, email, &subject, &body_html, &body_text, &unsub_headers(&state.cfg, &unsub_url)).await {
            Ok(_) => sent += 1,
            Err(e) => {
                failed += 1;
                tracing::warn!("send to {email} failed: {e}");
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }

    {
        let conn = state.db.lock().unwrap();
        crate::db::record_send(&conn, &post_url, &subject, crate::now(), sent as i64)?;
    }
    Ok(SendResult { sent, failed, subject, recipients: recipients.len() })
}

/// Send a custom composed email (HTML body from the WYSIWYG editor) to all confirmed
/// subscribers. Each gets the standard container + per-subscriber unsubscribe footer.
pub async fn send_custom(state: &AppState, subject: &str, body_html: &str) -> anyhow::Result<SendResult> {
    let recipients: Vec<(String, String)> = {
        let conn = state.db.lock().unwrap();
        crate::db::confirmed_recipients(&conn)?
    };
    let from = state.from();
    let mut sent = 0usize;
    let mut failed = 0usize;

    for (email, token) in &recipients {
        let unsub_url = format!("{}/unsubscribe?token={}", state.cfg.public_url.trim_end_matches('/'), token);
        let (html, text) = mail::wrap_custom(body_html, &unsub_url);
        match mail::send(&state.http, &state.cfg.resend_key, &from, email, subject, &html, &text, &unsub_headers(&state.cfg, &unsub_url)).await {
            Ok(_) => sent += 1,
            Err(e) => {
                failed += 1;
                tracing::warn!("send to {email} failed: {e}");
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    {
        let conn = state.db.lock().unwrap();
        crate::db::record_send(&conn, &format!("(compose) {subject}"), subject, crate::now(), sent as i64)?;
    }
    Ok(SendResult { sent, failed, subject: subject.to_string(), recipients: recipients.len() })
}

/// Send a single composed email to one test address (no DB record, sample unsubscribe link).
pub async fn send_test(state: &AppState, subject: &str, body_html: &str, to: &str) -> anyhow::Result<String> {
    let unsub_url = format!("{}/unsubscribe?token=preview", state.cfg.public_url.trim_end_matches('/'));
    let (html, text) = mail::wrap_custom(body_html, &unsub_url);
    mail::send(&state.http, &state.cfg.resend_key, &state.from(), to, subject, &html, &text, &HashMap::new()).await
}

fn unsub_headers(cfg: &Config, unsub_url: &str) -> HashMap<String, String> {
    let mut h = HashMap::new();
    h.insert(
        "List-Unsubscribe".to_string(),
        format!("<{unsub_url}>, <mailto:{}?subject=unsubscribe>", cfg.from_email),
    );
    h.insert("List-Unsubscribe-Post".to_string(), "List-Unsubscribe=One-Click".to_string());
    h
}
