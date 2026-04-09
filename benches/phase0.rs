use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use tide::model::Model;
use tide::msg::Msg;
use tide::tree::{build_tree, flatten, WindowInfo};
use tide::update::update;
use tide::view::build_tree_items;

fn fixture_windows(count: usize) -> Vec<WindowInfo> {
    (0..count)
        .map(|idx| WindowInfo {
            id: format!("@{}", idx + 1),
            index: idx + 1,
            name: match idx % 5 {
                0 => format!("proj{}:api:tab{}", idx % 25, idx),
                1 => format!("proj{}:ui:tab{}", idx % 25, idx),
                2 => format!("proj{}:ops:tab{}", idx % 25, idx),
                3 => format!("proj{}:tab{}", idx % 25, idx),
                _ => format!("scratch-{}", idx),
            },
            active: idx == 0,
        })
        .collect()
}

fn fixture_model(count: usize) -> Model {
    let mut model = Model::new(
        "bench".to_string(),
        "$1".to_string(),
        "%sidebar".to_string(),
        "%home".to_string(),
        "@1".to_string(),
    );
    let _ = update(&mut model, Msg::WindowListLoaded(fixture_windows(count)));
    model
}

fn bench_build_tree(c: &mut Criterion) {
    let mut group = c.benchmark_group("tree_build");
    for count in [100usize, 500, 1000] {
        let windows = fixture_windows(count);
        group.bench_with_input(
            BenchmarkId::from_parameter(count),
            &windows,
            |b, windows| {
                b.iter(|| build_tree(black_box(windows)));
            },
        );
    }
    group.finish();
}

fn bench_flatten(c: &mut Criterion) {
    let mut group = c.benchmark_group("tree_flatten");
    for count in [100usize, 500, 1000] {
        let tree = build_tree(&fixture_windows(count));
        group.bench_with_input(BenchmarkId::from_parameter(count), &tree, |b, tree| {
            b.iter(|| flatten(black_box(tree)));
        });
    }
    group.finish();
}

fn bench_view_build(c: &mut Criterion) {
    let mut group = c.benchmark_group("view_build_tree_items");
    for count in [100usize, 500, 1000] {
        let model = fixture_model(count);
        group.bench_with_input(BenchmarkId::from_parameter(count), &model, |b, model| {
            b.iter(|| build_tree_items(black_box(model), black_box(25)));
        });
    }
    group.finish();
}

criterion_group!(phase0, bench_build_tree, bench_flatten, bench_view_build);
criterion_main!(phase0);
