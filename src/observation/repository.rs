//! Stable, path-free repository identities for cloud observation replication.
//!
//! A repository is eligible for repository-scoped cloud context only when its
//! canonical Git metadata has one unambiguous `remote.origin.url` on a known
//! forge. Local paths and repository directory names are never identities.

use std::fs::OpenOptions;
use std::io::Read;
use std::path::Path;

use crate::sandbox::{WorkspaceRepositoryIdentity, git_worktree_root};

const MAX_GIT_CONFIG_BYTES: u64 = 1024 * 1024;
const MAX_REPOSITORY_KEY_BYTES: usize = 512;
const MAX_REPOSITORY_PATH_SEGMENTS: usize = 64;

/// Resolves a forge key from owner-local Git metadata. Any unsupported or
/// ambiguous configuration fails closed to `None`; errors and URLs are never
/// logged because remote URLs can contain credentials.
pub(crate) fn repository_key_for_workspace(path: &Path) -> Option<String> {
    let root = git_worktree_root(path).ok()?;
    let identity = WorkspaceRepositoryIdentity::for_workspace(&root).ok()?;
    let config_path = identity.common_dir.join("config");
    let config = read_git_config(&config_path).ok()?;
    let origin = origin_url(&config)?;
    canonical_forge_key(&origin)
}

fn read_git_config(path: &Path) -> std::io::Result<String> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options.open(path)?;
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file() || metadata.len() > MAX_GIT_CONFIG_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "unsupported Git config file",
        ));
    }
    let mut bytes = Vec::with_capacity(metadata.len().min(MAX_GIT_CONFIG_BYTES) as usize);
    Read::by_ref(&mut file)
        .take(MAX_GIT_CONFIG_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_GIT_CONFIG_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "Git config file exceeds the size limit",
        ));
    }
    String::from_utf8(bytes).map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, "Git config is not UTF-8")
    })
}

/// Reads only the simple, explicit `remote "origin"` URL form. Git config
/// includes and URL rewrites can change the effective remote, so those forms
/// are deliberately not interpreted here.
fn origin_url(config: &str) -> Option<String> {
    let mut section = String::new();
    let mut origin_url: Option<String> = None;
    for raw_line in config.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if line.starts_with('[') {
            let lower = line.to_ascii_lowercase();
            if lower.starts_with("[include") || lower.starts_with("[url ") {
                return None;
            }
            let header = line.strip_prefix('[')?.strip_suffix(']')?.trim();
            if let Some((name, subsection)) = header.split_once(' ')
                && name.eq_ignore_ascii_case("remote")
                && subsection.starts_with('"')
                && subsection.ends_with('"')
                && &subsection[1..subsection.len() - 1] == "origin"
            {
                section = "origin".to_owned();
            } else if header.eq_ignore_ascii_case("extensions") {
                section = "extensions".to_owned();
            } else {
                section = "other".to_owned();
            }
            continue;
        }
        let (key, value) = line
            .split_once('=')
            .map_or((line, ""), |(key, value)| (key, value));
        let key = key.trim().to_ascii_lowercase();
        if key == "insteadof" {
            return None;
        }
        let boolean_value = value
            .split(['#', ';'])
            .next()
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        if section == "extensions"
            && key == "worktreeconfig"
            && !matches!(boolean_value.as_str(), "false" | "no" | "off" | "0")
        {
            // Per-worktree config can override `remote.origin.url`. Resolving
            // it requires a separate trusted merge implementation, so fail
            // closed instead of assigning this worktree to the common URL.
            return None;
        }
        if section != "origin" || key != "url" {
            continue;
        }
        let value = parse_simple_git_config_value(value.trim())?;
        if origin_url.replace(value).is_some() {
            return None;
        }
    }
    origin_url
}

fn parse_simple_git_config_value(value: &str) -> Option<String> {
    if value.is_empty() || value.contains(['\n', '\r', '\0']) {
        return None;
    }
    let value = if let Some(inner) = value.strip_prefix('"') {
        let inner = inner.strip_suffix('"')?;
        // Git's C-style escaping has too many identity-affecting cases to
        // interpret here. Ordinary forge URLs need no escaped characters.
        if inner.contains(['"', '\\']) {
            return None;
        }
        inner
    } else {
        if value.chars().any(char::is_whitespace) || value.contains(['#', ';', '"', '\\']) {
            return None;
        }
        value
    };
    Some(value.to_owned())
}

/// Canonicalizes supported HTTPS and SSH remote forms without retaining
/// credentials, query strings, ports, or arbitrary path syntax.
fn canonical_forge_key(remote: &str) -> Option<String> {
    let (host, path) = if let Some(rest) = remote.strip_prefix("git@") {
        let (host, path) = rest.split_once(':')?;
        if host.is_empty() || host.contains(['@', '/', ':']) {
            return None;
        }
        (host.to_ascii_lowercase(), path.to_owned())
    } else {
        let (scheme, after_scheme) = remote.split_once("://")?;
        if !matches!(scheme, "https" | "ssh") || remote.contains(['%', '?', '#', '\\', '\0']) {
            return None;
        }
        let (authority, raw_path) = after_scheme.split_once('/')?;
        if authority.is_empty() || authority.contains(['/', '?', '#', '%', '\\', '\0']) {
            return None;
        }
        if raw_path
            .split('/')
            .any(|segment| matches!(segment, "." | ".."))
        {
            return None;
        }
        let host = match scheme {
            "https" if !authority.contains(['@', ':']) => authority,
            "ssh" => authority.strip_prefix("git@")?,
            _ => return None,
        };
        if host.is_empty() || host.contains(['@', '/', ':', '[', ']']) {
            return None;
        }
        (host.to_ascii_lowercase(), raw_path.to_owned())
    };

    if path.is_empty() || path.starts_with('/') || path.ends_with('/') || path.contains('%') {
        return None;
    }
    let mut segments = path
        .split('/')
        .map(str::to_ascii_lowercase)
        .collect::<Vec<_>>();
    let final_segment = segments.last_mut()?;
    if final_segment.ends_with(".git") {
        final_segment.truncate(final_segment.len() - 4);
    }
    if segments.iter().any(|segment| {
        segment.is_empty()
            || segment == "."
            || segment == ".."
            || !segment.chars().all(|character| {
                character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
            })
    }) {
        return None;
    }

    let (forge, min_segments, max_segments) = match host.as_str() {
        "github.com" => ("github", 2, 2),
        "gitlab.com" => ("gitlab", 2, usize::MAX),
        "bitbucket.org" => ("bitbucket", 2, 2),
        _ => return None,
    };
    if segments.len() < min_segments || segments.len() > max_segments {
        return None;
    }
    if segments.len() > MAX_REPOSITORY_PATH_SEGMENTS {
        return None;
    }
    let key = format!("{forge}:{}", segments.join("/"));
    (key.len() <= MAX_REPOSITORY_KEY_BYTES).then_some(key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn forge_urls_normalize_to_stable_path_free_keys() {
        for remote in [
            "https://github.com/F4AH6O/Temote-MCP.git",
            "git@github.com:f4ah6o/temote-mcp.git",
            "ssh://git@github.com/f4ah6o/temote-mcp.git",
        ] {
            assert_eq!(
                canonical_forge_key(remote).as_deref(),
                Some("github:f4ah6o/temote-mcp")
            );
        }
        assert_eq!(
            canonical_forge_key("https://gitlab.com/Group/Subgroup/Project.git").as_deref(),
            Some("gitlab:group/subgroup/project")
        );
        assert_eq!(
            canonical_forge_key("git@bitbucket.org:Workspace/Repo.git").as_deref(),
            Some("bitbucket:workspace/repo")
        );
    }

    #[test]
    fn unsupported_and_credential_bearing_remotes_fail_closed() {
        for remote in [
            "https://user:password@github.com/owner/repo.git",
            "https://github.com/owner/repo.git?token=secret",
            "https://github.com/owner/repo.git#fragment",
            "https://github.com:8443/owner/repo.git",
            "https://example.test/owner/repo.git",
            "git@example.test:owner/repo.git",
            "git@github.com:owner/%2e%2e/repo.git",
            "git@github.com:owner/repo/extra.git",
        ] {
            assert_eq!(canonical_forge_key(remote), None, "accepted {remote:?}");
        }
    }

    #[test]
    fn origin_config_must_be_explicit_and_unambiguous() {
        assert_eq!(
            origin_url("[core]\n\trepositoryformatversion = 0\n[remote \"origin\"]\n\turl = https://github.com/a/b.git\n")
                .as_deref(),
            Some("https://github.com/a/b.git")
        );
        assert_eq!(
            origin_url(
                "[remote \"origin\"]\nurl = https://github.com/a/b.git\nurl = https://github.com/c/d.git\n"
            ),
            None
        );
        assert_eq!(
            origin_url(
                "[include]\npath = ../shared-config\n[remote \"origin\"]\nurl = https://github.com/a/b.git\n"
            ),
            None
        );
        assert_eq!(
            origin_url(
                "[url \"ssh://git@github.com/\"]\ninsteadOf = https://github.com/\n[remote \"origin\"]\nurl = https://github.com/a/b.git\n"
            ),
            None
        );
        assert_eq!(
            origin_url("[remote \"Origin\"]\nurl = https://github.com/a/b.git\n"),
            None
        );
        assert_eq!(
            origin_url(
                "[extensions]\nworktreeConfig = true\n[remote \"origin\"]\nurl = https://github.com/a/b.git\n"
            ),
            None
        );
        assert_eq!(
            origin_url(
                "[extensions]\nworktreeConfig = true # enabled per worktree\n[remote \"origin\"]\nurl = https://github.com/a/b.git\n"
            ),
            None
        );
        assert_eq!(
            origin_url("[remote \"upstream\"]\nurl = https://github.com/a/b.git\n"),
            None
        );
    }

    #[test]
    fn workspace_repository_key_uses_canonical_git_metadata() {
        let directory = tempfile::tempdir().unwrap();
        let worktree = directory.path().join("checkout");
        let git = worktree.join(".git");
        fs::create_dir_all(&git).unwrap();
        fs::write(git.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        fs::write(
            git.join("config"),
            "[core]\nrepositoryformatversion = 0\n[remote \"origin\"]\nurl = git@github.com:owner/repository.git\n",
        )
        .unwrap();
        fs::create_dir_all(worktree.join("src")).unwrap();
        assert_eq!(
            repository_key_for_workspace(&worktree.join("src")).as_deref(),
            Some("github:owner/repository")
        );
        assert_eq!(repository_key_for_workspace(directory.path()), None);
    }
}
