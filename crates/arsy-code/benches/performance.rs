use arsy_code::{
    benchmark::{BenchmarkSample, Location, RegressionThreshold, RepositorySize, Temperature},
    edit::{self, EditAddress, EditOperation, EditTransaction},
    resource::Workspace,
};
use arsy_kernel::{
    context::{
        Authority, Confidence, ContextCandidate, ContextFragment, ContextScope, ContextView,
        FragmentKind, FragmentSourceKind, Freshness, RankingWeights, SelectionPolicy,
    },
    domain::{ArtifactId, ContextViewId, FragmentId, ResourceRef},
};
use std::{env, fs, hint::black_box, path::Path, time::Instant};

fn main() {
    let gate = env::args().any(|argument| argument == "--gate");
    let remote_root = env::var_os("ARSY_BENCH_REMOTE_ROOT");
    let location = if remote_root.is_some() {
        Location::Remote
    } else {
        Location::Local
    };
    let mut samples = Vec::new();
    // One unrecorded pass first. The first search and the first edit in a
    // process pay for it: a cold page cache, the first allocations, and the
    // first walk of a directory nothing has opened yet. Measured, that lands
    // entirely on the first sample — which is why `edit`/small/cold used to
    // read slower than `edit`/large/cold, an ordering no real cost produces.
    // `cold` still means a fixture this process has not touched; it no longer
    // also means a process that has done nothing at all.
    warm_up(remote_root.as_deref().map(Path::new));
    for size in [
        RepositorySize::Small,
        RepositorySize::Medium,
        RepositorySize::Large,
    ] {
        let fixture = fixture(size.files(), remote_root.as_deref().map(Path::new));
        let workspace = Workspace::open(fixture.path()).unwrap();
        measure_pair(&mut samples, "search", size, location, || {
            black_box(
                workspace
                    .search("needle", 32, size.files() + 1, 4096)
                    .unwrap(),
            );
        });
        measure_pair(&mut samples, "edit", size, location, || {
            black_box(edit_once(fixture.path()));
        });
        // Built once, outside the closure: the measurement is the selection,
        // not the construction of what it selects from.
        let fragments = context_fragments(size.files());
        let candidates = candidates(&fragments);
        measure_pair(&mut samples, "retrieval", size, location, || {
            black_box(select_context(&candidates, size.files()));
        });
    }
    println!("{}", serde_json::to_string_pretty(&samples).unwrap());
    if gate {
        for sample in &samples {
            assert!(
                RegressionThreshold::for_sample(sample).accepts(sample.elapsed_ns),
                "performance regression: {sample:?}"
            );
        }
    }
}

fn measure_pair(
    samples: &mut Vec<BenchmarkSample>,
    operation: &str,
    repository_size: RepositorySize,
    location: Location,
    mut run: impl FnMut(),
) {
    let cold = measure(1, &mut run);
    let warm = measure(5, &mut run);
    for (temperature, elapsed_ns) in [(Temperature::Cold, cold), (Temperature::Warm, warm)] {
        samples.push(BenchmarkSample {
            operation: operation.into(),
            repository_size,
            temperature,
            location,
            elapsed_ns,
        });
    }
}

fn measure(iterations: usize, run: &mut impl FnMut()) -> u64 {
    let mut elapsed = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        let started = Instant::now();
        run();
        elapsed.push(started.elapsed());
    }
    elapsed.sort_unstable();
    u64::try_from(elapsed[iterations / 2].as_nanos()).unwrap_or(u64::MAX)
}

/// Run each measured operation once on a fixture of its own, recording
/// nothing. Its cost is the process's start-up, not any operation's.
fn warm_up(root: Option<&Path>) {
    let fixture = fixture(RepositorySize::Small.files(), root);
    let workspace = Workspace::open(fixture.path()).unwrap();
    black_box(workspace.search("needle", 32, 8, 4096).unwrap());
    black_box(edit_once(fixture.path()));
    let fragments = context_fragments(RepositorySize::Small.files());
    black_box(select_context(
        &candidates(&fragments),
        RepositorySize::Small.files(),
    ));
}

/// The candidate list a selection ranks, one per fragment.
fn candidates(fragments: &[ContextFragment]) -> Vec<ContextCandidate<'_>> {
    fragments
        .iter()
        .map(|fragment| ContextCandidate {
            fragment,
            residency: "local",
            relevance: 8_000,
            recency: 8_000,
            novelty: 8_000,
        })
        .collect()
}

/// The selection both the warm-up and the measurement run.
fn select_context(candidates: &[ContextCandidate<'_>], budget: usize) -> ContextView {
    ContextView::select(
        ContextViewId::new(),
        candidates,
        &SelectionPolicy {
            scope: ContextScope::Global,
            require_trusted: false,
            allowed_residencies: vec!["local".into()],
            budget: u32::try_from(budget).unwrap(),
        },
        &RankingWeights {
            dependency: 1,
            authority: 1,
            relevance: 1,
            recency: 1,
            confidence: 1,
            novelty: 1,
            token_cost: 1,
        },
    )
    .unwrap()
}

/// The edit both the warm-up and the measurement run: write a file, then
/// replace its contents through a transaction.
fn edit_once(root: &Path) -> Vec<edit::FileEdit> {
    fs::write(root.join("edit.txt"), "old").unwrap();
    let transaction = EditTransaction {
        base: edit::workspace_version(root).unwrap(),
        operations: vec![EditOperation {
            path: "edit.txt".into(),
            address: EditAddress::TextAnchor {
                needle: "old".into(),
                occurrence: None,
            },
            replacement: b"new".to_vec(),
        }],
    };
    edit::apply(root, &transaction).unwrap()
}

fn fixture(files: usize, root: Option<&Path>) -> tempfile::TempDir {
    let fixture = root.map_or_else(
        || tempfile::tempdir().unwrap(),
        |root| {
            tempfile::Builder::new()
                .prefix("arsy-bench-")
                .tempdir_in(root)
                .unwrap()
        },
    );
    for index in 0..files {
        fs::write(
            fixture.path().join(format!("file-{index:04}.txt")),
            if index % 8 == 0 { "needle\n" } else { "hay\n" },
        )
        .unwrap();
    }
    fs::write(fixture.path().join("edit.txt"), "old").unwrap();
    fixture
}

fn context_fragments(count: usize) -> Vec<ContextFragment> {
    (0..count)
        .map(|_| {
            ContextFragment::new(
                FragmentId::new(),
                FragmentKind::Evidence,
                ResourceRef::new("file", "/repo/src/lib.rs").unwrap(),
                FragmentSourceKind::Repository,
                ContextScope::Global,
                ArtifactId::new(),
                1,
                Authority::Untrusted,
                Confidence::new(8_000).unwrap(),
                Freshness::Current,
                Vec::new(),
            )
            .unwrap()
        })
        .collect()
}
