//! The leaderboard: what the desktop sends `dirigent_api` and reads back from it (the api's
//! `leaderboard.rs` has the endpoints). Like `release`, a contract with installed versions:
//! change it additively.
//!
//! Usage is uploaded per service (a `dirigent_service`, identified by the id it sends in
//! `ServerMessage::Hello`), summed per UTC hour. The api keeps one row per service and hour
//! and the bigger of two snapshots of it, so desktops that share a service can upload it
//! as often as they like: it counts once.

use serde::{Deserialize, Serialize};

pub const HOUR: i64 = 3_600_000;
pub const DAY: i64 = 24 * HOUR;

/// Who joined: `POST /api/v0/join`'s answer. The desktop keeps it in its data dir, as JSON:
/// a field added later needs `#[serde(default)]`, or reading the old one fails and it's out
/// of the board; one dropped is ignored (desktops up to 0.0.6 saved a `token` too).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Account {
    /// The forge user id: survives renames.
    pub id: String,
    pub name: String,
    /// The forge profile picture's url.
    pub picture: String,
    /// Unix millis. Only usage from this hour on counts.
    pub joined_at: i64,
}

/// `PUT /api/v0/usage` (bearer: a forge JWT): one service's usage per hour.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Upload {
    pub service: String,
    pub hours: Vec<Slot>,
}

/// Usage summed over a time slot: an hour in an `Upload`, a board's slot in a `Player`.
/// Only slots with usage are sent.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Slot {
    /// Unix millis the slot starts at.
    pub start: i64,
    /// Fresh input tokens; `cache_read` / `cache_write` are the rest of the prompt.
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    /// Dollars.
    pub cost: f64,
    /// Assistant messages.
    pub messages: u64,
}

/// The windows the board is shown for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Range {
    Day,
    Week,
    #[default]
    Month,
    /// Since the first player joined.
    All,
}

impl Range {
    pub const ALL: [Range; 4] = [Range::Day, Range::Week, Range::Month, Range::All];

    /// As in `GET /api/v0/leaderboard?range=…`, and the desktop's label.
    pub fn as_str(self) -> &'static str {
        match self {
            Range::Day => "1d",
            Range::Week => "7d",
            Range::Month => "30d",
            Range::All => "all",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|r| r.as_str() == s)
    }
}

/// `GET /api/v0/leaderboard?range=…`: every player's usage in the window, in slots of
/// `slot` ms from `since` (UTC-aligned), up to `until` (exclusive, past now).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Board {
    pub since: i64,
    pub until: i64,
    pub slot: i64,
    pub players: Vec<Player>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Player {
    pub id: String,
    pub name: String,
    pub picture: String,
    pub joined_at: i64,
    /// The slots with usage, oldest first.
    pub slots: Vec<Slot>,
}
