use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tokio::process::Command;

use super::policy::SandboxSpec;

const MACOS_PATH_TO_SEATBELT_EXECUTABLE: &str = "/usr/bin/sandbox-exec";
const BASE_POLICY: &str = include_str!("macos_base_policy.sbpl");

pub(super) fn command(spec: &SandboxSpec, argv: &[String]) -> Result<Command> {
    anyhow::ensure!(!argv.is_empty(), "command must not be empty");
    let (write_policy, definitions) = build_write_policy(spec)?;
    let policy = format!(
        "{BASE_POLICY}\n; allow read-only file operations\n(allow file-read*)\n{write_policy}\n"
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
