//! Import benchmarks.
//!
//! Run with: PHOTON_BENCH_DIR=/path/to/photos cargo bench -p photon-import
//!
//! Point `PHOTON_BENCH_DIR` at a folder of real photos (ideally RAW + JPEG).
//! Without it the benchmarks are skipped.

use criterion::{criterion_group, criterion_main, Criterion};
use photon_core::db::Database;
use photon_import::dedup::blake3_hash_file;
use photon_import::engine::{ImportConfig, ImportEngine};
use photon_import::metadata;
use photon_import::sources::{DiskSource, ImportSource};
use std::path::PathBuf;

fn bench_dir() -> Option<PathBuf> {
    std::env::var_os("PHOTON_BENCH_DIR").map(PathBuf::from)
}

fn bench_import(c: &mut Criterion) {
    let Some(dir) = bench_dir() else {
        eprintln!("PHOTON_BENCH_DIR not set; skipping import benchmarks");
        return;
    };
    let source = DiskSource::new(dir, true);
    let files = source.scan().expect("scan bench dir");
    eprintln!("benchmarking with {} files", files.len());

    let mut group = c.benchmark_group("import");
    group.sample_size(10);

    group.bench_function("blake3_all_files", |b| {
        b.iter(|| files.iter().for_each(|f| drop(blake3_hash_file(f))))
    });
    group.bench_function("exif_all_files", |b| {
        b.iter(|| files.iter().for_each(|f| drop(metadata::extract(f))))
    });
    group.bench_function("full_pipeline_no_thumbnails", |b| {
        b.iter(|| {
            // Fresh DB per run so nothing is skipped as already known.
            let engine = ImportEngine::new(Database::open_in_memory().unwrap(), None);
            let config = ImportConfig {
                generate_thumbnails: false,
                ..Default::default()
            };
            engine.import(&source, &config, None).unwrap()
        })
    });
    group.finish();
}

criterion_group!(benches, bench_import);
criterion_main!(benches);
