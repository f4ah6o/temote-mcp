#![allow(dead_code)]

use std::path::PathBuf;

use anyhow::Result;

pub const MAX_DEV_TOOL_OPERATION_BYTES: usize = 64;
pub const MAX_DEV_TOOL_ARGUMENT_BYTES: usize = 8 * 1024;
pub const MAX_DEV_TOOL_ARGUMENTS: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DevTool {
    Cargo,
    Vp,
}

impl DevTool {
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "cargo" => Ok(Self::Cargo),
            "vp" => Ok(Self::Vp),
            _ => anyhow::bail!("unsupported developer tool {value:?}; expected cargo or vp"),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cargo => "cargo",
            Self::Vp => "vp",
        }
    }

    pub fn executable_name(self) -> &'static str {
        match self {
            Self::Cargo => "cargo",
            Self::Vp => "vp",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DevToolClass {
    DevOffline,
    DependencyNetwork,
    ArbitraryCodeSensitive,
    Rejected,
}

impl DevToolClass {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DevOffline => "dev_offline",
            Self::DependencyNetwork => "dependency_network",
            Self::ArbitraryCodeSensitive => "arbitrary_code_sensitive",
            Self::Rejected => "rejected",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DevToolClassification {
    pub class: DevToolClass,
    pub reason: &'static str,
}

impl DevToolClassification {
    fn new(class: DevToolClass, reason: &'static str) -> Self {
        Self { class, reason }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DevToolRequest {
    tool: DevTool,
    operation: String,
    args: Vec<String>,
    cwd: Option<PathBuf>,
}

impl DevToolRequest {
    pub fn new(
        tool: DevTool,
        operation: impl Into<String>,
        args: Vec<String>,
        cwd: Option<PathBuf>,
    ) -> Result<Self> {
        let operation = operation.into();
        validate_operation(&operation)?;
        validate_arguments(&args)?;
        Ok(Self {
            tool,
            operation,
            args,
            cwd,
        })
    }

    pub fn tool(&self) -> DevTool {
        self.tool
    }

    pub fn operation(&self) -> &str {
        &self.operation
    }

    pub fn args(&self) -> &[String] {
        &self.args
    }

    pub fn cwd(&self) -> Option<&PathBuf> {
        self.cwd.as_ref()
    }

    pub fn classification(&self) -> DevToolClassification {
        classify(self.tool, &self.operation)
    }
}

fn validate_operation(operation: &str) -> Result<()> {
    anyhow::ensure!(
        !operation.is_empty(),
        "developer tool operation must not be empty"
    );
    anyhow::ensure!(
        operation.len() <= MAX_DEV_TOOL_OPERATION_BYTES,
        "developer tool operation exceeds {MAX_DEV_TOOL_OPERATION_BYTES} bytes"
    );
    anyhow::ensure!(
        operation
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')),
        "developer tool operation accepts only ASCII letters, digits, '-' and '_'"
    );
    anyhow::ensure!(
        operation
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric),
        "developer tool operation must start with an ASCII letter or digit"
    );
    Ok(())
}

fn validate_arguments(args: &[String]) -> Result<()> {
    anyhow::ensure!(
        args.len() <= MAX_DEV_TOOL_ARGUMENTS,
        "developer tool request exceeds {MAX_DEV_TOOL_ARGUMENTS} arguments"
    );
    for argument in args {
        anyhow::ensure!(
            !argument.contains('\0'),
            "developer tool arguments must not contain NUL"
        );
        anyhow::ensure!(
            argument.len() <= MAX_DEV_TOOL_ARGUMENT_BYTES,
            "developer tool argument exceeds {MAX_DEV_TOOL_ARGUMENT_BYTES} bytes"
        );
    }
    Ok(())
}

pub fn classify(tool: DevTool, operation: &str) -> DevToolClassification {
    let class = match (tool, operation) {
        (DevTool::Cargo, "fmt" | "check" | "clippy" | "test" | "build") => DevToolClass::DevOffline,
        (DevTool::Cargo, "fetch" | "install" | "update") => DevToolClass::DependencyNetwork,
        (DevTool::Vp, "check" | "lint" | "fmt" | "format" | "test" | "build" | "pack") => {
            DevToolClass::DevOffline
        }
        (DevTool::Vp, "install" | "add" | "update" | "outdated" | "info" | "rebuild") => {
            DevToolClass::DependencyNetwork
        }
        (DevTool::Vp, "run" | "exec" | "dlx") => DevToolClass::ArbitraryCodeSensitive,
        (DevTool::Vp, "upgrade" | "implode") => DevToolClass::Rejected,
        _ => DevToolClass::Rejected,
    };
    let reason = match class {
        DevToolClass::DevOffline => {
            "offline development command; the sandbox still contains project code execution"
        }
        DevToolClass::DependencyNetwork => {
            "dependency or registry operation; requires the explicit network profile"
        }
        DevToolClass::ArbitraryCodeSensitive => {
            "arbitrary code execution; never treated as safe because the executable is vp"
        }
        DevToolClass::Rejected => "operation is not part of the developer-tool contract",
    };
    DevToolClassification::new(class, reason)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support;

    #[test]
    fn cargo_operations_are_classified_by_table() {
        for operation in ["fmt", "check", "clippy", "test", "build"] {
            assert_eq!(
                classify(DevTool::Cargo, operation).class,
                DevToolClass::DevOffline,
                "cargo {operation}"
            );
        }
        for operation in ["fetch", "install", "update"] {
            assert_eq!(
                classify(DevTool::Cargo, operation).class,
                DevToolClass::DependencyNetwork,
                "cargo {operation}"
            );
        }
        for operation in [
            "run", "bench", "publish", "login", "owner", "fmt ", "FMT", "",
        ] {
            assert_eq!(
                classify(DevTool::Cargo, operation).class,
                DevToolClass::Rejected,
                "cargo {operation:?}"
            );
        }
    }

    #[test]
    fn vite_plus_operations_are_classified_by_table() {
        for operation in ["check", "lint", "fmt", "format", "test", "build", "pack"] {
            assert_eq!(
                classify(DevTool::Vp, operation).class,
                DevToolClass::DevOffline,
                "vp {operation}"
            );
        }
        for operation in ["install", "add", "update", "outdated", "info", "rebuild"] {
            assert_eq!(
                classify(DevTool::Vp, operation).class,
                DevToolClass::DependencyNetwork,
                "vp {operation}"
            );
        }
        for operation in ["run", "exec", "dlx"] {
            let classification = classify(DevTool::Vp, operation);
            assert_eq!(
                classification.class,
                DevToolClass::ArbitraryCodeSensitive,
                "vp {operation}"
            );
            assert!(classification.reason.contains("arbitrary code"));
        }
        for operation in ["upgrade", "implode"] {
            assert_eq!(
                classify(DevTool::Vp, operation).class,
                DevToolClass::Rejected,
                "vp {operation} must stay outside the initial contract"
            );
        }
    }

    #[test]
    fn unknown_operations_fail_closed_for_every_tool() {
        for tool in [DevTool::Cargo, DevTool::Vp] {
            assert_eq!(
                classify(tool, "definitely-not-a-subcommand").class,
                DevToolClass::Rejected
            );
        }
    }

    #[test]
    fn tool_names_reject_paths_and_unknown_values() {
        assert_eq!(DevTool::parse("cargo").unwrap(), DevTool::Cargo);
        assert_eq!(DevTool::parse("vp").unwrap(), DevTool::Vp);
        for value in [
            "/usr/bin/cargo",
            "./cargo",
            "cargo extra",
            "sh",
            "",
            "CARGO",
            "cargo\0",
        ] {
            assert!(
                DevTool::parse(value).is_err(),
                "tool selection accepted {value:?}"
            );
        }
    }

    #[test]
    fn operation_names_reject_paths_and_shell_fragments() {
        for operation in [
            "../cargo",
            "build;rm",
            "build&&rm",
            "-p",
            "build/../test",
            "build test",
        ] {
            assert!(
                DevToolRequest::new(DevTool::Cargo, operation, Vec::new(), None).is_err(),
                "operation accepted {operation:?}"
            );
        }
    }

    #[test]
    fn arguments_reject_nul_oversize_and_excessive_counts() {
        let exactly_max = "a".repeat(MAX_DEV_TOOL_ARGUMENT_BYTES);
        assert!(
            DevToolRequest::new(DevTool::Cargo, "build", vec![exactly_max.clone()], None).is_ok()
        );
        assert!(
            DevToolRequest::new(
                DevTool::Cargo,
                "build",
                vec![format!("{exactly_max}a")],
                None,
            )
            .is_err()
        );
        assert!(
            DevToolRequest::new(DevTool::Cargo, "build", vec!["a\0b".to_owned()], None).is_err()
        );
        let too_many = vec!["a".to_owned(); MAX_DEV_TOOL_ARGUMENTS + 1];
        assert!(DevToolRequest::new(DevTool::Cargo, "build", too_many, None).is_err());
    }

    #[test]
    fn request_keeps_arguments_and_cwd_without_inference() {
        let cwd = PathBuf::from("/tmp/project");
        let request = DevToolRequest::new(
            DevTool::Vp,
            "check",
            vec!["--reporter".to_owned(), "dot".to_owned()],
            Some(cwd.clone()),
        )
        .unwrap();
        assert_eq!(request.tool(), DevTool::Vp);
        assert_eq!(request.operation(), "check");
        assert_eq!(request.args(), ["--reporter", "dot"]);
        assert_eq!(request.cwd(), Some(&cwd));
        assert_eq!(request.classification().class, DevToolClass::DevOffline);
    }

    #[test]
    fn generated_operations_classify_deterministically_and_fail_closed() -> noprop::TestResult {
        test_support::run(0x4445_5654_4f4f_4c01, test_support::DEFAULT_CASES, |ctx| {
            let operation = test_support::ascii_string(ctx, 80);
            for tool in [DevTool::Cargo, DevTool::Vp] {
                let first = classify(tool, &operation);
                assert_eq!(
                    first,
                    classify(tool, &operation),
                    "classification must be pure"
                );
                if first.class == DevToolClass::Rejected {
                    continue;
                }
                let well_formed = !operation.is_empty()
                    && operation.len() <= MAX_DEV_TOOL_OPERATION_BYTES
                    && operation
                        .as_bytes()
                        .first()
                        .is_some_and(u8::is_ascii_alphanumeric)
                    && operation
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'));
                assert!(
                    well_formed,
                    "classified operation must be a valid name: {operation:?}",
                );
                assert!(
                    DevToolRequest::new(tool, operation.clone(), Vec::new(), None).is_ok(),
                    "classified operation must pass request validation: {operation:?}",
                );
            }
            Ok(())
        })
    }
}
