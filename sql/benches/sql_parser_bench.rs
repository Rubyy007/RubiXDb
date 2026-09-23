//! Parser performance — item 35. Representative SQL sizes (tiny/small/
//! medium/large) plus malformed large SQL (a hostile-input latency
//! check, not just a correctness one). Numbers belong in `PHASE_
//! RELATIONAL_SQL_INCREMENT6_RESULTS.md` once actually run, never
//! claimed without measurement.

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use rubixdb_sql::limits::SqlLimits;
use rubixdb_sql::parse::parse_statement;

fn tiny() -> String {
    "SELECT id FROM t".to_string()
}

fn small() -> String {
    "SELECT id, name FROM t WHERE id = $1 AND name = 'abc' ORDER BY id LIMIT 10".to_string()
}

fn medium() -> String {
    let mut sql = "SELECT t.id, t.name, o.customer, o.amount FROM t INNER JOIN orders o ON t.id = o.t_id WHERE ".to_string();
    for i in 0..20 {
        if i > 0 {
            sql.push_str(" AND ");
        }
        sql.push_str(&format!("t.id <> {i}"));
    }
    sql.push_str(" ORDER BY t.id DESC LIMIT 50 OFFSET 10");
    sql
}

fn large() -> String {
    let mut sql = "INSERT INTO t (id, name, active) VALUES ".to_string();
    let rows: Vec<String> = (0..500)
        .map(|i| format!("({i}, 'name-{i}', TRUE)"))
        .collect();
    sql.push_str(&rows.join(", "));
    sql
}

fn malformed_large() -> String {
    // Same scale as `large`, but syntactically broken partway through --
    // exercises the tokenizer/parser's error path at realistic size, not
    // just its happy path.
    let mut sql = large();
    sql.push_str(" GARBAGE TOKENS HERE ((( not valid");
    sql
}

fn bench_parser_sizes(c: &mut Criterion) {
    let limits = SqlLimits {
        max_statement_bytes: 4 * 1024 * 1024,
        max_values_rows: 10_000,
        ..SqlLimits::default()
    };
    let mut group = c.benchmark_group("sql_parser");
    for (name, sql) in [
        ("tiny", tiny()),
        ("small", small()),
        ("medium", medium()),
        ("large_500row_insert", large()),
        ("malformed_large", malformed_large()),
    ] {
        group.throughput(Throughput::Bytes(sql.len() as u64));
        group.bench_with_input(BenchmarkId::from_parameter(name), &sql, |b, sql| {
            b.iter(|| {
                let _ = std::hint::black_box(parse_statement(sql, &limits));
            });
        });
    }
    group.finish();
}

criterion_group!(benches, bench_parser_sizes);
criterion_main!(benches);
