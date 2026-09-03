use arsy_kernel::{
    domain::FragmentId,
    prompt::{
        compile, ModelFamily, PromptFragment, PromptFragmentKind, PromptStrategy,
        BUILT_IN_STRATEGIES,
    },
    secret::Redactor,
};

fn snapshot(family: ModelFamily) -> String {
    let ids = [
        "00000000-0000-0000-0000-000000000001",
        "00000000-0000-0000-0000-000000000002",
        "00000000-0000-0000-0000-000000000003",
        "00000000-0000-0000-0000-000000000004",
    ];
    let fragments = [
        (
            PromptFragmentKind::StableInstruction,
            "mandatory instruction",
        ),
        (PromptFragmentKind::Task, "fix the failing test"),
        (PromptFragmentKind::Context, "src/lib.rs is relevant"),
        (PromptFragmentKind::PermissionState, "network is denied"),
    ]
    .into_iter()
    .zip(ids)
    .map(|((kind, content), id)| PromptFragment {
        id: id.parse::<FragmentId>().unwrap(),
        kind,
        content: content.into(),
    })
    .collect();
    let strategies = BUILT_IN_STRATEGIES
        .iter()
        .map(|strategy| strategy as &dyn PromptStrategy)
        .collect::<Vec<_>>();
    let compiled = compile(family, fragments, &strategies, &Redactor::new(), 1_000).unwrap();
    serde_json::to_string_pretty(
        &compiled
            .segments
            .iter()
            .map(|segment| &segment.text)
            .collect::<Vec<_>>(),
    )
    .unwrap()
        + "\n"
}

#[test]
fn every_family_matches_its_reviewable_golden_snapshot() {
    for (family, expected) in [
        (
            ModelFamily::Gpt,
            include_str!("../../../fixtures/prompt/gpt.json"),
        ),
        (
            ModelFamily::Claude,
            include_str!("../../../fixtures/prompt/claude.json"),
        ),
        (
            ModelFamily::Gemini,
            include_str!("../../../fixtures/prompt/gemini.json"),
        ),
        (
            ModelFamily::QwenDeepseek,
            include_str!("../../../fixtures/prompt/qwen-deepseek.json"),
        ),
        (
            ModelFamily::Local,
            include_str!("../../../fixtures/prompt/local-weak.json"),
        ),
    ] {
        assert_eq!(snapshot(family), expected.replace("\r\n", "\n"));
    }
}
