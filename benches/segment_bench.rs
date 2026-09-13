use criterion::{Criterion, criterion_group, criterion_main};
use filex::catalog::segment::{Identity, Record, Root, Segment};
fn bench(c: &mut Criterion) {
    c.bench_function("build_10k_segment", |b| {
        b.iter(|| {
            let records = (0..10_000).map(|i| Record {
                id: i + 1,
                parent: if i == 0 { 0 } else { 1 },
                root: 1,
                name: format!("file-{i}.txt").into_bytes(),
                flags: u8::from(i == 0),
                identity: Identity::default(),
                size: Some(i),
                mtime: Some(0),
            });
            Segment::build(
                records,
                vec![Root {
                    id: 1,
                    path: "/bench".into(),
                    device: 0,
                }],
                0,
            )
            .unwrap()
        })
    });
}
criterion_group!(benches, bench);
criterion_main!(benches);
