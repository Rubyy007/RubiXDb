p = r'E:\RubiXDb\cli\tests\index_backfill_crash_integration.rs'
s = open(p, encoding='utf-8', newline='').read()
nl = '\r\n' if '\r\n' in s else '\n'
old_start = s.index("    // By the time /healthz answers, `recover_incomplete_index_operations`")
old_end = s.index("    // Query correctness: the recovered index must actually be")
new = '''    // CONTRACT CHANGE (production-operations phase): recovery of an interrupted
    // CREATE INDEX re-runs the whole backfill, which measured 21-35 s on a
    // 600,000-row table. It used to run synchronously BEFORE readiness, so
    // after such a kill `rubixdb gui` could exceed its 30 s readiness bound and
    // refuse to start. It now runs on its own thread once the server is
    // serving (graceful shutdown joins it; a kill retries at the next start).
    // The properties this test protects are unchanged and still asserted:
    //   (1) a recovered index must END `ready` -- never stuck `building`
    //       (bounded wait below; a hang or `failed` fails the test),
    //   (2) it must never be exposed while partial: reads are correct at every
    //       moment, including while it is still `building` (the planner must
    //       not use it),
    //   (3) once `ready` it must be complete (probes + counts below).
    let recovery_deadline = Instant::now() + Duration::from_secs(180);
    let mut served_while_building = 0u32;
    let idx_val = loop {
        let indexes = list_indexes(&verify_client, &restarted.base_url, &restarted.admin_key);
        let idx_val = indexes
            .iter()
            .find(|i| i["name"] == "idx_val")
            .expect("idx_val must still exist in the catalog after restart")
            .clone();
        if idx_val["state"] == "ready" {
            break idx_val;
        }
        assert_eq!(
            idx_val["state"], "building",
            "a recovering index may only be `building` or `ready`, never `failed`/absent: {idx_val}"
        );
        // Correct answer while the index is still being rebuilt.
        let probe = ROW_COUNT / 3;
        let result = exec_sql(
            &verify_client,
            &restarted.base_url,
            &restarted.admin_key,
            &format!("SELECT id FROM bigidx WHERE val = {probe}"),
        );
        assert_eq!(
            result["result"]["row_count"], 1,
            "a partial `building` index must never change query results: {result}"
        );
        served_while_building += 1;
        assert!(
            Instant::now() < recovery_deadline,
            "a recovered index must reach Ready, never stay stuck Building: {idx_val}"
        );
        std::thread::sleep(Duration::from_millis(250));
    };
    eprintln!("queries answered correctly while the index was still building: {served_while_building}");
    assert_eq!(
        idx_val["state"], "ready",
        "a recovered index must reach Ready, never stay stuck Building or be exposed while partial: {idx_val}"
    );

'''.replace('\n', nl)
s = s[:old_start] + new + s[old_end:]
open(p, 'w', encoding='utf-8', newline='').write(s)
print("ok")
