//! PTY connect-ticket cache — port of `packages/core/src/pty/ticket.ts`.
//!
//! Tickets are UUIDv4 strings held in a bounded (10 000 entries) cache with
//! a 60 s TTL. `issue` mints one, `consume` removes it atomically iff the
//! stored scope (ptyID + directory + workspaceID) matches the request scope
//! (`ticket.ts:20-56`).

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use alforria_core::Clock;
use alforria_schema::pty_ticket::PtyTicketConnectToken;

/// `DEFAULT_TTL` (`ticket.ts:9`).
pub const DEFAULT_TTL: Duration = Duration::from_secs(60);
/// `CAPACITY` (`ticket.ts:10`).
pub const CAPACITY: usize = 10_000;

/// `Scope` (`ticket.ts:14-18`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scope {
    pub pty_id: String,
    pub directory: Option<String>,
    pub workspace_id: Option<String>,
}

#[derive(Debug, Clone)]
struct Ticket {
    expires_at_ms: u64,
    scope: Scope,
}

#[derive(Default)]
struct Entries {
    order: VecDeque<String>,
    map: HashMap<String, Ticket>,
}

/// `PtyTicket.Service` — the process-wide single-use connect-ticket cache.
#[derive(Clone)]
pub struct TicketCache {
    ttl: Duration,
    clock: Arc<dyn Clock>,
    entries: Arc<Mutex<Entries>>,
}

impl TicketCache {
    pub fn new(ttl: Duration, clock: Arc<dyn Clock>) -> TicketCache {
        TicketCache {
            ttl,
            clock,
            entries: Arc::new(Mutex::new(Entries::default())),
        }
    }

    /// `issue` (`ticket.ts:43-47`): fresh UUIDv4 ticket, `expires_in` in
    /// seconds (at least 1).
    pub fn issue(&self, scope: Scope) -> PtyTicketConnectToken {
        let ticket = uuid::Uuid::new_v4().to_string();
        let now = self.clock.now_ms();
        let mut entries = self.entries.lock().unwrap_or_else(|p| p.into_inner());
        prune_expired(&mut entries, now);
        while entries.map.len() >= CAPACITY {
            if let Some(oldest) = entries.order.pop_front() {
                entries.map.remove(&oldest);
            } else {
                break;
            }
        }
        entries.order.push_back(ticket.clone());
        entries.map.insert(
            ticket.clone(),
            Ticket {
                expires_at_ms: now + self.ttl.as_millis() as u64,
                scope,
            },
        );
        PtyTicketConnectToken {
            ticket,
            expires_in: self.ttl.as_secs().max(1),
        }
    }

    /// `consume` (`ticket.ts:48-50`): removes the ticket iff it is live and
    /// the stored scope matches; a mismatched ticket is *not* consumed.
    pub fn consume(&self, ticket: &str, scope: &Scope) -> bool {
        let now = self.clock.now_ms();
        let mut entries = self.entries.lock().unwrap_or_else(|p| p.into_inner());
        prune_expired(&mut entries, now);
        match entries.map.get(ticket) {
            Some(stored) if stored.expires_at_ms > now && stored.scope == *scope => {
                entries.map.remove(ticket);
                entries.order.retain(|entry| entry != ticket);
                true
            }
            _ => false,
        }
    }
}

impl Default for TicketCache {
    fn default() -> TicketCache {
        TicketCache::new(DEFAULT_TTL, Arc::new(alforria_core::catalog::SystemClock))
    }
}

fn prune_expired(entries: &mut Entries, now: u64) {
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

#[cfg(test)]
mod tests {
    use super::*;

    struct MutableClock(Mutex<u64>);

    impl Clock for MutableClock {
        fn now_ms(&self) -> u64 {
            *self.0.lock().unwrap_or_else(|p| p.into_inner())
        }
    }

    fn cache() -> (TicketCache, Arc<MutableClock>) {
        let clock = Arc::new(MutableClock(Mutex::new(1_000)));
        (
            TicketCache::new(Duration::from_secs(60), clock.clone()),
            clock,
        )
    }

    fn scope() -> Scope {
        Scope {
            pty_id: "pty_1".to_string(),
            directory: Some("/repo".to_string()),
            workspace_id: None,
        }
    }

    #[test]
    fn issues_uuid_v4_with_ttl_expiry() {
        let (cache, _) = cache();
        let token = cache.issue(scope());
        assert_eq!(token.expires_in, 60);
        assert!(uuid::Uuid::try_parse(&token.ticket).is_ok());
        assert_eq!(
            uuid::Uuid::try_parse(&token.ticket)
                .unwrap()
                .get_version_num(),
            4
        );
    }

    #[test]
    fn consumption_is_single_use_and_scope_bound() {
        let (cache, _) = cache();
        let token = cache.issue(scope());
        assert!(cache.consume(&token.ticket, &scope()));
        assert!(
            !cache.consume(&token.ticket, &scope()),
            "tickets are single-use"
        );

        let token = cache.issue(scope());
        assert!(!cache.consume(
            &token.ticket,
            &Scope {
                directory: Some("/other".to_string()),
                ..scope()
            }
        ));
        assert!(
            cache.consume(&token.ticket, &scope()),
            "a mismatched consume leaves the ticket alive"
        );
    }

    #[test]
    fn tickets_expire_after_the_ttl() {
        let (cache, clock) = cache();
        let token = cache.issue(scope());
        *clock.0.lock().unwrap() += 60_000;
        assert!(!cache.consume(&token.ticket, &scope()));

        let token = cache.issue(scope());
        *clock.0.lock().unwrap() += 59_999;
        assert!(cache.consume(&token.ticket, &scope()));
    }

    #[test]
    fn capacity_bounds_the_cache() {
        let (cache, _) = cache();
        let tickets: Vec<String> = (0..CAPACITY + 10)
            .map(|_| cache.issue(scope()).ticket)
            .collect();
        // The first 10 tickets were evicted; the rest are still consumable.
        assert!(!cache.consume(&tickets[0], &scope()));
        assert!(cache.consume(&tickets[CAPACITY + 9], &scope()));
    }

    #[test]
    fn unknown_tickets_are_not_consumed() {
        let (cache, _) = cache();
        assert!(!cache.consume("nope", &scope()));
    }
}
