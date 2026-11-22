//! Counting what the web interface sends, per role and UTC calendar month,
//! and the anonymous monthly budget (PLAN.md 3.5b).
//!
//! HTTP responses are counted by a middleware (body plus headers);
//! WebSocket messages through [`Sock`]. Both are application bytes: TCP
//! and TLS overhead come on top, roughly 5 % more.
//!
//! As anonymous traffic nears the budget, anonymous spectrum drops to one
//! frame a second ([`Level::Low`], from 80 %). Once it is spent, anonymous
//! spectrum, live audio and recording audio stop until the month turns.
//! Admin traffic is counted but never held back.
//!
//! Totals are kept in `settings.sqlite`, flushed every few seconds and on
//! shutdown, so a crash loses at most that much counting.

use crate::Role;
use axum::extract::ws::{Message, WebSocket};
use scanner_store::settings::Settings;
use serde::Serialize;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

/// Anonymous traffic is slowed past this share of its budget.
pub const LOW_SHARE: f64 = 0.8;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Ok,
    Low,
    Spent,
}

#[derive(Debug, Default)]
struct Month {
    key: String,
    resets_ns: i64,
    anonymous: u64,
    admin: u64,
    /// Not yet written to the database.
    pending_anonymous: u64,
    pending_admin: u64,
}

pub struct Egress {
    /// Anonymous bytes per month; 0 means no budget.
    budget: u64,
    month: Mutex<Month>,
    settings: Option<Arc<Settings>>,
    now: fn() -> SystemTime,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EgressStatus {
    /// `YYYY-MM`, UTC.
    pub month: String,
    pub anonymous_bytes: u64,
    pub admin_bytes: u64,
    /// 0: no budget.
    pub budget_bytes: u64,
    pub level: Level,
    /// When the month (and the budget) turns over.
    pub resets_ns: i64,
}

fn month_of(t: SystemTime) -> (String, i64) {
    let ((y, m), _, next) = scanner_core::clock::utc_month(t);
    (format!("{y:04}-{m:02}"), next * 1_000_000_000)
}

fn role_name(r: Role) -> &'static str {
    match r {
        Role::Anonymous => "anonymous",
        Role::Admin => "admin",
    }
}

impl Egress {
    /// Picks up this month's totals from `settings`, if given.
    pub fn new(budget_bytes: u64, settings: Option<Arc<Settings>>) -> Self {
        Self::with_clock(budget_bytes, settings, SystemTime::now)
    }

    fn with_clock(
        budget_bytes: u64,
        settings: Option<Arc<Settings>>,
        now: fn() -> SystemTime,
    ) -> Self {
        let e = Egress {
            budget: budget_bytes,
            month: Mutex::new(Month::default()),
            settings,
            now,
        };
        drop(e.lock()); // load this month's totals
        e
    }

    /// The current month, rolled over (and reloaded) if the clock moved on.
    fn lock(&self) -> std::sync::MutexGuard<'_, Month> {
        let mut g = self.month.lock().unwrap_or_else(|p| p.into_inner());
        let (key, resets_ns) = month_of((self.now)());
        if g.key != key {
            // Write what the old month still owes, then start the new one
            // from whatever the database has (a restart mid-month).
            let old = std::mem::take(&mut *g);
            if let Some(s) = &self.settings {
                if !old.key.is_empty() {
                    write(s, &old.key, old.pending_anonymous, old.pending_admin);
                }
                for e in s.egress(1).unwrap_or_default() {
                    if e.month == key {
                        match e.role.as_str() {
                            "anonymous" => g.anonymous = e.bytes,
                            "admin" => g.admin = e.bytes,
                            _ => {}
                        }
                    }
                }
            }
            g.key = key;
            g.resets_ns = resets_ns;
        }
        g
    }

    pub fn add(&self, role: Role, bytes: u64) {
        let mut g = self.lock();
        match role {
            Role::Anonymous => {
                g.anonymous += bytes;
                g.pending_anonymous += bytes;
            }
            Role::Admin => {
                g.admin += bytes;
                g.pending_admin += bytes;
            }
        }
    }

    fn level_of(&self, anonymous: u64) -> Level {
        if self.budget == 0 {
            Level::Ok
        } else if anonymous >= self.budget {
            Level::Spent
        } else if anonymous as f64 >= LOW_SHARE * self.budget as f64 {
            Level::Low
        } else {
            Level::Ok
        }
    }

    /// What `role` is allowed right now. Admins are always `Ok`.
    pub fn level(&self, role: Role) -> Level {
        match role {
            Role::Admin => Level::Ok,
            Role::Anonymous => {
                let a = self.lock().anonymous;
                self.level_of(a)
            }
        }
    }

    pub fn status(&self) -> EgressStatus {
        let g = self.lock();
        EgressStatus {
            month: g.key.clone(),
            anonymous_bytes: g.anonymous,
            admin_bytes: g.admin,
            budget_bytes: self.budget,
            level: self.level_of(g.anonymous),
            resets_ns: g.resets_ns,
        }
    }

    /// Write the counts gathered since the last flush. Blocking (SQLite).
    pub fn flush(&self) {
        let Some(s) = &self.settings else { return };
        let (key, a, b) = {
            let mut g = self.lock();
            let out = (g.key.clone(), g.pending_anonymous, g.pending_admin);
            g.pending_anonymous = 0;
            g.pending_admin = 0;
            out
        };
        write(s, &key, a, b);
    }

    pub fn meter(self: &Arc<Self>, role: Role) -> Meter {
        Meter {
            egress: Some(self.clone()),
            role,
        }
    }
}

fn write(s: &Settings, month: &str, anonymous: u64, admin: u64) {
    for (role, n) in [(Role::Anonymous, anonymous), (Role::Admin, admin)] {
        if n > 0
            && let Err(e) = s.add_egress(month, role_name(role), n)
        {
            eprintln!("egress: {e}");
        }
    }
}

/// Counts for one connection.
#[derive(Clone)]
pub struct Meter {
    egress: Option<Arc<Egress>>,
    role: Role,
}

impl Meter {
    pub fn none(role: Role) -> Self {
        Meter { egress: None, role }
    }

    pub fn add(&self, bytes: usize) {
        if let Some(e) = &self.egress {
            e.add(self.role, bytes as u64);
        }
    }

    pub fn level(&self) -> Level {
        self.egress
            .as_ref()
            .map_or(Level::Ok, |e| e.level(self.role))
    }
}

/// A WebSocket that counts what it sends.
pub struct Sock {
    inner: WebSocket,
    pub meter: Meter,
}

impl Sock {
    pub fn new(inner: WebSocket, meter: Meter) -> Self {
        Sock { inner, meter }
    }

    pub async fn send(&mut self, m: Message) -> Result<(), axum::Error> {
        let n = match &m {
            Message::Text(t) => t.len(),
            Message::Binary(b) => b.len(),
            Message::Ping(b) | Message::Pong(b) => b.len(),
            Message::Close(_) => 2,
        };
        // Server frames are unmasked: 2 to 10 bytes of header.
        self.meter.add(
            n + if n < 126 {
                2
            } else if n < 65_536 {
                4
            } else {
                10
            },
        );
        self.inner.send(m).await
    }

    pub async fn recv(&mut self) -> Option<Result<Message, axum::Error>> {
        self.inner.recv().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, UNIX_EPOCH};

    #[test]
    fn levels_follow_the_anonymous_budget() {
        let e = Egress::new(1000, None);
        assert_eq!(e.level(Role::Anonymous), Level::Ok);
        e.add(Role::Admin, 5000);
        assert_eq!(
            e.level(Role::Anonymous),
            Level::Ok,
            "admin bytes don't count"
        );
        e.add(Role::Anonymous, 800);
        assert_eq!(e.level(Role::Anonymous), Level::Low);
        e.add(Role::Anonymous, 200);
        assert_eq!(e.level(Role::Anonymous), Level::Spent);
        assert_eq!(e.level(Role::Admin), Level::Ok);
        let st = e.status();
        assert_eq!((st.anonymous_bytes, st.admin_bytes), (1000, 5000));
        assert_eq!(Egress::new(0, None).level(Role::Anonymous), Level::Ok);
    }

    #[test]
    fn totals_survive_a_restart_and_reset_with_the_month() {
        let s = Arc::new(Settings::in_memory().unwrap());
        fn nov() -> SystemTime {
            UNIX_EPOCH + Duration::from_secs(1_795_330_443) // 2026-11-22
        }
        fn dec() -> SystemTime {
            UNIX_EPOCH + Duration::from_secs(1_796_083_200 + 60) // 2026-12-01
        }
        let e = Egress::with_clock(1000, Some(s.clone()), nov);
        e.add(Role::Anonymous, 900);
        e.flush();
        e.add(Role::Anonymous, 50); // not flushed yet
        // A restart picks up what was flushed.
        let e2 = Egress::with_clock(1000, Some(s.clone()), nov);
        assert_eq!(e2.status().anonymous_bytes, 900);
        assert_eq!(e2.level(Role::Anonymous), Level::Low);
        e.flush();
        // The month turns: a fresh budget, and November keeps its total.
        let e3 = Egress::with_clock(1000, Some(s.clone()), dec);
        let st = e3.status();
        assert_eq!((st.month.as_str(), st.anonymous_bytes), ("2026-12", 0));
        assert_eq!(e3.level(Role::Anonymous), Level::Ok);
        let nov_total: Vec<_> = s.egress(12).unwrap();
        assert_eq!(nov_total[0].month, "2026-11");
        assert_eq!(nov_total[0].bytes, 950);
        assert_eq!(st.resets_ns, 1_798_761_600 * 1_000_000_000); // 2027-01-01
    }
}
