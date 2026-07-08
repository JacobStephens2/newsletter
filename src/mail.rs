//! Resend HTTP API client and the HTML/text bodies the service sends and serves.

use std::collections::HashMap;

/// Send one email via the Resend API. Returns the Resend message id on success.
pub async fn send(
    http: &reqwest::Client,
    api_key: &str,
    from: &str,
    to: &str,
    subject: &str,
    html: &str,
    text: &str,
    headers: &HashMap<String, String>,
) -> anyhow::Result<String> {
    let mut body = serde_json::json!({
        "from": from,
        "to": [to],
        "subject": subject,
        "html": html,
        "text": text,
    });
    if !headers.is_empty() {
        body["headers"] = serde_json::to_value(headers)?;
    }
    let resp = http
        .post("https://api.resend.com/emails")
        .bearer_auth(api_key)
        .json(&body)
        .send()
        .await?;
    let status = resp.status();
    let json: serde_json::Value = resp.json().await.unwrap_or_default();
    if status.is_success() {
        if let Some(id) = json.get("id").and_then(|v| v.as_str()) {
            return Ok(id.to_string());
        }
    }
    let msg = json
        .get("message")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| format!("HTTP {status}"));
    anyhow::bail!("resend: {msg}")
}

/// Minimal HTML-escape for interpolating user/site text into pages and emails.
pub fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// Confirmation (double opt-in) email body.
pub fn confirm_email(confirm_url: &str) -> (String, String) {
    let u = esc(confirm_url);
    let html = format!(
        r#"<div style="font-family:-apple-system,Segoe UI,Arial,sans-serif;font-size:16px;line-height:1.6;color:#181512;max-width:520px;">
  <p>Thanks for subscribing to <strong>Jacob Stephens' blog</strong>.</p>
  <p>Please confirm your email address to start receiving new posts:</p>
  <p style="margin:24px 0;">
    <a href="{u}" style="background:#9b4d24;color:#fff;text-decoration:none;padding:11px 18px;border-radius:6px;font-weight:600;display:inline-block;">Confirm subscription</a>
  </p>
  <p style="color:#625a52;font-size:14px;">Or paste this link into your browser:<br><a href="{u}" style="color:#9b4d24;">{u}</a></p>
  <p style="color:#625a52;font-size:14px;">If you didn't request this, you can ignore this email and you won't be added.</p>
</div>"#
    );
    let text = format!(
        "Thanks for subscribing to Jacob Stephens' blog.\n\nConfirm your email address to start receiving new posts:\n{confirm_url}\n\nIf you didn't request this, ignore this email and you won't be added."
    );
    (html, text)
}

/// Newsletter (per-post) email body for one subscriber.
pub fn post_email(title: &str, desc: &str, post_url: &str, unsub_url: &str) -> (String, String) {
    let t = esc(title);
    let d = esc(desc);
    let p = esc(post_url);
    let u = esc(unsub_url);
    let html = format!(
        r#"<div style="font-family:-apple-system,Segoe UI,Arial,sans-serif;font-size:16px;line-height:1.6;color:#181512;max-width:560px;">
  <p style="color:#625a52;font-size:14px;margin:0 0 16px;">New post on Jacob Stephens' blog</p>
  <h1 style="font-size:22px;line-height:1.25;margin:0 0 12px;color:#181512;">{t}</h1>
  <p style="margin:0 0 24px;color:#333;">{d}</p>
  <p style="margin:0 0 28px;">
    <a href="{p}" style="background:#9b4d24;color:#fff;text-decoration:none;padding:11px 18px;border-radius:6px;font-weight:600;display:inline-block;">Read the post</a>
  </p>
  <hr style="border:none;border-top:1px solid #d6d1c9;margin:24px 0;">
  <p style="color:#8a8178;font-size:12px;margin:0;">
    You're receiving this because you subscribed at stephens.page/blog.
    <a href="{u}" style="color:#8a8178;">Unsubscribe</a>.
  </p>
</div>"#
    );
    let text = format!(
        "New post on Jacob Stephens' blog\n\n{title}\n\n{desc}\n\nRead it: {post_url}\n\n---\nUnsubscribe: {unsub_url}\n"
    );
    (html, text)
}

/// Branded landing page (confirm / unsubscribe screens), mirrors the blog style.
pub fn landing_page(title: &str, heading: &str, body_html: &str) -> String {
    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<link rel="icon" type="image/png" href="https://stephens.page/bee-favicon.png">
<meta name="viewport" content="width=device-width, initial-scale=1.0">
<meta name="robots" content="noindex">
<title>{title} | Jacob Stephens</title>
<link href="https://fonts.googleapis.com/css2?family=Source+Serif+4:wght@600;700&family=Source+Sans+3:ital,wght@0,400;0,600;0,700;1,400&display=swap" rel="stylesheet">
<style>
:root{{--ink:#181512;--brand:#9b4d24;--muted:#625a52;--surface:#fff;--rule:#d6d1c9;}}
*{{margin:0;padding:0;box-sizing:border-box;}}
body{{font-family:'Source Sans 3',Arial,sans-serif;background:var(--surface);color:var(--ink);line-height:1.7;min-height:100vh;display:flex;align-items:center;justify-content:center;padding:1.5rem;}}
.card{{max-width:520px;text-align:center;}}
h1{{font-family:'Source Serif 4',Georgia,serif;font-weight:700;font-size:clamp(1.8rem,4vw,2.4rem);line-height:1.1;color:var(--brand);margin-bottom:1rem;}}
p{{color:var(--ink);margin-bottom:1rem;}}
a{{color:var(--brand);}}
.btn{{display:inline-block;margin-top:1.5rem;padding:0.6rem 1.1rem;border:1px solid var(--rule);border-radius:6px;color:var(--ink);text-decoration:none;font-weight:600;}}
.btn:hover{{border-color:var(--brand);color:var(--brand);}}
</style>
</head>
<body>
<div class="card">
<h1>{heading}</h1>
{body_html}
<a class="btn" href="https://stephens.page/blog/">&larr; Back to the blog</a>
</div>
</body>
</html>"#
    )
}
