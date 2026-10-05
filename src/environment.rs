//! Environment-name compatibility for the Temote/Fabric naming migration.
//! Lookup is read-only: it never copies credentials into the process environment.
use std::ffi::{OsStr, OsString};

/// Preferred canonical name, followed by supported compatibility aliases.
pub fn names(key: &OsStr) -> Vec<OsString> {
    let Some(key_text) = key.to_str() else {
        return vec![key.to_owned()];
    };
    let Some(suffix) = key_text.strip_prefix("TEMOTE_MCP_") else {
        return vec![key.to_owned()];
    };
    if suffix.is_empty() {
        return vec![key.to_owned()];
    }
    let mut result = Vec::new();
    if let Some(fabric_suffix) = suffix.strip_prefix("GATEWAY_") {
        result.push(OsString::from(format!("TEMOTE_FABRIC_{fabric_suffix}")));
    }
    result.push(OsString::from(format!("TEMOTE_{suffix}")));
    result.push(key.to_owned());
    result
}

fn lookup(key: &OsStr, mut read: impl FnMut(&OsStr) -> Option<OsString>) -> Option<OsString> {
    names(key).iter().find_map(|name| read(name))
}

/// Read a variable using canonical names before legacy aliases.
pub fn var_os(key: impl AsRef<OsStr>) -> Option<OsString> {
    lookup(key.as_ref(), |name| std::env::var_os(name))
}

/// UTF-8 variant of [`var_os`], preserving ordinary `std::env::VarError` semantics.
pub fn var(key: impl AsRef<OsStr>) -> Result<String, std::env::VarError> {
    var_os(key)
        .ok_or(std::env::VarError::NotPresent)?
        .into_string()
        .map_err(std::env::VarError::NotUnicode)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support;

    #[test]
    fn names_preserve_host_identity_and_fabric_compatibility() {
        assert_eq!(
            names(OsStr::new("TEMOTE_MCP_ROOTS")),
            ["TEMOTE_ROOTS", "TEMOTE_MCP_ROOTS"].map(OsString::from)
        );
        assert_eq!(
            names(OsStr::new("TEMOTE_MCP_GATEWAY_HOST_TOKEN")),
            [
                "TEMOTE_FABRIC_HOST_TOKEN",
                "TEMOTE_GATEWAY_HOST_TOKEN",
                "TEMOTE_MCP_GATEWAY_HOST_TOKEN"
            ]
            .map(OsString::from)
        );
        assert_eq!(names(OsStr::new("PATH")), [OsString::from("PATH")]);
    }

    #[test]
    fn canonical_priority_is_deterministic_for_all_presence_combinations() -> noprop::TestResult {
        test_support::run(0x454e_564e_414d_4553, 128, |ctx| {
            let present = noprop::sample_usize_in(ctx, 0..=7);
            let keys = names(OsStr::new("TEMOTE_MCP_GATEWAY_URL"));
            let result = lookup(OsStr::new("TEMOTE_MCP_GATEWAY_URL"), |key| {
                let index = keys.iter().position(|candidate| candidate == key).unwrap();
                ((present & (1 << index)) != 0).then(|| OsString::from(index.to_string()))
            });
            let expected = (0..3)
                .find(|index| present & (1 << index) != 0)
                .map(|index| OsString::from(index.to_string()));
            assert_eq!(result, expected);
            Ok(())
        })
    }
}
