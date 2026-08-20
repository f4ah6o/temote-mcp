use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::Value;

#[derive(Clone, Debug, Default)]
pub struct NamedRoots {
    roots: BTreeMap<String, PathBuf>,
}

impl NamedRoots {
    pub fn from_env() -> Result<Self> {
        let Some(value) = std::env::var_os("TEMOTE_MCP_ROOTS") else {
            return Ok(Self::default());
        };
        let value = value
            .into_string()
            .map_err(|_| anyhow::anyhow!("TEMOTE_MCP_ROOTS must be valid UTF-8"))?;
        Self::parse(&value)
    }

    pub fn parse(value: &str) -> Result<Self> {
        let value = value.trim();
        if value.is_empty() {
            return Ok(Self::default());
        }

        let configured = if value.starts_with('{') {
            parse_json_roots(value)?
        } else {
            let (name, path) = value.split_once('=').context(
                "TEMOTE_MCP_ROOTS must be either a single name=path mapping or a JSON object",
            )?;
            anyhow::ensure!(!path.is_empty(), "TEMOTE_MCP_ROOTS path must not be empty");
            BTreeMap::from([(name.to_owned(), path.to_owned())])
        };

        let mut roots = BTreeMap::new();
        for (name, configured_path) in configured {
            validate_root_name(&name)?;
            let expanded = expand_home(Path::new(&configured_path))?;
            let canonical = std::fs::canonicalize(&expanded).with_context(|| {
                format!(
                    "cannot resolve configured named root {name} at {}",
                    expanded.display()
                )
            })?;
            anyhow::ensure!(
                canonical.is_dir(),
                "configured named root {name} is not a directory: {}",
                canonical.display()
            );
            roots.insert(name, canonical);
        }
        Ok(Self { roots })
    }

    pub fn resolve(&self, logical_path: &str) -> Result<PathBuf> {
        anyhow::ensure!(
            !logical_path.trim().is_empty(),
            "session path must not be empty"
        );
        let path = Path::new(logical_path);
        anyhow::ensure!(
            !path.is_absolute(),
            "absolute session paths are not allowed"
        );

        let mut components = path.components();
        let root_component = components.next().context("session path must name a root")?;
        let root_name = match root_component {
            Component::Normal(value) => value
                .to_str()
                .context("root name must be valid UTF-8")?
                .to_owned(),
            _ => anyhow::bail!("session path must begin with a valid root name"),
        };
        validate_root_name(&root_name)?;
        let physical_root = self
            .roots
            .get(&root_name)
            .with_context(|| format!("unknown named root: {root_name}"))?;

        let mut candidate = physical_root.clone();
        for component in components {
            candidate.push(component.as_os_str());
        }
        let target = std::fs::canonicalize(&candidate)
            .with_context(|| format!("cannot resolve session path {}", candidate.display()))?;
        anyhow::ensure!(
            target.is_dir(),
            "session path is not a directory: {}",
            target.display()
        );
        anyhow::ensure!(
            target == *physical_root || target.starts_with(physical_root),
            "session path escapes named root {root_name}: {}",
            target.display()
        );
        Ok(target)
    }

    #[cfg(test)]
    pub fn from_canonical_roots(roots: BTreeMap<String, PathBuf>) -> Result<Self> {
        for (name, root) in &roots {
            validate_root_name(name)?;
            anyhow::ensure!(root.is_absolute(), "test root must be absolute");
            anyhow::ensure!(root.is_dir(), "test root must be a directory");
        }
        Ok(Self { roots })
    }

    pub fn is_empty(&self) -> bool {
        self.roots.is_empty()
    }
}

fn parse_json_roots(value: &str) -> Result<BTreeMap<String, String>> {
    let parsed: Value = serde_json::from_str(value).context("invalid TEMOTE_MCP_ROOTS JSON")?;
    let object = parsed
        .as_object()
        .context("TEMOTE_MCP_ROOTS JSON must be an object")?;
    anyhow::ensure!(
        !object.is_empty(),
        "TEMOTE_MCP_ROOTS JSON must not be empty"
    );
    object
        .iter()
        .map(|(name, value)| {
            let path = value
                .as_str()
                .with_context(|| format!("named root {name} path must be a string"))?;
            anyhow::ensure!(!path.is_empty(), "named root {name} path must not be empty");
            Ok((name.clone(), path.to_owned()))
        })
        .collect()
}

fn expand_home(path: &Path) -> Result<PathBuf> {
    let text = path
        .to_str()
        .context("configured named root path must be valid UTF-8")?;
    if text == "~" {
        return dirs::home_dir().context("cannot determine HOME for named root");
    }
    if let Some(rest) = text.strip_prefix("~/") {
        return Ok(dirs::home_dir()
            .context("cannot determine HOME for named root")?
            .join(rest));
    }
    Ok(path.to_owned())
}

pub fn validate_root_name(name: &str) -> Result<()> {
    anyhow::ensure!(!name.is_empty(), "root name must not be empty");
    anyhow::ensure!(name != "." && name != "..", "invalid root name: {name}");
    anyhow::ensure!(
        name.chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_')),
        "root name may contain only ASCII letters, numbers, '-', and '_'"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use super::*;

    #[test]
    fn validates_root_names() {
        for valid in ["src", "work", "dev-storage", "foo_bar", "A1"] {
            assert!(validate_root_name(valid).is_ok(), "{valid}");
        }
        for invalid in ["", ".", "..", "foo/bar", "/foo", "foo bar"] {
            assert!(validate_root_name(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn parses_single_mapping_and_json_object() {
        let root = tempfile::tempdir().unwrap();
        let single = NamedRoots::parse(&format!("src={}", root.path().display())).unwrap();
        assert!(!single.is_empty());

        let json = serde_json::json!({
            "src": root.path().to_string_lossy(),
            "work": root.path().to_string_lossy(),
        });
        let multiple = NamedRoots::parse(&json.to_string()).unwrap();
        assert!(!multiple.is_empty());
    }

    #[test]
    fn rejects_ambiguous_multi_mapping_text() {
        let error = NamedRoots::parse("src=/tmp/src,work=/tmp/work").unwrap_err();
        assert!(
            error
                .to_string()
                .contains("cannot resolve configured named root")
        );
    }

    #[test]
    fn resolves_root_alias_symlink_and_rejects_descendant_escape() {
        let fixture = tempfile::tempdir().unwrap();
        let volume = fixture.path().join("volume");
        let home = fixture.path().join("home");
        let outside = fixture.path().join("outside");
        std::fs::create_dir_all(volume.join("repo-a")).unwrap();
        std::fs::create_dir_all(volume.join("repo-b")).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        symlink(&volume, home.join("src")).unwrap();
        symlink(&outside, volume.join("outside-link")).unwrap();

        let roots = NamedRoots::parse(&format!("src={}", home.join("src").display())).unwrap();
        assert_eq!(
            roots.resolve("src").unwrap(),
            std::fs::canonicalize(&volume).unwrap()
        );
        assert_eq!(
            roots.resolve("src/repo-a").unwrap(),
            std::fs::canonicalize(volume.join("repo-a")).unwrap()
        );
        assert_eq!(
            roots.resolve("src/repo-b").unwrap(),
            std::fs::canonicalize(volume.join("repo-b")).unwrap()
        );
        assert!(roots.resolve("/tmp").is_err());
        assert!(roots.resolve("unknown/repo-a").is_err());
        assert!(roots.resolve("src/../outside").is_err());
        assert!(roots.resolve("src/outside-link").is_err());
        assert!(roots.resolve("src/missing").is_err());
    }

    #[test]
    fn empty_configuration_fails_closed_on_resolution() {
        let roots = NamedRoots::default();
        assert!(roots.resolve("src").is_err());
    }
}
