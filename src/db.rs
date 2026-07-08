//! SQLite persistence for the newsletter service.
//!
//! Single owner: this service is the only process that touches the database,
//! which avoids the cross-user / WAL sharing problems of the old PHP setup.

use rusqlite::{Connection, OptionalExtension};
use serde::Serialize;

/// A subscriber row as exposed to the admin API.
#[derive(Debug, Serialize)]
pub struct Subscriber {
    pub email: String,
    pub status: String,
    pub created_at: i64,
    pub confirmed_at: Option<i64>,
    pub unsubscribed_at: Option<i64>,
}

/// Aggregate counts for the manager page.
#[derive(Debug, Serialize, Default)]
pub struct Stats {
    pub confirmed: i64,
    pub pending: i64,
    pub unsubscribed: i64,
    pub total: i64,
}

/// A recorded send (one blast of a post).
#[derive(Debug, Serialize)]
pub struct SendRecord {
    pub id: i64,
    pub post_url: String,
    pub subject: Option<String>,
    pub sent_at: i64,
    pub recipient_count: Option<i64>,
}

/// Outcome of a manual admin add.
pub enum AddOutcome {
    Added,
    Reactivated,
    AlreadyConfirmed,
}

/// Open the database and create the schema if needed.
pub fn open(path: &str) -> anyhow::Result<Connection> {
    let conn = Connection::open(path)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "busy_timeout", 5000)?;
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS subscribers (
            id                INTEGER PRIMARY KEY AUTOINCREMENT,
            email             TEXT NOT NULL UNIQUE COLLATE NOCASE,
            status            TEXT NOT NULL DEFAULT 'pending',
            confirm_token     TEXT,
            unsubscribe_token TEXT NOT NULL,
            created_at        INTEGER NOT NULL,
            confirmed_at      INTEGER,
            unsubscribed_at   INTEGER,
            ip                TEXT
        );
        CREATE TABLE IF NOT EXISTS sends (
            id              INTEGER PRIMARY KEY AUTOINCREMENT,
            post_url        TEXT NOT NULL,
            subject         TEXT,
            sent_at         INTEGER NOT NULL,
            recipient_count INTEGER
        );
        "#,
    )?;
    // Migration: store a representative copy of the sent email so it can be viewed
    // later. Ignore the "duplicate column" error on subsequent startups.
    let _ = conn.execute("ALTER TABLE sends ADD COLUMN body_html TEXT", []);
    Ok(conn)
}

/// Manually add a subscriber as already-confirmed (admin vouches for them).
pub fn add_confirmed(conn: &Connection, email: &str, unsubscribe_token: &str, now: i64) -> rusqlite::Result<AddOutcome> {
    match find_by_email(conn, email)? {
        Some((_, status)) if status == "confirmed" => Ok(AddOutcome::AlreadyConfirmed),
        Some((id, _)) => {
            conn.execute(
                "UPDATE subscribers SET status='confirmed', confirmed_at=?1, confirm_token=NULL, unsubscribed_at=NULL WHERE id=?2",
                rusqlite::params![now, id],
            )?;
            Ok(AddOutcome::Reactivated)
        }
        None => {
            conn.execute(
                "INSERT INTO subscribers (email, status, confirm_token, unsubscribe_token, created_at, confirmed_at, ip)
                 VALUES (?1, 'confirmed', NULL, ?2, ?3, ?3, 'admin')",
                rusqlite::params![email, unsubscribe_token, now],
            )?;
            Ok(AddOutcome::Added)
        }
    }
}

/// The stored HTML of a recorded send, for viewing.
pub fn sent_html(conn: &Connection, id: i64) -> rusqlite::Result<Option<String>> {
    conn.query_row("SELECT body_html FROM sends WHERE id = ?", [id], |r| r.get::<_, Option<String>>(0))
        .optional()
        .map(|o| o.flatten())
}

/// Existing subscriber lookup result: (id, status).
pub fn find_by_email(conn: &Connection, email: &str) -> rusqlite::Result<Option<(i64, String)>> {
    conn.query_row(
        "SELECT id, status FROM subscribers WHERE email = ? COLLATE NOCASE",
        [email],
        |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
    )
    .optional()
}

/// Reset an existing row back to pending with a fresh confirm token.
pub fn set_pending(conn: &Connection, id: i64, confirm_token: &str, ip: &str) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE subscribers
            SET status = 'pending', confirm_token = ?1, unsubscribed_at = NULL, ip = ?2
          WHERE id = ?3",
        rusqlite::params![confirm_token, ip, id],
    )?;
    Ok(())
}

/// Insert a brand-new pending subscriber.
pub fn insert_pending(
    conn: &Connection,
    email: &str,
    confirm_token: &str,
    unsubscribe_token: &str,
    now: i64,
    ip: &str,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO subscribers (email, status, confirm_token, unsubscribe_token, created_at, ip)
         VALUES (?1, 'pending', ?2, ?3, ?4, ?5)",
        rusqlite::params![email, confirm_token, unsubscribe_token, now, ip],
    )?;
    Ok(())
}

/// Confirm a pending subscriber by confirm token. Returns true if one was confirmed.
pub fn confirm(conn: &Connection, token: &str, now: i64) -> rusqlite::Result<bool> {
    let n = conn.execute(
        "UPDATE subscribers
            SET status = 'confirmed', confirmed_at = ?1, confirm_token = NULL
          WHERE confirm_token = ?2",
        rusqlite::params![now, token],
    )?;
    Ok(n > 0)
}

/// Outcome of an unsubscribe attempt.
pub enum UnsubOutcome {
    Done,
    Already,
    NotFound,
}

/// Unsubscribe by opaque token (from an email link).
pub fn unsubscribe_by_token(conn: &Connection, token: &str, now: i64) -> rusqlite::Result<UnsubOutcome> {
    let row = conn
        .query_row(
            "SELECT id, status FROM subscribers WHERE unsubscribe_token = ?",
            [token],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
        )
        .optional()?;
    match row {
        None => Ok(UnsubOutcome::NotFound),
        Some((_, status)) if status == "unsubscribed" => Ok(UnsubOutcome::Already),
        Some((id, _)) => {
            conn.execute(
                "UPDATE subscribers SET status = 'unsubscribed', unsubscribed_at = ?1 WHERE id = ?2",
                rusqlite::params![now, id],
            )?;
            Ok(UnsubOutcome::Done)
        }
    }
}

/// Admin: unsubscribe by email address. Returns rows affected.
pub fn unsubscribe_by_email(conn: &Connection, email: &str, now: i64) -> rusqlite::Result<usize> {
    conn.execute(
        "UPDATE subscribers SET status = 'unsubscribed', unsubscribed_at = ?1
          WHERE email = ?2 COLLATE NOCASE AND status != 'unsubscribed'",
        rusqlite::params![now, email],
    )
}

/// Admin: hard-delete by email. Returns rows affected.
pub fn delete_by_email(conn: &Connection, email: &str) -> rusqlite::Result<usize> {
    conn.execute("DELETE FROM subscribers WHERE email = ? COLLATE NOCASE", [email])
}

/// Aggregate status counts.
pub fn stats(conn: &Connection) -> rusqlite::Result<Stats> {
    let mut s = Stats::default();
    let mut stmt = conn.prepare("SELECT status, COUNT(*) FROM subscribers GROUP BY status")?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
    for row in rows {
        let (status, count) = row?;
        match status.as_str() {
            "confirmed" => s.confirmed = count,
            "pending" => s.pending = count,
            "unsubscribed" => s.unsubscribed = count,
            _ => {}
        }
        s.total += count;
    }
    Ok(s)
}

/// Most-recent subscribers (capped), newest first.
pub fn recent_subscribers(conn: &Connection, limit: i64) -> rusqlite::Result<Vec<Subscriber>> {
    let mut stmt = conn.prepare(
        "SELECT email, status, created_at, confirmed_at, unsubscribed_at
           FROM subscribers ORDER BY created_at DESC LIMIT ?",
    )?;
    let rows = stmt.query_map([limit], |r| {
        Ok(Subscriber {
            email: r.get(0)?,
            status: r.get(1)?,
            created_at: r.get(2)?,
            confirmed_at: r.get(3)?,
            unsubscribed_at: r.get(4)?,
        })
    })?;
    rows.collect()
}

/// Confirmed subscribers for a send: (email, unsubscribe_token).
pub fn confirmed_recipients(conn: &Connection) -> rusqlite::Result<Vec<(String, String)>> {
    let mut stmt = conn.prepare(
        "SELECT email, unsubscribe_token FROM subscribers WHERE status = 'confirmed' ORDER BY id",
    )?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
    rows.collect()
}

/// Recent send history.
pub fn recent_sends(conn: &Connection, limit: i64) -> rusqlite::Result<Vec<SendRecord>> {
    let mut stmt = conn.prepare(
        "SELECT id, post_url, subject, sent_at, recipient_count FROM sends ORDER BY sent_at DESC LIMIT ?",
    )?;
    let rows = stmt.query_map([limit], |r| {
        Ok(SendRecord {
            id: r.get(0)?,
            post_url: r.get(1)?,
            subject: r.get(2)?,
            sent_at: r.get(3)?,
            recipient_count: r.get(4)?,
        })
    })?;
    rows.collect()
}

/// Whether a post URL was already sent.
pub fn last_send_at(conn: &Connection, post_url: &str) -> rusqlite::Result<Option<i64>> {
    conn.query_row(
        "SELECT sent_at FROM sends WHERE post_url = ? ORDER BY sent_at DESC LIMIT 1",
        [post_url],
        |r| r.get::<_, i64>(0),
    )
    .optional()
}

/// Record a completed send, storing a representative copy of the email HTML.
pub fn record_send(conn: &Connection, post_url: &str, subject: &str, now: i64, count: i64, body_html: &str) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO sends (post_url, subject, sent_at, recipient_count, body_html) VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![post_url, subject, now, count, body_html],
    )?;
    Ok(())
}
