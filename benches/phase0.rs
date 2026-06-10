use criterion::{black_box, criterion_group, criterion_main, BatchSize, BenchmarkId, Criterion};
use tide::model::Model;
use tide::msg::Msg;
use tide::tree::{build_tree, flatten, TreeNode, WindowInfo};
use tide::update::update;
use tide::view::build_tree_items;

fn fixture_windows(count: usize) -> Vec<WindowInfo> {
    (0..count)
        .map(|idx| {
            let name = match idx % 5 {
                0 => format!("proj{}:api:tab{}", idx % 25, idx),
                1 => format!("proj{}:ui:tab{}", idx % 25, idx),
                2 => format!("proj{}:ops:tab{}", idx % 25, idx),
                3 => format!("proj{}:tab{}", idx % 25, idx),
                _ => format!("scratch-{}", idx),
            };
            WindowInfo::new(&format!("@{}", idx + 1), idx + 1, &name, idx == 0)
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

fn rename_only_windows(count: usize) -> Vec<WindowInfo> {
    let mut windows = fixture_windows(count);
    if let Some(first) = windows.first_mut() {
        let name = match first.name.rsplit_once(':') {
            Some((folder, _)) => format!("{folder}:renamed"),
            None => "renamed".to_string(),
        };
        first.name = name.clone();
        first.tide_name = Some(name);
    }
    windows
}

fn add_only_windows(count: usize) -> Vec<WindowInfo> {
    let mut windows = fixture_windows(count);
    windows.push(WindowInfo::new(
        &format!("@{}", count + 1),
        count + 1,
        &format!("proj{}:added", count % 25),
        false,
    ));
    windows
}

fn remove_only_windows(count: usize) -> Vec<WindowInfo> {
    let mut windows = fixture_windows(count);
    if !windows.is_empty() {
        windows.remove(0);
    }
    windows
}

fn bench_window_list_loaded(c: &mut Criterion) {
    let mut group = c.benchmark_group("window_list_loaded");

    for count in [100usize, 500, 1000] {
        let base = fixture_windows(count);

        group.bench_with_input(BenchmarkId::new("same_list", count), &base, |b, windows| {
            b.iter_batched(
                || fixture_model(count),
                |mut model| {
                    update(
                        &mut model,
                        Msg::WindowListLoaded(black_box(windows.clone())),
                    );
                },
                BatchSize::SmallInput,
            );
        });

        let renamed = rename_only_windows(count);
        group.bench_with_input(
            BenchmarkId::new("rename_only", count),
            &renamed,
            |b, windows| {
                b.iter_batched(
                    || fixture_model(count),
                    |mut model| {
                        update(
                            &mut model,
                            Msg::WindowListLoaded(black_box(windows.clone())),
                        );
                    },
                    BatchSize::SmallInput,
                );
            },
        );

        let added = add_only_windows(count);
        group.bench_with_input(BenchmarkId::new("add_only", count), &added, |b, windows| {
            b.iter_batched(
                || fixture_model(count),
                |mut model| {
                    update(
                        &mut model,
                        Msg::WindowListLoaded(black_box(windows.clone())),
                    );
                },
                BatchSize::SmallInput,
            );
        });

        let removed = remove_only_windows(count);
        group.bench_with_input(
            BenchmarkId::new("remove_only", count),
            &removed,
            |b, windows| {
                b.iter_batched(
                    || fixture_model(count),
                    |mut model| {
                        update(
                            &mut model,
                            Msg::WindowListLoaded(black_box(windows.clone())),
                        );
                    },
                    BatchSize::SmallInput,
                );
            },
        );

        let collapsed = base.clone();
        group.bench_with_input(
            BenchmarkId::new("collapsed_folder", count),
            &collapsed,
            |b, windows| {
                b.iter_batched(
                    || {
                        let mut model = fixture_model(count);
                        model.mutate_tree(|tree| {
                            if let Some(TreeNode::Folder { expanded, .. }) = tree.get_mut(0) {
                                *expanded = false;
                            }
                        });
                        model
                    },
                    |mut model| {
                        update(
                            &mut model,
                            Msg::WindowListLoaded(black_box(windows.clone())),
                        );
                    },
                    BatchSize::SmallInput,
                );
            },
        );
    }

    group.finish();
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

criterion_group!(
    phase0,
    bench_build_tree,
    bench_flatten,
    bench_view_build,
    bench_window_list_loaded
);
criterion_main!(phase0);
