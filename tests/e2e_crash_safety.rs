//! Crash-safety: sealing is idempotent (the COR-1 property). Re-sealing an already-sealed range —
//! what a `kill -9` between "segment written" and "watermark advanced" makes the next run do — must
//! not add a duplicate segment or double-count rows. The pipeline's `maybe_seal` is private, so this
//! targets its content-addressed core, `seal::seal_range`, exercised twice against the SAME dir (the
//! inline seal tests only ever seal into two DIFFERENT dirs, so this same-dir re-seal is new coverage).

use nuthatch::seal;

/// One `usdc__transfer` row as stored JSON (mirrors the shape `DecodedRow::to_json` seals).
fn transfer_json(block: u64, log_index: u64, value: &str) -> String {
    format!(
        r#"{{"table":"usdc__transfer","from":"0xaaaa","to":"0xbbbb","value":"{value}","block_number":{block},"tx_hash":"0xcc","log_index":{log_index}}}"#
    )
}

#[test]
fn seal_is_idempotent_over_the_same_range() {
    let dir = tempfile::tempdir().unwrap();
    let rows = vec![
        transfer_json(1, 0, "5"),
        transfer_json(1, 1, "7"),
        transfer_json(2, 0, "9"),
    ];

    // First seal: one segment, three rows.
    let first = seal::seal_range(dir.path(), &rows, 1, 2)
        .unwrap()
        .expect("range holds rows");
    assert_eq!(first.tables, 1);
    assert_eq!(first.rows, 3);

    let manifest_1 = seal::load_manifest(dir.path()).unwrap();
    let segs_1 = manifest_1.tables["usdc__transfer"].clone();
    assert_eq!(segs_1.len(), 1, "exactly one segment after the first seal");

    // Re-seal the identical range into the SAME dir (the crash-replay path). The segment is content-
    // addressed, so it is recognised as already catalogued: nothing new is sealed.
    let second = seal::seal_range(dir.path(), &rows, 1, 2).unwrap();
    if let Some(summary) = second {
        assert_eq!(summary.tables, 0, "re-seal must catalogue no new segment");
        assert_eq!(summary.rows, 0, "re-seal must count no new rows");
    }

    let manifest_2 = seal::load_manifest(dir.path()).unwrap();
    let segs_2 = &manifest_2.tables["usdc__transfer"];
    assert_eq!(
        segs_2.len(),
        1,
        "no duplicate segment after re-seal (COR-1)"
    );
    assert_eq!(segs_1[0].hash, segs_2[0].hash, "same content address");
    assert_eq!(segs_2[0].rows, 3, "no double-count");

    // Exactly one Parquet file physically exists for the table.
    let parquet_files = std::fs::read_dir(dir.path().join(seal::SEGMENTS_DIR))
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().ends_with(".parquet"))
        .count();
    assert_eq!(parquet_files, 1, "no orphaned duplicate parquet file");
}
