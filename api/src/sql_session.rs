//! SQL session/transaction registry — `PHASE_RELATIONAL_SQL_API_
//! ARCHITECTURE.md` §3 (the "HTTP transaction model" ADR this crate's
//! own governing directive singled out as "a critical design area").
//!
//! A session exists **only** while an explicit SQL transaction
//! (`BEGIN` ... `COMMIT`/`ROLLBACK`) is open. An ordinary, non-
//! transactional statement never touches this registry at all — it
//! runs through `rubixdb_sql::exec::execute_autocommit`/`execute_
//! write_autocommit`, which already begin-and-commit their own
//! throwaway `Transaction` per call, exactly as any other caller of
//! `rubixdb-sql` does. This keeps the common case (the overwhelming
//! majority of CLI script-mode/frontend one-shot queries) completely
//! stateless, and keeps this registry's own size bounded by "how many
//! clients currently have an open transaction," not "how many clients
//! have ever connected."
//!
//! Session IDs are `Uuid::new_v4()` — the same cryptographically-random
//! (CSPRNG-backed, `getrandom`) generator `rubixdb-api`'s own snapshot
//! registry (`AppState::snapshots`) already uses for an identical
//! "server-issued opaque handle a client references in later requests"
//! shape (`PHASE_API_ARCHITECTURE.md` §2.1) — never a counter, a
//! transaction ID, or any other physical/incrementing identifier
//! (item 23's own explicit prohibition).

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use rubixdb::relational::Transaction;
use uuid::Uuid;

/// item 24: every bound reused from an already-existing, already-
/// documented limit where one exists (`rubixdb::relational::txn::
/// TxnLimits::max_concurrent_transactions` is the *global* process-wide
/// cap every session's own `Transaction` already counts against —
/// this module adds no second, disconnected global cap). The three
/// values below are this increment's own smallest-necessary addition,
/// for the resource shapes D27/`TxnLimits` does not itself bound
/// (per-principal fan-out, wall-clock session lifetime, idle time).
#[derive(Debug, Clone, Copy)]
pub struct SqlSessionLimits {
    /// Most sessions (= open transactions) one authenticated principal
    /// may hold at once — bounds fan-out from a single compromised or
    /// misbehaving credential well below the global `TxnLimits::max_
    /// concurrent_transactions` (1,000) ceiling.
    pub max_sessions_per_principal: usize,
    /// A session with no statement executed against it for longer than
    /// this is eligible for the reaper to roll back and remove.
    pub idle_timeout: Duration,
    /// A hard cap on total session age regardless of activity — an
    /// adversarial or buggy client that keeps a transaction "alive" with
    /// frequent no-op statements can still only pin a snapshot/write-set
    /// for this long (item 24/124: no abandoned transaction may pin a
    /// snapshot indefinitely).
    pub max_lifetime: Duration,
}

impl Default for SqlSessionLimits {
    fn default() -> Self {
        SqlSessionLimits {
            max_sessions_per_principal: 50,
            idle_timeout: Duration::from_secs(5 * 60),
            max_lifetime: Duration::from_secs(30 * 60),
        }
    }
}

/// One open server-side SQL transaction, bound to the principal that
/// issued its `BEGIN` — item 23/130: a session belongs to exactly one
/// principal for its entire lifetime; nothing here ever re-assigns
/// `principal` after creation, and every lookup (`SqlSessionRegistry::
/// take`) requires the caller's own authenticated principal to match it
/// exactly (item 74/26: a stolen/guessed session id from a *different*
/// authenticated principal must fail, not merely "an unauthenticated
/// caller must fail" — checked here, not left to chance).
pub struct SqlSession {
    pub principal: String,
    pub txn: Transaction,
    pub created_at: Instant,
    pub last_active: Instant,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SqlSessionMetricsSnapshot {
    pub active_sessions: u64,
}

pub struct SqlSessionRegistry {
    sessions: Mutex<HashMap<Uuid, SqlSession>>,
    limits: SqlSessionLimits,
}

/// Why a session lookup failed — deliberately collapsed to one opaque
/// API-facing error at the call site (item 74's own "wrong principal"
/// case and a genuinely-unknown/expired id must be indistinguishable to
/// the caller, the same existence-hiding discipline the SQL binder's
/// own `UnknownObject` already applies one layer down — never let a
/// session-lookup response reveal whether an id was ever valid for
/// *someone else*).
#[derive(Debug)]
pub enum SessionLookupError {
    NotFound,
}

impl SqlSessionRegistry {
    pub fn new(limits: SqlSessionLimits) -> Self {
        SqlSessionRegistry {
            sessions: Mutex::new(HashMap::new()),
            limits,
        }
    }

    /// item 24: rejected *before* the new session is inserted if the
    /// owning principal is already at its own per-principal cap —
    /// checked here, not left to the global `TxnLimits` cap alone (that
    /// cap protects the process; this one protects every other
    /// principal's fair share of it).
    pub fn create(&self, principal: &str, txn: Transaction) -> Result<Uuid, SqlSessionLimitError> {
        let mut sessions = self.sessions.lock().unwrap_or_else(|p| p.into_inner());
        let owned_by_principal = sessions
            .values()
            .filter(|s| s.principal == principal)
            .count();
        if owned_by_principal >= self.limits.max_sessions_per_principal {
            return Err(SqlSessionLimitError::TooManySessionsForPrincipal);
        }
        let id = Uuid::new_v4();
        let now = Instant::now();
        sessions.insert(
            id,
            SqlSession {
                principal: principal.to_string(),
                txn,
                created_at: now,
                last_active: now,
            },
        );
        Ok(id)
    }

    /// Removes and returns the session, iff `id` resolves to a live,
    /// non-expired session owned by `principal` — the one, single check
    /// every statement against an existing session goes through (item
    /// 26/74/130). The caller is responsible for putting the
    /// `Transaction` back (`put_back`) unless the statement was
    /// `COMMIT`/`ROLLBACK`, which consumes it.
    pub fn take(&self, id: Uuid, principal: &str) -> Result<Transaction, SessionLookupError> {
        let mut sessions = self.sessions.lock().unwrap_or_else(|p| p.into_inner());
        let now = Instant::now();
        let Some(session) = sessions.get(&id) else {
            return Err(SessionLookupError::NotFound);
        };
        // item 74: wrong principal and expired both resolve to the same
        // NotFound outcome the caller sees -- never distinguishable.
        let expired = now.duration_since(session.last_active) > self.limits.idle_timeout
            || now.duration_since(session.created_at) > self.limits.max_lifetime;
        if session.principal != principal || expired {
            if expired {
                // Still owned by the right principal, just timed out --
                // clean it up now rather than waiting for the reaper,
                // and roll back its transaction (never leave it
                // pinning a snapshot after this request observes it as
                // gone).
                if let Some(s) = sessions.remove(&id) {
                    let _ = s.txn.rollback();
                }
            }
            return Err(SessionLookupError::NotFound);
        }
        let session = sessions.remove(&id).expect("checked present above");
        Ok(session.txn)
    }

    /// Restores a still-active transaction after a statement that did
    /// not commit/rollback it, refreshing `last_active` (item 24's own
    /// idle-timeout clock).
    pub fn put_back(&self, id: Uuid, principal: String, txn: Transaction, created_at: Instant) {
        let mut sessions = self.sessions.lock().unwrap_or_else(|p| p.into_inner());
        sessions.insert(
            id,
            SqlSession {
                principal,
                txn,
                created_at,
                last_active: Instant::now(),
            },
        );
    }

    pub fn active_count(&self) -> usize {
        self.sessions
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .len()
    }

    pub fn metrics(&self) -> SqlSessionMetricsSnapshot {
        SqlSessionMetricsSnapshot {
            active_sessions: self.active_count() as u64,
        }
    }

    /// item 25/122/124: sweeps every session past its idle timeout or
    /// max lifetime, rolling each one back (releasing its snapshot/
    /// write-set) — called periodically by a background task
    /// (`spawn_reaper`) and also, defensively, inline by `take` for the
    /// one a request just happened to touch. Returns the number reaped
    /// (observability only).
    pub fn reap_expired(&self) -> usize {
        let mut sessions = self.sessions.lock().unwrap_or_else(|p| p.into_inner());
        let now = Instant::now();
        let expired_ids: Vec<Uuid> = sessions
            .iter()
            .filter(|(_, s)| {
                now.duration_since(s.last_active) > self.limits.idle_timeout
                    || now.duration_since(s.created_at) > self.limits.max_lifetime
            })
            .map(|(id, _)| *id)
            .collect();
        let count = expired_ids.len();
        for id in expired_ids {
            if let Some(s) = sessions.remove(&id) {
                let _ = s.txn.rollback();
            }
        }
        count
    }

    /// item 25: every open session rolled back — called once, at
    /// process shutdown, before the engine itself shuts down (never
    /// left for `Drop` to discover mid-shutdown, since `Drop` order
    /// across a `HashMap`'s values is unspecified and this needs to
    /// happen deterministically before the engine goes away).
    pub fn shutdown(&self) {
        let mut sessions = self.sessions.lock().unwrap_or_else(|p| p.into_inner());
        for (_, session) in sessions.drain() {
            let _ = session.txn.rollback();
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SqlSessionLimitError {
    TooManySessionsForPrincipal,
}

/// Spawns the periodic reaper task (item 25) — every `interval`, sweeps
/// expired sessions. Runs for the lifetime of the returned `JoinHandle`;
/// the caller aborts it at shutdown (before calling `SqlSessionRegistry::
/// shutdown`, so no reap-vs-shutdown race can double-rollback the same
/// session — `Transaction::rollback` itself is safe to call at most
/// once by construction, `self` consumption, so even a benign race would
/// only ever double-remove from the map, never double-rollback a live
/// handle).
pub fn spawn_reaper(
    registry: std::sync::Arc<SqlSessionRegistry>,
    interval: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        loop {
            ticker.tick().await;
            let reaped = registry.reap_expired();
            if reaped > 0 {
                tracing::info!(reaped, "sql session reaper: expired sessions rolled back");
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_txn(txm: &rubixdb::relational::TransactionManager) -> Transaction {
        txm.begin().unwrap()
    }

    fn test_txm() -> (
        rubixdb::relational::TransactionManager,
        std::sync::Arc<rubixdb::lsm::LsmEngine>,
        std::path::PathBuf,
    ) {
        use rubixdb::catalog::CatalogService;
        use rubixdb::relational::table_store::TableStore;
        use rubixdb::wal::{SyncMode, WalConfig};
        use std::sync::Arc;
        let dir = std::env::temp_dir().join(format!("rubixdb_sql_session_test_{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let engine = Arc::new(
            rubixdb::lsm::LsmEngine::open(
                &dir,
                WalConfig {
                    sync_mode: SyncMode::GroupCommit {
                        max_wait: Duration::from_millis(5),
                        max_batch_bytes: 256 * 1024,
                    },
                    ..WalConfig::default()
                },
                Default::default(),
                Default::default(),
            )
            .unwrap(),
        );
        let catalog = Arc::new(CatalogService::new(Arc::clone(&engine)));
        catalog.bootstrap().unwrap();
        let store = Arc::new(TableStore::new(Arc::clone(&engine), Arc::clone(&catalog)));
        let txm = rubixdb::relational::TransactionManager::new(Arc::clone(&engine), store);
        (txm, engine, dir)
    }

    #[test]
    fn create_take_round_trip_returns_the_same_transaction() {
        let (txm, engine, dir) = test_txm();
        let registry = SqlSessionRegistry::new(SqlSessionLimits::default());
        let txn = dummy_txn(&txm);
        let txn_id = txn.id();
        let session_id = registry.create("alice", txn).unwrap();
        assert_eq!(registry.active_count(), 1);
        let taken = registry.take(session_id, "alice").unwrap();
        assert_eq!(taken.id(), txn_id);
        assert_eq!(registry.active_count(), 0);
        taken.rollback().unwrap();
        engine.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn take_with_wrong_principal_fails_and_does_not_remove_the_session() {
        let (txm, engine, dir) = test_txm();
        let registry = SqlSessionRegistry::new(SqlSessionLimits::default());
        let session_id = registry.create("alice", dummy_txn(&txm)).unwrap();
        assert!(matches!(
            registry.take(session_id, "mallory"),
            Err(SessionLookupError::NotFound)
        ));
        // Still there for the real owner -- a wrong-principal attempt
        // must not have consumed/removed it.
        let taken = registry.take(session_id, "alice").unwrap();
        taken.rollback().unwrap();
        engine.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn take_with_unknown_id_fails() {
        let (_txm, engine, dir) = test_txm();
        let registry = SqlSessionRegistry::new(SqlSessionLimits::default());
        assert!(matches!(
            registry.take(Uuid::new_v4(), "alice"),
            Err(SessionLookupError::NotFound)
        ));
        engine.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn per_principal_session_limit_is_enforced() {
        let (txm, engine, dir) = test_txm();
        let registry = SqlSessionRegistry::new(SqlSessionLimits {
            max_sessions_per_principal: 2,
            ..SqlSessionLimits::default()
        });
        registry.create("alice", dummy_txn(&txm)).unwrap();
        registry.create("alice", dummy_txn(&txm)).unwrap();
        assert_eq!(
            registry.create("alice", dummy_txn(&txm)),
            Err(SqlSessionLimitError::TooManySessionsForPrincipal)
        );
        // A different principal is unaffected -- independent budgets.
        assert!(registry.create("bob", dummy_txn(&txm)).is_ok());
        engine.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn expired_session_is_reaped_and_rolled_back() {
        let (txm, engine, dir) = test_txm();
        let registry = SqlSessionRegistry::new(SqlSessionLimits {
            idle_timeout: Duration::from_millis(1),
            ..SqlSessionLimits::default()
        });
        registry.create("alice", dummy_txn(&txm)).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        let reaped = registry.reap_expired();
        assert_eq!(reaped, 1);
        assert_eq!(registry.active_count(), 0);
        engine.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn take_of_an_idle_expired_session_also_reaps_it() {
        let (txm, engine, dir) = test_txm();
        let registry = SqlSessionRegistry::new(SqlSessionLimits {
            idle_timeout: Duration::from_millis(1),
            ..SqlSessionLimits::default()
        });
        let session_id = registry.create("alice", dummy_txn(&txm)).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        assert!(matches!(
            registry.take(session_id, "alice"),
            Err(SessionLookupError::NotFound)
        ));
        assert_eq!(
            registry.active_count(),
            0,
            "expired session must be cleaned up, not merely rejected"
        );
        engine.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn shutdown_rolls_back_every_open_session() {
        let (txm, engine, dir) = test_txm();
        let registry = SqlSessionRegistry::new(SqlSessionLimits::default());
        registry.create("alice", dummy_txn(&txm)).unwrap();
        registry.create("bob", dummy_txn(&txm)).unwrap();
        assert_eq!(registry.active_count(), 2);
        registry.shutdown();
        assert_eq!(registry.active_count(), 0);
        engine.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
