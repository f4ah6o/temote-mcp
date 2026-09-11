use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tokio::process::Command;

use super::policy::SandboxSpec;

const MACOS_PATH_TO_SEATBELT_EXECUTABLE: &str = "/usr/bin/sandbox-exec";
const BASE_POLICY: &str = include_str!("macos_base_policy.sbpl");

pub(super) fn command(spec: &SandboxSpec, argv: &[String]) -> Result<Command> {
    anyhow::ensure!(!argv.is_empty(), "command must not be empty");
    let (read_policy, mut definitions) = build_read_policy(spec)?;
    let (write_policy, write_definitions) = build_write_policy(spec)?;
    definitions.extend(write_definitions);
    let network_policy = if spec.network_access() {
        "(allow network-outbound)"
    } else {
        ""
    };
    let policy = format!(
        "{BASE_POLICY}\n; allow read-only file operations\n{read_policy}\n{network_policy}\n{write_policy}\n"
    );

    let mut args = vec!["-p".to_owned(), policy];
    for (key, value) in definitions {
        let value = value
            .to_str()
            .with_context(|| format!("Seatbelt path is not valid UTF-8: {}", value.display()))?;
        args.push(format!("-D{key}={value}"));
    }
    args.push("--".to_owned());
    args.extend(argv.iter().cloned());

    let mut command = Command::new(MACOS_PATH_TO_SEATBELT_EXECUTABLE);
    command.args(args);
    Ok(command)
}

fn build_read_policy(spec: &SandboxSpec) -> Result<(String, Vec<(String, PathBuf)>)> {
    if spec.hidden_roots().is_empty() {
        return Ok(("(allow file-read*)".to_owned(), Vec::new()));
    }

    let mut hidden_requirements = Vec::new();
    let mut definitions = Vec::new();
    for (index, root) in spec.hidden_roots().iter().enumerate() {
        ensure_utf8(root)?;
        let key = format!("HIDDEN_ROOT_{index}");
        definitions.push((key.clone(), root.clone()));
        hidden_requirements.push(format!("(require-not (subpath (param \"{key}\")))"));
    }
    let mut clauses = vec![format!(
        "(allow file-read* (require-all {}))",
        hidden_requirements.join(" ")
    )];

    let mut visible_roots = spec.writable_roots().to_vec();
    visible_roots.extend(spec.read_only_roots().iter().cloned());
    visible_roots.sort();
    visible_roots.dedup();
    for (index, root) in visible_roots.iter().enumerate() {
        if !spec
            .hidden_roots()
            .iter()
            .any(|hidden| root.starts_with(hidden))
        {
            continue;
        }
        ensure_utf8(root)?;
        let key = format!("VISIBLE_ROOT_{index}");
        definitions.push((key.clone(), root.clone()));
        clauses.push(format!("(allow file-read* (subpath (param \"{key}\")))"));
        // Seatbelt must be able to stat each directory on the way to a
        // re-exposed root, even though the containing hidden root remains
        // unreadable. Metadata access does not allow directory listing or
        // file contents, so this does not re-expose hidden siblings.
        clauses.push(format!(
            "(allow file-read-metadata (path-ancestors (param \"{key}\")))"
        ));
    }

    Ok((clauses.join("\n"), definitions))
}

fn build_write_policy(spec: &SandboxSpec) -> Result<(String, Vec<(String, PathBuf)>)> {
    let mut clauses = Vec::new();
    let mut definitions = Vec::new();

    for (root_index, root) in spec.writable_roots().iter().enumerate() {
        ensure_utf8(root)?;
        let root_key = format!("WRITABLE_ROOT_{root_index}");
        definitions.push((root_key.clone(), root.clone()));
        let mut requirements = vec![format!("(subpath (param \"{root_key}\"))")];

        let mut excluded = spec.protected_metadata_paths(root);
        for protected_root in spec.writable_roots() {
            excluded.extend(
                spec.protected_metadata_paths(protected_root)
                    .into_iter()
                    .filter(|path| path.starts_with(root) && path != root),
            );
        }
        excluded.extend(
            spec.read_only_overrides()
                .iter()
                .filter(|path| path.starts_with(root))
                .cloned(),
        );
        excluded.sort();
        excluded.dedup();

        for (excluded_index, path) in excluded.into_iter().enumerate() {
            ensure_utf8(&path)?;
            let key = format!("WRITABLE_ROOT_{root_index}_EXCLUDED_{excluded_index}");
            definitions.push((key.clone(), path));
            requirements.push(format!("(require-not (literal (param \"{key}\")))"));
            requirements.push(format!("(require-not (subpath (param \"{key}\")))"));
        }
        clauses.push(format!("(require-all {})", requirements.join(" ")));
    }

    if clauses.is_empty() {
        Ok((String::new(), definitions))
    } else {
        Ok((
            format!("(allow file-write*\n{}\n)", clauses.join(" ")),
            definitions,
        ))
    }
}

fn ensure_utf8(path: &Path) -> Result<()> {
    anyhow::ensure!(
        path.to_str().is_some(),
        "Seatbelt path is not valid UTF-8: {}",
        path.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support;

    #[test]
    fn generated_write_policy_parameterizes_every_writable_root_and_protection()
    -> noprop::TestResult {
        let fixture = tempfile::tempdir().unwrap();
        let workspace = fixture.path().join("workspace");
        let outer = fixture.path().join("outer");
        let nested = outer.join("nested");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&nested).unwrap();
        let candidates = [
            std::fs::canonicalize(&outer).unwrap(),
            std::fs::canonicalize(&nested).unwrap(),
        ];

        test_support::run(0x4d41_434f_5350_4254, 512, |ctx| {
            let requested = candidates
                .iter()
                .filter(|_| noprop::sample_bool(ctx))
                .cloned()
                .collect::<Vec<_>>();
            let spec = SandboxSpec::command(&workspace, &requested).unwrap();
            let (policy, definitions) = build_write_policy(&spec).unwrap();

            let defined_paths = definitions
                .iter()
                .map(|(_, path)| path.clone())
                .collect::<std::collections::BTreeSet<_>>();
            let defined_keys = definitions
                .iter()
                .map(|(key, _)| key.as_str())
                .collect::<std::collections::BTreeSet<_>>();
            assert_eq!(
                defined_keys.len(),
                definitions.len(),
                "duplicate Seatbelt parameter key"
            );

            for root in spec.writable_roots() {
                assert!(
                    defined_paths.contains(root),
                    "missing writable root parameter: {root:?}"
                );
                assert!(
                    !policy.contains(root.to_string_lossy().as_ref()),
                    "raw writable root leaked into Seatbelt policy: {root:?}"
                );
                for protected in spec.protected_metadata_paths(root) {
                    assert!(
                        defined_paths.contains(&protected),
                        "missing protected metadata exclusion: {protected:?}"
                    );
                    assert!(
                        !policy.contains(protected.to_string_lossy().as_ref()),
                        "raw protected path leaked into Seatbelt policy: {protected:?}"
                    );
                }
            }
            Ok(())
        })
    }

    #[test]
    fn generated_write_policy_protects_nested_metadata() {
        let fixture = tempfile::tempdir().unwrap();
        let workspace = fixture.path().join("workspace");
        std::fs::create_dir_all(workspace.join("nested/.git")).unwrap();
        std::fs::create_dir_all(workspace.join("nested/.agents")).unwrap();
        std::fs::create_dir_all(workspace.join("nested/deep")).unwrap();
        std::fs::write(workspace.join("nested/deep/.codex"), b"metadata").unwrap();
        std::fs::create_dir_all(workspace.join("ordinary")).unwrap();
        std::fs::write(workspace.join("ordinary/.git"), b"gitdir: linked").unwrap();

        let workspace = std::fs::canonicalize(workspace).unwrap();
        let spec = SandboxSpec::local_agent(
            &workspace,
            std::slice::from_ref(&workspace),
            &[],
            &[],
            &[],
            &[],
        )
        .unwrap();
        let (policy, definitions) = build_write_policy(&spec).unwrap();
        let defined_paths = definitions
            .iter()
            .map(|(_, path)| path.clone())
            .collect::<std::collections::BTreeSet<_>>();

        for expected in [
            workspace.join("nested/.git"),
            workspace.join("nested/.agents"),
            workspace.join("nested/deep/.codex"),
            workspace.join("ordinary/.git"),
        ] {
            assert!(
                defined_paths.contains(&expected),
                "missing nested protected path {expected:?}"
            );
            assert!(
                !policy.contains(expected.to_string_lossy().as_ref()),
                "raw nested protected path leaked into Seatbelt policy: {expected:?}"
            );
        }
    }

    #[test]
    fn generated_read_policy_allows_only_selected_hidden_root_ancestors() {
        let fixture = tempfile::tempdir().unwrap();
        let hidden = fixture.path().join("hidden");
        let selected = hidden.join("repo/crate");
        std::fs::create_dir_all(&selected).unwrap();

        let spec = SandboxSpec::local_agent(
            &selected,
            std::slice::from_ref(&selected),
            &[],
            &[],
            &[],
            std::slice::from_ref(&hidden),
        )
        .unwrap();
        let (policy, definitions) = build_read_policy(&spec).unwrap();

        assert!(definitions.iter().any(|(_, path)| path == &hidden));
        assert!(definitions.iter().any(|(_, path)| path == &selected));
        assert!(policy.contains("(allow file-read-metadata (path-ancestors"));
        assert!(policy.contains("(allow file-read* (subpath"));
    }

    #[test]
    fn generated_git_policy_parameterizes_every_read_only_override() -> noprop::TestResult {
        let fixture = tempfile::tempdir().unwrap();
        let workspace = fixture.path().join("workspace");
        let git = workspace.join(".git");
        let broader = fixture.path().to_path_buf();
        std::fs::create_dir_all(&git).unwrap();

        test_support::run(0x4d41_434f_5347_4954, 512, |ctx| {
            let writable_roots = if noprop::sample_bool(ctx) {
                vec![broader.clone()]
            } else {
                vec![workspace.clone()]
            };
            let spec =
                SandboxSpec::git(&workspace, &writable_roots, std::slice::from_ref(&git)).unwrap();
            let (policy, definitions) = build_write_policy(&spec).unwrap();
            let defined_paths = definitions
                .iter()
                .map(|(_, path)| path.clone())
                .collect::<std::collections::BTreeSet<_>>();

            for path in spec.read_only_overrides() {
                assert!(
                    defined_paths.contains(path),
                    "missing read-only Git metadata exclusion: {path:?}"
                );
                assert!(
                    !policy.contains(path.to_string_lossy().as_ref()),
                    "raw read-only path leaked into Seatbelt policy: {path:?}"
                );
            }
            Ok(())
        })
    }

    #[test]
    fn generated_command_argv_is_preserved_after_seatbelt_separator() -> noprop::TestResult {
        let fixture = tempfile::tempdir().unwrap();
        let workspace = fixture.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let spec = SandboxSpec::command(&workspace, &[]).unwrap();

        test_support::run(0x4d41_434f_5341_5247, 512, |ctx| {
            let count = noprop::sample_usize_in(ctx, 1..=8);
            let argv = (0..count)
                .map(|_| {
                    test_support::ascii_string(ctx, 32)
                        .chars()
                        .filter(|character| *character != '\0')
                        .collect::<String>()
                })
                .collect::<Vec<_>>();
            let process = command(&spec, &argv).unwrap();
            let args = process
                .as_std()
                .get_args()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect::<Vec<_>>();
            let separator = args
                .iter()
                .position(|arg| arg == "--")
                .expect("Seatbelt separator");
            assert_eq!(&args[separator + 1..], argv.as_slice());
            Ok(())
        })
    }

    #[test]
    fn profile_has_no_network_allowance_and_uses_parameterized_paths() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let spec = SandboxSpec::command(&workspace, &[]).unwrap();
        let (policy, definitions) = build_write_policy(&spec).unwrap();

        assert!(policy.contains("(param \"WRITABLE_ROOT_"));
        assert!(!policy.contains(workspace.to_string_lossy().as_ref()));
        assert!(
            definitions
                .iter()
                .any(|(_, path)| path == &std::fs::canonicalize(&workspace).unwrap())
        );
        assert!(!BASE_POLICY.contains("allow network-outbound"));
        assert!(!BASE_POLICY.contains("allow network-inbound"));
    }
}
