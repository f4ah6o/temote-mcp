use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub permitted_directories: Vec<PathBuf>,
    #[serde(default)]
    pub default_cwd: Option<PathBuf>,
}

pub fn state_dir() -> Result<PathBuf> {
    dirs::state_dir()
        .or_else(dirs::data_local_dir)
        .map(|path| path.join("local-mcp"))
        .context("could not determine a local state directory")
}

fn config_path() -> Result<PathBuf> {
    Ok(state_dir()?.join("config.json"))
}

pub async fn load() -> Result<Config> {
    let path = config_path()?;
    match tokio::fs::read(&path).await {
        Ok(bytes) => serde_json::from_slice(&bytes).context("invalid local-mcp config"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
        Err(error) => Err(error).context("failed to read local-mcp config"),
    }
}

async fn save(config: &Config) -> Result<()> {
    let path = config_path()?;
    tokio::fs::create_dir_all(path.parent().unwrap()).await?;
    let temporary = path.with_extension(format!("json.{}.tmp", std::process::id()));
    tokio::fs::write(&temporary, serde_json::to_vec_pretty(config)?).await?;
    tokio::fs::rename(temporary, path).await?;
    Ok(())
}

fn canonical_directory(path: &Path) -> Result<PathBuf> {
    let path = std::fs::canonicalize(path)
        .with_context(|| format!("cannot resolve {}", path.display()))?;
    anyhow::ensure!(path.is_dir(), "{} is not a directory", path.display());
    Ok(path)
}

pub async fn permit(path: &Path) -> Result<PathBuf> {
    let path = canonical_directory(path)?;
    let mut config = load().await?;
    if !config.permitted_directories.contains(&path) {
        config.permitted_directories.push(path.clone());
        config.permitted_directories.sort();
        save(&config).await?;
    }
    Ok(path)
}

pub async fn revoke(path: &Path) -> Result<PathBuf> {
    let path = canonical_directory(path)?;
    let mut config = load().await?;
    config.permitted_directories.retain(|item| item != &path);
    save(&config).await?;
    Ok(path)
}

pub async fn set_default_cwd(path: &Path) -> Result<PathBuf> {
    let path = canonical_directory(path)?;
    let mut config = load().await?;
    config.default_cwd = Some(path.clone());
    save(&config).await?;
    Ok(path)
}
