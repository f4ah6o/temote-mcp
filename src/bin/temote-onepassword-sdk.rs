#[cfg(target_os = "macos")]
#[cfg(target_os = "macos")]
use std::collections::HashMap;
use std::io::{BufRead, Write};

use anyhow::{Context, Result};
#[cfg(target_os = "macos")]
#[cfg(target_os = "macos")]
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::Value;
#[cfg(target_os = "macos")]
use serde_json::json;

const MAX_REQUEST_BYTES: usize = 1024 * 1024;
#[cfg(target_os = "macos")]
#[cfg(target_os = "macos")]
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
    let mut clients = SdkClients::new()?;

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
                error: Some(error.as_str()),
            },
        };
        serde_json::to_writer(&mut stdout, &response)?;
        stdout.write_all(b"\n")?;
        stdout.flush()?;
    }
    Ok(())
}

struct SdkClients {
    #[cfg(target_os = "macos")]
    library: SdkLibrary,
    #[cfg(target_os = "macos")]
    #[cfg(target_os = "macos")]
    clients: HashMap<String, u64>,
}

impl SdkClients {
    fn new() -> Result<Self> {
        #[cfg(target_os = "macos")]
        {
            Ok(Self {
                library: SdkLibrary::load()?,
                clients: HashMap::new(),
            })
        }
        #[cfg(not(target_os = "macos"))]
        {
            anyhow::bail!("1Password SDK desktop sidecar is currently supported on macOS only")
        }
    }

    fn resolve_all(
        &mut self,
        account: &str,
        references: &[String],
    ) -> std::result::Result<Value, String> {
        #[cfg(target_os = "macos")]
        {
            let client_id = match self.clients.get(account).copied() {
                Some(id) => id,
                None => {
                    if self.clients.len() >= MAX_CLIENTS {
                        return Err("1Password SDK sidecar client limit reached".to_owned());
                    }
                    let id = self.library.init_client(account)?;
                    self.clients.insert(account.to_owned(), id);
                    id
                }
            };
            let invoke = json!({
                "invocation": {
                    "clientId": client_id,
                    "parameters": {
                        "name": "SecretsResolveAll",
                        "parameters": {"secret_references": references}
                    }
                }
            });
            match self.library.call(
                account,
                "invoke",
                &serde_json::to_vec(&invoke).map_err(|e| e.to_string())?,
            ) {
                Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| e.to_string()),
                Err(error)
                    if error.contains("DesktopSessionExpired")
                        || error.contains("desktop session expired") =>
                {
                    self.clients.remove(account);
                    let id = self.library.init_client(account)?;
                    self.clients.insert(account.to_owned(), id);
                    let retry = json!({
                        "invocation": {
                            "clientId": id,
                            "parameters": {
                                "name": "SecretsResolveAll",
                                "parameters": {"secret_references": references}
                            }
                        }
                    });
                    let bytes = self.library.call(
                        account,
                        "invoke",
                        &serde_json::to_vec(&retry).map_err(|e| e.to_string())?,
                    )?;
                    serde_json::from_slice(&bytes).map_err(|e| e.to_string())
                }
                Err(error) => Err(error),
            }
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (account, references);
            Err("1Password SDK desktop sidecar is currently supported on macOS only".to_owned())
        }
    }
}

#[cfg(target_os = "macos")]
impl Drop for SdkClients {
    fn drop(&mut self) {
        for (account, client_id) in self.clients.drain() {
            if let Ok(payload) = serde_json::to_vec(&client_id) {
                let _ = self.library.call(&account, "release_client", &payload);
            }
        }
    }
}

#[cfg(target_os = "macos")]
struct SdkLibrary {
    _handle: *mut libc::c_void,
    send_message: SendMessage,
    free_response: FreeResponse,
}

#[cfg(target_os = "macos")]
type SendMessage =
    unsafe extern "C" fn(*const u8, usize, *mut *mut u8, *mut usize, *mut usize) -> i32;

#[cfg(target_os = "macos")]
type FreeResponse = unsafe extern "C" fn(*mut u8, usize, usize);

#[cfg(target_os = "macos")]
impl SdkLibrary {
    fn load() -> Result<Self> {
        use std::ffi::CString;
        use std::path::PathBuf;

        let mut candidates = vec![PathBuf::from(
            "/Applications/1Password.app/Contents/Frameworks/libop_sdk_ipc_client.dylib",
        )];
        if let Some(home) = std::env::var_os("HOME") {
            candidates.push(
                PathBuf::from(home).join(
                    "Applications/1Password.app/Contents/Frameworks/libop_sdk_ipc_client.dylib",
                ),
            );
        }
        let path = candidates
            .into_iter()
            .find(|path| path.is_file())
            .context("1Password SDK IPC library was not found; install 1Password desktop app")?;
        let c_path = CString::new(path.to_string_lossy().as_bytes())?;
        // SAFETY: c_path is a valid NUL-terminated string and RTLD_NOW requests eager symbol resolution.
        let handle = unsafe { libc::dlopen(c_path.as_ptr(), libc::RTLD_NOW) };
        anyhow::ensure!(
            !handle.is_null(),
            "failed to open 1Password SDK IPC library"
        );

        let send_name = c"op_sdk_ipc_send_message";
        let free_name = c"op_sdk_ipc_free_response";
        // SAFETY: the library is the 1Password-owned SDK IPC library and the symbols are the ABI used by the official SDKs.
        let send = unsafe { libc::dlsym(handle, send_name.as_ptr()) };
        // SAFETY: same as above.
        let free = unsafe { libc::dlsym(handle, free_name.as_ptr()) };
        if send.is_null() || free.is_null() {
            // SAFETY: handle was returned by dlopen above and has not been closed.
            unsafe { libc::dlclose(handle) };
            anyhow::bail!("1Password SDK IPC library is missing required symbols");
        }
        // SAFETY: the symbol signatures match the ABI used by the official 1Password Go SDK.
        let send_message: SendMessage = unsafe { std::mem::transmute(send) };
        // SAFETY: same ABI contract as above.
        let free_response: FreeResponse = unsafe { std::mem::transmute(free) };
        Ok(Self {
            _handle: handle,
            send_message,
            free_response,
        })
    }

    fn init_client(&self, account: &str) -> std::result::Result<u64, String> {
        let config = json!({
            "serviceAccountToken": "",
            "programmingLanguage": "Rust",
            "sdkVersion": env!("CARGO_PKG_VERSION"),
            "integrationName": "temote-mcp",
            "integrationVersion": env!("CARGO_PKG_VERSION"),
            "requestLibraryName": "temote-onepassword-sdk",
            "requestLibraryVersion": env!("CARGO_PKG_VERSION"),
            "os": "darwin",
            "osVersion": "0.0.0",
            "architecture": std::env::consts::ARCH,
            "account_name": account,
        });
        let payload = serde_json::to_vec(&config).map_err(|error| error.to_string())?;
        let bytes = self.call(account, "init_client", &payload)?;
        serde_json::from_slice(&bytes).map_err(|error| error.to_string())
    }

    fn call(
        &self,
        account: &str,
        kind: &str,
        payload: &[u8],
    ) -> std::result::Result<Vec<u8>, String> {
        let request = json!({
            "kind": kind,
            "account_name": account,
            "payload": base64::engine::general_purpose::STANDARD.encode(payload),
        });
        let input = serde_json::to_vec(&request).map_err(|error| error.to_string())?;
        let mut output_ptr: *mut u8 = std::ptr::null_mut();
        let mut output_len = 0usize;
        let mut output_cap = 0usize;
        // SAFETY: input remains alive for the call; output pointers are valid out-parameters for the documented SDK ABI.
        let code = unsafe {
            (self.send_message)(
                input.as_ptr(),
                input.len(),
                &mut output_ptr,
                &mut output_len,
                &mut output_cap,
            )
        };
        if code != 0 {
            return Err(format!("1Password SDK IPC failed with code {code}"));
        }
        if output_ptr.is_null() {
            return Err("1Password SDK IPC returned a null response".to_owned());
        }
        // SAFETY: the SDK returned output_len initialized bytes and retains ownership until free_response.
        let raw = unsafe { std::slice::from_raw_parts(output_ptr, output_len).to_vec() };
        // SAFETY: output_ptr/len/cap are exactly the allocation tuple returned by send_message.
        unsafe { (self.free_response)(output_ptr, output_len, output_cap) };
        let response: Value = serde_json::from_slice(&raw).map_err(|error| error.to_string())?;
        let success = response
            .get("success")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let payload = decode_response_payload(response.get("payload"))?;
        if success {
            Ok(payload)
        } else {
            Err(String::from_utf8_lossy(&payload).into_owned())
        }
    }
}

#[cfg(target_os = "macos")]
impl Drop for SdkLibrary {
    fn drop(&mut self) {
        if !self._handle.is_null() {
            // SAFETY: _handle is owned by this value and came from dlopen.
            unsafe { libc::dlclose(self._handle) };
        }
    }
}

#[cfg(target_os = "macos")]
fn decode_response_payload(value: Option<&Value>) -> std::result::Result<Vec<u8>, String> {
    let value = value.ok_or_else(|| "1Password SDK response is missing payload".to_owned())?;
    if let Some(encoded) = value.as_str() {
        return base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|error| error.to_string());
    }
    let array = value
        .as_array()
        .ok_or_else(|| "1Password SDK response payload has an unsupported shape".to_owned())?;
    array
        .iter()
        .map(|byte| {
            byte.as_u64()
                .filter(|byte| *byte <= u8::MAX as u64)
                .map(|byte| byte as u8)
                .ok_or_else(|| {
                    "1Password SDK response payload contains a non-byte value".to_owned()
                })
        })
        .collect()
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    #[cfg(target_os = "macos")]
    #[test]
    fn sdk_payload_decoder_accepts_official_byte_array_and_base64_shapes() {
        let bytes = json!([123, 34, 111, 107, 34, 58, 116, 114, 117, 101, 125]);
        assert_eq!(
            decode_response_payload(Some(&bytes)).unwrap(),
            br#"{"ok":true}"#
        );

        let encoded = json!(base64::engine::general_purpose::STANDARD.encode(br#"{"ok":true}"#));
        assert_eq!(
            decode_response_payload(Some(&encoded)).unwrap(),
            br#"{"ok":true}"#
        );
        assert!(decode_response_payload(Some(&json!([256]))).is_err());
        assert!(decode_response_payload(Some(&json!({"unexpected": true}))).is_err());
    }
}
