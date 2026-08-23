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
                        "duplicate named root in TEMOTE_MCP_ROOTS JSON: {name}"
                    )));
                }
            }
            if roots.is_empty() {
                return Err(de::Error::custom("TEMOTE_MCP_ROOTS JSON must not be empty"));
            }
            Ok(roots)
        }
    }

    let mut deserializer = serde_json::Deserializer::from_str(value);
    let roots = deserializer
        .deserialize_map(UniqueRootMapVisitor)
        .context("invalid TEMOTE_MCP_ROOTS JSON")?;
    deserializer
        .end()
        .context("invalid trailing TEMOTE_MCP_ROOTS JSON content")?;
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
