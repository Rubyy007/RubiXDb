//! Authorization resolution — D25, bound into the same pass as
//! identifier resolution (D15, item 15: "identifier resolution +
//! authorization must occur in the same binder pass"). This module is
//! the check itself; `crate::bind`'s table/index-resolution helpers are
//! the single call site that collapses "does not exist" and "exists but
//! forbidden" into the one `SqlError::UnknownObject` outcome (item 15/
//! 32) — this module never returns that distinction either, only a
//! plain `bool`.

use rubixdb::catalog::schema::{ObjectKind, Privilege};
use rubixdb::catalog::CatalogService;

use crate::error::Result;

/// D25's v1 default-privilege mapping, stated explicitly (not silently
/// assumed): "existing `reader` keys receive `SELECT` on every object;
/// existing `admin` keys receive every privilege on every object." This
/// crate does not depend on `rubixdb-api`'s own `Role` type (the reverse
/// dependency direction would be architecturally wrong — `rubixdb-api`
/// is a *consumer* of a future SQL execution layer, not the other way
/// around) — whatever assembles a session for a principal is responsible
/// for mapping its own role concept onto one of these three, once that
/// wiring exists (explicitly out of this increment's scope, item 48).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DefaultAccess {
    /// No privilege beyond whatever `system.grants` rows exist for this
    /// principal explicitly.
    None,
    /// `SELECT` on every object, plus explicit grants.
    Reader,
    /// Every privilege on every object.
    Admin,
}

#[derive(Debug, Clone)]
pub struct AuthContext {
    pub principal: String,
    pub default_access: DefaultAccess,
}

impl AuthContext {
    pub fn admin(principal: impl Into<String>) -> Self {
        AuthContext {
            principal: principal.into(),
            default_access: DefaultAccess::Admin,
        }
    }

    pub fn reader(principal: impl Into<String>) -> Self {
        AuthContext {
            principal: principal.into(),
            default_access: DefaultAccess::Reader,
        }
    }

    pub fn none(principal: impl Into<String>) -> Self {
        AuthContext {
            principal: principal.into(),
            default_access: DefaultAccess::None,
        }
    }
}

/// Whether `ctx` holds `privilege` on the object hierarchy in
/// `ancestor_objects` — table-level grants and their schema/database
/// ancestors, in any order (D25: grants at database/schema/table level
/// each imply access to everything beneath them; a grant at *any* level
/// in the chain is sufficient). The caller resolves the real ancestor
/// chain from the catalog (`crate::bind`'s table-resolution helper) —
/// this function never independently re-derives it, per item 16's "do
/// not maintain another authoritative metadata map."
///
/// Column-level authorization (item 18) is **not** a separate check:
/// D25 v1 scope is table-level granularity only ("finer-grained security
/// is real future work"), so every column of an authorized table is
/// authorized by construction — `SELECT *` wildcard expansion never
/// needs, and never performs, a second per-column grant lookup.
pub fn is_authorized(
    catalog: &CatalogService,
    ctx: &AuthContext,
    privilege: Privilege,
    ancestor_objects: &[(ObjectKind, u32)],
) -> Result<bool> {
    match ctx.default_access {
        DefaultAccess::Admin => return Ok(true),
        DefaultAccess::Reader if privilege == Privilege::Select => return Ok(true),
        DefaultAccess::Reader | DefaultAccess::None => {}
    }
    let grants = catalog.list_grants_for_principal(&ctx.principal)?;
    Ok(ancestor_objects.iter().any(|&(kind, id)| {
        grants
            .iter()
            .any(|g| g.object_kind == kind && g.object_id == id && g.privilege == privilege)
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rubixdb::catalog::CatalogService;
    use rubixdb::lsm::{LsmConfig, LsmEngine};
    use rubixdb::wal::{SyncMode, WalConfig};
    use std::sync::Arc;
    use std::time::Duration;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("rubixdb_sql_auth_test_{tag}_{nanos}"));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn open(dir: &std::path::Path) -> Arc<LsmEngine> {
        Arc::new(
            LsmEngine::open(
                dir,
                WalConfig {
                    sync_mode: SyncMode::GroupCommit {
                        max_wait: Duration::from_millis(5),
                        max_batch_bytes: 256 * 1024,
                    },
                    ..WalConfig::default()
                },
                Default::default(),
                LsmConfig::default(),
            )
            .unwrap(),
        )
    }

    #[test]
    fn admin_default_access_bypasses_explicit_grants() {
        let dir = temp_dir("admin_bypass");
        let engine = open(&dir);
        let catalog = CatalogService::new(Arc::clone(&engine));
        catalog.bootstrap().unwrap();
        let ctx = AuthContext::admin("svc-a");
        assert!(is_authorized(&catalog, &ctx, Privilege::Ddl, &[(ObjectKind::Table, 999)]).unwrap());
        engine.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reader_default_access_only_covers_select() {
        let dir = temp_dir("reader_select_only");
        let engine = open(&dir);
        let catalog = CatalogService::new(Arc::clone(&engine));
        catalog.bootstrap().unwrap();
        let ctx = AuthContext::reader("svc-b");
        assert!(is_authorized(&catalog, &ctx, Privilege::Select, &[(ObjectKind::Table, 1)]).unwrap());
        assert!(!is_authorized(&catalog, &ctx, Privilege::Insert, &[(ObjectKind::Table, 1)]).unwrap());
        engine.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn explicit_grant_at_any_ancestor_level_is_sufficient() {
        let dir = temp_dir("ancestor_grant");
        let engine = open(&dir);
        let catalog = CatalogService::new(Arc::clone(&engine));
        catalog.bootstrap().unwrap();
        catalog
            .grant("svc-c", ObjectKind::Schema, 1, Privilege::Select)
            .unwrap();
        let ctx = AuthContext::none("svc-c");
        // No grant directly on table 42, but its schema (1) has one.
        assert!(is_authorized(
            &catalog,
            &ctx,
            Privilege::Select,
            &[(ObjectKind::Table, 42), (ObjectKind::Schema, 1), (ObjectKind::Database, 1)]
        )
        .unwrap());
        assert!(!is_authorized(
            &catalog,
            &ctx,
            Privilege::Insert,
            &[(ObjectKind::Table, 42), (ObjectKind::Schema, 1), (ObjectKind::Database, 1)]
        )
        .unwrap());
        engine.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
