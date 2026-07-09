//! Sending posts / composed emails to a list's confirmed subscribers.
//! Shared by the CLI, the admin send endpoints, and compose. List-scoped.

use crate::{mail, AppState, ListCfg};
use std::collections::{HashMap, HashSet};

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

fn slug_from_href(href: &str) -> Option<String> {
    let trimmed = href.trim();
    let path = if let Some(rest) = trimmed.strip_prefix("/blog/") {
        rest
    } else if let Some(rest) = trimmed.strip_prefix("https://stephens.page/blog/") {
        rest
    } else {
        return None;
    };
    let slug = path.split('/').find(|part| !part.is_empty())?;
    if valid_slug(slug) {
        Some(slug.to_string())
    } else {
        None
    }
}

fn published_slugs(lc: &ListCfg) -> Vec<String> {
    let index_file = format!("{}/index.html", lc.blog_dir);
    let html = match std::fs::read_to_string(index_file) {
        Ok(html) => html,
        Err(_) => return Vec::new(),
    };
    let mut slugs = Vec::new();
    let mut seen = HashSet::new();
    let mut rest = html.as_str();
    while let Some(start) = rest.find("href=\"") {
        rest = &rest[start + 6..];
        let Some(end) = rest.find('"') else { break; };
        let href = &rest[..end];
        if let Some(slug) = slug_from_href(href) {
            if seen.insert(slug.clone()) {
                slugs.push(slug);
            }
        }
        rest = &rest[end + 1..];
    }
    slugs
}

/// Read a published post for a list: returns (slug, title, description, post_url).
pub fn read_post(lc: &ListCfg, slug: &str) -> anyhow::Result<(String, String, String, String)> {
    let slug = slug.rsplit('/').find(|s| !s.is_empty()).unwrap_or(slug).trim();
    if !valid_slug(slug) {
        anyhow::bail!("invalid slug: {slug}");
    }
    if !published_slugs(lc).iter().any(|published| published == slug) {
        anyhow::bail!("post is not published: {slug}");
    }
    let post_file = format!("{}/{}/index.html", lc.blog_dir, slug);
    let html = std::fs::read_to_string(&post_file)
        .map_err(|e| anyhow::anyhow!("cannot read post {post_file}: {e}"))?;
    let title = between(&html, "<title>", "</title>")
        .map(|t| unescape(t.trim()))
        .map(|t| t.trim_end_matches("| Jacob Stephens").trim().to_string())
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| slug.to_string());
    let desc = between(&html, r#"<meta name="description" content=""#, "\"")
        .map(|d| unescape(d.trim()))
        .unwrap_or_default();
    let post_url = format!("{}/{}/", lc.blog_url.trim_end_matches('/'), slug);
    Ok((slug.to_string(), title, desc, post_url))
}

pub fn seed_body(title: &str, desc: &str, post_url: &str) -> String {
    format!(
        "<p style=\"color:#625a52;font-size:14px;margin:0 0 16px;\">New post</p>\
         <h1 style=\"font-size:22px;line-height:1.25;margin:0 0 12px;\">{t}</h1>\
         <p>{d}</p>\
         <p style=\"margin:24px 0;\"><a href=\"{u}\">Read the post</a></p>",
        t = mail::esc(title),
        d = mail::esc(desc),
        u = mail::esc(post_url),
    )
}

/// List published posts available to send for a list, as (slug, title). Sorted by title.
pub fn list_posts(lc: &ListCfg) -> Vec<(String, String)> {
    let mut posts = Vec::new();
    for slug in published_slugs(lc) {
        let idx = format!("{}/{}/index.html", lc.blog_dir, slug);
        let title = std::fs::read_to_string(&idx)
            .ok()
            .and_then(|h| between(&h, "<title>", "</title>").map(str::to_string))
            .map(|t| unescape(t.trim()).trim_end_matches("| Jacob Stephens").trim().to_string())
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| slug.clone());
        posts.push((slug, title));
    }
    posts
}

pub struct SendResult {
    pub sent: usize,
    pub failed: usize,
    pub subject: String,
    pub recipients: usize,
}

fn unsub_url(state: &AppState, token: &str) -> String {
    format!("{}/unsubscribe?token={}", state.cfg.public_url.trim_end_matches('/'), token)
}

fn unsub_headers(from_email: &str, unsub_url: &str) -> HashMap<String, String> {
    let mut h = HashMap::new();
    h.insert(
        "List-Unsubscribe".to_string(),
        format!("<{unsub_url}>, <mailto:{from_email}?subject=unsubscribe>"),
    );
    h.insert("List-Unsubscribe-Post".to_string(), "List-Unsubscribe=One-Click".to_string());
    h
}

/// Send a published post (auto-built email) to a list's confirmed subscribers.
pub async fn send_post(state: &AppState, list: &str, slug: &str, force: bool) -> anyhow::Result<SendResult> {
    let lc = crate::list_cfg(list);
    let (_slug, title, desc, post_url) = read_post(&lc, slug)?;

    let recipients: Vec<(String, String)> = {
        let conn = state.db.lock().unwrap();
        if !force {
            if let Some(at) = crate::db::last_send_at(&conn, &post_url, list)? {
                anyhow::bail!("already sent at {at}; pass force to send again");
            }
        }
        crate::db::confirmed_recipients(&conn, list)?
    };

    let subject = title.clone();
    let from = format!("{} <{}>", lc.from_name, lc.from_email);
    let mut sent = 0usize;
    let mut failed = 0usize;

    for (email, token) in &recipients {
        let uu = unsub_url(state, token);
        let (body_html, body_text) = mail::post_email(&title, &desc, &post_url, &uu);
        match mail::send(&state.http, &state.cfg.resend_key, &from, email, &subject, &body_html, &body_text, &unsub_headers(lc.from_email, &uu)).await {
            Ok(_) => sent += 1,
            Err(e) => {
                failed += 1;
                tracing::warn!("send to {email} failed: {e}");
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }

    let repr = mail::post_email(&title, &desc, &post_url, &unsub_url(state, "preview")).0;
    {
        let conn = state.db.lock().unwrap();
        crate::db::record_send(&conn, &post_url, &subject, crate::now(), sent as i64, &repr, list)?;
    }
    Ok(SendResult { sent, failed, subject, recipients: recipients.len() })
}

/// Send a custom composed email to a list's confirmed subscribers.
pub async fn send_custom(state: &AppState, list: &str, subject: &str, body_html: &str) -> anyhow::Result<SendResult> {
    let lc = crate::list_cfg(list);
    let recipients: Vec<(String, String)> = {
        let conn = state.db.lock().unwrap();
        crate::db::confirmed_recipients(&conn, list)?
    };
    let from = format!("{} <{}>", lc.from_name, lc.from_email);
    let mut sent = 0usize;
    let mut failed = 0usize;

    for (email, token) in &recipients {
        let uu = unsub_url(state, token);
        let (html, text) = mail::wrap_custom(body_html, &uu);
        match mail::send(&state.http, &state.cfg.resend_key, &from, email, subject, &html, &text, &unsub_headers(lc.from_email, &uu)).await {
            Ok(_) => sent += 1,
            Err(e) => {
                failed += 1;
                tracing::warn!("send to {email} failed: {e}");
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    let repr = mail::wrap_custom(body_html, &unsub_url(state, "preview")).0;
    {
        let conn = state.db.lock().unwrap();
        crate::db::record_send(&conn, &format!("(compose) {subject}"), subject, crate::now(), sent as i64, &repr, list)?;
    }
    Ok(SendResult { sent, failed, subject: subject.to_string(), recipients: recipients.len() })
}

/// Send a single composed email to one test address (no DB record).
pub async fn send_test(state: &AppState, list: &str, subject: &str, body_html: &str, to: &str) -> anyhow::Result<String> {
    let lc = crate::list_cfg(list);
    let uu = unsub_url(state, "preview");
    let (html, text) = mail::wrap_custom(body_html, &uu);
    let from = format!("{} <{}>", lc.from_name, lc.from_email);
    mail::send(&state.http, &state.cfg.resend_key, &from, to, subject, &html, &text, &HashMap::new()).await
}
