use criterion::{Criterion, criterion_group, criterion_main};
use filex::{
    catalog::segment::{Identity, Record, Root, Segment},
    search_filter::Filter,
};
fn bench(c: &mut Criterion) {
    let records = (0..50_000).map(|i| Record {
        id: i + 1,
        parent: if i == 0 { 0 } else { 1 },
        root: 1,
        name: format!("file-{i}.{}", if i % 100 == 0 { "pdf" } else { "txt" }).into_bytes(),
        flags: u8::from(i == 0),
        identity: Identity::default(),
        size: Some(i),
        mtime: Some(0),
    });
    let segment = Segment::build(
        records,
        vec![Root {
            id: 1,
            path: "/bench".into(),
            device: 0,
        }],
        0,
    )
    .unwrap();
    c.bench_function("selective_extension", |b| {
        b.iter(|| segment.filter_candidates(&[Filter::Ext("pdf".into())], 20001))
    });
}
criterion_group!(benches, bench);
criterion_main!(benches);
