//! The harness core, exercised the way a turn exercises it.
//!
//! Every assertion goes through [`ToolRuntime`] rather than the executors
//! underneath, because the thing worth protecting is the whole path: decode,
//! authorize, dispatch, read the result artifact back, render it. A test that
//! called an executor directly would pass while the model saw nothing.

use arsy_code::{
    agent::{self, Authorization, ToolRuntime},
    resource::Workspace,
};
use arsy_kernel::{
    artifact::{ArtifactStore, FileArtifactStore},
    capability::{CapabilityAction, PolicySource, ResourcePattern},
    domain::Principal,
    policy::{
        ActorMatch, PolicyRule, RiskContext, RuleEffect, RuleSet, SandboxAssurance,
        WorkspaceCleanliness,
    },
};
use serde_json::{json, Value};
use std::sync::Arc;

/// A ruleset that allows `actions` outright and says nothing about the rest,
/// which the engine denies: silence is a refusal, so a test that forgets to
/// name an action gets a denial rather than a surprise effect.
fn rules(actions: &[CapabilityAction]) -> RuleSet {
    RuleSet::compile(actions.iter().map(|action| PolicyRule {
        source: PolicySource::User,
        effect: RuleEffect::Allow,
        actor: ActorMatch::Any,
        action: *action,
        pattern: ResourcePattern::new(action.default_scheme(), "**").unwrap(),
        expires_at_ms: None,
        delegation_depth: 0,
        minimum_assurance: SandboxAssurance::None,
    }))
}

fn runtime(root: &std::path::Path, rules: RuleSet) -> ToolRuntime {
    let workspace = Workspace::open(root).unwrap();
    let artifacts: Arc<dyn ArtifactStore> =
        Arc::new(FileArtifactStore::open(root.join(".arsy/artifacts"), 0).unwrap());
    agent::runtime(
        &workspace,
        rules,
        artifacts,
        0,
        Principal::System,
        RiskContext {
            reversible: true,
            workspace: WorkspaceCleanliness::Clean,
            sandbox: SandboxAssurance::None,
        },
        arsy_code::operations::Reachable::default(),
    )
    .unwrap()
}

/// A runtime that may do anything, for the tests about behaviour rather than
/// authority.
fn permissive(root: &std::path::Path) -> ToolRuntime {
    runtime(root, rules(CapabilityAction::ALL))
}

/// Run a call the way an operator at a keyboard would: whatever policy asks
/// for is answered yes.
///
/// [`ToolRuntime::invoke`] is the unattended path and refuses an approval, so
/// a test about what a tool *does* has to go the attended way — which is also
/// the path the TUI takes, so this exercises it.
fn attended(runtime: &ToolRuntime, tool: &str, arguments: &Value) -> agent::ToolResult {
    let Ok(request) = runtime.prepare(tool, arguments) else {
        return *runtime.prepare(tool, arguments).unwrap_err();
    };
    match runtime.authorize(&request).approve() {
        Ok(grants) => runtime.dispatch(tool, &request, &grants, std::time::Instant::now()),
        Err(reason) => agent::ToolResult::refused(tool, reason),
    }
}

fn ok(runtime: &ToolRuntime, tool: &str, arguments: Value) -> String {
    let result = attended(runtime, tool, &arguments);
    assert!(result.success, "{tool} failed: {}", result.output);
    result.output
}

fn err(runtime: &ToolRuntime, tool: &str, arguments: Value) -> String {
    let result = attended(runtime, tool, &arguments);
    assert!(
        !result.success,
        "{tool} unexpectedly succeeded: {}",
        result.output
    );
    result.output
}

#[test]
fn the_offered_tools_are_the_ones_the_registry_can_dispatch() {
    let root = tempfile::tempdir().unwrap();
    let runtime = permissive(root.path());

    // A WASM build offers `plugin.invoke` as well; the rest of the list is the
    // same, and what this asserts is that the offer follows registration.
    let offered: Vec<String> = runtime
        .schemas()
        .into_iter()
        .map(|schema| schema.name)
        .filter(|name| name != "plugin.invoke")
        .collect();
    assert_eq!(
        offered,
        [
            "fs.read",
            "fs.list",
            "search.files",
            "search.text",
            "code.symbol",
            "code.explain",
            "code.references",
            "code.diagnostics",
            "code.rename",
            "fs.edit",
            "apply_patch",
            "fs.write",
            "fs.delete",
            "fs.move",
            "bash",
            "repo_discover",
            "plan_add",
            "plan_update",
            "plan_remove",
            "plan_reorder",
            "plan_list",
            "validate_record",
            "validate_status",
        ]
    );
    // Every offered tool publishes a schema the model can fill in, and no tool
    // is offered whose operation is not registered.
    for schema in runtime.schemas() {
        assert_eq!(schema.input_schema["type"], "object", "{}", schema.name);
        assert!(!schema.description.is_empty(), "{}", schema.name);
    }

    let unknown = err(&runtime, "grep", json!({}));
    assert!(
        unknown.contains("not a tool this session offers"),
        "{unknown}"
    );
    // The refusal names what is available, so the model can pick again rather
    // than guess a second time.
    assert!(unknown.contains("search.text"), "{unknown}");
}

#[test]
fn reading_returns_text_line_windows_and_reports_binary_without_decoding_it() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("src")).unwrap();
    std::fs::write(root.path().join("src/lib.rs"), "one\ntwo\nthree\nfour\n").unwrap();
    std::fs::write(root.path().join("blob"), b"\x00\x01binary").unwrap();
    let runtime = permissive(root.path());

    assert_eq!(
        ok(&runtime, "fs.read", json!({"path": "src/lib.rs"})),
        "one\ntwo\nthree\nfour"
    );

    // A window says which lines it is, so a following edit can be addressed.
    let window = ok(
        &runtime,
        "fs.read",
        json!({"path": "src/lib.rs", "offset": 2, "limit": 2}),
    );
    assert_eq!(window, "lines 2-3 of 4\ntwo\nthree");

    let binary = ok(&runtime, "fs.read", json!({"path": "blob"}));
    assert!(binary.contains("binary file"), "{binary}");
    assert!(!binary.contains('\u{fffd}'), "{binary}");

    let missing = err(&runtime, "fs.read", json!({"path": "nowhere.rs"}));
    assert!(!missing.is_empty());

    // Nothing to return is two different questions, and an empty string would
    // be read as "the file is empty" for both.
    std::fs::write(root.path().join("empty.rs"), "").unwrap();
    assert_eq!(
        ok(&runtime, "fs.read", json!({"path": "empty.rs"})),
        "(empty file)"
    );
    let past = ok(
        &runtime,
        "fs.read",
        json!({"path": "src/lib.rs", "offset": 900}),
    );
    assert_eq!(past, "(offset 900 is past the end; the file has 4 lines)");
}

#[test]
fn listing_sorts_directories_first_and_names_the_root_by_default() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("src")).unwrap();
    std::fs::write(root.path().join("b.txt"), "bb").unwrap();
    std::fs::write(root.path().join("a.txt"), "a").unwrap();
    let runtime = permissive(root.path());

    let listed = ok(&runtime, "fs.list", json!({}));
    let rows: Vec<&str> = listed.lines().collect();
    assert_eq!(rows[0], "src/");
    assert!(rows.contains(&"a.txt (1 bytes)"), "{listed}");
    assert!(rows.contains(&"b.txt (2 bytes)"), "{listed}");

    assert_eq!(
        ok(&runtime, "fs.list", json!({"path": "src"})),
        "(empty directory)"
    );
}

#[test]
fn search_finds_files_by_glob_and_text_by_content_honouring_gitignore() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("src/deep")).unwrap();
    std::fs::write(root.path().join(".gitignore"), "ignored.rs\n").unwrap();
    std::fs::write(root.path().join("src/lib.rs"), "fn authenticate() {}\n").unwrap();
    std::fs::write(root.path().join("src/deep/mod.rs"), "// authenticate\n").unwrap();
    std::fs::write(root.path().join("ignored.rs"), "fn authenticate() {}\n").unwrap();
    let runtime = permissive(root.path());

    // A pattern without a separator matches file names at any depth.
    let found = ok(&runtime, "search.files", json!({"pattern": "*.rs"}));
    let paths: Vec<&str> = found.lines().collect();
    assert_eq!(paths, ["src/deep/mod.rs", "src/lib.rs"], "{found}");
    assert_eq!(
        ok(&runtime, "search.files", json!({"pattern": "src/*.rs"})),
        "src/lib.rs"
    );

    let hits = ok(&runtime, "search.text", json!({"query": "authenticate"}));
    assert!(hits.contains("src/lib.rs:1:"), "{hits}");
    assert!(
        !hits.contains("ignored.rs"),
        "an ignored file is not searched: {hits}"
    );

    assert_eq!(
        ok(&runtime, "search.text", json!({"query": "nothing here"})),
        "no matches"
    );
    // A limit that cuts the answer says so, rather than looking complete.
    let limited = ok(
        &runtime,
        "search.text",
        json!({"query": "authenticate", "limit": 1}),
    );
    assert!(limited.contains("more results were available"), "{limited}");
}

#[test]
fn editing_replaces_a_unique_anchor_and_refuses_an_ambiguous_one() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("main.rs"), "let a = 1;\nlet b = 1;\n").unwrap();
    let runtime = permissive(root.path());

    ok(
        &runtime,
        "fs.edit",
        json!({"path": "main.rs", "old_text": "let a = 1;", "new_text": "let a = 2;"}),
    );
    assert_eq!(
        std::fs::read_to_string(root.path().join("main.rs")).unwrap(),
        "let a = 2;\nlet b = 1;\n"
    );

    // Two candidates and no choice between them is refused, not guessed.
    let ambiguous = err(
        &runtime,
        "fs.edit",
        json!({"path": "main.rs", "old_text": " = ", "new_text": " := "}),
    );
    assert!(ambiguous.contains("candidates"), "{ambiguous}");
    assert_eq!(
        std::fs::read_to_string(root.path().join("main.rs")).unwrap(),
        "let a = 2;\nlet b = 1;\n",
        "a refused edit leaves the file alone"
    );

    // Naming the occurrence resolves it.
    ok(
        &runtime,
        "fs.edit",
        json!({"path": "main.rs", "old_text": " = ", "new_text": " := ", "occurrence": 2}),
    );
    assert_eq!(
        std::fs::read_to_string(root.path().join("main.rs")).unwrap(),
        "let a = 2;\nlet b := 1;\n"
    );

    let absent = err(
        &runtime,
        "fs.edit",
        json!({"path": "main.rs", "old_text": "not present", "new_text": "x"}),
    );
    assert!(absent.contains("anchor"), "{absent}");
}

#[test]
fn writing_creating_moving_and_deleting_report_what_they_changed() {
    let root = tempfile::tempdir().unwrap();
    let runtime = permissive(root.path());

    let written = attended(
        &runtime,
        "fs.write",
        &json!({"path": "nested/deep/file.txt", "content": "hello"}),
    );
    assert!(written.success, "{}", written.output);
    assert_eq!(written.changed_files, ["nested/deep/file.txt"]);
    assert_eq!(
        std::fs::read_to_string(root.path().join("nested/deep/file.txt")).unwrap(),
        "hello"
    );

    // Writing over the same path reports an update, not a creation, so the
    // model is not told it made a file it replaced.
    let again = attended(
        &runtime,
        "fs.write",
        &json!({"path": "nested/deep/file.txt", "content": "hello again"}),
    );
    assert!(again.success, "{}", again.output);
    assert_eq!(again.metadata["created"], false);
    assert!(again.output.starts_with("updated"), "{}", again.output);

    ok(
        &runtime,
        "fs.move",
        json!({"from": "nested/deep/file.txt", "to": "moved.txt"}),
    );
    assert!(!root.path().join("nested/deep/file.txt").exists());
    assert_eq!(
        std::fs::read_to_string(root.path().join("moved.txt")).unwrap(),
        "hello again"
    );

    // A move never silently replaces the destination.
    std::fs::write(root.path().join("occupied.txt"), "keep me").unwrap();
    let clash = err(
        &runtime,
        "fs.move",
        json!({"from": "moved.txt", "to": "occupied.txt"}),
    );
    assert!(clash.contains("already exists"), "{clash}");
    assert_eq!(
        std::fs::read_to_string(root.path().join("occupied.txt")).unwrap(),
        "keep me"
    );

    assert_eq!(
        ok(&runtime, "fs.delete", json!({"path": "moved.txt"})),
        "deleted moved.txt"
    );
    assert!(!root.path().join("moved.txt").exists());
}

#[test]
fn a_patch_adds_updates_moves_and_deletes_and_is_refused_when_it_cannot_be_placed() {
    let root = tempfile::tempdir().unwrap();
    let runtime = permissive(root.path());

    let added = ok(
        &runtime,
        "apply_patch",
        json!({"patch": "*** Begin Patch\n*** Add File: src/main.rs\n+fn main() {\n+    println!(\"one\");\n+}\n*** End Patch\n"}),
    );
    assert_eq!(added, "added src/main.rs");
    assert_eq!(
        std::fs::read_to_string(root.path().join("src/main.rs")).unwrap(),
        "fn main() {\n    println!(\"one\");\n}\n"
    );

    // Context anchors the change: the `-` line is found by the lines around it.
    ok(
        &runtime,
        "apply_patch",
        json!({"patch": "*** Begin Patch\n*** Update File: src/main.rs\n@@ fn main\n fn main() {\n-    println!(\"one\");\n+    println!(\"two\");\n }\n*** End Patch\n"}),
    );
    assert!(std::fs::read_to_string(root.path().join("src/main.rs"))
        .unwrap()
        .contains("two"));

    // Indentation that drifted still matches, after the exact pass fails.
    ok(
        &runtime,
        "apply_patch",
        json!({"patch": "*** Begin Patch\n*** Update File: src/main.rs\n-  println!(\"two\");\n+    println!(\"three\");\n*** End Patch\n"}),
    );
    assert!(std::fs::read_to_string(root.path().join("src/main.rs"))
        .unwrap()
        .contains("three"));

    ok(
        &runtime,
        "apply_patch",
        json!({"patch": "*** Begin Patch\n*** Update File: src/main.rs\n*** Move to: src/app.rs\n-}\n+}\n*** End Patch\n"}),
    );
    assert!(!root.path().join("src/main.rs").exists());
    assert!(root.path().join("src/app.rs").exists());

    assert_eq!(
        ok(
            &runtime,
            "apply_patch",
            json!({"patch": "*** Begin Patch\n*** Delete File: src/app.rs\n*** End Patch\n"})
        ),
        "deleted src/app.rs"
    );

    std::fs::write(root.path().join("file.txt"), "alpha\nbeta\n").unwrap();
    let stale = err(
        &runtime,
        "apply_patch",
        json!({"patch": "*** Begin Patch\n*** Update File: file.txt\n-gamma\n+delta\n*** End Patch\n"}),
    );
    assert!(stale.contains("no line matched"), "{stale}");
    assert_eq!(
        std::fs::read_to_string(root.path().join("file.txt")).unwrap(),
        "alpha\nbeta\n",
        "a rejected patch leaves the file alone"
    );

    let unmarked = err(&runtime, "apply_patch", json!({"patch": "just text"}));
    assert!(unmarked.contains("*** Begin Patch"), "{unmarked}");
}

#[test]
fn a_failed_hunk_names_the_closest_line_so_the_model_can_recover() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("runtime.rs"),
        "fn build_context(session: &Session) {\n    todo!()\n}\n",
    )
    .unwrap();
    let runtime = permissive(root.path());

    let rejected = err(
        &runtime,
        "apply_patch",
        json!({"patch": "*** Begin Patch\n*** Update File: runtime.rs\n-fn build_context(session: &mut Session) {\n+fn build_context(session: &Session, budget: u32) {\n*** End Patch\n"}),
    );
    assert!(rejected.contains("no line matched"), "{rejected}");
    assert!(
        rejected.contains("the closest is line 1"),
        "a rejected hunk points at what is actually there: {rejected}"
    );
    assert!(
        rejected.contains("fn build_context(session: &Session)"),
        "{rejected}"
    );
}

#[test]
fn a_patch_that_half_applies_says_what_already_landed() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("first.txt"), "alpha\n").unwrap();
    std::fs::write(root.path().join("second.txt"), "beta\n").unwrap();
    let runtime = permissive(root.path());

    // The first file matches; the second does not.
    let failed = err(
        &runtime,
        "apply_patch",
        json!({"patch": "*** Begin Patch\n*** Update File: first.txt\n-alpha\n+ALPHA\n*** Update File: second.txt\n-gamma\n+GAMMA\n*** End Patch\n"}),
    );

    assert!(failed.contains("no line matched"), "{failed}");
    assert!(
        failed.contains("already made") && failed.contains("updated first.txt"),
        "a half-applied patch must name what is on disk, or the model retries \
         the whole thing against files it already changed: {failed}"
    );
    assert_eq!(
        std::fs::read_to_string(root.path().join("first.txt")).unwrap(),
        "ALPHA\n"
    );
    assert_eq!(
        std::fs::read_to_string(root.path().join("second.txt")).unwrap(),
        "beta\n"
    );
}

#[test]
fn no_tool_reaches_outside_the_workspace() {
    let root = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    std::fs::write(elsewhere.path().join("target.txt"), "outside\n").unwrap();
    let runtime = permissive(root.path());

    for escape in ["../outside.txt", "/etc/passwd"] {
        for (tool, arguments) in [
            ("fs.read", json!({"path": escape})),
            ("fs.write", json!({"path": escape, "content": "x"})),
            ("fs.delete", json!({"path": escape})),
        ] {
            let refused = err(&runtime, tool, arguments);
            assert!(
                refused.contains("outside the workspace") || refused.contains("path"),
                "{tool} {escape}: {refused}"
            );
        }
    }

    // A symlink is a local-looking name that resolves out of the tree, so the
    // lexical check passes it and the capability directory has to catch it.
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(elsewhere.path(), root.path().join("link")).unwrap();
        let escaped = err(
            &runtime,
            "apply_patch",
            json!({"patch": "*** Begin Patch\n*** Update File: link/target.txt\n-outside\n+captured\n*** End Patch\n"}),
        );
        assert!(!escaped.is_empty());
        assert_eq!(
            std::fs::read_to_string(elsewhere.path().join("target.txt")).unwrap(),
            "outside\n",
            "the file outside the workspace is untouched"
        );
    }
}

/// The commands are POSIX and the tool spawns `sh`, so this is a unix test —
/// the same gate `process::tests` uses, and for the same reason.
#[cfg(unix)]
#[test]
fn bash_runs_in_the_workspace_and_reports_output_exit_codes_and_deadlines() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("marker"), "x").unwrap();
    let runtime = permissive(root.path());

    assert_eq!(
        ok(&runtime, "bash", json!({"command": "printf 'hi\\n'"})),
        "hi\n"
    );
    // The workspace is the working directory, not wherever ARSY was launched.
    assert!(ok(&runtime, "bash", json!({"command": "ls"})).contains("marker"));

    let failed = err(
        &runtime,
        "bash",
        json!({"command": "printf 'oops\\n' >&2; exit 3"}),
    );
    assert!(failed.contains("oops"), "{failed}");
    assert!(failed.ends_with("Command exited with code 3"), "{failed}");

    let started = std::time::Instant::now();
    let timed_out = err(
        &runtime,
        "bash",
        json!({"command": "trap '' TERM; sleep 30", "timeout_ms": 300}),
    );
    assert!(timed_out.contains("deadline"), "{timed_out}");
    assert!(started.elapsed() < std::time::Duration::from_secs(20));

    // Output beyond the cap keeps the tail, which is where a failure is.
    let long = ok(
        &runtime,
        "bash",
        json!({"command": "seq 1 100000; printf 'LAST\\n'"}),
    );
    assert!(
        long.len() < agent::MAX_TOOL_OUTPUT_BYTES + 128,
        "{}",
        long.len()
    );
    assert!(long.contains("earlier bytes omitted"), "{long}");
    assert!(long.ends_with("LAST\n"), "{long}");

    let empty = err(&runtime, "bash", json!({"command": "   "}));
    assert!(empty.contains("non-empty"), "{empty}");
}

#[test]
fn policy_decides_every_call_and_a_refusal_performs_nothing() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("secret.txt"), "before").unwrap();
    // Reading is allowed; writing, deleting, and executing are not named, and
    // silence is a denial.
    let runtime = runtime(root.path(), rules(&[CapabilityAction::FsRead]));

    assert_eq!(
        ok(&runtime, "fs.read", json!({"path": "secret.txt"})),
        "before"
    );

    for (tool, arguments) in [
        (
            "fs.write",
            json!({"path": "secret.txt", "content": "after"}),
        ),
        ("fs.delete", json!({"path": "secret.txt"})),
        ("bash", json!({"command": "rm secret.txt"})),
    ] {
        let refused = err(&runtime, tool, arguments);
        assert!(refused.contains("denied by policy"), "{tool}: {refused}");
    }
    assert_eq!(
        std::fs::read_to_string(root.path().join("secret.txt")).unwrap(),
        "before",
        "a denied call performed nothing"
    );
}

#[test]
fn an_approval_grants_exactly_what_was_shown_and_nothing_beside_it() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("a.txt"), "a").unwrap();
    std::fs::write(root.path().join("b.txt"), "b").unwrap();
    let asks = RuleSet::compile([PolicyRule {
        source: PolicySource::User,
        effect: RuleEffect::RequireApproval,
        actor: ActorMatch::Any,
        action: CapabilityAction::FsWrite,
        pattern: ResourcePattern::new(CapabilityAction::FsWrite.default_scheme(), "**").unwrap(),
        expires_at_ms: None,
        delegation_depth: 0,
        minimum_assurance: SandboxAssurance::None,
    }]);
    let runtime = runtime(root.path(), asks);

    // Unattended, the call is refused rather than run: `invoke` is the path a
    // pipeline takes, and it has no operator to ask.
    let unattended = runtime.invoke("fs.write", &json!({"path": "a.txt", "content": "changed"}));
    assert!(!unattended.success, "{}", unattended.output);
    assert!(
        unattended.output.contains("approval"),
        "{}",
        unattended.output
    );
    assert_eq!(
        std::fs::read_to_string(root.path().join("a.txt")).unwrap(),
        "a"
    );

    // With one, the grant covers the file that was shown.
    let request = runtime
        .prepare("fs.write", &json!({"path": "a.txt", "content": "changed"}))
        .unwrap();
    let authorization = runtime.authorize(&request);
    assert!(matches!(authorization, Authorization::NeedsApproval { .. }));
    let grants = authorization.approve().unwrap();
    let result = runtime.dispatch("fs.write", &request, &grants, std::time::Instant::now());
    assert!(result.success, "{}", result.output);
    assert_eq!(
        std::fs::read_to_string(root.path().join("a.txt")).unwrap(),
        "changed"
    );

    // The same grants do not carry to another file: an approval is not a mode.
    let other = runtime
        .prepare("fs.write", &json!({"path": "b.txt", "content": "changed"}))
        .unwrap();
    let leaked = runtime.dispatch("fs.write", &other, &grants, std::time::Instant::now());
    assert!(!leaked.success, "{}", leaked.output);
    assert_eq!(
        std::fs::read_to_string(root.path().join("b.txt")).unwrap(),
        "b"
    );
}

#[test]
fn instructions_are_discovered_root_first_and_only_where_they_belong() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("crates/inner")).unwrap();
    std::fs::write(root.path().join("AGENTS.md"), "root rule").unwrap();
    // Two names in one directory contribute once, not twice.
    std::fs::write(root.path().join("CLAUDE.md"), "duplicate").unwrap();
    std::fs::write(root.path().join("crates/inner/CLAUDE.md"), "nested rule").unwrap();
    // Ordinary documentation is not an instruction.
    std::fs::write(root.path().join("README.md"), "not an instruction").unwrap();
    std::fs::write(root.path().join("crates/notes.md"), "also not").unwrap();

    let workspace = Workspace::open(root.path()).unwrap();
    let found = agent::instructions::discover(&workspace, &root.path().join("crates/inner"));

    assert_eq!(
        found
            .iter()
            .map(|instruction| (instruction.path.as_str(), instruction.text.as_str()))
            .collect::<Vec<_>>(),
        [
            ("AGENTS.md", "root rule"),
            ("crates/inner/CLAUDE.md", "nested rule"),
        ],
        "root first, one per directory, and nothing that is merely Markdown"
    );

    let prompt = agent::instructions::system_prompt(
        arsy_kernel::prompt::ModelFamily::Claude,
        &found,
        None,
        &arsy_kernel::secret::Redactor::new(),
        arsy_kernel::prompt::MAX_PROMPT_BYTES as u32,
    )
    .unwrap();
    let rendered = agent::instructions::render(&prompt);
    assert!(rendered.contains("root rule"), "{rendered}");
    assert!(rendered.contains("nested rule"), "{rendered}");
    assert!(rendered.contains(agent::instructions::HARNESS_INSTRUCTIONS.trim_end()));
    assert!(
        !rendered.contains("not an instruction"),
        "README is not injected: {rendered}"
    );
}

#[test]
fn every_tool_result_is_recoverable_evidence_in_the_artifact_store() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("file.txt"), "content").unwrap();
    let runtime = permissive(root.path());

    let result = attended(&runtime, "fs.read", &json!({"path": "file.txt"}));
    assert!(result.success);
    // The rendered text is for the model; the structured result is what an
    // audit reads, and it carries the digest a later edit is checked against.
    assert_eq!(result.metadata["path"], "file.txt");
    assert_eq!(result.metadata["total_lines"], 1);
    assert!(result.metadata["digest"]
        .as_str()
        .is_some_and(|digest| !digest.is_empty()));
}
