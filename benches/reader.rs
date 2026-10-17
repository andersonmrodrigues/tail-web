//! How the backward reader scales: `-n 10` should cost the same on a 1 MiB
//! file as on a 256 MiB one, while `+N` reads everything after line N.

use std::fs::File;
use std::hint::black_box;
use std::io::{self, BufWriter, Write};

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use tail_web::reader::{Count, Mode, tail_seekable, tail_stream};

const LINE: &[u8] = b"2026-10-17T11:35:10Z INFO request id=12345 path=/api/orders took=42ms\n";

fn log_file(bytes: usize) -> io::Result<File> {
    let mut file = tempfile::tempfile()?;
    let mut out = BufWriter::new(&mut file);
    for _ in 0..bytes / LINE.len() {
        out.write_all(LINE)?;
    }
    out.flush()?;
    drop(out);
    Ok(file)
}

fn last_lines(c: &mut Criterion) {
    let mut group = c.benchmark_group("last_10_lines");
    for mib in [1, 64, 256] {
        let mut file = log_file(mib << 20).expect("temp file");
        group.bench_with_input(
            BenchmarkId::from_parameter(format!("{mib}MiB")),
            &mib,
            |b, _| {
                b.iter(|| {
                    let mut out = Vec::with_capacity(1024);
                    tail_seekable(&mut file, Mode::Lines(Count::Last(10)), &mut out).unwrap();
                    // Each run starts from the beginning, as a fresh open would.
                    io::Seek::rewind(&mut file).unwrap();
                    black_box(out)
                })
            },
        );
    }
    group.finish();
}

fn from_line(c: &mut Criterion) {
    let size = 64 << 20;
    let mut file = log_file(size).expect("temp file");
    let mut group = c.benchmark_group("from_line");
    group.throughput(Throughput::Bytes(size as u64));
    group.bench_function("+1_64MiB", |b| {
        b.iter(|| {
            tail_seekable(&mut file, Mode::Lines(Count::From(1)), &mut io::sink()).unwrap();
            io::Seek::rewind(&mut file).unwrap();
        })
    });
    group.finish();
}

fn stream(c: &mut Criterion) {
    let data = LINE.repeat((16 << 20) / LINE.len());
    let mut group = c.benchmark_group("stream_last_10_lines");
    group.throughput(Throughput::Bytes(data.len() as u64));
    group.bench_function("16MiB", |b| {
        b.iter(|| {
            let mut out = Vec::with_capacity(1024);
            tail_stream(&data[..], Mode::Lines(Count::Last(10)), &mut out).unwrap();
            black_box(out)
        })
    });
    group.finish();
}

criterion_group!(benches, last_lines, from_line, stream);
criterion_main!(benches);
