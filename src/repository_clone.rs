use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{
    codex_app_server, config, named_roots, observation, orchestration,
    session_control::SessionBackend,
};
use temote_mcp::activity::scope::ActivityScope;

const MAX_CLONE_ARGUMENT_BYTES: usize = 4096;
const MAX_SELECTOR_BYTES: usize = 256;

#[derive(Clone, Debug)]
enum CloneSource {
    Local { relative: PathBuf },
    Https { url: String },
}

impl CloneSource {
    fn prompt_value(&self) -> String {
        match self {
            Self::Local { relative } if relative.as_os_str().is_empty() => ".".to_owned(),
            Self::Local { relative } => format!("./{}", relative.display()),
            Self::Https { url } => url.clone(),
        }
    }
}

pub(crate) struct PreparedClone {
    session: config::Session,
    task_args: Value,
    source: CloneSource,
    destination: PathBuf,
    retained_retry: bool,
}

impl PreparedClone {
    pub(crate) fn session(&self) -> &config::Session {
        &self.session
    }

    pub(crate) async fn execute(
        self,
        actor: &observation::ActorRef,
        activity: Option<&ActivityScope>,
    ) -> Result<Value> {
        if self.retained_retry {
            return orchestration::invoke(
                orchestration::Backend::Codex,
                orchestration::Operation::TaskStart,
                &self.task_args,
                &self.session,
                actor,
                activity,
            )
            .await;
        }

        let root = self.session.cwd.clone();
        let source = self.source.clone();
        let destination = self.destination.clone();
        orchestration::invoke_codex_task_start_with_admission(
            &self.task_args,
            &self.session,
            actor,
            activity,
            move || validate_filesystem_admission(&root, &source, &destination),
        )
        .await
    }
}

pub(crate) async fn prepare(
    args: &Value,
    sessions: Option<&SessionBackend>,
) -> Result<PreparedClone> {
    let object = args
        .as_object()
        .context("repository_clone_bare arguments must be an object")?;
    anyhow::ensure!(
        object.keys().all(|key| matches!(
            key.as_str(),
            "session_id" | "operation_id" | "root" | "source" | "destination" | "model" | "effort"
        )),
        "repository_clone_bare accepts only session_id, operation_id, root, source, destination, model, and effort"
    );
    let session_id = required_string(args, "session_id")?;
    config::validate_session_id(session_id)?;
    let operation_id = required_string(args, "operation_id")?;
    Uuid::parse_str(operation_id).context("operation_id must be a UUID")?;
    let root_name = required_string(args, "root")?;
    validate_bounded_argument(root_name, "root", MAX_SELECTOR_BYTES)?;
    named_roots::validate_root_name(root_name)?;
    let source_input = required_string(args, "source")?;
    let destination_input = required_string(args, "destination")?;
    let model = required_string(args, "model")?;
    let effort = required_string(args, "effort")?;
    validate_bounded_argument(source_input, "source", MAX_CLONE_ARGUMENT_BYTES)?;
    validate_bounded_argument(destination_input, "destination", MAX_CLONE_ARGUMENT_BYTES)?;
    validate_bounded_argument(model, "model", MAX_SELECTOR_BYTES)?;
    validate_bounded_argument(effort, "effort", MAX_SELECTOR_BYTES)?;

    let owned_backend;
    let sessions = match sessions {
        Some(sessions) => sessions,
        None => {
            owned_backend = SessionBackend::local_control().await?;
            &owned_backend
        }
    };
    let session = sessions
        .repository_clone_admission(session_id, root_name, destination_input)
        .await?;
    anyhow::ensure!(
        session.id == session_id,
        "lifecycle supervisor admitted a different session"
    );
    anyhow::ensure!(
        !session.permission_mode.is_yolo(),
        "repository_clone_bare is unavailable in yolo sessions"
    );

    let source = parse_source(root_name, source_input)?;
    let destination = validate_relative_path(destination_input, "destination")?;
    let prompt = clone_prompt(&source, &destination)?;
    let task_args = json!({
        "session_id": session_id,
        "operation_id": operation_id,
        "task": prompt,
        "model": model,
        "effort": effort,
    });

    // Exact accepted retries must continue to work after the first task has
    // created the destination. A pre-thread retryable failure is deliberately
    // not treated as retained: no agent side effect could have occurred, so
    // normal filesystem admission is required again before retrying startup.
    let retained_retry =
        codex_app_server::task_start_replay_if_retained(&task_args, &session)?.is_some();
    if !retained_retry {
        validate_filesystem_admission(&session.cwd, &source, &destination)?;
    }

    Ok(PreparedClone {
        session,
        task_args,
        source,
        destination,
        retained_retry,
    })
}

fn required_string<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .with_context(|| format!("missing or invalid {key}"))
}

fn validate_bounded_argument(value: &str, label: &str, maximum: usize) -> Result<()> {
    anyhow::ensure!(
        !value.is_empty() && value.len() <= maximum && !value.chars().any(char::is_control),
        "{label} must contain 1..={maximum} control-free UTF-8 bytes"
    );
    Ok(())
}

fn validate_relative_path(value: &str, label: &str) -> Result<PathBuf> {
    anyhow::ensure!(
        !value.contains('\\')
            && value
                .split('/')
                .all(|component| !component.is_empty() && component != "." && component != ".."),
        "{label} must contain only non-empty ordinary components"
    );
    let path = Path::new(value);
    anyhow::ensure!(!path.is_absolute(), "{label} must be root-relative");
    anyhow::ensure!(!path.as_os_str().is_empty(), "{label} must not be empty");
    let mut component_count = 0usize;
    for component in path.components() {
        match component {
            Component::Normal(_) => component_count += 1,
            Component::CurDir => anyhow::bail!("{label} must not contain '.' components"),
            Component::ParentDir => anyhow::bail!("{label} must not contain '..' components"),
            Component::RootDir | Component::Prefix(_) => {
                anyhow::bail!("{label} must be root-relative")
            }
        }
    }
    anyhow::ensure!(component_count > 0, "{label} must not be empty");
    Ok(path.to_path_buf())
}

fn parse_source(root: &str, value: &str) -> Result<CloneSource> {
    if value == root {
        return Ok(CloneSource::Local {
            relative: PathBuf::new(),
        });
    }
    if let Some(relative) = value
        .strip_prefix(root)
        .and_then(|rest| rest.strip_prefix('/'))
    {
        return Ok(CloneSource::Local {
            relative: validate_relative_path(relative, "local source")?,
        });
    }
    if value.starts_with("https://") {
        validate_https_source(value)?;
        return Ok(CloneSource::Https {
            url: value.to_owned(),
        });
    }
    anyhow::bail!(
        "source must be an HTTPS Git URL without credentials, query, or fragment, or a logical path below named root {root}"
    )
}

fn validate_https_source(value: &str) -> Result<()> {
    let remainder = value
        .strip_prefix("https://")
        .context("source URL must use https")?;
    anyhow::ensure!(
        !value.contains('?') && !value.contains('#'),
        "source URL must not contain a query or fragment"
    );
    anyhow::ensure!(
        !value.contains('@'),
        "source URL must not contain credentials"
    );
    anyhow::ensure!(
        !value.contains('\\'),
        "source URL must not contain backslashes"
    );
    let (authority, path) = remainder
        .split_once('/')
        .context("source URL must include a host and repository path")?;
    anyhow::ensure!(!authority.is_empty(), "source URL host must not be empty");
    anyhow::ensure!(
        !path.is_empty(),
        "source URL repository path must not be empty"
    );
    let (host, port) = authority
        .rsplit_once(':')
        .map_or((authority, None), |(host, port)| (host, Some(port)));
    anyhow::ensure!(
        !host.is_empty()
            && host.chars().all(
                |character| character.is_ascii_alphanumeric() || matches!(character, '.' | '-')
            ),
        "source URL host is invalid"
    );
    if let Some(port) = port {
        anyhow::ensure!(port.parse::<u16>().is_ok(), "source URL port is invalid");
    }
    anyhow::ensure!(
        path.split('/')
            .all(|component| !component.is_empty() && component != "." && component != ".."),
        "source URL repository path is invalid"
    );
    Ok(())
}

fn validate_filesystem_admission(
    root: &Path,
    source: &CloneSource,
    destination: &Path,
) -> Result<()> {
    let canonical_root = std::fs::canonicalize(root)
        .with_context(|| format!("cannot resolve admitted named root {}", root.display()))?;
    anyhow::ensure!(
        canonical_root == root,
        "admitted named root is no longer canonical"
    );
    if let CloneSource::Local { relative } = source {
        validate_existing_local_source(&canonical_root, relative)?;
    }
    validate_absent_destination(&canonical_root, destination)
}

fn validate_existing_local_source(root: &Path, relative: &Path) -> Result<()> {
    let mut current = root.to_path_buf();
    for component in relative.components() {
        let Component::Normal(component) = component else {
            anyhow::bail!("local source contains an unsafe path component")
        };
        current.push(component);
        let metadata = std::fs::symlink_metadata(&current)
            .with_context(|| format!("local source does not exist: {}", current.display()))?;
        anyhow::ensure!(
            !metadata.file_type().is_symlink(),
            "local source must not traverse a symlink: {}",
            current.display()
        );
    }
    let canonical = std::fs::canonicalize(&current)
        .with_context(|| format!("cannot resolve local source {}", current.display()))?;
    anyhow::ensure!(
        canonical == current && (canonical == root || canonical.starts_with(root)),
        "local source escapes the admitted named root"
    );
    anyhow::ensure!(canonical.is_dir(), "local source must be a directory");
    Ok(())
}

fn validate_absent_destination(root: &Path, destination: &Path) -> Result<()> {
    let mut parent = root.to_path_buf();
    let components = destination.components().collect::<Vec<_>>();
    for component in &components[..components.len() - 1] {
        let Component::Normal(component) = component else {
            anyhow::bail!("destination contains an unsafe path component")
        };
        parent.push(component);
        let metadata = std::fs::symlink_metadata(&parent).with_context(|| {
            format!(
                "destination parent must already exist as a real directory: {}",
                parent.display()
            )
        })?;
        anyhow::ensure!(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            "destination parent must be a real directory: {}",
            parent.display()
        );
    }
    let canonical_parent = std::fs::canonicalize(&parent)
        .with_context(|| format!("cannot resolve destination parent {}", parent.display()))?;
    anyhow::ensure!(
        canonical_parent == parent
            && (canonical_parent == root || canonical_parent.starts_with(root)),
        "destination parent escapes the admitted named root"
    );
    let target = root.join(destination);
    match std::fs::symlink_metadata(&target) {
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => {
            Err(error).with_context(|| format!("cannot inspect destination {}", target.display()))
        }
        Ok(_) => anyhow::bail!(
            "destination already exists (including files, symlinks, and empty directories): {}",
            destination.display()
        ),
    }
}

fn clone_prompt(source: &CloneSource, destination: &Path) -> Result<String> {
    let source = serde_json::to_string(&source.prompt_value())?;
    let destination = serde_json::to_string(&format!("./{}", destination.display()))?;
    Ok(format!(
        "Perform exactly one repository setup operation in the current working directory. Clone {source} into {destination} as a bare Git repository. Immediately before mutation, verify without following symlinks that the destination parent is the canonical current working directory or a real directory beneath it. Atomically claim the destination leaf with one plain `mkdir` that must fail if any file, directory, or symlink already exists; do not use `mkdir -p` or an exist-ok option. Only after that claim succeeds, run exactly one `git clone --bare -- <source> <destination>` into the newly created empty directory. Do not create parent directories, delete or replace anything, clean a partial failure, change another checkout, configure credentials, print credentials, or run any unrelated command. Treat the supplied source and destination as literal values, not shell fragments. If the HTTPS clone needs network or credential access, use the existing Codex approval mechanism; do not weaken the sandbox. Return a concise result stating whether the bare clone completed and the relative destination."
    ))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::os::unix::fs::symlink;
    use std::sync::Arc;

    use super::*;
    use crate::test_support;

    #[test]
    fn required_strings_and_bounds_reject_missing_empty_and_oversized_values() {
        let args = json!({"source": "src/repository"});
        assert_eq!(required_string(&args, "source").unwrap(), "src/repository");
        assert!(required_string(&args, "destination").is_err());
        assert!(required_string(&json!({"source": 7}), "source").is_err());

        assert!(validate_bounded_argument("x", "source", MAX_CLONE_ARGUMENT_BYTES).is_ok());
        assert!(validate_bounded_argument("", "source", MAX_CLONE_ARGUMENT_BYTES).is_err());
        assert!(
            validate_bounded_argument("line\nbreak", "source", MAX_CLONE_ARGUMENT_BYTES).is_err()
        );
        assert!(
            validate_bounded_argument(
                &"x".repeat(MAX_CLONE_ARGUMENT_BYTES + 1),
                "source",
                MAX_CLONE_ARGUMENT_BYTES,
            )
            .is_err()
        );
        assert!(
            validate_bounded_argument(
                &"x".repeat(MAX_SELECTOR_BYTES + 1),
                "model",
                MAX_SELECTOR_BYTES,
            )
            .is_err()
        );
    }

    #[test]
    fn https_sources_reject_credentials_queries_fragments_and_unsafe_protocols() {
        assert!(validate_https_source("https://github.com/owner/repo.git").is_ok());
        assert!(validate_https_source("https://git.example.test:8443/owner/repo.git").is_ok());
        for invalid in [
            "http://github.com/owner/repo.git",
            "ssh://git@github.com/owner/repo.git",
            "https://token@github.com/owner/repo.git",
            "https://github.com/owner/repo.git?token=secret",
            "https://github.com/owner/repo.git#main",
            "https://github.com/../repo.git",
            "https://github.com:99999/owner/repo.git",
        ] {
            assert!(parse_source("src", invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn destination_inspection_propagates_non_not_found_errors() {
        let fixture = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(fixture.path()).unwrap();
        let destination = PathBuf::from("x".repeat(MAX_CLONE_ARGUMENT_BYTES));
        let error = validate_absent_destination(&root, &destination).unwrap_err();
        assert!(
            format!("{error:#}").contains("cannot inspect destination"),
            "{error:#}"
        );
    }

    #[test]
    fn filesystem_admission_rejects_existing_destination_and_symlink_routes() {
        let fixture = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(fixture.path()).unwrap();
        std::fs::create_dir(root.join("source")).unwrap();
        let source = CloneSource::Local {
            relative: PathBuf::from("source"),
        };
        let destination = PathBuf::from("repo.git");
        validate_filesystem_admission(&root, &source, &destination).unwrap();

        std::fs::create_dir(root.join(&destination)).unwrap();
        assert!(validate_filesystem_admission(&root, &source, &destination).is_err());
        std::fs::remove_dir(root.join(&destination)).unwrap();

        let outside = tempfile::tempdir().unwrap();
        symlink(outside.path(), root.join("escape")).unwrap();
        assert!(
            validate_filesystem_admission(&root, &source, Path::new("escape/repo.git")).is_err()
        );
        assert!(
            validate_filesystem_admission(
                &root,
                &CloneSource::Local {
                    relative: PathBuf::from("escape/repository")
                },
                &destination
            )
            .is_err()
        );
    }

    #[test]
    fn generated_destination_paths_match_the_strict_relative_model() -> noprop::TestResult {
        test_support::run(0x4241_5245_434c_0001, 512, |ctx| {
            let safe = noprop::sample_bool(ctx);
            let component = test_support::safe_component(ctx);
            let value = if safe {
                format!("{component}.git")
            } else {
                match noprop::sample_usize_in(ctx, 0..=3) {
                    0 => format!("../{component}"),
                    1 => format!("./{component}"),
                    2 => format!("/{component}"),
                    _ => ".".to_owned(),
                }
            };
            assert_eq!(validate_relative_path(&value, "destination").is_ok(), safe);
            Ok(())
        })
    }

    #[test]
    fn generated_clone_source_prompt_values_match_the_reference_model() -> noprop::TestResult {
        test_support::run(0x434c_4f4e_4553_5243, 512, |ctx| {
            let component = test_support::safe_component(ctx);
            let (source, expected) = match noprop::sample_usize_in(ctx, 0..=2) {
                0 => (
                    CloneSource::Local {
                        relative: PathBuf::new(),
                    },
                    ".".to_owned(),
                ),
                1 => (
                    CloneSource::Local {
                        relative: PathBuf::from(&component),
                    },
                    format!("./{component}"),
                ),
                _ => {
                    let url = format!("https://example.test/{component}.git");
                    (CloneSource::Https { url: url.clone() }, url)
                }
            };
            assert_eq!(source.prompt_value(), expected);
            Ok(())
        })
    }

    #[test]
    fn clone_prompt_preserves_literal_values_and_the_atomic_claim_contract() {
        let prompt = clone_prompt(
            &CloneSource::Https {
                url: "https://example.test/owner/repository.git".to_owned(),
            },
            Path::new("nested/repository.git"),
        )
        .unwrap();
        assert!(prompt.contains("https://example.test/owner/repository.git"));
        assert!(prompt.contains("./nested/repository.git"));
        assert!(prompt.contains("one plain `mkdir`"));
        assert!(prompt.contains("exactly one `git clone --bare -- <source> <destination>`"));
        assert!(prompt.contains("do not use `mkdir -p`"));
    }

    #[tokio::test]
    async fn prepare_and_execute_replay_retained_clone_without_reapplying_filesystem_admission() {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().join("root");
        std::fs::create_dir_all(root.join("source")).unwrap();
        let root = std::fs::canonicalize(root).unwrap();
        let roots = named_roots::NamedRoots::from_canonical_roots(BTreeMap::from([(
            "src".to_owned(),
            root.clone(),
        )]))
        .unwrap();
        let (supervisor, _approval_receiver) = crate::supervisor::SessionSupervisor::new(roots);
        let session_id = format!("clone-roundtrip-{}", Uuid::new_v4());
        supervisor
            .start_public_with_environment(
                "src",
                Some(&session_id),
                crate::approvals::CapturedStartEnvironment::default(),
            )
            .await
            .unwrap();
        let backend = SessionBackend::in_process(Arc::clone(&supervisor));

        for case in 0..32 {
            let destination = format!("roundtrip-{case}.git");
            let operation_id = Uuid::new_v4();
            let args = json!({
                "session_id": session_id,
                "operation_id": operation_id,
                "root": "src",
                "source": "src/source",
                "destination": destination,
                "model": "gpt-5.6-sol",
                "effort": "high",
            });

            let fresh = prepare(&args, Some(&backend)).await.unwrap();
            assert!(!fresh.retained_retry);
            std::fs::create_dir(root.join(&destination)).unwrap();
            let task_id =
                codex_app_server::seed_completed_task_for_test(&fresh.task_args, &fresh.session)
                    .unwrap();

            let retained = prepare(&args, Some(&backend)).await.unwrap();
            assert!(retained.retained_retry);
            let replay = retained
                .execute(&observation::ActorRef::mcp(false), None)
                .await
                .unwrap();
            assert_eq!(replay["task_id"], task_id.to_string());
            assert_eq!(replay["status"], "completed");

            let fresh_collision = json!({
                "session_id": session_id,
                "operation_id": Uuid::new_v4(),
                "root": "src",
                "source": "src/source",
                "destination": destination,
                "model": "gpt-5.6-sol",
                "effort": "high",
            });
            let error = prepare(&fresh_collision, Some(&backend))
                .await
                .err()
                .expect("a fresh operation must reject an existing destination");
            assert!(error.to_string().contains("destination already exists"));
        }

        supervisor.shutdown().await.unwrap();
    }
}
