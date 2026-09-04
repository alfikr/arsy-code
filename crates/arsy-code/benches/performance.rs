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
            let path = fixture.path().join("edit.txt");
            fs::write(&path, "old").unwrap();
            let transaction = EditTransaction {
                base: edit::workspace_version(fixture.path()).unwrap(),
                operations: vec![EditOperation {
                    path: "edit.txt".into(),
                    address: EditAddress::TextAnchor {
                        needle: "old".into(),
                        occurrence: None,
                    },
                    replacement: b"new".to_vec(),
                }],
            };
            black_box(edit::apply(fixture.path(), &transaction).unwrap());
        });
        let fragments = context_fragments(size.files());
        let candidates = fragments
            .iter()
            .map(|fragment| ContextCandidate {
                fragment,
                residency: "local",
                relevance: 8_000,
                recency: 8_000,
                novelty: 8_000,
            })
            .collect::<Vec<_>>();
        measure_pair(&mut samples, "retrieval", size, location, || {
            black_box(
                ContextView::select(
                    ContextViewId::new(),
                    &candidates,
                    &SelectionPolicy {
                        scope: ContextScope::Global,
                        require_trusted: false,
                        allowed_residencies: vec!["local".into()],
                        budget: u32::try_from(size.files()).unwrap(),
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
                .unwrap(),
            );
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
