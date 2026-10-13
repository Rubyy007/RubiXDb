//! Relational workload shared by `examples/fault_child.rs` (the process that gets killed) and the parent that judges it:
//! a table `t(id INTEGER PRIMARY KEY, name TEXT, grp INTEGER)` with a secondary index `t_name_idx` on `name`, written through
//! the real relational path (`TableStore::put_row` / `delete_row`: one `write_batch` per commit unit carrying the row and its
//! index entries). Operations are a pure function of `(seed, i)`. No oracle or rig dependency here.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::workload::{splitmix64, Params};
use rubixdb::catalog::schema::IndexKind;
use rubixdb::catalog::service::ColumnDef;
use rubixdb::catalog::CatalogService;
use rubixdb::lsm::LsmEngine;
use rubixdb::relational::index::IndexBuilder;
use rubixdb::relational::table_store::TableStore;
use rubixdb::relational::value::{TYPE_TAG_INTEGER, TYPE_TAG_TEXT};
use rubixdb::relational::RelationalValue;

pub type RowModel = BTreeMap<i32, (Option<String>, Option<i32>)>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RelOp {
    Upsert {
        id: i32,
        name: Option<String>,
        grp: Option<i32>,
    },
    Delete {
        id: i32,
    },
}

/// Operation `i`: a pure function of `(seed, i)`.
pub fn gen_rel_op(p: &Params, i: usize) -> RelOp {
    let h = splitmix64(p.seed ^ 0x5EED_0000 ^ (i as u64).wrapping_mul(0x1_0000_0001));
    let id = ((h >> 8) % p.keys as u64) as i32;
    if h % 100 < 78 {
        RelOp::Upsert {
            id,
            name: if (h >> 40) % 100 < 85 {
                Some(format!("n{}", (h >> 20) % 6))
            } else {
                None
            },
            grp: if (h >> 50) % 100 < 90 {
                Some(((h >> 30) % 50) as i32)
            } else {
                None
            },
        }
    } else {
        RelOp::Delete { id }
    }
}

pub fn rel_model(p: &Params, k: usize) -> RowModel {
    let mut m = RowModel::new();
    for i in 0..k {
        match gen_rel_op(p, i) {
            RelOp::Upsert { id, name, grp } => {
                m.insert(id, (name, grp));
            }
            RelOp::Delete { id } => {
                m.remove(&id);
            }
        }
    }
    m
}

/// Everything the workload needs from an open database.
pub struct RelStack {
    pub engine: Arc<LsmEngine>,
    pub catalog: Arc<CatalogService>,
    pub store: Arc<TableStore>,
    pub builder: Arc<IndexBuilder>,
    pub table_id: u32,
    pub index_id: u32,
}

pub fn attach(engine: Arc<LsmEngine>) -> Result<RelStack, String> {
    let catalog = Arc::new(CatalogService::new(Arc::clone(&engine)));
    let db = catalog.list_databases().map_err(|e| e.to_string())?;
    let db = db.first().ok_or("no database in the catalog")?;
    let schema = catalog
        .list_schemas(db.database_id)
        .map_err(|e| e.to_string())?;
    let schema = schema.first().ok_or("no schema in the catalog")?;
    let table = catalog
        .get_table_by_name(schema.schema_id, "t")
        .map_err(|e| e.to_string())?
        .ok_or("table t is missing")?;
    let index = catalog
        .list_indexes(table.table_id)
        .map_err(|e| e.to_string())?
        .into_iter()
        .find(|i| i.name == "t_name_idx")
        .ok_or("index t_name_idx is missing")?;
    let store = Arc::new(TableStore::new(Arc::clone(&engine), Arc::clone(&catalog)));
    let builder = Arc::new(IndexBuilder::new(
        Arc::clone(&engine),
        Arc::clone(&catalog),
        Arc::clone(&store),
    ));
    Ok(RelStack {
        engine,
        catalog,
        store,
        builder,
        table_id: table.table_id,
        index_id: index.index_id,
    })
}

/// Creates `t(id INTEGER PRIMARY KEY, name TEXT, grp INTEGER)` with a secondary index `t_name_idx` on `name`.
pub fn setup(engine: Arc<LsmEngine>) -> Result<(), String> {
    let catalog = Arc::new(CatalogService::new(Arc::clone(&engine)));
    catalog.bootstrap().map_err(|e| e.to_string())?;
    let db = catalog.list_databases().map_err(|e| e.to_string())?[0].database_id;
    let schema = catalog.list_schemas(db).map_err(|e| e.to_string())?[0].schema_id;
    let col = |n: &str, t: u8, nullable: bool| ColumnDef {
        name: n.to_string(),
        data_type: t,
        nullable,
        default_value: None,
        type_params: None,
    };
    catalog
        .create_table(
            schema,
            "t",
            &[
                col("id", TYPE_TAG_INTEGER, false),
                col("name", TYPE_TAG_TEXT, true),
                col("grp", TYPE_TAG_INTEGER, true),
            ],
            &[0],
        )
        .map_err(|e| e.to_string())?;
    let store = Arc::new(TableStore::new(Arc::clone(&engine), Arc::clone(&catalog)));
    let builder = IndexBuilder::new(Arc::clone(&engine), Arc::clone(&catalog), store);
    let table = catalog
        .get_table_by_name(schema, "t")
        .map_err(|e| e.to_string())?
        .ok_or("table t vanished")?;
    builder
        .create_index_online(table.table_id, "t_name_idx", IndexKind::NonUnique, &[1])
        .map_err(|e| e.to_string())?;
    Ok(())
}

pub fn apply_op(s: &RelStack, op: &RelOp) -> Result<u64, String> {
    match op {
        RelOp::Upsert { id, name, grp } => s
            .store
            .put_row(
                s.table_id,
                &[
                    Some(RelationalValue::Integer(*id)),
                    name.clone().map(RelationalValue::Text),
                    grp.map(RelationalValue::Integer),
                ],
            )
            .map_err(|e| e.to_string()),
        RelOp::Delete { id } => s
            .store
            .delete_row(s.table_id, &[RelationalValue::Integer(*id)])
            .map_err(|e| e.to_string()),
    }
}

/// Decoded rows of the table, as the model type.
pub fn decoded_rows(s: &RelStack) -> Result<RowModel, String> {
    let mut m = RowModel::new();
    for (pk, row) in s.store.scan_table(s.table_id).map_err(|e| e.to_string())? {
        let id = match pk.as_slice() {
            [RelationalValue::Integer(i)] => *i,
            other => return Err(format!("unexpected primary key shape {other:?}")),
        };
        let name = match row.get(1) {
            Some(Some(RelationalValue::Text(t))) => Some(t.clone()),
            Some(None) => None,
            other => return Err(format!("unexpected name column {other:?}")),
        };
        let grp = match row.get(2) {
            Some(Some(RelationalValue::Integer(g))) => Some(*g),
            Some(None) => None,
            other => return Err(format!("unexpected grp column {other:?}")),
        };
        if m.insert(id, (name, grp)).is_some() {
            return Err(format!("primary key {id} returned twice by the table scan"));
        }
    }
    Ok(m)
}
