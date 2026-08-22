use std::process::Command;

use serde_json::Value;

#[test]
fn linux_runtime_graph_is_registry_only_and_has_no_codex_packages() {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let output = Command::new(cargo)
        .args([
            "metadata",
            "--format-version",
            "1",
            "--locked",
            "--filter-platform",
            "x86_64-unknown-linux-gnu",
        ])
        .output()
        .expect("failed to run cargo metadata");
    assert!(
        output.status.success(),
        "cargo metadata failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let metadata: Value = serde_json::from_slice(&output.stdout).expect("invalid cargo metadata");
    let packages = metadata["packages"]
        .as_array()
        .expect("cargo metadata packages missing");
    let package_by_id = packages
        .iter()
        .map(|package| (package["id"].as_str().expect("package id missing"), package))
        .collect::<std::collections::HashMap<_, _>>();
    let root_id = metadata["resolve"]["root"]
        .as_str()
        .expect("cargo metadata root missing");
    let nodes = metadata["resolve"]["nodes"]
        .as_array()
        .expect("cargo metadata resolve nodes missing");

    for node in nodes {
        let id = node["id"].as_str().expect("resolve node id missing");
        let package = package_by_id
            .get(id)
            .unwrap_or_else(|| panic!("resolve node package missing: {id}"));
        let name = package["name"].as_str().expect("package name missing");
        assert!(
            name != "codex"
                && !name.starts_with("codex-")
                && !name.starts_with("unofficial-codex-"),
            "forbidden runtime package remains: {name}"
        );

        if id == root_id {
            continue;
        }
        let source = package["source"].as_str().unwrap_or_default();
        assert!(
            source.starts_with("registry+"),
            "non-registry runtime dependency remains: {name} ({source})"
        );
    }
}
