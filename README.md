# newsletter

Self-hosted, double opt-in newsletter service for [stephens.page/blog](https://stephens.page/blog),
in Rust (Axum + SQLite). It backs the subscribe form on the blog and sends new posts to
confirmed subscribers. Live at `https://newsletter.stephens.page`.

## Why it exists

The blog's subscribe form needs somewhere to store subscribers and something to send mail.
This service owns that end to end: it verifies Cloudflare Turnstile, records a **pending**
subscriber, emails a confirmation link (double opt-in), and only adds confirmed addresses to
the list. Sends carry a per-subscriber one-click unsubscribe (RFC 8058). A token-gated admin
API drives the manager page in the private dashboard.

Being a single owner of the SQLite database is deliberate - it avoids the cross-process / WAL
sharing problems the earlier PHP prototype had.

## Endpoints

Public (behind Apache at `newsletter.stephens.page`):

| Method | Path | Purpose |
| --- | --- | --- |
| POST | `/subscribe` | Turnstile + honeypot + per-IP rate limit; records pending, mails a confirm link |
| GET | `/confirm?token=` | Confirm a pending subscriber |
| GET | `/unsubscribe?token=` | Opt-out landing page |
| POST | `/unsubscribe?token=` | RFC 8058 one-click unsubscribe |
| GET | `/health` | Liveness |

Admin (require `NEWSLETTER_ADMIN_TOKEN` as `Authorization: Bearer` or `X-Admin-Token`):

| Method | Path | Purpose |
| --- | --- | --- |
| GET | `/admin/subscribers` | Stats + recent subscribers + send history (JSON) |
| POST | `/admin/unsubscribe` | `{email}` |
| POST | `/admin/delete` | `{email}` |
| POST | `/admin/send` | `{slug, force?}` - send a post to confirmed subscribers |

## CLI

```
newsletter send <slug> [--force]
```

Reads the post from `NEWSLETTER_BLOG_DIR/<slug>/index.html`, pulls its title and description,
and emails all confirmed subscribers. Guards against a double-send unless `--force`.

## Configuration (environment)

| Var | Default | Notes |
| --- | --- | --- |
| `NEWSLETTER_ADDR` | `127.0.0.1:3462` | Bind address |
| `NEWSLETTER_DB` | `/var/lib/stephens-newsletter/newsletter.sqlite` | SQLite path |
| `NEWSLETTER_PUBLIC_URL` | `https://newsletter.stephens.page` | For confirm/unsubscribe links |
| `NEWSLETTER_BLOG_URL` | `https://stephens.page/blog` | For "read the post" links |
| `NEWSLETTER_BLOG_DIR` | `/var/www/stephens.page/blog` | Where posts are read from |
| `NEWSLETTER_FROM_EMAIL` / `NEWSLETTER_FROM_NAME` | `jacob@stephens.page` / `Jacob Stephens` | Sender |
| `TURNSTILE_SECRET` | - | Cloudflare Turnstile secret |
| `NEWSLETTER_ADMIN_TOKEN` | - | Bearer token for the admin API |
| `RESEND_API_KEY` / `SMTP_PASS` | - | Resend key; injected at runtime by `secret-env` from the shared fleet secret (no private copy) |

## Deploy

`deploy/newsletter.service` (systemd, runs as `jacob`, wraps the binary in `secret-env` to
inject the shared Resend key) and `deploy/newsletter.stephens.page.conf` (Apache reverse
proxy). Build with `cargo build --release`.

## License

MIT
