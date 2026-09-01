use std::path::Path;

use terrarium::{FilesystemMode, Kernel, RunFilesystemAuthority, WriteScope};

fn display(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

#[test]
fn read_only_kernel_denies_every_write() {
    let root = std::env::temp_dir().join(format!("terrarium-api-ro-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("temp root");

    let kernel = Kernel::new(RunFilesystemAuthority::ReadOnly);
    assert_eq!(kernel.authority().mode(), FilesystemMode::ReadOnly);
    let outcome = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("runtime")
        .block_on(kernel.run(
            &format!(
                "return host.fs.write('{}', 'no')",
                display(&root.join("f.txt"))
            ),
            2_000,
        ));
    assert!(!outcome.ok);
    let message = outcome.error.expect("denied write").message;
    assert!(message.contains("write_denied"), "{message}");
    assert!(!root.join("f.txt").exists());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn operator_scopes_bound_a_planned_write_kernel() {
    let root = std::env::temp_dir().join(format!("terrarium-api-scoped-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("temp root");

    let scope = WriteScope::from_operator_spec(&display(&root)).expect("prefix scope");
    let kernel = Kernel::new(RunFilesystemAuthority::Scoped(vec![scope]));
    assert_eq!(kernel.authority().mode(), FilesystemMode::PlannedWrite);

    let runtime = || {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("runtime")
    };
    let inside = runtime().block_on(kernel.run(
        &format!(
            "return host.fs.write('{}', 'api payload')",
            display(&root.join("nested/api.txt"))
        ),
        2_000,
    ));
    assert!(inside.ok, "error: {:?}", inside.error);
    assert_eq!(
        std::fs::read_to_string(root.join("nested/api.txt")).unwrap(),
        "api payload"
    );

    // a valid absolute path outside every scope: '/definitely/...' is not absolute on
    // Windows and would fail path validation before the authorization check
    let uncovered = std::env::temp_dir().join(format!(
        "terrarium-api-uncovered-{}.txt",
        std::process::id()
    ));
    let outside = runtime().block_on(kernel.run(
        &format!("return host.fs.write('{}', 'x')", display(&uncovered)),
        2_000,
    ));
    assert!(!outside.ok);
    let message = outside.error.expect("unauthorized write").message;
    assert!(message.contains("write_not_authorized"), "{message}");

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn kernel_preserves_json_values_without_the_cli() {
    let kernel = Kernel::new(RunFilesystemAuthority::ReadOnly);
    let outcome = kernel
        .run("return {answer: 6 * 7, items: [1, 2]}", 1_000)
        .await;

    assert!(outcome.ok, "error: {:?}", outcome.error);
    assert_eq!(
        outcome.value,
        Some(serde_json::json!({"answer": 42, "items": [1, 2]}))
    );
    assert_eq!(outcome.termination, terrarium::Termination::Returned);
}

#[tokio::test]
async fn documented_async_scan_syntax_yields_lines() {
    let root = std::env::temp_dir().join(format!("terrarium-scan-api-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("scan root");
    std::fs::write(root.join("one.txt"), "first\nsecond\n").expect("scan file");

    let kernel = Kernel::new(RunFilesystemAuthority::ReadOnly);
    let outcome = kernel
        .run(
            &format!(
                "for await (const line of host.fs.scan('{}')) {{ return line.text; }}",
                display(&root)
            ),
            1_000,
        )
        .await;

    assert!(outcome.ok, "error: {:?}", outcome.error);
    assert_eq!(outcome.value, Some(serde_json::json!("first")));
}

#[tokio::test]
async fn every_program_uses_async_function_body_semantics() {
    let kernel = Kernel::new(RunFilesystemAuthority::ReadOnly);
    let outcome = kernel
        .run("function value() { return 41; }\nreturn value() + 1", 1_000)
        .await;
    assert!(outcome.ok, "error: {:?}", outcome.error);
    assert_eq!(outcome.value, Some(serde_json::json!(42)));
}

#[tokio::test]
async fn timeout_zero_is_rejected_without_starting_a_runtime() {
    let kernel = Kernel::new(RunFilesystemAuthority::ReadOnly);
    let outcome = kernel.run("return 1", 0).await;
    assert!(!outcome.ok);
    assert_eq!(outcome.termination, terrarium::Termination::Fatal);
    assert_eq!(
        outcome.error.as_ref().map(|error| &error.kind),
        Some(&terrarium::ErrorKind::Configuration)
    );
}

#[test]
fn kernel_renders_the_current_contract() {
    let kernel = Kernel::new(RunFilesystemAuthority::ReadOnly);
    let contract = kernel.contract();

    assert!(contract.contains("host.fs.list(dir)"));
    assert!(contract.contains("host.fs.read(path, from, to)"));
    assert!(contract.contains("N: text"));
    assert!(contract.contains(
        "host.fs.scan(path, {glob?, contains?, skipDirs?, skipExts?, gitignore?, hidden?})"
    ));
    assert!(contract.contains("enters the next model context only through"));
    assert!(
        contract.contains("Do not return complete scan results, whole file contents, large arrays")
    );
    assert!(contract.contains("authorized file"));
    assert!(contract.contains("return only its path"));
    assert!(contract.contains("host-derived write receipts"));
    assert!(contract.contains("For one known file, use `host.fs.read` or `host.fs.text` instead"));
    assert!(contract.contains("fails instead of guessing"));
    assert!(contract.contains("to: \"model\""));
    assert!(contract.contains("to: \"user\""));
    assert!(contract.contains("```access"));
    assert!(contract.contains("read-only"));
    assert!(contract.contains("planned-write"));
    assert!(contract.contains("full-access"));
    assert!(!contract.contains("host.agent.answer"));
    assert!(!contract.contains("{{MOUNTS}}"));
}
