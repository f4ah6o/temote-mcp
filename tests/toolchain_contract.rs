use std::fs;
use std::path::PathBuf;

fn repository_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn read_repository_file(path: &str) -> String {
    let full_path = repository_root().join(path);
    fs::read_to_string(&full_path)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", full_path.display()))
}

fn toolchain_channel(toolchain: &str) -> String {
    toolchain
        .lines()
        .map(str::trim)
        .find_map(|line| line.strip_prefix("channel"))
        .and_then(|line| line.split('=').nth(1))
        .map(|value| value.trim().trim_matches('"').to_string())
        .expect("rust-toolchain.toml must declare a channel")
}

#[test]
fn rust_toolchain_file_pins_an_exact_channel_with_lint_components() {
    let toolchain = read_repository_file("rust-toolchain.toml");
    let channel = toolchain_channel(&toolchain);

    let segments = channel.split('.').collect::<Vec<_>>();
    assert_eq!(
        segments.len(),
        3,
        "rust-toolchain.toml channel must be an exact X.Y.Z release, got {channel}"
    );
    assert!(
        segments.iter().all(|segment| {
            !segment.is_empty() && segment.chars().all(|character| character.is_ascii_digit())
        }),
        "rust-toolchain.toml channel must be numeric, got {channel}"
    );
    assert!(
        toolchain.contains("rustfmt") && toolchain.contains("clippy"),
        "rust-toolchain.toml must install rustfmt and clippy for the shared gate"
    );
}

#[test]
fn ci_and_release_setup_consume_the_repository_toolchain() {
    for workflow in ["ci.yaml", "release.yaml"] {
        let text = read_repository_file(&format!(".github/workflows/{workflow}"));
        assert!(
            text.contains("rustup show"),
            "{workflow} must set up Rust from the repository-managed rust-toolchain.toml"
        );
        assert!(
            !text.contains("dtolnay/rust-toolchain@"),
            "{workflow} must not use a floating toolchain action"
        );
    }

    let generated = read_repository_file(".github/workflows/release.yml");
    assert!(
        !generated.contains("dtolnay/rust-toolchain@"),
        "generated release.yml must not pin a toolchain that disagrees with rust-toolchain.toml"
    );
}

#[test]
fn development_docs_record_the_contract_and_motivating_regression() {
    let docs = read_repository_file("docs/development.md");
    assert!(
        docs.contains("rust-toolchain.toml"),
        "development docs must point at the repository-managed toolchain contract"
    );
    assert!(
        docs.contains("rustup show"),
        "development docs must describe how CI consumes the toolchain contract"
    );
    assert!(
        docs.contains("2026.9.10"),
        "development docs must record the motivating Clippy drift regression"
    );
}
