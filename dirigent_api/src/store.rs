use anyhow::Result;
use dirigent_protocol::leaderboard::{Account, Board, DAY, HOUR, Player, Range, Slot, Upload};
use rusqlite::{Connection, OptionalExtension, params};
use std::collections::HashMap;
use std::path::Path;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS users (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    picture TEXT NOT NULL,
    joined_at INTEGER NOT NULL
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

const SLOTS: [i64; 8] = [HOUR, 3 * HOUR, 6 * HOUR, 12 * HOUR, DAY, 2 * DAY, 7 * DAY, 30 * DAY];
const MAX_SLOTS: i64 = 60;
pub const MAX_HOURS: usize = 40 * 24;
const MAX_SERVICES: i64 = 32;
const MAX_TOKENS: u64 = 1 << 36;
const MAX_MESSAGES: u64 = 1 << 20;
const MAX_COST: f64 = 1e5;

pub struct Store {
    conn: Connection,
}

pub enum Uploaded {
    Saved(usize),
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
    pub(crate) fn in_memory() -> Self {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        Self { conn }
    }

    pub fn join(&self, id: &str, name: &str, picture: &str, now: i64) -> Result<Account> {
        self.conn.execute(
            "INSERT INTO users (id, name, picture, joined_at) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (id) DO UPDATE SET name = excluded.name, picture = excluded.picture",
            params![id, name, picture, now],
        )?;
        let joined_at = self.conn.query_row("SELECT joined_at FROM users WHERE id = ?1", [id], |r| r.get(0))?;
        Ok(Account { id: id.to_owned(), name: name.to_owned(), picture: picture.to_owned(), joined_at })
    }

    pub fn joined(&self, id: &str) -> Result<bool> {
        Ok(self.conn.query_row("SELECT 1 FROM users WHERE id = ?1", [id], |_| Ok(())).optional()?.is_some())
    }

    pub fn upload(&mut self, user: &str, up: &Upload, now: i64) -> Result<Uploaded> {
        if !self.claim_service(user, &up.service)? {
            return Ok(Uploaded::Refused);
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

    fn claim_service(&self, user: &str, service: &str) -> Result<bool> {
        let owner: Option<String> = self.conn.query_row("SELECT user FROM services WHERE id = ?1", [service], |r| r.get(0)).optional()?;
        if let Some(owner) = owner {
            return Ok(owner == user);
        }
        let n: i64 = self.conn.query_row("SELECT COUNT(*) FROM services WHERE user = ?1", [user], |r| r.get(0))?;
        if n >= MAX_SERVICES {
            return Ok(false);
        }
        self.conn.execute("INSERT INTO services (id, user) VALUES (?1, ?2)", params![service, user])?;
        Ok(true)
    }

    pub fn leave(&mut self, user: &str) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM usage WHERE service IN (SELECT id FROM services WHERE user = ?1)", [user])?;
        tx.execute("DELETE FROM services WHERE user = ?1", [user])?;
        tx.execute("DELETE FROM users WHERE id = ?1", [user])?;
        tx.commit()?;
        Ok(())
    }

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
        s.join("u1", "seb", "", T0 + 10).unwrap();
        s.join("u2", "ana", "", T0).unwrap();
        let up = |hours: Vec<Slot>| Upload { service: "svc".into(), hours };
        let now = T0 + 5 * HOUR;
        assert!(matches!(s.upload("u1", &up(vec![hour(T0 + HOUR, 3, 3.), hour(T0 + 2 * HOUR, 1, 1.)]), now), Ok(Uploaded::Saved(2))));
        assert!(matches!(s.upload("u1", &up(vec![hour(T0 + HOUR, 2, 2.), hour(T0 + 2 * HOUR, 1, 1.)]), now), Ok(Uploaded::Saved(0))));
        assert!(matches!(s.upload("u2", &up(vec![hour(T0 + 3 * HOUR, 9, 9.)]), now), Ok(Uploaded::Refused)));
        assert_eq!(costs(&s, now), [("ana".to_owned(), 0.), ("seb".to_owned(), 4.)]);
    }

    #[test]
    fn uploads_stay_within_bounds() {
        let mut s = Store::in_memory();
        s.join("u1", "seb", "", T0).unwrap();
        let now = T0 + 5 * HOUR;
        let huge = Slot { start: T0 + HOUR, input: u64::MAX, messages: 1, ..Slot::default() };
        let costly = Slot { start: T0 + 2 * HOUR, cost: 1e308, messages: 1, ..Slot::default() };
        let up = |service: &str, hours: Vec<Slot>| Upload { service: service.into(), hours };
        assert!(matches!(s.upload("u1", &up("a", vec![huge, costly, hour(T0 + 3 * HOUR, 1, 1.)]), now), Ok(Uploaded::Saved(1))));
        assert_eq!(costs(&s, now), [("seb".to_owned(), 1.)]);
        for i in 1..MAX_SERVICES {
            assert!(matches!(s.upload("u1", &up(&format!("s{i}"), vec![]), now), Ok(Uploaded::Saved(0))));
        }
        assert!(matches!(s.upload("u1", &up("one-too-many", vec![]), now), Ok(Uploaded::Refused)));
    }

    #[test]
    fn only_counts_from_joining_on() {
        let mut s = Store::in_memory();
        s.join("u1", "seb", "", T0 + 2 * HOUR + 5).unwrap();
        let now = T0 + 5 * HOUR;
        let hours = vec![hour(T0 + HOUR, 1, 1.), hour(T0 + 2 * HOUR, 1, 2.), hour(T0 + 3 * HOUR + 1, 1, 4.), hour(T0 + 9 * HOUR, 1, 8.)];
        assert!(matches!(s.upload("u1", &Upload { service: "a".into(), hours }, now), Ok(Uploaded::Saved(1))));
        assert_eq!(s.join("u1", "seb2", "", now).unwrap().joined_at, T0 + 2 * HOUR + 5);
        assert_eq!(costs(&s, now), [("seb2".to_owned(), 2.)]);
        s.leave("u1").unwrap();
        assert!(s.board(Range::All, now).unwrap().players.is_empty());
    }

    #[test]
    fn board_slots() {
        let mut s = Store::in_memory();
        s.join("u1", "seb", "", T0 - 10 * DAY).unwrap();
        let now = T0 + 5 * HOUR;
        let mut hours: Vec<Slot> = (1..=3).map(|d| hour(T0 - d * DAY, 1, 1.)).collect();
        hours.extend([hour(T0 + 3 * HOUR, 1, 1.), hour(T0 + 4 * HOUR, 2, 2.)]);
        s.upload("u1", &Upload { service: "a".into(), hours }, now).unwrap();
        let b = s.board(Range::Week, now).unwrap();
        assert_eq!((b.until, b.slot, b.until - b.since), (T0 + 6 * HOUR, 3 * HOUR, 7 * DAY));
        let p = &b.players[0];
        assert_eq!(p.slots.last().map(|s| (s.start, s.messages, s.cost)), Some((T0 + 3 * HOUR, 3, 3.)));
        let b = s.board(Range::All, now).unwrap();
        assert_eq!((b.slot, b.since), (6 * HOUR, T0 - 10 * DAY));
    }
}
