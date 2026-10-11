use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserializer;
use serde::de::{self, MapAccess, Visitor};
use serde_json::Value;

#[derive(Clone, Debug, Default)]
pub struct NamedRoots {
    roots: BTreeMap<String, PathBuf>,
}

impl NamedRoots {
    pub fn from_env() -> Result<Self> {
        let Some(value) = temote_mcp::environment::var_os("TEMOTE_MCP_ROOTS") else {
            return Ok(Self::default());
        };
        let value = value
            .into_string()
            .map_err(|_| anyhow::anyhow!("TEMOTE_ROOTS must be valid UTF-8"))?;
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
                "TEMOTE_ROOTS must be either a single name=path mapping or a JSON object",
            )?;
            anyhow::ensure!(!path.is_empty(), "TEMOTE_ROOTS path must not be empty");
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
            if let Some((other, _)) = roots.iter().find(|(_, path)| **path == canonical) {
                anyhow::bail!(
                    "named roots {other} and {name} resolve to the same directory: {}",
                    canonical.display()
                );
            }
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
            .with_context(|| {
                if self.roots.is_empty() {
                    "no named roots are configured; set TEMOTE_ROOTS on the host before starting temote-mcp (a single mapping like TEMOTE_ROOTS='src=~/src' or a JSON object like {\"src\":\"~/src\"}) and restart temote-mcp, then retry session_start".to_owned()
                } else {
                    format!(
                        "unknown named root: {root_name} (configured: {}; to add a root, include it in TEMOTE_ROOTS on the host and restart temote-mcp)",
                        self.roots
                            .keys()
                            .cloned()
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                }
            })?;

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

    /// Resolve a physical cwd back to its most specific logical named root.
    pub fn reverse_resolve(&self, cwd: &Path) -> Result<Option<String>> {
        let cwd = crate::config::canonical_directory(cwd)?;
        let mut best: Option<(&str, &Path, usize)> = None;
        let mut tied_with = None;
        for (name, root) in &self.roots {
            if cwd != *root && !cwd.starts_with(root) {
                continue;
            }
            let depth = root.components().count();
            match best {
                Some((_, _, best_depth)) if depth == best_depth => tied_with = Some(name.as_str()),
                Some((_, _, best_depth)) if depth < best_depth => {}
                _ => {
                    best = Some((name, root, depth));
                    tied_with = None;
                }
            }
        }
        let Some((name, root, _)) = best else {
            return Ok(None);
        };
        if let Some(other) = tied_with {
            anyhow::bail!(
                "named roots {name} and {other} ambiguously match {}",
                cwd.display()
            );
        }
        let relative = cwd.strip_prefix(root)?;
        if relative.as_os_str().is_empty() {
            Ok(Some(name.to_owned()))
        } else {
            let relative = relative
                .to_str()
                .context("session cwd must be valid UTF-8")?;
            Ok(Some(format!("{name}/{relative}")))
        }
    }

    /// Canonical physical path of the configured named root called `name`.
    ///
    /// Callers use this for policy anchors (for example the managed-worktree
    /// `src` root) so the physical root always comes from `TEMOTE_ROOTS`
    /// rather than from `HOME` or a client-supplied path.
    pub fn canonical_root(&self, name: &str) -> Option<&Path> {
        self.roots.get(name).map(PathBuf::as_path)
    }

    /// Validate a server-generated mutation path against a named root without
    /// requiring the leaf to exist. Existing symlink components and any
    /// nested named root inside the intended mutation tree fail closed.
    pub(crate) fn validate_mutation_path_under_root(
        &self,
        requested_root: &str,
        path: &Path,
    ) -> Result<()> {
        let root = self
            .canonical_root(requested_root)
            .context("requested named root is unavailable")?;
        anyhow::ensure!(
            path.is_absolute() && path.starts_with(root),
            "generated mutation path is outside its named root"
        );
        let relative = path.strip_prefix(root)?;
        let mut candidate = root.to_path_buf();
        for component in relative.components() {
            let Component::Normal(component) = component else {
                anyhow::bail!("generated mutation path has an unsafe component")
            };
            candidate.push(component);

            let mut most_specific: Option<(&str, &Path, usize)> = None;
            for (name, configured) in &self.roots {
                if candidate != *configured && !candidate.starts_with(configured) {
                    continue;
                }
                let depth = configured.components().count();
                if most_specific.is_none_or(|(_, _, best_depth)| depth > best_depth) {
                    most_specific = Some((name, configured, depth));
                } else if most_specific.is_some_and(|(_, _, best_depth)| depth == best_depth) {
                    anyhow::bail!("generated mutation path matches ambiguous named roots");
                }
            }
            anyhow::ensure!(
                most_specific.is_some_and(|(name, _, _)| name == requested_root),
                "generated mutation path resolves through a different, more-specific named root"
            );
            match std::fs::symlink_metadata(&candidate) {
                Ok(metadata) => anyhow::ensure!(
                    !metadata.file_type().is_symlink(),
                    "generated mutation path contains a symbolic link"
                ),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        anyhow::ensure!(
            !self.roots.iter().any(|(name, configured)| {
                name != requested_root && configured.starts_with(path)
            }),
            "generated mutation tree contains a nested named root"
        );
        Ok(())
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

    pub fn names(&self) -> Vec<String> {
        self.roots.keys().cloned().collect()
    }
}

fn parse_json_roots(value: &str) -> Result<BTreeMap<String, String>> {
    struct UniqueRootMapVisitor;

    impl<'de> Visitor<'de> for UniqueRootMapVisitor {
        type Value = BTreeMap<String, String>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("a non-empty JSON object of unique named-root string paths")
        }

        fn visit_map<A>(self, mut map: A) -> std::result::Result<Self::Value, A::Error>
        where
            A: MapAccess<'de>,
        {
            let mut roots = BTreeMap::new();
            while let Some((name, value)) = map.next_entry::<String, Value>()? {
                let path = value.as_str().ok_or_else(|| {
                    de::Error::custom(format!("named root {name} path must be a string"))
                })?;
                if path.is_empty() {
                    return Err(de::Error::custom(format!(
                        "named root {name} path must not be empty"
                    )));
                }
                if roots.insert(name.clone(), path.to_owned()).is_some() {
                    return Err(de::Error::custom(format!(
                        "duplicate named root in TEMOTE_ROOTS JSON: {name}"
                    )));
                }
            }
            if roots.is_empty() {
                return Err(de::Error::custom("TEMOTE_ROOTS JSON must not be empty"));
            }
            Ok(roots)
        }
    }

    let mut deserializer = serde_json::Deserializer::from_str(value);
    let roots = deserializer
        .deserialize_map(UniqueRootMapVisitor)
        .context("invalid TEMOTE_ROOTS JSON")?;
    deserializer
        .end()
        .context("invalid trailing TEMOTE_ROOTS JSON content")?;
    Ok(roots)
}

fn expand_home(path: &Path) -> Result<PathBuf> {
    let text = path
        .to_str()
        .context("configured named root path must be valid UTF-8")?;
    if text == "~" {
        return crate::platform_paths::home_dir().context("cannot determine HOME for named root");
    }
    if let Some(rest) = text.strip_prefix("~/") {
        return Ok(crate::platform_paths::home_dir()
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
    use crate::test_support;

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
        let other = tempfile::tempdir().unwrap();
        let single = NamedRoots::parse(&format!("src={}", root.path().display())).unwrap();
        assert!(!single.is_empty());

        let json = serde_json::json!({
            "src": root.path().to_string_lossy(),
            "work": other.path().to_string_lossy(),
        });
        let multiple = NamedRoots::parse(&json.to_string()).unwrap();
        assert!(!multiple.is_empty());
    }

    #[test]
    fn rejects_distinct_names_for_the_same_canonical_root() {
        let fixture = tempfile::tempdir().unwrap();
        let real = fixture.path().join("real");
        std::fs::create_dir(&real).unwrap();
        symlink(&real, fixture.path().join("alias")).unwrap();
        let config = serde_json::json!({"first":real,"second":fixture.path().join("alias")});
        let error = NamedRoots::parse(&config.to_string())
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("first") && error.contains("second"),
            "{error}"
        );
    }

    #[test]
    fn reverse_resolves_longest_root_and_canonicalizes_symlinks() {
        let fixture = tempfile::tempdir().unwrap();
        let outer = fixture.path().join("outer");
        let nested = outer.join("nested");
        let leaf = nested.join("repo");
        let outside = fixture.path().join("outside");
        std::fs::create_dir_all(&leaf).unwrap();
        std::fs::create_dir(&outside).unwrap();
        symlink(&leaf, fixture.path().join("inside-link")).unwrap();
        symlink(&outside, fixture.path().join("outside-link")).unwrap();
        let roots =
            NamedRoots::parse(&serde_json::json!({"outer":outer,"inner":nested}).to_string())
                .unwrap();
        assert_eq!(
            roots.reverse_resolve(&leaf).unwrap().as_deref(),
            Some("inner/repo")
        );
        assert_eq!(
            roots
                .reverse_resolve(&fixture.path().join("inside-link"))
                .unwrap()
                .as_deref(),
            Some("inner/repo")
        );
        assert_eq!(
            roots
                .reverse_resolve(&fixture.path().join("outside-link"))
                .unwrap(),
            None
        );
        assert_eq!(
            roots.reverse_resolve(&outer).unwrap().as_deref(),
            Some("outer")
        );
    }

    #[test]
    fn reverse_resolution_fails_on_defensive_equal_depth_tie() {
        let fixture = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(fixture.path()).unwrap();
        let roots = NamedRoots {
            roots: BTreeMap::from([
                ("a".to_owned(), root.clone()),
                ("b".to_owned(), root.clone()),
            ]),
        };
        assert!(roots.reverse_resolve(&root).is_err());
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
    fn duplicate_json_root_names_fail_closed() {
        let root_a = tempfile::tempdir().unwrap();
        let root_b = tempfile::tempdir().unwrap();
        let json = format!(
            r#"{{"src":"{}","src":"{}"}}"#,
            root_a.path().display(),
            root_b.path().display()
        );
        assert!(NamedRoots::parse(&json).is_err());
    }

    #[test]
    fn generated_json_root_maps_round_trip_to_canonical_model() -> noprop::TestResult {
        let fixture = tempfile::tempdir().unwrap();
        let roots = (0..6)
            .map(|index| {
                let path = fixture.path().join(format!("root-{index}"));
                std::fs::create_dir(&path).unwrap();
                std::fs::canonicalize(path).unwrap()
            })
            .collect::<Vec<_>>();

        test_support::run(0x4e52_4a53_4f4e_0001, 512, |ctx| {
            let count = noprop::sample_usize_in(ctx, 1..=roots.len());
            let mut configured = BTreeMap::new();
            let mut expected = BTreeMap::new();
            for (index, root) in roots.iter().take(count).enumerate() {
                let name = format!("{}-{index}", test_support::safe_component(ctx));
                configured.insert(name.clone(), root.display().to_string());
                expected.insert(name, root.clone());
            }
            let json = serde_json::to_string(&configured).unwrap();
            let parsed = NamedRoots::parse(&json).unwrap();
            assert_eq!(parsed.roots, expected);
            Ok(())
        })
    }

    #[test]
    fn generated_invalid_json_root_values_fail_closed() -> noprop::TestResult {
        test_support::run(0x4e52_4a53_4f4e_0002, 512, |ctx| {
            let name = test_support::safe_component(ctx);
            let invalid = match noprop::sample_usize_in(ctx, 0..=4) {
                0 => serde_json::json!({}),
                1 => serde_json::json!({name.clone(): ""}),
                2 => serde_json::json!({name.clone(): 42}),
                3 => serde_json::json!({name.clone(): null}),
                _ => serde_json::json!({name: ["not", "a", "path"]}),
            };
            assert!(NamedRoots::parse(&invalid.to_string()).is_err());
            Ok(())
        })
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

    #[test]
    fn exposes_the_configured_physical_root_by_exact_name() {
        let fixture = tempfile::tempdir().unwrap();
        let src = fixture.path().join("src");
        let work = fixture.path().join("work");
        std::fs::create_dir(&src).unwrap();
        std::fs::create_dir(&work).unwrap();
        let canonical_src = std::fs::canonicalize(&src).unwrap();
        let canonical_work = std::fs::canonicalize(&work).unwrap();
        let roots = NamedRoots::from_canonical_roots(BTreeMap::from([
            ("src".to_owned(), canonical_src.clone()),
            ("work".to_owned(), canonical_work.clone()),
        ]))
        .unwrap();

        assert_eq!(roots.canonical_root("src"), Some(canonical_src.as_path()));
        assert_eq!(roots.canonical_root("work"), Some(canonical_work.as_path()));
        assert_eq!(roots.canonical_root("other"), None);
        assert_eq!(NamedRoots::default().canonical_root("src"), None);
    }

    #[test]
    fn managed_mutation_paths_respect_target_boundaries_symlinks_and_inode_replacement() {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().join("src");
        let sibling_root = root.join(".temote-mcp/workspaces/other");
        let outside = fixture.path().join("outside");
        std::fs::create_dir_all(&sibling_root).unwrap();
        std::fs::create_dir(&outside).unwrap();
        let root = std::fs::canonicalize(root).unwrap();
        let sibling_root = std::fs::canonicalize(sibling_root).unwrap();
        let other = std::fs::canonicalize(outside).unwrap();
        let roots = NamedRoots::from_canonical_roots(BTreeMap::from([
            ("src".to_owned(), root.clone()),
            ("workspace".to_owned(), sibling_root),
        ]))
        .unwrap();
        let repository = root.join(".temote-mcp/repositories/github.com/owner/repo.git");
        roots
            .validate_mutation_path_under_root("src", &repository)
            .expect("an unrelated nested root under a sibling mutation tree is allowed");

        let nested_target_root = repository.join("owned");
        std::fs::create_dir_all(&nested_target_root).unwrap();
        let nested_target_root = std::fs::canonicalize(nested_target_root).unwrap();
        let roots_with_target_root = NamedRoots::from_canonical_roots(BTreeMap::from([
            ("src".to_owned(), root.clone()),
            ("target".to_owned(), nested_target_root),
            (
                "workspace".to_owned(),
                root.join(".temote-mcp/workspaces/other"),
            ),
        ]))
        .unwrap();
        assert!(
            roots_with_target_root
                .validate_mutation_path_under_root("src", &repository)
                .is_err(),
            "a named root inside the intended mutation target remains rejected"
        );

        let ordinary_roots =
            NamedRoots::from_canonical_roots(BTreeMap::from([("src".to_owned(), root.clone())]))
                .unwrap();
        let escaped = root.join(".temote-mcp/escape/repo.git");
        std::os::unix::fs::symlink(other.join("."), root.join(".temote-mcp/escape")).unwrap();
        assert!(
            ordinary_roots
                .validate_mutation_path_under_root("src", &escaped)
                .is_err()
        );

        let replacement = fixture.path().join("replacement");
        let old_root = fixture.path().join("src-old");
        let old_identity = std::fs::metadata(&root).unwrap();
        std::fs::create_dir(&replacement).unwrap();
        std::fs::rename(&root, &old_root).unwrap();
        std::fs::rename(&replacement, &root).unwrap();
        assert!(
            ordinary_roots
                .validate_mutation_path_under_root("src", &root.join("new-output"))
                .is_ok(),
            "the lexical checker is intentionally independent of identity proof"
        );
        use std::os::unix::fs::MetadataExt;
        let replacement_identity = std::fs::metadata(&root).unwrap();
        assert!(
            old_identity.dev() != replacement_identity.dev()
                || old_identity.ino() != replacement_identity.ino()
        );
    }

    #[test]
    fn root_name_validation_matches_reference_grammar() -> noprop::TestResult {
        test_support::run(0x524f_4f54_4e41_4d45, test_support::DEFAULT_CASES, |ctx| {
            let name = test_support::ascii_string(ctx, 80);
            let expected = !name.is_empty()
                && name.chars().all(|character| {
                    character.is_ascii_alphanumeric() || matches!(character, '-' | '_')
                });
            assert_eq!(
                validate_root_name(&name).is_ok(),
                expected,
                "root name mismatch for {name:?}"
            );
            Ok(())
        })
    }

    #[test]
    fn generated_descendants_resolve_to_canonical_paths_inside_root() -> noprop::TestResult {
        let fixture = tempfile::tempdir().unwrap();
        let physical_root = fixture.path().join("volume");
        std::fs::create_dir(&physical_root).unwrap();
        let physical_root = std::fs::canonicalize(physical_root).unwrap();
        let roots = NamedRoots::from_canonical_roots(BTreeMap::from([(
            "src".to_owned(),
            physical_root.clone(),
        )]))
        .unwrap();

        test_support::run(0x4445_5343_454e_4401, 512, |ctx| {
            let depth = noprop::sample_usize_in(ctx, 0..=4);
            let components = (0..depth)
                .map(|_| test_support::safe_component(ctx))
                .collect::<Vec<_>>();
            let mut target = physical_root.clone();
            for component in &components {
                target.push(component);
            }
            std::fs::create_dir_all(&target).unwrap();

            let logical = if components.is_empty() {
                "src".to_owned()
            } else {
                format!("src/{}", components.join("/"))
            };
            let resolved = roots.resolve(&logical).unwrap();
            let canonical = std::fs::canonicalize(&target).unwrap();
            assert_eq!(resolved, canonical, "logical={logical:?}");
            assert!(
                resolved == physical_root || resolved.starts_with(&physical_root),
                "resolved path escaped root: logical={logical:?}, resolved={resolved:?}"
            );
            Ok(())
        })
    }

    #[test]
    fn generated_parent_and_symlink_escapes_fail_closed() -> noprop::TestResult {
        let fixture = tempfile::tempdir().unwrap();
        let physical_root = fixture.path().join("volume");
        let outside = fixture.path().join("outside");
        std::fs::create_dir(&physical_root).unwrap();
        std::fs::create_dir(&outside).unwrap();
        symlink(&outside, physical_root.join("escape-link")).unwrap();
        let physical_root = std::fs::canonicalize(physical_root).unwrap();
        let roots =
            NamedRoots::from_canonical_roots(BTreeMap::from([("src".to_owned(), physical_root)]))
                .unwrap();

        test_support::run(0x4553_4341_5045_0001, 512, |ctx| {
            let leaf = test_support::safe_component(ctx);
            std::fs::create_dir_all(outside.join(&leaf)).unwrap();
            let logical = if noprop::sample_bool(ctx) {
                format!("src/../outside/{leaf}")
            } else {
                format!("src/escape-link/{leaf}")
            };
            assert!(
                roots.resolve(&logical).is_err(),
                "escape unexpectedly resolved: {logical:?}"
            );
            Ok(())
        })
    }
}
