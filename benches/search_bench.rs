use criterion::{Criterion, criterion_group, criterion_main};
use filex::search::literal::LiteralIndex;
fn bench(c: &mut Criterion) {
    let names: Vec<_> = (0..50_000).map(|i| format!("report_{i:06}.pdf")).collect();
    let index = LiteralIndex::build(names.iter().map(String::as_str))
        .unwrap()
        .into_fm();
    for q in ["e", "re", "report_000001.pdf", "0001", "none"] {
        c.bench_function(q, |b| b.iter(|| index.search(std::hint::black_box(q), 100)));
    }
}
criterion_group!(benches, bench);
criterion_main!(benches);
