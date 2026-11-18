//! Short-lived URL tickets (alforria extension over `pty/ticket.ts`).
//!
//! A browser cannot put an `Authorization` header on a websocket handshake,
//! an `<img>` or a download link. A ticket stands in for it: a random,
//! short-lived string, bound to one thing (its *scope*), passed as
//! `?ticket=`. Upstream has this for terminal websockets only; [`Tickets`]
//! is that cache with the scope left open, so files can use it too
//! ([`FileTickets`]).

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use alforria_core::Clock;

/// `CAPACITY` (`ticket.ts:10`): the oldest tickets are evicted beyond it.
pub const CAPACITY: usize = 10_000;

struct Ticket<S> {
    expires_at_ms: u64,
    scope: S,
}

struct Entries<S> {
    order: VecDeque<String>,
    map: HashMap<String, Ticket<S>>,
}

/// A bounded cache of live tickets, each bound to a scope `S`.
pub struct Tickets<S> {
    clock: Arc<dyn Clock>,
    entries: Arc<Mutex<Entries<S>>>,
}

impl<S> Clone for Tickets<S> {
    fn clone(&self) -> Self {
        Tickets {
            clock: self.clock.clone(),
            entries: self.entries.clone(),
        }
    }
}

fn prune_expired<S>(entries: &mut Entries<S>, now: u64) {
    let expired: Vec<String> = entries
        .map
        .iter()
        .filter(|(_, ticket)| ticket.expires_at_ms <= now)
        .map(|(ticket, _)| ticket.clone())
        .collect();
    for ticket in expired {
        entries.map.remove(&ticket);
        entries.order.retain(|entry| entry != &ticket);
    }
}

impl<S: Clone> Tickets<S> {
    pub fn new(clock: Arc<dyn Clock>) -> Tickets<S> {
        Tickets {
            clock,
            entries: Arc::new(Mutex::new(Entries {
                order: VecDeque::new(),
                map: HashMap::new(),
            })),
        }
    }

    /// Mint a UUIDv4 ticket for `scope`, live for `ttl`
    /// (`issue`, `ticket.ts:43-47`).
    pub fn issue(&self, scope: S, ttl: Duration) -> String {
        let ticket = uuid::Uuid::new_v4().to_string();
        let now = self.clock.now_ms();
        let mut entries = self.entries.lock().unwrap_or_else(|p| p.into_inner());
        prune_expired(&mut entries, now);
        while entries.map.len() >= CAPACITY {
            match entries.order.pop_front() {
                Some(oldest) => entries.map.remove(&oldest),
                None => break,
            };
        }
        entries.order.push_back(ticket.clone());
        entries.map.insert(
            ticket.clone(),
            Ticket {
                expires_at_ms: now + ttl.as_millis() as u64,
                scope,
            },
        );
        ticket
    }

    /// The scope of a live ticket, without consuming it.
    pub fn peek(&self, ticket: &str) -> Option<S> {
        let now = self.clock.now_ms();
        let entries = self.entries.lock().unwrap_or_else(|p| p.into_inner());
        entries
            .map
            .get(ticket)
            .filter(|stored| stored.expires_at_ms > now)
            .map(|stored| stored.scope.clone())
    }

    /// Remove the ticket iff it is live and `accept`s its scope; a ticket
    /// that does not match is left alone (`consume`, `ticket.ts:48-50`).
    pub fn consume(&self, ticket: &str, accept: impl FnOnce(&S) -> bool) -> bool {
        let now = self.clock.now_ms();
        let mut entries = self.entries.lock().unwrap_or_else(|p| p.into_inner());
        prune_expired(&mut entries, now);
        match entries.map.get(ticket) {
            Some(stored) if stored.expires_at_ms > now && accept(&stored.scope) => {
                entries.map.remove(ticket);
                entries.order.retain(|entry| entry != ticket);
                true
            }
            _ => false,
        }
    }
}

/// What a file ticket is good for: reading one file of one project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileScope {
    /// The project directory, resolved.
    pub directory: PathBuf,
    /// The file, resolved (symlinks followed).
    pub file: PathBuf,
}

/// `ttl` of a file ticket when the request names none.
pub const FILE_TICKET_TTL: Duration = Duration::from_secs(60);
/// The longest `ttl` a request may ask for.
pub const FILE_TICKET_MAX_TTL: Duration = Duration::from_secs(600);

/// Read-only, single-file tickets for `GET /file/raw` and
/// `GET /file/content`. Unlike a terminal ticket a file ticket is not
/// consumed by use: a browser fetches an embedded image more than once
/// (range requests, a retry, a reload) and each fetch reads the same file.
/// It simply stops working when it expires.
#[derive(Clone)]
pub struct FileTickets {
    inner: Tickets<FileScope>,
}

impl FileTickets {
    pub fn new(clock: Arc<dyn Clock>) -> FileTickets {
        FileTickets {
            inner: Tickets::new(clock),
        }
    }

    pub fn issue(&self, scope: FileScope, ttl: Duration) -> String {
        self.inner.issue(scope, ttl)
    }

    /// Whether `ticket` is live and was issued for exactly this file of
    /// this project.
    pub fn admits(&self, ticket: &str, scope: &FileScope) -> bool {
        self.inner
            .peek(ticket)
            .is_some_and(|stored| stored == *scope)
    }
}

impl Default for FileTickets {
    fn default() -> FileTickets {
        FileTickets::new(Arc::new(alforria_core::catalog::SystemClock))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MutableClock(Mutex<u64>);

    impl Clock for MutableClock {
        fn now_ms(&self) -> u64 {
            *self.0.lock().unwrap()
        }
    }

    fn scope(file: &str) -> FileScope {
        FileScope {
            directory: PathBuf::from("/repo"),
            file: PathBuf::from(file),
        }
    }

    #[test]
    fn a_file_ticket_admits_its_own_file_until_it_expires() {
        let clock = Arc::new(MutableClock(Mutex::new(1_000)));
        let tickets = FileTickets::new(clock.clone());
        let ticket = tickets.issue(scope("/repo/a.png"), Duration::from_secs(60));
        assert!(tickets.admits(&ticket, &scope("/repo/a.png")));
        assert!(
            tickets.admits(&ticket, &scope("/repo/a.png")),
            "use does not consume it"
        );
        assert!(!tickets.admits(&ticket, &scope("/repo/b.png")));
        assert!(!tickets.admits(
            &ticket,
            &FileScope {
                directory: PathBuf::from("/elsewhere"),
                file: PathBuf::from("/repo/a.png"),
            }
        ));
        assert!(!tickets.admits("nope", &scope("/repo/a.png")));

        *clock.0.lock().unwrap() += 59_999;
        assert!(tickets.admits(&ticket, &scope("/repo/a.png")));
        *clock.0.lock().unwrap() += 1;
        assert!(!tickets.admits(&ticket, &scope("/repo/a.png")));
    }

    #[test]
    fn consume_is_single_use_and_leaves_a_mismatch_alone() {
        let clock = Arc::new(MutableClock(Mutex::new(0)));
        let tickets: Tickets<u32> = Tickets::new(clock);
        let ticket = tickets.issue(7, Duration::from_secs(1));
        assert!(!tickets.consume(&ticket, |scope| *scope == 8));
        assert_eq!(tickets.peek(&ticket), Some(7));
        assert!(tickets.consume(&ticket, |scope| *scope == 7));
        assert!(!tickets.consume(&ticket, |scope| *scope == 7));
        assert_eq!(tickets.peek(&ticket), None);
    }
}
