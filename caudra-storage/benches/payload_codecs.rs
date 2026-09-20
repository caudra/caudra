//! Codec gate for compressing the session payload columns.
//!
//! Rows are compressed one at a time, the way the storage layer will, because a
//! single concatenated stream lets rows share a dictionary and overstates the
//! ratio by roughly 2.5x. Point it at a real session database: synthetic JSON
//! does not compress like the real corpus, which is the whole question.

use std::env;
use std::path::PathBuf;

use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use rusqlite::{Connection, OpenFlags};
use std::io::{Read, Write};

const DATABASE_ENV: &str = "CAUDRA_BENCH_DB";
const DEFAULT_DATABASE: &str = ".local/state/caudra/caudra.sqlite";
const TABLES: [&str; 3] = [
    "main_history_items",
    "tool_outputs",
    "subagent_history_items",
];
const SMALL_LIMIT: i64 = 4 * 1024;
const MEDIUM_LIMIT: i64 = 32 * 1024;
const ROWS_PER_BAND: usize = 300;
const DICTIONARY_BYTES: usize = 16 * 1024;
const ZSTD_FAST: i32 = 3;
const ZSTD_DENSE: i32 = 9;
const GZIP_LEVEL: u32 = 6;
const MISSING_CORPUS: &str =
    "no session database found; set CAUDRA_BENCH_DB to one to run the codec gate";

struct Band {
    name: &'static str,
    floor: i64,
    ceiling: Option<i64>,
}

impl Band {
    fn predicate(&self) -> String {
        match self.ceiling {
            Some(ceiling) => {
                format!("byte_count >= {} AND byte_count < {ceiling}", self.floor)
            }
            None => format!("byte_count >= {}", self.floor),
        }
    }
}

const BANDS: [Band; 3] = [
    Band {
        name: "small",
        floor: 0,
        ceiling: Some(SMALL_LIMIT),
    },
    Band {
        name: "medium",
        floor: SMALL_LIMIT,
        ceiling: Some(MEDIUM_LIMIT),
    },
    Band {
        name: "large",
        floor: MEDIUM_LIMIT,
        ceiling: None,
    },
];

fn database_path() -> Option<PathBuf> {
    if let Some(path) = env::var_os(DATABASE_ENV) {
        let path = PathBuf::from(path);
        return path.exists().then_some(path);
    }
    let home = env::var_os("HOME")?;
    let path = PathBuf::from(home).join(DEFAULT_DATABASE);
    path.exists().then_some(path)
}

fn sample_band(connection: &Connection, band: &Band) -> Vec<Vec<u8>> {
    let predicate = band.predicate();
    let mut rows = Vec::new();
    for table in TABLES {
        let sql = format!("SELECT payload FROM {table} WHERE {predicate} LIMIT {ROWS_PER_BAND}");
        let Ok(mut statement) = connection.prepare(&sql) else {
            continue;
        };
        let Ok(mapped) = statement.query_map([], |row| row.get::<_, String>(0)) else {
            continue;
        };
        rows.extend(mapped.flatten().map(String::into_bytes));
    }
    rows
}

fn zstd_roundtrip(rows: &[Vec<u8>], level: i32) -> (usize, usize) {
    let raw = rows.iter().map(Vec::len).sum();
    let compressed = rows
        .iter()
        .map(|row| {
            zstd::encode_all(row.as_slice(), level)
                .expect("zstd encode")
                .len()
        })
        .sum();
    (raw, compressed)
}

fn zstd_dictionary_roundtrip(rows: &[Vec<u8>], dictionary: &[u8]) -> (usize, usize) {
    let raw = rows.iter().map(Vec::len).sum();
    let mut compressor =
        zstd::bulk::Compressor::with_dictionary(ZSTD_FAST, dictionary).expect("zstd dictionary");
    let compressed = rows
        .iter()
        .map(|row| compressor.compress(row).expect("zstd encode").len())
        .sum();
    (raw, compressed)
}

fn gzip_compress(row: &[u8]) -> Vec<u8> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::new(GZIP_LEVEL));
    encoder.write_all(row).expect("gzip encode");
    encoder.finish().expect("gzip finish")
}

fn gzip_decompress(row: &[u8]) -> Vec<u8> {
    let mut decoded = Vec::new();
    GzDecoder::new(row)
        .read_to_end(&mut decoded)
        .expect("gzip decode");
    decoded
}

fn gzip_roundtrip(rows: &[Vec<u8>]) -> (usize, usize) {
    let raw = rows.iter().map(Vec::len).sum();
    let compressed = rows.iter().map(|row| gzip_compress(row).len()).sum();
    (raw, compressed)
}

fn ratio(raw: usize, compressed: usize) -> f64 {
    if compressed == 0 {
        return 0.0;
    }
    raw as f64 / compressed as f64
}

fn report_ratios(band: &str, rows: &[Vec<u8>], dictionary: &[u8]) {
    let (raw, fast) = zstd_roundtrip(rows, ZSTD_FAST);
    let (_, dense) = zstd_roundtrip(rows, ZSTD_DENSE);
    let (_, gzip) = gzip_roundtrip(rows);
    let (_, dictionary_compressed) = zstd_dictionary_roundtrip(rows, dictionary);
    println!(
        "band {band}: rows {}, raw {raw} B, zstd-{ZSTD_FAST} {:.2}x, zstd-{ZSTD_DENSE} {:.2}x, \
         gzip-{GZIP_LEVEL} {:.2}x, zstd-{ZSTD_FAST}+dict {:.2}x",
        rows.len(),
        ratio(raw, fast),
        ratio(raw, dense),
        ratio(raw, gzip),
        ratio(raw, dictionary_compressed),
    );
}

fn bench_band(criterion: &mut Criterion, band: &str, rows: &[Vec<u8>], dictionary: &[u8]) {
    let raw: usize = rows.iter().map(Vec::len).sum();
    let mut group = criterion.benchmark_group(format!("compress/{band}"));
    group.throughput(Throughput::Bytes(raw as u64));
    group.bench_function(
        BenchmarkId::from_parameter(format!("zstd-{ZSTD_FAST}")),
        |b| b.iter(|| zstd_roundtrip(rows, ZSTD_FAST)),
    );
    group.bench_function(
        BenchmarkId::from_parameter(format!("zstd-{ZSTD_DENSE}")),
        |b| b.iter(|| zstd_roundtrip(rows, ZSTD_DENSE)),
    );
    group.bench_function(
        BenchmarkId::from_parameter(format!("gzip-{GZIP_LEVEL}")),
        |b| b.iter(|| gzip_roundtrip(rows)),
    );
    group.bench_function(
        BenchmarkId::from_parameter(format!("zstd-{ZSTD_FAST}-dict")),
        |b| b.iter(|| zstd_dictionary_roundtrip(rows, dictionary)),
    );
    group.finish();

    let fast: Vec<Vec<u8>> = rows
        .iter()
        .map(|row| zstd::encode_all(row.as_slice(), ZSTD_FAST).expect("zstd encode"))
        .collect();
    let gzipped: Vec<Vec<u8>> = rows.iter().map(|row| gzip_compress(row)).collect();
    let mut group = criterion.benchmark_group(format!("decompress/{band}"));
    group.throughput(Throughput::Bytes(raw as u64));
    group.bench_function(
        BenchmarkId::from_parameter(format!("zstd-{ZSTD_FAST}")),
        |b| {
            b.iter_batched(
                || &fast,
                |rows| {
                    rows.iter()
                        .map(|row| zstd::decode_all(row.as_slice()).expect("zstd decode").len())
                        .sum::<usize>()
                },
                BatchSize::SmallInput,
            )
        },
    );
    group.bench_function(
        BenchmarkId::from_parameter(format!("gzip-{GZIP_LEVEL}")),
        |b| {
            b.iter_batched(
                || &gzipped,
                |rows| {
                    rows.iter()
                        .map(|row| gzip_decompress(row).len())
                        .sum::<usize>()
                },
                BatchSize::SmallInput,
            )
        },
    );
    group.finish();
}

fn payload_codecs(criterion: &mut Criterion) {
    let Some(path) = database_path() else {
        println!("{MISSING_CORPUS}");
        return;
    };
    let connection = Connection::open_with_flags(
        &path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .expect("open corpus");

    for band in &BANDS {
        let rows = sample_band(&connection, band);
        if rows.is_empty() {
            continue;
        }
        let samples: Vec<&[u8]> = rows.iter().map(Vec::as_slice).collect();
        let dictionary = zstd::dict::from_samples(&samples, DICTIONARY_BYTES).unwrap_or_default();
        report_ratios(band.name, &rows, &dictionary);
        bench_band(criterion, band.name, &rows, &dictionary);
    }
}

criterion_group!(benches, payload_codecs);
criterion_main!(benches);
