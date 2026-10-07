//! Browse listing cost at 1k/10k/100k entries: the full `read_dir_sorted`
//! (one stat per entry + sort; warm cache) and the name sort alone.
use criterion::{BatchSize, BenchmarkId, Criterion, criterion_group, criterion_main};
use filex::{
    listing::{Entry, read_dir_sorted, sort_entries},
    settings::SortSettings,
};

const SIZES: [usize; 3] = [1_000, 10_000, 100_000];

fn name(i: usize) -> String {
    // Mixed case so the case-insensitive comparator does real work.
    if i.is_multiple_of(2) {
        format!("File-{i:06}.txt")
    } else {
        format!("file-{i:06}.TXT")
    }
}

fn bench(c: &mut Criterion) {
    let sort = SortSettings::default();
    let mut group = c.benchmark_group("listing");
    group.sample_size(10);
    for n in SIZES {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..n {
            std::fs::File::create(dir.path().join(name(i))).unwrap();
        }
        group.bench_with_input(BenchmarkId::new("read_dir_sorted", n), &n, |b, _| {
            b.iter(|| read_dir_sorted(dir.path(), &sort).unwrap())
        });
        let entries = read_dir_sorted(dir.path(), &sort).unwrap();
        group.bench_with_input(BenchmarkId::new("sort_by_name", n), &n, |b, _| {
            b.iter_batched(
                || entries.iter().rev().cloned().collect::<Vec<Entry>>(),
                |mut entries| sort_entries(&mut entries, &sort),
                BatchSize::LargeInput,
            )
        });
    }
    group.finish();
}
criterion_group!(benches, bench);
criterion_main!(benches);
