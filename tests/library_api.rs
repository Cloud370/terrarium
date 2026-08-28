use terrarium::{Kernel, Mount};

#[test]
fn kernel_rejects_overlapping_mounts() {
    let root = std::env::current_dir().expect("workspace root");
    let outer = Mount::new("/proj", &root, false).expect("outer mount");
    let inner = Mount::new("/proj/src", &root, false).expect("inner mount");
    let error = Kernel::new(vec![outer, inner]).expect_err("overlap must be rejected");
    assert!(error.contains("overlapping"), "{error}");
}

#[tokio::test]
async fn kernel_preserves_json_values_without_the_cli() {
    let kernel = Kernel::new(Vec::new()).expect("empty mounts are valid");
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

    let mount = Mount::new("/t", &root, false).expect("mount");
    let kernel = Kernel::new(vec![mount]).expect("kernel");
    let outcome = kernel
        .run(
            "for await (const line of host.fs.scan('/t')) { return line.text; }",
            1_000,
        )
        .await;

    assert!(outcome.ok, "error: {:?}", outcome.error);
    assert_eq!(outcome.value, Some(serde_json::json!("first")));
}

#[tokio::test]
async fn every_program_uses_async_function_body_semantics() {
    let kernel = Kernel::new(Vec::new()).expect("empty mounts are valid");
    let outcome = kernel
        .run("function value() { return 41; }\nreturn value() + 1", 1_000)
        .await;

    assert!(outcome.ok, "error: {:?}", outcome.error);
    assert_eq!(outcome.value, Some(serde_json::json!(42)));
}

#[tokio::test]
async fn timeout_zero_is_rejected_without_starting_a_runtime() {
    let kernel = Kernel::new(Vec::new()).expect("empty mounts are valid");
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
    let kernel = Kernel::new(Vec::new()).expect("empty mounts are valid");
    let contract = kernel.contract();

    assert!(contract.contains("host.fs.list(dir)"));
    assert!(contract.contains("host.agent.answer(text)"));
}
