//! The leaderboard's sqlite db (`DIRIGENT_API_DB`): who joined, their sign-ins (tokens),
//! the services they upload and each service's usage per UTC hour.
//!
//! Counting each service once, however many desktops upload it:
//! - A service belongs to the first player who uploads it; anyone else's uploads of it are
//!   refused. One service's usage never counts for two players.
//! - Usage is kept per service and hour, and an hour is only replaced by a snapshot with more
//!   messages. A service's usage only grows (its rows outlive its threads), so desktops
//!   uploading the same hour at different moments, in any order, leave its latest state.
//!
//! Numbers are taken on trust (it's a larp), but within bounds no real hour gets near: a
//! board sums them, and one absurd hour mustn't overflow that for everyone.
//! - Hours before the player joined are ignored: the board counts from joining on.

use anyhow::Result;
use dirigent_protocol::leaderboard::{Account, Board, DAY, HOUR, Player, Range, Slot, Upload};
use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::Path;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS users (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    picture TEXT NOT NULL,
    joined_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS tokens (
    hash TEXT PRIMARY KEY,
    user TEXT NOT NULL,
    created_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS services (
    id TEXT PRIMARY KEY,
    user TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS usage (
    service TEXT NOT NULL,
    start INTEGER NOT NULL,
    input INTEGER NOT NULL,
    output INTEGER NOT NULL,
    cache_read INTEGER NOT NULL,
    cache_write INTEGER NOT NULL,
    cost REAL NOT NULL,
    messages INTEGER NOT NULL,
    PRIMARY KEY (service, start)
);
CREATE INDEX IF NOT EXISTS usage_start ON usage (start);
";

/// Slot widths the `All` board picks from, smallest first.
const SLOTS: [i64; 8] = [HOUR, 3 * HOUR, 6 * HOUR, 12 * HOUR, DAY, 2 * DAY, 7 * DAY, 30 * DAY];
/// A board has at most this many slots.
const MAX_SLOTS: i64 = 60;
/// How many hours an upload may carry: the desktop sends at most 30 days.
pub const MAX_HOURS: usize = 40 * 24;
/// How many services a player may upload.
const MAX_SERVICES: i64 = 32;
/// Bounds of an hour's numbers: each token count, messages, dollars.
const MAX_TOKENS: u64 = 1 << 36;
const MAX_MESSAGES: u64 = 1 << 20;
const MAX_COST: f64 = 1e5;

pub struct Store {
    conn: Connection,
}

pub enum Uploaded {
    /// How many hours were new or bigger.
    Saved(usize),
    /// The service is another player's, or a new one past `MAX_SERVICES`.
    Refused,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn })
    }

    #[cfg(test)]
    fn in_memory() -> Self {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        Self { conn }
    }

    /// A forge user signed in: join them if they're new, else take their current name and
    /// picture, and give them a fresh token (one per sign-in, so per desktop).
    pub fn sign_in(&self, id: &str, name: &str, picture: &str, now: i64) -> Result<Account> {
        self.conn.execute(
            "INSERT INTO users (id, name, picture, joined_at) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (id) DO UPDATE SET name = excluded.name, picture = excluded.picture",
            params![id, name, picture, now],
        )?;
        let joined_at = self.conn.query_row("SELECT joined_at FROM users WHERE id = ?1", [id], |r| r.get(0))?;
        let token = format!("{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple());
        self.conn.execute("INSERT INTO tokens (hash, user, created_at) VALUES (?1, ?2, ?3)", params![hash(&token), id, now])?;
        Ok(Account { token, id: id.to_owned(), name: name.to_owned(), picture: picture.to_owned(), joined_at })
    }

    /// Whose token this is.
    pub fn user(&self, token: &str) -> Result<Option<String>> {
        Ok(self.conn.query_row("SELECT user FROM tokens WHERE hash = ?1", [hash(token)], |r| r.get(0)).optional()?)
    }

    /// Save `user`'s upload of a service: its hours that are on the hour, from the hour they
    /// joined to `now`, within bounds, and bigger than what's kept (see the module doc).
    pub fn upload(&mut self, user: &str, up: &Upload, now: i64) -> Result<Uploaded> {
        let owner: Option<String> =
            self.conn.query_row("SELECT user FROM services WHERE id = ?1", [&up.service], |r| r.get(0)).optional()?;
        match owner {
            Some(owner) if owner != user => return Ok(Uploaded::Refused),
            Some(_) => {}
            None => {
                let n: i64 = self.conn.query_row("SELECT COUNT(*) FROM services WHERE user = ?1", [user], |r| r.get(0))?;
                if n >= MAX_SERVICES {
                    return Ok(Uploaded::Refused);
                }
                self.conn.execute("INSERT INTO services (id, user) VALUES (?1, ?2)", params![up.service, user])?;
            }
        }
        let joined: i64 = self.conn.query_row("SELECT joined_at FROM users WHERE id = ?1", [user], |r| r.get(0))?;
        let from = joined - joined.rem_euclid(HOUR);
        let tx = self.conn.transaction()?;
        let mut saved = 0;
        {
            let mut upsert = tx.prepare(
                "INSERT INTO usage (service, start, input, output, cache_read, cache_write, cost, messages)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT (service, start) DO UPDATE SET input = excluded.input, output = excluded.output,
                   cache_read = excluded.cache_read, cache_write = excluded.cache_write, cost = excluded.cost,
                   messages = excluded.messages
                 WHERE excluded.messages > usage.messages",
            )?;
            for h in &up.hours {
                let on_time = h.start.rem_euclid(HOUR) == 0 && (from..=now).contains(&h.start);
                let tokens = [h.input, h.output, h.cache_read, h.cache_write].iter().all(|n| *n <= MAX_TOKENS);
                let sane = tokens && (1..=MAX_MESSAGES).contains(&h.messages) && (0. ..=MAX_COST).contains(&h.cost);
                if on_time && sane {
                    saved += upsert.execute(params![
                        up.service,
                        h.start,
                        h.input as i64,
                        h.output as i64,
                        h.cache_read as i64,
                        h.cache_write as i64,
                        h.cost,
                        h.messages as i64
                    ])?;
                }
            }
        }
        tx.commit()?;
        Ok(Uploaded::Saved(saved))
    }

    /// `user` leaves: gone from the board with all their usage, signed out everywhere.
    pub fn leave(&mut self, user: &str) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM usage WHERE service IN (SELECT id FROM services WHERE user = ?1)", [user])?;
        for table in ["services", "tokens"] {
            tx.execute(&format!("DELETE FROM {table} WHERE user = ?1"), [user])?;
        }
        tx.execute("DELETE FROM users WHERE id = ?1", [user])?;
        tx.commit()?;
        Ok(())
    }

    /// Everyone who joined, with their usage in `range`'s window.
    pub fn board(&self, range: Range, now: i64) -> Result<Board> {
        let first: Option<i64> = self.conn.query_row("SELECT MIN(joined_at) FROM users", [], |r| r.get(0))?;
        let (since, until, slot) = window(range, now, first);
        let mut stmt = self.conn.prepare("SELECT id, name, picture, joined_at FROM users ORDER BY joined_at")?;
        let mut players: Vec<Player> = stmt
            .query_map([], |r| Ok(Player { id: r.get(0)?, name: r.get(1)?, picture: r.get(2)?, joined_at: r.get(3)?, slots: vec![] }))?
            .collect::<rusqlite::Result<_>>()?;
        let index: HashMap<String, usize> = players.iter().enumerate().map(|(i, p)| (p.id.clone(), i)).collect();

        let mut stmt = self.conn.prepare(
            "SELECT s.user, ?1 + ((u.start - ?1) / ?3) * ?3 AS slot, SUM(u.input), SUM(u.output), SUM(u.cache_read),
                    SUM(u.cache_write), SUM(u.cost), SUM(u.messages)
             FROM usage u JOIN services s ON s.id = u.service
             WHERE u.start >= ?1 AND u.start < ?2
             GROUP BY s.user, slot ORDER BY slot",
        )?;
        let rows = stmt.query_map(params![since, until, slot], |r| {
            let n = |i| r.get::<_, i64>(i).map(|v| v as u64);
            let slot = Slot {
                start: r.get(1)?,
                input: n(2)?,
                output: n(3)?,
                cache_read: n(4)?,
                cache_write: n(5)?,
                cost: r.get(6)?,
                messages: n(7)?,
            };
            Ok((r.get::<_, String>(0)?, slot))
        })?;
        for row in rows {
            let (user, slot) = row?;
            if let Some(&i) = index.get(&user) {
                players[i].slots.push(slot);
            }
        }
        Ok(Board { since, until, slot, players })
    }
}

/// `range`'s window: (since, until, slot width), UTC-aligned, `until` past `now`. `All`
/// starts at the first join (`first`), showing a day at least.
fn window(range: Range, now: i64, first: Option<i64>) -> (i64, i64, i64) {
    let (span, slot) = match range {
        Range::Day => (DAY, HOUR),
        Range::Week => (7 * DAY, 3 * HOUR),
        Range::Month => (30 * DAY, 12 * HOUR),
        Range::All => {
            let start = first.unwrap_or(now).min(now - DAY);
            let slot = SLOTS.into_iter().find(|s| (now - start) / s < MAX_SLOTS).unwrap_or(SLOTS[SLOTS.len() - 1]);
            return (floor(start, slot), floor(now, slot) + slot, slot);
        }
    };
    let until = floor(now, slot) + slot;
    (until - span, until, slot)
}

fn floor(t: i64, step: i64) -> i64 {
    t - t.rem_euclid(step)
}

/// Tokens are kept hashed: a leaked db signs nobody in.
fn hash(token: &str) -> String {
    format!("{:x}", Sha256::digest(token.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: i64 = 1_800_000_000_000 - 1_800_000_000_000 % DAY;

    fn hour(start: i64, messages: u64, cost: f64) -> Slot {
        Slot { start, input: messages * 10, output: messages, cache_read: 0, cache_write: 0, cost, messages }
    }

    fn costs(s: &Store, now: i64) -> Vec<(String, f64)> {
        let b = s.board(Range::Day, now).unwrap();
        b.players.iter().map(|p| (p.name.clone(), p.slots.iter().map(|s| s.cost).sum())).collect()
    }

    #[test]
    fn a_service_counts_once() {
        let mut s = Store::in_memory();
        let seb = s.sign_in("u1", "seb", "", T0 + 10).unwrap();
        s.sign_in("u2", "ana", "", T0).unwrap();
        assert_eq!(s.user(&seb.token).unwrap().as_deref(), Some("u1"));
        let up = |hours: Vec<Slot>| Upload { service: "svc".into(), hours };
        let now = T0 + 5 * HOUR;
        // The laptop's later snapshot of hour 1 lands first, the desktop's older one after:
        // the bigger stays. Hour 2 is uploaded by both: counted once.
        assert!(matches!(s.upload("u1", &up(vec![hour(T0 + HOUR, 3, 3.), hour(T0 + 2 * HOUR, 1, 1.)]), now), Ok(Uploaded::Saved(2))));
        assert!(matches!(s.upload("u1", &up(vec![hour(T0 + HOUR, 2, 2.), hour(T0 + 2 * HOUR, 1, 1.)]), now), Ok(Uploaded::Saved(0))));
        // Another player can't count the same service.
        assert!(matches!(s.upload("u2", &up(vec![hour(T0 + 3 * HOUR, 9, 9.)]), now), Ok(Uploaded::Refused)));
        assert_eq!(costs(&s, now), [("ana".to_owned(), 0.), ("seb".to_owned(), 4.)]);
    }

    #[test]
    fn uploads_stay_within_bounds() {
        let mut s = Store::in_memory();
        s.sign_in("u1", "seb", "", T0).unwrap();
        let now = T0 + 5 * HOUR;
        // Numbers past any real hour would overflow (or turn to inf in) every board's sums.
        let huge = Slot { start: T0 + HOUR, input: u64::MAX, messages: 1, ..Slot::default() };
        let costly = Slot { start: T0 + 2 * HOUR, cost: 1e308, messages: 1, ..Slot::default() };
        let up = |service: &str, hours: Vec<Slot>| Upload { service: service.into(), hours };
        assert!(matches!(s.upload("u1", &up("a", vec![huge, costly, hour(T0 + 3 * HOUR, 1, 1.)]), now), Ok(Uploaded::Saved(1))));
        assert_eq!(costs(&s, now), [("seb".to_owned(), 1.)]);
        // And a player has only so many services.
        for i in 1..MAX_SERVICES {
            assert!(matches!(s.upload("u1", &up(&format!("s{i}"), vec![]), now), Ok(Uploaded::Saved(0))));
        }
        assert!(matches!(s.upload("u1", &up("one-too-many", vec![]), now), Ok(Uploaded::Refused)));
    }

    #[test]
    fn only_counts_from_joining_on() {
        let mut s = Store::in_memory();
        s.sign_in("u1", "seb", "", T0 + 2 * HOUR + 5).unwrap();
        let now = T0 + 5 * HOUR;
        let hours = vec![hour(T0 + HOUR, 1, 1.), hour(T0 + 2 * HOUR, 1, 2.), hour(T0 + 3 * HOUR + 1, 1, 4.), hour(T0 + 9 * HOUR, 1, 8.)];
        // Before joining, off the hour, and in the future: dropped.
        assert!(matches!(s.upload("u1", &Upload { service: "a".into(), hours }, now), Ok(Uploaded::Saved(1))));
        // Signing in again keeps the join time.
        assert_eq!(s.sign_in("u1", "seb2", "", now).unwrap().joined_at, T0 + 2 * HOUR + 5);
        assert_eq!(costs(&s, now), [("seb2".to_owned(), 2.)]);
        s.leave("u1").unwrap();
        assert!(s.board(Range::All, now).unwrap().players.is_empty());
    }

    #[test]
    fn board_slots() {
        let mut s = Store::in_memory();
        s.sign_in("u1", "seb", "", T0 - 10 * DAY).unwrap();
        let now = T0 + 5 * HOUR;
        // Usage on three days before today, and two hours of today in one 3h slot.
        let mut hours: Vec<Slot> = (1..=3).map(|d| hour(T0 - d * DAY, 1, 1.)).collect();
        hours.extend([hour(T0 + 3 * HOUR, 1, 1.), hour(T0 + 4 * HOUR, 2, 2.)]);
        s.upload("u1", &Upload { service: "a".into(), hours }, now).unwrap();
        let b = s.board(Range::Week, now).unwrap();
        assert_eq!((b.until, b.slot, b.until - b.since), (T0 + 6 * HOUR, 3 * HOUR, 7 * DAY));
        let p = &b.players[0];
        assert_eq!(p.slots.last().map(|s| (s.start, s.messages, s.cost)), Some((T0 + 3 * HOUR, 3, 3.)));
        // `All` starts at the first join, in slots that keep it under 60.
        let b = s.board(Range::All, now).unwrap();
        assert_eq!((b.slot, b.since), (6 * HOUR, T0 - 10 * DAY));
    }
}
