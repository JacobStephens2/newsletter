//! Sending a published post to all confirmed subscribers.
//! Shared by the `newsletter send <slug>` CLI command and the admin API.

use crate::{mail, AppState};
use std::collections::HashMap;

/// Extract text between the first `open` and the following `close`.
fn between<'a>(haystack: &'a str, open: &str, close: &str) -> Option<&'a str> {
    let start = haystack.find(open)? + open.len();
    let rest = &haystack[start..];
    let end = rest.find(close)?;
    Some(&rest[..end])
}

/// Very small HTML entity decode for the handful we emit in titles/descriptions.
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

/// Result of a send: (sent, failed, subject).
pub struct SendResult {
    pub sent: usize,
    pub failed: usize,
    pub subject: String,
    pub recipients: usize,
}

/// Send `slug` to all confirmed subscribers. `force` overrides the double-send guard.
pub async fn send_post(state: &AppState, slug: &str, force: bool) -> anyhow::Result<SendResult> {
    let slug = slug.rsplit('/').find(|s| !s.is_empty()).unwrap_or(slug).trim();
    if !valid_slug(slug) {
        anyhow::bail!("invalid slug: {slug}");
    }
    let cfg = &state.cfg;
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

    // Double-send guard.
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
        let unsub_url = format!("{}/unsubscribe?token={}", cfg.public_url.trim_end_matches('/'), token);
        let (body_html, body_text) = mail::post_email(&title, &desc, &post_url, &unsub_url);
        let mut headers = HashMap::new();
        headers.insert(
            "List-Unsubscribe".to_string(),
            format!("<{unsub_url}>, <mailto:{}?subject=unsubscribe>", cfg.from_email),
        );
        headers.insert("List-Unsubscribe-Post".to_string(), "List-Unsubscribe=One-Click".to_string());

        match mail::send(&state.http, &cfg.resend_key, &from, email, &subject, &body_html, &body_text, &headers).await {
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
