#[path = "../src/opencode_workspace.rs"]
mod opencode_workspace;

use std::fs;

use opencode_workspace::{ManagedWorkspace, bind, parse_check_request, require_scoped_commands};
use serde_json::json;
use uuid::Uuid;

#[test]
fn ready_managed_checkout_binds_but_commands_block_before_acceptance() {
    let root = tempfile::tempdir().unwrap();
    let checkout = root.path().join("checkout");
    fs::create_dir(&checkout).unwrap();
    fs::create_dir(checkout.join(".git")).unwrap();
    let checkout = fs::canonicalize(checkout).unwrap();
    let workspace = ManagedWorkspace {
        workspace_id: Uuid::new_v4(),
        repository_id: "github.com/owner/repo".into(),
        checkout: checkout.clone(),
    };
    let bound = bind(&checkout, Some(&workspace)).unwrap();
    assert_eq!(bound.cwd, checkout);
    let blocker = require_scoped_commands(&bound);
    assert_eq!(blocker.class, "execution_unavailable");
    assert_eq!(blocker.missing_capability, "scoped_command_execution");
    assert_eq!(blocker.workspace_id, Some(workspace.workspace_id));
    assert_eq!(
        blocker.repository_id.as_deref(),
        Some("github.com/owner/repo")
    );
}

#[test]
fn absent_mismatched_and_missing_checkout_are_distinct_pre_acceptance_blockers() {
    let root = tempfile::tempdir().unwrap();
    let checkout = fs::canonicalize(root.path()).unwrap();
    assert_eq!(bind(&checkout, None).unwrap_err().class, "workspace_absent");
    let workspace = ManagedWorkspace {
        workspace_id: Uuid::new_v4(),
        repository_id: "github.com/owner/repo".into(),
        checkout: checkout.clone(),
    };
    assert_eq!(
        bind(&checkout, Some(&workspace))
            .unwrap_err()
            .missing_capability,
        "working_checkout"
    );
    fs::create_dir(checkout.join(".git")).unwrap();
    let other = checkout.join("other");
    fs::create_dir(&other).unwrap();
    assert_eq!(
        bind(&other, Some(&workspace)).unwrap_err().class,
        "workspace_mismatch"
    );
}

#[cfg(unix)]
#[test]
fn symlink_alias_and_metadata_link_do_not_bind() {
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir().unwrap();
    let checkout = root.path().join("checkout");
    fs::create_dir(&checkout).unwrap();
    let checkout = fs::canonicalize(checkout).unwrap();
    let outside = root.path().join("outside");
    fs::create_dir(&outside).unwrap();
    symlink(&outside, checkout.join(".git")).unwrap();
    let workspace = ManagedWorkspace {
        workspace_id: Uuid::new_v4(),
        repository_id: "github.com/owner/repo".into(),
        checkout: checkout.clone(),
    };
    assert_eq!(
        bind(&checkout, Some(&workspace)).unwrap_err().class,
        "workspace_mismatch"
    );
    fs::remove_file(checkout.join(".git")).unwrap();
    fs::create_dir(checkout.join(".git")).unwrap();
    let alias = root.path().join("alias");
    symlink(&checkout, &alias).unwrap();
    assert_eq!(
        bind(&alias, Some(&workspace)).unwrap_err().class,
        "workspace_mismatch"
    );
}

#[test]
fn private_check_schema_rejects_every_caller_command_surface() {
    let id = Uuid::new_v4();
    for action in ["inspect", "build", "test", "lint"] {
        let request = parse_check_request(&json!({"action":action,"operation_id":id})).unwrap();
        assert_eq!(request.action.name(), action);
        assert!(request.action.task().contains("checkout"));
        assert!(!request.status);
        let status =
            parse_check_request(&json!({"action":"status","target":action,"operation_id":id}))
                .unwrap();
        assert_eq!(status.action, request.action);
        assert!(status.status);
    }
    for key in [
        "command",
        "argv",
        "executable",
        "env",
        "cwd",
        "path",
        "network_policy",
        "task",
    ] {
        let mut value = json!({"action":"test","operation_id":id});
        value[key] = json!("caller input");
        assert!(
            parse_check_request(&value).is_none(),
            "accepted forbidden {key}"
        );
    }
    for value in [
        json!({"action":"test","operation_id":id,"target":"build"}),
        json!({"action":"status","operation_id":id}),
        json!({"action":"status","target":"shell","operation_id":id}),
        json!({"action":"shell","operation_id":id}),
        json!({"action":"build","operation_id":"not-a-uuid"}),
    ] {
        assert!(parse_check_request(&value).is_none(), "accepted {value}");
    }
}
