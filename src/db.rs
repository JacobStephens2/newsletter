//! SQLite persistence for the newsletter service.
//!
//! Multi-list: one service backs several blogs. Every subscriber and send belongs
//! to a `list` (e.g. "stephens", "personal"); the same email may be on more than
//! one list, so uniqueness is (email, list). This service is the sole DB owner.

use rusqlite::{Connection, OptionalExtension};
use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct Subscriber {
    pub email: String,
    pub status: String,
    pub created_at: i64,
    pub confirmed_at: Option<i64>,
    pub unsubscribed_at: Option<i64>,
}

#[derive(Debug, Serialize, Default)]
pub struct Stats {
    pub confirmed: i64,
    pub pending: i64,
    pub unsubscribed: i64,
    pub total: i64,
}

#[derive(Debug, Serialize)]
pub struct SendRecord {
    pub id: i64,
    pub post_url: String,
    pub subject: Option<String>,
    pub sent_at: i64,
    pub recipient_count: Option<i64>,
}

pub enum AddOutcome {
    Added,
    Reactivated,
    AlreadyConfirmed,
}

pub enum UnsubOutcome {
    Done,
    Already,
    NotFound,
}

fn table_has_column(conn: &Connection, table: &str, col: &str) -> bool {
    let mut stmt = match conn.prepare(&format!("PRAGMA table_info({table})")) {
        Ok(s) => s,
        Err(_) => return false,
    };
    let cols: Vec<String> = stmt
        .query_map([], |r| r.get::<_, String>(1))
        .map(|rows| rows.filter_map(Result::ok).collect())
        .unwrap_or_default();
    cols.iter().any(|c| c == col)
}

/// Open the database, create the schema, and run the multi-list migration.
pub fn open(path: &str) -> anyhow::Result<Connection> {
    let conn = Connection::open(path)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "busy_timeout", 5000)?;
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS subscribers (
            id                INTEGER PRIMARY KEY AUTOINCREMENT,
            email             TEXT NOT NULL COLLATE NOCASE,
            status            TEXT NOT NULL DEFAULT 'pending',
            confirm_token     TEXT,
            unsubscribe_token TEXT NOT NULL,
            created_at        INTEGER NOT NULL,
            confirmed_at      INTEGER,
            unsubscribed_at   INTEGER,
            ip                TEXT,
            list              TEXT NOT NULL DEFAULT 'stephens',
            UNIQUE(email, list)
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

    // Migration: sends gains list + body_html; subscribers gains list (+ composite
    // uniqueness). Guarded so it only runs on the pre-multi-list schema.
    let _ = conn.execute("ALTER TABLE sends ADD COLUMN body_html TEXT", []);
    let _ = conn.execute("ALTER TABLE sends ADD COLUMN list TEXT NOT NULL DEFAULT 'stephens'", []);
    if !table_has_column(&conn, "subscribers", "list") {
        conn.execute_batch(
            r#"
            BEGIN;
            ALTER TABLE subscribers RENAME TO subscribers_old;
            CREATE TABLE subscribers (
                id                INTEGER PRIMARY KEY AUTOINCREMENT,
                email             TEXT NOT NULL COLLATE NOCASE,
                status            TEXT NOT NULL DEFAULT 'pending',
                confirm_token     TEXT,
                unsubscribe_token TEXT NOT NULL,
                created_at        INTEGER NOT NULL,
                confirmed_at      INTEGER,
                unsubscribed_at   INTEGER,
                ip                TEXT,
                list              TEXT NOT NULL DEFAULT 'stephens',
                UNIQUE(email, list)
            );
            INSERT INTO subscribers (id,email,status,confirm_token,unsubscribe_token,created_at,confirmed_at,unsubscribed_at,ip,list)
                SELECT id,email,status,confirm_token,unsubscribe_token,created_at,confirmed_at,unsubscribed_at,ip,'stephens' FROM subscribers_old;
            DROP TABLE subscribers_old;
            COMMIT;
            "#,
        )?;
    }
    Ok(conn)
}

pub fn find_by_email(conn: &Connection, email: &str, list: &str) -> rusqlite::Result<Option<(i64, String)>> {
    conn.query_row(
        "SELECT id, status FROM subscribers WHERE email = ?1 COLLATE NOCASE AND list = ?2",
        rusqlite::params![email, list],
        |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
    )
    .optional()
}

pub fn set_pending(conn: &Connection, id: i64, confirm_token: &str, ip: &str) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE subscribers SET status='pending', confirm_token=?1, unsubscribed_at=NULL, ip=?2 WHERE id=?3",
        rusqlite::params![confirm_token, ip, id],
    )?;
    Ok(())
}

pub fn insert_pending(conn: &Connection, email: &str, confirm_token: &str, unsubscribe_token: &str, now: i64, ip: &str, list: &str) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO subscribers (email, status, confirm_token, unsubscribe_token, created_at, ip, list)
         VALUES (?1, 'pending', ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![email, confirm_token, unsubscribe_token, now, ip, list],
    )?;
    Ok(())
}

pub fn confirm(conn: &Connection, token: &str, now: i64) -> rusqlite::Result<bool> {
    let n = conn.execute(
        "UPDATE subscribers SET status='confirmed', confirmed_at=?1, confirm_token=NULL WHERE confirm_token=?2",
        rusqlite::params![now, token],
    )?;
    Ok(n > 0)
}

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
        Some((_, s)) if s == "unsubscribed" => Ok(UnsubOutcome::Already),
        Some((id, _)) => {
            conn.execute(
                "UPDATE subscribers SET status='unsubscribed', unsubscribed_at=?1 WHERE id=?2",
                rusqlite::params![now, id],
            )?;
            Ok(UnsubOutcome::Done)
        }
    }
}

pub fn unsubscribe_by_email(conn: &Connection, email: &str, now: i64, list: &str) -> rusqlite::Result<usize> {
    conn.execute(
        "UPDATE subscribers SET status='unsubscribed', unsubscribed_at=?1
          WHERE email=?2 COLLATE NOCASE AND list=?3 AND status!='unsubscribed'",
        rusqlite::params![now, email, list],
    )
}

pub fn delete_by_email(conn: &Connection, email: &str, list: &str) -> rusqlite::Result<usize> {
    conn.execute("DELETE FROM subscribers WHERE email=?1 COLLATE NOCASE AND list=?2", rusqlite::params![email, list])
}

/// Manually add a subscriber as already-confirmed for a list.
pub fn add_confirmed(conn: &Connection, email: &str, unsubscribe_token: &str, now: i64, list: &str) -> rusqlite::Result<AddOutcome> {
    match find_by_email(conn, email, list)? {
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
                "INSERT INTO subscribers (email, status, confirm_token, unsubscribe_token, created_at, confirmed_at, ip, list)
                 VALUES (?1, 'confirmed', NULL, ?2, ?3, ?3, 'admin', ?4)",
                rusqlite::params![email, unsubscribe_token, now, list],
            )?;
            Ok(AddOutcome::Added)
        }
    }
}

pub fn stats(conn: &Connection, list: &str) -> rusqlite::Result<Stats> {
    let mut s = Stats::default();
    let mut stmt = conn.prepare("SELECT status, COUNT(*) FROM subscribers WHERE list=? GROUP BY status")?;
    let rows = stmt.query_map([list], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
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

pub fn recent_subscribers(conn: &Connection, limit: i64, list: &str) -> rusqlite::Result<Vec<Subscriber>> {
    let mut stmt = conn.prepare(
        "SELECT email, status, created_at, confirmed_at, unsubscribed_at
           FROM subscribers WHERE list=?1 ORDER BY created_at DESC LIMIT ?2",
    )?;
    let rows = stmt.query_map(rusqlite::params![list, limit], |r| {
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

pub fn confirmed_recipients(conn: &Connection, list: &str) -> rusqlite::Result<Vec<(String, String)>> {
    let mut stmt = conn.prepare(
        "SELECT email, unsubscribe_token FROM subscribers WHERE list=? AND status='confirmed' ORDER BY id",
    )?;
    let rows = stmt.query_map([list], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
    rows.collect()
}

pub fn recent_sends(conn: &Connection, limit: i64, list: &str) -> rusqlite::Result<Vec<SendRecord>> {
    let mut stmt = conn.prepare(
        "SELECT id, post_url, subject, sent_at, recipient_count FROM sends WHERE list=?1 ORDER BY sent_at DESC LIMIT ?2",
    )?;
    let rows = stmt.query_map(rusqlite::params![list, limit], |r| {
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

pub fn last_send_at(conn: &Connection, post_url: &str, list: &str) -> rusqlite::Result<Option<i64>> {
    conn.query_row(
        "SELECT sent_at FROM sends WHERE post_url=?1 AND list=?2 ORDER BY sent_at DESC LIMIT 1",
        rusqlite::params![post_url, list],
        |r| r.get::<_, i64>(0),
    )
    .optional()
}

pub fn record_send(conn: &Connection, post_url: &str, subject: &str, now: i64, count: i64, body_html: &str, list: &str) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO sends (post_url, subject, sent_at, recipient_count, body_html, list) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![post_url, subject, now, count, body_html, list],
    )?;
    Ok(())
}

pub fn sent_html(conn: &Connection, id: i64) -> rusqlite::Result<Option<String>> {
    conn.query_row("SELECT body_html FROM sends WHERE id = ?", [id], |r| r.get::<_, Option<String>>(0))
        .optional()
        .map(|o| o.flatten())
}
