use std::sync::OnceLock;

static BOOT_GENERATION: OnceLock<String> = OnceLock::new();

/// Returns the stable, non-secret identity for this process lifetime.
pub fn generation() -> &'static str {
    BOOT_GENERATION
        .get_or_init(|| uuid::Uuid::new_v4().to_string())
        .as_str()
}

#[cfg(test)]
mod tests {
    use super::generation;

    #[test]
    fn generation_is_a_stable_uuid() {
        let first = generation();
        assert_eq!(first, generation());
        assert_eq!(uuid::Uuid::parse_str(first).unwrap().to_string(), first);
    }
}
