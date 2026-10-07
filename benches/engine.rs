//! Engine benches (D-11): page-size insert/scan throughput + group-commit latency.
//! Run: `cargo bench --bench engine`. No criterion dep — std timing only.
//!
//! Page-size model: measures memmove-style record packing throughput for
//! 8 KiB / 16 KiB / 64 KiB page buffers (insert = serialize N rows into
//! page-sized buffers; scan = linear scan over buffers). Group-commit
//! model: N concurrent `Wal::commit` threads sharing one WAL — leader
//! batching should collapse N fsyncs toward ~1 per 10 ms window.

use std::time::Instant;

use titan_db::storage::wal::{LogRecord, Wal};

fn bench_page_size(page_size: usize, rows: usize) -> (f64, f64) {
    // Insert: pack `rows` 256-byte records into page buffers.
    let rec = vec![7u8; 256];
    let t0 = Instant::now();
    let mut pages: Vec<Vec<u8>> = Vec::new();
    let mut cur = Vec::with_capacity(page_size);
    let mut packed = 0;
    for _ in 0..rows {
        if cur.len() + rec.len() > page_size {
            pages.push(std::mem::replace(&mut cur, Vec::with_capacity(page_size)));
        }
        cur.extend_from_slice(&rec);
        packed += 1;
    }
    if !cur.is_empty() {
        pages.push(cur);
    }
    let insert_s = t0.elapsed().as_secs_f64();
    // Scan: linear scan over all packed bytes.
    let t1 = Instant::now();
    let mut sum: u64 = 0;
    for p in &pages {
        for chunk in p.chunks(256) {
            sum += chunk[0] as u64;
        }
    }
    std::hint::black_box(sum);
    let scan_s = t1.elapsed().as_secs_f64();
    let mb = (packed * 256) as f64 / 1e6;
    (mb / insert_s, mb / scan_s)
}

fn bench_group_commit(threads: usize, commits_each: usize) -> f64 {
    let dir = std::env::temp_dir().join(format!("titan_bench_gc_{}.wal", std::process::id()));
    let _ = std::fs::remove_file(&dir);
    let wal = std::sync::Arc::new(Wal::open(&dir).expect("wal open"));
    // Seed one data record per tx so commits have something to ack.
    let t0 = Instant::now();
    let mut handles = Vec::new();
    for t in 0..threads {
        let wal = wal.clone();
        handles.push(std::thread::spawn(move || {
            for i in 0..commits_each {
                let tx = (t * commits_each + i) as u64 + 1;
                wal.append(&LogRecord::Put {
                    table: "b".into(),
                    key: vec![1],
                    after: vec![2],
                    tx,
                })
                .expect("append");
                wal.commit(tx).expect("commit");
            }
        }));
    }
    for h in handles {
        h.join().expect("thread");
    }
    let s = t0.elapsed().as_secs_f64();
    let _ = std::fs::remove_file(&dir);
    s
}

fn main() {
    let rows = 20_000;
    println!("=== page-size throughput ({} x 256B rows) ===", rows);
    let mut best = (0usize, 0.0);
    for &ps in &[8192usize, 16384, 65536] {
        let (ins, scan) = bench_page_size(ps, rows);
        println!(
            "page {:>6}B: insert {:>8.1} MB/s  scan {:>8.1} MB/s",
            ps, ins, scan
        );
        let score = ins + scan;
        if score > best.1 {
            best = (ps, score);
        }
    }
    println!("WINNER: {}B page", best.0);

    println!("=== group-commit latency (8 threads x 25 commits) ===");
    let s = bench_group_commit(8, 25);
    println!("200 commits in {:.3}s ({:.1} commits/s)", s, 200.0 / s);
    println!("(group commit: 10 ms window / 32-batch; followers share leader fsync)");
}
