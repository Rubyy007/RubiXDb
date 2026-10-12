//! Statement-wide binding environment (SQL-mission-1).
//!
//! A subquery is bound by the same binder as the statement that contains
//! it, against the same catalog, authorization context and metrics. This
//! struct carries those handles, plus the two pieces of statement-wide state
//! a nested query needs and a single `SELECT` never did:
//!
//! * the **table-reference counter** -- every `TableRefId` in one statement
//!   is allocated from one counter, so an inner query's rows can be merged
//!   with an outer row context at execution time without two different
//!   tables ever sharing an id (`exec::RowContext` looks a row up by id);
//! * the **subquery counter and nesting depth** -- bounded by
//!   `SqlLimits::{max_subqueries_per_statement, max_subquery_depth}`.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use rubixdb::catalog::CatalogService;

use crate::auth::AuthContext;
use crate::bind::scope::{BindContext, Scope};
use crate::bound::ColumnRef;
use crate::error::{Result, SqlError};
use crate::limits::SqlLimits;
use crate::metrics::SqlMetrics;

/// First id handed out by `BindEnv::alloc_relation_id`.
pub const RELATION_ID_BASE: u32 = 0x8000_0000;

/// One outer-query column a nested query depends on (SQL-mission-1
/// correlation), recorded while the nested query is bound.
#[derive(Debug, Clone)]
pub struct Capture {
    pub column: ColumnRef,
    /// `alias.column`, for error messages that name the outer row.
    pub label: String,
    /// `true` when the nested query actually reads this column (it becomes
    /// one of the subquery's `outer_refs`); `false` when it is recorded only
    /// so an error can identify the outer row (a primary-key column of a
    /// table the nested query is correlated to).
    pub correlating: bool,
}

pub struct BindEnv<'a> {
    pub catalog: &'a CatalogService,
    pub ctx: &'a BindContext,
    pub auth: &'a AuthContext,
    pub metrics: &'a SqlMetrics,
    ref_counter: Rc<Cell<u32>>,
    next_subquery_id: Cell<u32>,
    next_relation_id: Cell<u32>,
    depth: Cell<usize>,
    /// One frame per nested query currently being bound (innermost last);
    /// a column resolved `n` levels out is recorded in the innermost `n`
    /// frames -- the query that wrote it and every query between it and the
    /// level that owns the column -- so a subquery that merely *contains* a
    /// correlated subquery is itself known to be correlated.
    captures: RefCell<Vec<Vec<Capture>>>,
}

impl<'a> BindEnv<'a> {
    pub fn new(
        catalog: &'a CatalogService,
        ctx: &'a BindContext,
        auth: &'a AuthContext,
        metrics: &'a SqlMetrics,
    ) -> Self {
        BindEnv {
            catalog,
            ctx,
            auth,
            metrics,
            ref_counter: Rc::new(Cell::new(0)),
            next_subquery_id: Cell::new(0),
            next_relation_id: Cell::new(0),
            depth: Cell::new(0),
            captures: RefCell::new(Vec::new()),
        }
    }

    /// A fresh, empty scope whose table-reference ids continue this
    /// statement's single numbering.
    pub fn new_scope(&self) -> Scope {
        Scope::with_counter(Rc::clone(&self.ref_counter))
    }

    /// Allocates the next statement-unique subquery id, failing closed past
    /// `max_subqueries_per_statement`.
    pub fn alloc_subquery_id(&self, limits: &SqlLimits) -> Result<u32> {
        let id = self.next_subquery_id.get();
        if id as usize >= limits.max_subqueries_per_statement {
            return Err(SqlError::ResourceLimit {
                detail: format!(
                    "statement contains more than max_subqueries_per_statement ({}) subqueries",
                    limits.max_subqueries_per_statement
                ),
            });
        }
        self.next_subquery_id.set(id + 1);
        Ok(id)
    }

    /// Allocates the next statement-unique id of a derived table or a CTE.
    /// Ids start at `RELATION_ID_BASE`, so they never collide with a
    /// subquery expression's id in the executor's per-id counters.
    pub fn alloc_relation_id(&self) -> u32 {
        let id = self.next_relation_id.get();
        self.next_relation_id.set(id + 1);
        RELATION_ID_BASE + id
    }

    /// Enters one level of subquery nesting, failing closed past
    /// `max_subquery_depth`. Every successful call must be paired with
    /// `leave`.
    pub fn enter(&self, limits: &SqlLimits) -> Result<()> {
        let depth = self.depth.get() + 1;
        if depth > limits.max_subquery_depth {
            return Err(SqlError::ResourceLimit {
                detail: format!(
                    "subquery nesting exceeds max_subquery_depth ({})",
                    limits.max_subquery_depth
                ),
            });
        }
        self.depth.set(depth);
        Ok(())
    }

    pub fn leave(&self) {
        self.depth.set(self.depth.get().saturating_sub(1));
    }

    /// Opens the capture frame of a nested query about to be bound.
    pub fn push_frame(&self) {
        self.captures.borrow_mut().push(Vec::new());
    }

    /// Closes the innermost capture frame, returning what the nested query
    /// (and anything nested inside it) read from enclosing queries.
    pub fn pop_frame(&self) -> Vec<Capture> {
        self.captures.borrow_mut().pop().unwrap_or_default()
    }

    /// Records a capture in the innermost `levels` frames (see `captures`).
    pub fn record_capture(&self, levels: usize, capture: &Capture) {
        let mut frames = self.captures.borrow_mut();
        debug_assert!(levels <= frames.len(), "scope chain deeper than capture frames");
        let n = frames.len();
        for frame in frames.iter_mut().skip(n.saturating_sub(levels)) {
            match frame.iter_mut().find(|c| c.column == capture.column) {
                Some(existing) => existing.correlating |= capture.correlating,
                None => frame.push(capture.clone()),
            }
        }
    }
}
