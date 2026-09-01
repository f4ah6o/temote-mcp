use std::collections::HashMap;
use std::io::{BufRead, Write};

use anyhow::{Context, Result};
use onepassword_sdk_unofficial::{Client, DesktopAuth, Error as SdkError};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

const MAX_REQUEST_BYTES: usize = 1024 * 1024;
const MAX_CLIENTS: usize = 16;

#[derive(Debug, Deserialize)]
struct Request {
    id: u64,
    account: String,
    references: Vec<String>,
}

#[derive(Debug, Serialize)]
struct Response<'a> {
    id: u64,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<&'a Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<&'a str>,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("temote-onepassword-sdk: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let mut args = std::env::args().skip(1);
    if let Some(mode) = args.next() {
        anyhow::ensure!(mode == "--emit-env-json", "unsupported sidecar mode");
        let count = args
            .next()
            .context("--emit-env-json requires a count")?
            .parse::<usize>()
            .context("invalid --emit-env-json count")?;
        anyhow::ensure!(args.next().is_none(), "unexpected sidecar arguments");
        anyhow::ensure!(count <= 100, "--emit-env-json count exceeds 100");
        let values = (0..count)
            .map(|index| {
                let name = format!("TEMOTE_MCP_OP_REF_{index:03}");
                std::env::var(&name).with_context(|| format!("missing {name}"))
            })
            .collect::<Result<Vec<_>>>()?;
        serde_json::to_writer(std::io::stdout().lock(), &values)?;
        return Ok(());
    }

    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout().lock();
    let mut clients = SdkClients::default();

    for line in stdin.lock().lines() {
        let line = line.context("failed to read sidecar request")?;
        if line.len() > MAX_REQUEST_BYTES {
            anyhow::bail!("sidecar request exceeds {MAX_REQUEST_BYTES} bytes");
        }
        if line.trim().is_empty() {
            continue;
        }
        let request: Request =
            serde_json::from_str(&line).context("invalid sidecar request JSON")?;
        let outcome = clients.resolve_all(&request.account, &request.references);
        let response = match &outcome {
            Ok(result) => Response {
                id: request.id,
                ok: true,
                result: Some(result),
                error: None,
            },
            Err(error) => Response {
                id: request.id,
                ok: false,
                result: None,
                error: Some(error),
            },
        };
        serde_json::to_writer(&mut stdout, &response)?;
        stdout.write_all(b"\n")?;
        stdout.flush()?;
    }
    Ok(())
}

#[derive(Default)]
struct SdkClients {
    clients: HashMap<String, Client>,
}

impl SdkClients {
    fn resolve_all(
        &mut self,
        account: &str,
        references: &[String],
    ) -> std::result::Result<Value, String> {
        if !self.clients.contains_key(account) {
            if self.clients.len() >= MAX_CLIENTS {
                return Err("1Password SDK sidecar client limit reached".to_owned());
            }
            let auth = DesktopAuth::new(account.to_owned()).map_err(safe_sdk_error)?;
            let client = Client::builder(auth)
                .integration_name("temote-mcp")
                .integration_version(env!("CARGO_PKG_VERSION"))
                .build()
                .map_err(safe_sdk_error)?;
            self.clients.insert(account.to_owned(), client);
        }

        let client = self
            .clients
            .get_mut(account)
            .ok_or_else(|| "1Password SDK client was unavailable".to_owned())?;
        match client.secrets().resolve_all(references) {
            Ok(values) => sdk_result_json(references, values),
            Err(SdkError::SecretResolutionFailed) => Ok(sdk_reference_error_result(references)),
            Err(error) => Err(safe_sdk_error(error)),
        }
    }
}

fn sdk_result_json(
    references: &[String],
    values: Vec<String>,
) -> std::result::Result<Value, String> {
    if values.len() != references.len() {
        return Err("1Password SDK returned an unexpected number of values".to_owned());
    }
    let mut responses = Map::with_capacity(references.len());
    for (reference, value) in references.iter().zip(values) {
        responses.insert(reference.clone(), json!({"content": {"secret": value}}));
    }
    Ok(json!({"individualResponses": responses}))
}

fn sdk_reference_error_result(references: &[String]) -> Value {
    let mut responses = Map::with_capacity(references.len());
    for reference in references {
        responses.insert(
            reference.clone(),
            json!({"error": {"kind": "reference_failed"}}),
        );
    }
    json!({"individualResponses": responses})
}

fn safe_sdk_error(error: SdkError) -> String {
    match error {
        SdkError::AuthorizationDenied => "1Password SDK desktop authorization was denied",
        SdkError::DesktopSessionExpired => "1Password SDK desktop session expired",
        SdkError::SecretResolutionFailed => "1Password SDK secret resolution failed",
        _ => "1Password SDK secret resolution failed",
    }
    .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn result_shape_supports_duplicate_references() {
        let references = vec![
            "op://v/i/a".to_owned(),
            "op://v/i/b".to_owned(),
            "op://v/i/a".to_owned(),
        ];
        let result = sdk_result_json(
            &references,
            vec!["alpha".to_owned(), "beta".to_owned(), "alpha".to_owned()],
        )
        .unwrap();
        assert_eq!(
            result["individualResponses"]["op://v/i/a"]["content"]["secret"],
            "alpha"
        );
        assert_eq!(
            result["individualResponses"]["op://v/i/b"]["content"]["secret"],
            "beta"
        );
    }

    #[test]
    fn reference_failures_remain_reference_failures_for_parent_protocol() {
        let refs = vec!["op://v/i/a".to_owned(), "op://v/i/b".to_owned()];
        let result = sdk_reference_error_result(&refs);
        for reference in refs {
            assert!(result["individualResponses"][reference]["error"].is_object());
        }
    }

    #[test]
    fn sdk_errors_are_sanitized() {
        assert_eq!(
            safe_sdk_error(SdkError::AuthorizationDenied),
            "1Password SDK desktop authorization was denied"
        );
        assert_eq!(
            safe_sdk_error(SdkError::Protocol("op://secret/value".to_owned())),
            "1Password SDK secret resolution failed"
        );
    }
}
