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
pub fn confirm_email(confirm_url: &str, display: &str) -> (String, String) {
    let u = esc(confirm_url);
    let d = esc(display);
    let html = format!(
        r#"<div style="font-family:-apple-system,Segoe UI,Arial,sans-serif;font-size:16px;line-height:1.6;color:#181512;max-width:520px;">
  <p>Thanks for subscribing to <strong>{d}</strong>.</p>
  <p>Please confirm your email address to start receiving new posts:</p>
  <p style="margin:24px 0;">
    <a href="{u}" style="background:#9b4d24;color:#fff;text-decoration:none;padding:11px 18px;border-radius:6px;font-weight:600;display:inline-block;">Confirm subscription</a>
  </p>
  <p style="color:#625a52;font-size:14px;">Or paste this link into your browser:<br><a href="{u}" style="color:#9b4d24;">{u}</a></p>
  <p style="color:#625a52;font-size:14px;">If you didn't request this, you can ignore this email and you won't be added.</p>
</div>"#
    );
    let text = format!(
        "Thanks for subscribing to {display}.\n\nConfirm your email address to start receiving new posts:\n{confirm_url}\n\nIf you didn't request this, ignore this email and you won't be added."
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

/// Crude HTML -> text: drop tags, collapse whitespace. For the text/plain part.
fn strip_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    let decoded = out
        .replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&nbsp;", " ");
    decoded.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Wrap a custom composed body (from the WYSIWYG editor) in the standard email
/// container and unsubscribe footer. Returns (html, text).
pub fn wrap_custom(body_html: &str, unsub_url: &str) -> (String, String) {
    let u = esc(unsub_url);
    let html = format!(
        r#"<div style="font-family:-apple-system,Segoe UI,Arial,sans-serif;font-size:16px;line-height:1.6;color:#181512;max-width:560px;">
  {body_html}
  <hr style="border:none;border-top:1px solid #d6d1c9;margin:24px 0;">
  <p style="color:#8a8178;font-size:12px;margin:0;">
    You're receiving this because you subscribed at stephens.page/blog.
    <a href="{u}" style="color:#8a8178;">Unsubscribe</a>.
  </p>
</div>"#
    );
    let text = format!("{}\n\n---\nUnsubscribe: {}\n", strip_tags(body_html), unsub_url);
    (html, text)
}

/// Standalone subscribe page served at the service root (newsletter.stephens.page/).
pub fn subscribe_page(sitekey: &str, list: &str, display: &str) -> String {
    let k = esc(sitekey);
    let list = esc(list);
    let d = esc(display);
    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<link rel="icon" type="image/png" href="https://stephens.page/bee-favicon.png">
<meta name="viewport" content="width=device-width, initial-scale=1.0">
<title>Subscribe | Jacob Stephens' blog</title>
<meta name="description" content="Subscribe to get new posts from Jacob Stephens' blog by email.">
<link href="https://fonts.googleapis.com/css2?family=Source+Serif+4:wght@600;700&family=Source+Sans+3:ital,wght@0,400;0,600;0,700;1,400&display=swap" rel="stylesheet">
<script src="https://challenges.cloudflare.com/turnstile/v0/api.js" async defer></script>
<style>
:root{{--ink:#181512;--brand:#9b4d24;--muted:#625a52;--surface:#fff;--soft:#efe9df;--rule:#d6d1c9;}}
*{{margin:0;padding:0;box-sizing:border-box;}}
body{{font-family:'Source Sans 3',Arial,sans-serif;background:var(--surface);color:var(--ink);line-height:1.7;min-height:100vh;display:flex;align-items:center;justify-content:center;padding:1.5rem;}}
.card{{max-width:480px;width:100%;}}
.eyebrow{{color:var(--brand);font-size:.78rem;font-weight:700;letter-spacing:.08em;text-transform:uppercase;margin-bottom:.6rem;}}
h1{{font-family:'Source Serif 4',Georgia,serif;font-weight:700;font-size:clamp(1.8rem,4vw,2.4rem);line-height:1.1;margin-bottom:.6rem;}}
p.lead{{color:var(--muted);margin-bottom:1.4rem;}}
form{{display:flex;flex-direction:column;gap:.8rem;}}
.row{{display:flex;gap:.6rem;flex-wrap:wrap;}}
input[type=email]{{flex:1 1 220px;padding:.65rem .8rem;font:inherit;color:var(--ink);background:var(--surface);border:1px solid var(--rule);border-radius:6px;}}
input[type=email]:focus-visible{{outline:2px solid var(--brand);outline-offset:1px;border-color:var(--brand);}}
button{{padding:.65rem 1.3rem;font:inherit;font-weight:700;color:#fff;background:var(--brand);border:1px solid var(--brand);border-radius:6px;cursor:pointer;}}
button:hover:not(:disabled){{background:#843f1d;}}
button:disabled{{opacity:.6;cursor:default;}}
.hp{{position:absolute;left:-9999px;width:1px;height:1px;overflow:hidden;}}
.status{{font-size:.95rem;min-height:1.2em;margin:0;}}
.status.ok{{color:#2f6b34;}}
.status.err{{color:#a3372a;}}
.fine{{color:var(--muted);font-size:.82rem;margin:0;}}
.foot{{margin-top:1.6rem;font-size:.85rem;}}
a{{color:var(--brand);}}
</style>
</head>
<body>
<div class="card">
  <div class="eyebrow">{d}</div>
  <h1>Get new posts by email</h1>
  <p class="lead">Occasional writeups, sent when I publish. No spam, unsubscribe anytime.</p>
  <form id="f" novalidate>
    <div class="row">
      <input type="email" name="email" id="email" placeholder="you@example.com" autocomplete="email" required aria-label="Email address">
      <button type="submit" id="btn">Subscribe</button>
    </div>
    <input type="hidden" name="list" value="{list}">
    <div class="hp" aria-hidden="true"><label>Leave this field empty<input type="text" name="website_url" tabindex="-1" autocomplete="off"></label></div>
    <div class="cf-turnstile" data-sitekey="{k}" data-theme="light"></div>
    <p class="status" id="status" role="status" aria-live="polite"></p>
    <p class="fine">You'll get a confirmation email to opt in, and every email has a one-click unsubscribe.</p>
  </form>
  <p class="foot"><a href="https://stephens.page/blog/">&larr; Read the blog</a></p>
</div>
<script>
(function(){{
  var f=document.getElementById('f'),btn=document.getElementById('btn'),s=document.getElementById('status');
  f.addEventListener('submit',function(e){{
    e.preventDefault();s.className='status';s.textContent='';btn.disabled=true;btn.textContent='Subscribing...';
    fetch('/subscribe',{{method:'POST',body:new URLSearchParams(new FormData(f))}})
      .then(function(r){{return r.json().catch(function(){{return {{ok:false,message:'Unexpected response.'}};}});}})
      .then(function(j){{s.textContent=j.message||(j.ok?'Thanks!':'Something went wrong.');s.className='status '+(j.ok?'ok':'err');if(j.ok){{f.reset();}}}})
      .catch(function(){{s.textContent='Network error. Please try again.';s.className='status err';}})
      .finally(function(){{btn.disabled=false;btn.textContent='Subscribe';if(window.turnstile){{try{{window.turnstile.reset();}}catch(e){{}}}}}});
  }});
}})();
</script>
</body>
</html>"#
    )
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
