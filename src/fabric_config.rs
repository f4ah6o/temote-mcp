//! Public browser connection settings. Credentials remain in the OS store.
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use crate::fabric_browser::BrowserConnectOptions;

const MAX_BYTES: u64 = 16 * 1024;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublicSettings {
    schema_version: u32,
    gateway_url: String,
    issuer: String,
    client_id: String,
    host_id: Option<String>,
    resource: String,
    callback_port: Option<u16>,
}

impl PublicSettings {
    pub fn from_options(options: &BrowserConnectOptions) -> Result<Self> {
        crate::fabric_browser::validate_oauth_options(options)?;
        Ok(Self {
            schema_version: 1,
            gateway_url: url::Url::parse(&options.gateway_url)?
                .origin()
                .ascii_serialization(),
            issuer: url::Url::parse(&options.issuer)?
                .origin()
                .ascii_serialization(),
            client_id: options.client_id.clone(),
            host_id: options.host_id.clone(),
            resource: options.resource.clone(),
            callback_port: options.oauth_callback_port,
        })
    }

    pub fn options(&self) -> Result<BrowserConnectOptions> {
        anyhow::ensure!(
            self.schema_version == 1,
            "unsupported public Fabric settings schema"
        );
        let origins = [&self.gateway_url, &self.issuer]
            .into_iter()
            .map(|value| url::Url::parse(value).map(|url| url.origin().ascii_serialization()))
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|_| anyhow::anyhow!("invalid public Fabric settings URL"))?;
        let mut options = BrowserConnectOptions {
            gateway_url: self.gateway_url.clone(),
            issuer: self.issuer.clone(),
            client_id: self.client_id.clone(),
            host_id: self.host_id.clone(),
            resource: self.resource.clone(),
            oauth_callback_port: self.callback_port,
            permitted_origins: origins,
        };
        crate::fabric_browser::validate_oauth_options(&options)?;
        options.gateway_url = options.permitted_origins[0].clone();
        options.issuer = options.permitted_origins[1].clone();
        Ok(options)
    }
}

pub fn default_path() -> Result<PathBuf> {
    #[cfg(test)]
    return crate::test_support::private_process_root()
        .map(|root| root.join("config/fabric/public.json"))
        .map_err(anyhow::Error::msg);
    #[cfg(not(test))]
    crate::platform_paths::config_dir()
        .map(|root| root.join("temote-mcp/fabric/public.json"))
        .context("public Fabric settings directory is unavailable")
}

fn trusted_file(file: &File) -> Result<()> {
    let metadata = file.metadata()?;
    anyhow::ensure!(
        metadata.is_file() && metadata.len() <= MAX_BYTES,
        "public Fabric settings file is unsafe or oversized"
    );
    #[cfg(unix)]
    anyhow::ensure!(
        metadata.uid() == unsafe { libc::geteuid() } && metadata.mode() & 0o077 == 0,
        "public Fabric settings must be owner-only"
    );
    Ok(())
}

pub fn load(path: &Path) -> Result<Option<PublicSettings>> {
    let mut open = OpenOptions::new();
    open.read(true);
    #[cfg(unix)]
    open.custom_flags(libc::O_NOFOLLOW);
    let mut file = match open.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => anyhow::bail!("public Fabric settings cannot be read safely"),
    };
    trusted_file(&file)?;
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(MAX_BYTES + 1)
        .read_to_end(&mut bytes)?;
    anyhow::ensure!(
        bytes.len() as u64 <= MAX_BYTES,
        "public Fabric settings are oversized"
    );
    let settings: PublicSettings = serde_json::from_slice(&bytes).map_err(|_| {
        anyhow::anyhow!("public Fabric settings are malformed; run `temote fabric configure`")
    })?;
    settings.options()?;
    Ok(Some(settings))
}

pub fn load_default() -> Result<Option<PublicSettings>> {
    load(&default_path()?)
}

pub fn save(path: &Path, settings: &PublicSettings) -> Result<()> {
    settings.options()?;
    let parent = path
        .parent()
        .context("public Fabric settings path has no directory")?;
    std::fs::create_dir_all(parent)?;
    let metadata = std::fs::symlink_metadata(parent)?;
    anyhow::ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "public Fabric settings directory is unsafe"
    );
    #[cfg(unix)]
    {
        anyhow::ensure!(
            metadata.uid() == unsafe { libc::geteuid() } && metadata.mode() & 0o022 == 0,
            "public Fabric settings directory ownership is unsafe"
        );
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
    }
    // Validate any existing destination before replacement; never follow links.
    if let Ok(metadata) = std::fs::symlink_metadata(path) {
        anyhow::ensure!(
            !metadata.file_type().is_symlink(),
            "public Fabric settings destination is unsafe"
        );
        let file = File::open(path)?;
        trusted_file(&file)?;
    }
    let temporary = parent.join(format!(".public-{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| -> Result<()> {
        let mut open = OpenOptions::new();
        open.write(true).create_new(true);
        #[cfg(unix)]
        open.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        let mut file = open.open(&temporary)?;
        file.write_all(&serde_json::to_vec_pretty(settings)?)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result.context("public Fabric settings could not be saved")
}

#[cfg(test)]
mod tests {
    use super::*;
    fn options() -> BrowserConnectOptions {
        BrowserConnectOptions {
            gateway_url: "https://fabric.example".into(),
            issuer: "https://login.example".into(),
            client_id: "public-client".into(),
            host_id: Some("host-a".into()),
            resource: "https://fabric.example".into(),
            oauth_callback_port: Some(8400),
            permitted_origins: vec![
                "https://fabric.example".into(),
                "https://login.example".into(),
            ],
        }
    }
    #[test]
    fn public_settings_round_trip_and_atomic_replace() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("fabric/public.json");
        assert!(load(&path).unwrap().is_none());
        let settings = PublicSettings::from_options(&options()).unwrap();
        save(&path, &settings).unwrap();
        assert_eq!(
            load(&path)
                .unwrap()
                .unwrap()
                .options()
                .unwrap()
                .oauth_callback_port,
            Some(8400)
        );
        let mut replacement = options();
        replacement.oauth_callback_port = Some(8401);
        save(&path, &PublicSettings::from_options(&replacement).unwrap()).unwrap();
        assert_eq!(
            load(&path)
                .unwrap()
                .unwrap()
                .options()
                .unwrap()
                .oauth_callback_port,
            Some(8401)
        );
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("token") && !text.contains("secret"));
        #[cfg(unix)]
        assert_eq!(std::fs::metadata(path).unwrap().mode() & 0o777, 0o600);
    }
    #[test]
    fn malformed_or_secret_fields_are_rejected_without_echo() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("public.json");
        std::fs::write(&path, b"{\"refresh_token\":\"SECRET_SENTINEL\"}").unwrap();
        #[cfg(unix)]
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let error = load(&path).err().unwrap().to_string();
        assert!(!error.contains("SECRET_SENTINEL"));
        // Explicit configure can repair safe metadata with malformed contents.
        save(&path, &PublicSettings::from_options(&options()).unwrap()).unwrap();
        assert!(load(&path).unwrap().is_some());
        let mut invalid = options();
        invalid.issuer = "https://user:SECRET_SENTINEL@login.example/".into();
        let error = PublicSettings::from_options(&invalid)
            .err()
            .unwrap()
            .to_string();
        assert!(!error.contains("SECRET_SENTINEL"));
    }
    #[cfg(unix)]
    #[test]
    fn symlink_and_public_file_are_rejected() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("target");
        save(&target, &PublicSettings::from_options(&options()).unwrap()).unwrap();
        let link = root.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(load(&link).is_err());
        assert!(save(&link, &PublicSettings::from_options(&options()).unwrap()).is_err());
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(load(&target).is_err());
    }
}
