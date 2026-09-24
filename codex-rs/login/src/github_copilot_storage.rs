//! Private, file-backed storage for Copilot credentials and model metadata.

use std::fs;
use std::fs::File;
use std::fs::OpenOptions;
use std::io;
use std::io::Read;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;

#[cfg(unix)]
use std::os::unix::fs::DirBuilderExt;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use rand::TryRngCore;
use rand::rngs::OsRng;

use super::CopilotCredentials;

const CREDENTIALS_FILE: &str = "credentials.json";
const MODELS_FILE: &str = "models.json";
const MAX_CREDENTIALS_BYTES: u64 = 32 * 1024;
const MAX_MODELS_BYTES: u64 = 4 * 1024 * 1024;

pub(super) enum CredentialWrite<'a> {
    Login,
    RefreshIfGithubToken(&'a str),
}

fn storage_dir(home: &Path) -> PathBuf {
    home.join("github-copilot")
}

fn check_dir(dir: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(dir)?;
    if !metadata.is_dir() {
        return Err(io::Error::other(
            "Copilot credential directory is not a directory",
        ));
    }
    #[cfg(unix)]
    if metadata.permissions().mode() & 0o077 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Copilot credential directory must have permissions 0700",
        ));
    }
    Ok(())
}

fn create_dir(home: &Path) -> io::Result<PathBuf> {
    let dir = storage_dir(home);
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    builder.mode(0o700);
    builder.create(&dir)?;
    check_dir(&dir)?;
    Ok(dir)
}

fn read_file(home: &Path, filename: &str, max_bytes: u64) -> io::Result<Option<Vec<u8>>> {
    let dir = storage_dir(home);
    match check_dir(&dir) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    }
    let path = dir.join(filename);
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if !metadata.is_file() {
        return Err(io::Error::other(format!(
            "Copilot storage file is not a regular file: {}",
            path.display()
        )));
    }
    #[cfg(unix)]
    if metadata.permissions().mode() & 0o077 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "Copilot storage file must have permissions 0600: {}",
                path.display()
            ),
        ));
    }
    if metadata.len() > max_bytes {
        return Err(io::Error::other("Copilot storage file is too large"));
    }
    let mut bytes = Vec::new();
    File::open(&path)?
        .take(max_bytes + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max_bytes {
        return Err(io::Error::other("Copilot storage file is too large"));
    }
    Ok(Some(bytes))
}

fn with_lock<T>(home: &Path, action: impl FnOnce(&Path) -> io::Result<T>) -> io::Result<T> {
    let dir = create_dir(home)?;
    let mut options = OpenOptions::new();
    options.create(true).read(true).write(true);
    #[cfg(unix)]
    options.mode(0o600);
    let lock = options.open(dir.join(".lock"))?;
    lock.lock()?;
    action(&dir)
}

fn write_file(dir: &Path, filename: &str, bytes: &[u8]) -> io::Result<()> {
    let nonce = OsRng.try_next_u64().map_err(io::Error::other)?;
    let temporary = dir.join(format!(".{filename}.{}-{nonce:x}.tmp", std::process::id()));
    let path = dir.join(filename);
    let result = (|| {
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options.open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, &path)?;
        #[cfg(unix)]
        File::open(dir)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

pub(super) fn load_credentials(home: &Path) -> io::Result<Option<CopilotCredentials>> {
    read_file(home, CREDENTIALS_FILE, MAX_CREDENTIALS_BYTES)?
        .map(|bytes| serde_json::from_slice(&bytes).map_err(io::Error::other))
        .transpose()
}

pub(super) fn save_credentials(
    home: &Path,
    credential: &CopilotCredentials,
    write: CredentialWrite<'_>,
) -> io::Result<()> {
    let bytes = serde_json::to_vec(credential).map_err(io::Error::other)?;
    if bytes.len() as u64 > MAX_CREDENTIALS_BYTES {
        return Err(io::Error::other("Copilot credentials are too large"));
    }
    with_lock(home, |dir| {
        if let CredentialWrite::RefreshIfGithubToken(token) = write
            && super::load(home)?
                .as_ref()
                .map(|stored| stored.github_token.as_str())
                != Some(token)
        {
            return Err(io::Error::other(
                "GitHub Copilot login changed while refreshing its token",
            ));
        }
        write_file(dir, CREDENTIALS_FILE, &bytes)
    })
}

pub(super) fn load_models_cache(home: &Path) -> io::Result<Option<String>> {
    read_file(home, MODELS_FILE, MAX_MODELS_BYTES)?
        .map(|bytes| String::from_utf8(bytes).map_err(io::Error::other))
        .transpose()
}

pub(super) fn save_models_cache(home: &Path, value: &str) -> io::Result<()> {
    if value.len() as u64 > MAX_MODELS_BYTES {
        return Err(io::Error::other("Copilot model cache is too large"));
    }
    with_lock(home, |dir| write_file(dir, MODELS_FILE, value.as_bytes()))
}

pub(super) fn remove_files(home: &Path) -> io::Result<bool> {
    let dir = storage_dir(home);
    match check_dir(&dir) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    }
    with_lock(home, |dir| {
        let mut removed = false;
        for filename in [CREDENTIALS_FILE, MODELS_FILE] {
            match fs::remove_file(dir.join(filename)) {
                Ok(()) if filename == CREDENTIALS_FILE => removed = true,
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        Ok(removed)
    })
}

#[cfg(test)]
#[path = "github_copilot_storage_tests.rs"]
mod tests;
