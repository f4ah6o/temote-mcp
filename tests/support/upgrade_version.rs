pub fn distinct_target_version_for(source_version: &str) -> String {
    let (core, prerelease) = source_version
        .split_once('-')
        .map_or((source_version, None), |(core, suffix)| {
            (core, Some(suffix))
        });
    let components = core
        .split('.')
        .map(|component| {
            component
                .parse::<u64>()
                .expect("CalVer component is not numeric")
        })
        .collect::<Vec<_>>();
    assert_eq!(components.len(), 3, "package version is not CalVer");
    assert!((1..=12).contains(&components[1]), "CalVer month is invalid");
    assert_eq!(
        core,
        format!("{}.{}.{}", components[0], components[1], components[2]),
        "CalVer core is not canonical"
    );

    match prerelease {
        Some(suffix) => {
            let number = suffix
                .strip_prefix("alpha.")
                .expect("unsupported package prerelease")
                .parse::<u64>()
                .expect("alpha prerelease number is not numeric");
            assert!(number > 0, "alpha prerelease number must be positive");
            assert_eq!(
                suffix,
                format!("alpha.{number}"),
                "alpha suffix is not canonical"
            );
            format!(
                "{}.{}.{}-alpha.{}",
                components[0],
                components[1],
                components[2],
                number
                    .checked_add(1)
                    .expect("alpha prerelease number overflow")
            )
        }
        None => format!(
            "{}.{}.{}",
            components[0],
            components[1],
            components[2]
                .checked_add(1)
                .expect("CalVer patch version overflow")
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::distinct_target_version_for;

    #[test]
    fn stable_target_increments_calver_patch() {
        assert_eq!(distinct_target_version_for("2026.10.2"), "2026.10.3");
    }

    #[test]
    fn alpha_target_increments_prerelease_without_changing_core() {
        assert_eq!(
            distinct_target_version_for("2026.10.3-alpha.4"),
            "2026.10.3-alpha.5"
        );
    }

    #[test]
    #[should_panic(expected = "unsupported package prerelease")]
    fn rejects_unrecognized_prerelease() {
        distinct_target_version_for("2026.10.3-beta.1");
    }

    #[test]
    #[should_panic(expected = "CalVer core is not canonical")]
    fn rejects_leading_zero_components() {
        distinct_target_version_for("2026.08.3");
    }
}
