use super::*;
use pretty_assertions::assert_eq;

fn credential(token: &str) -> CopilotCredentials {
    CopilotCredentials {
        github_token: token.to_string(),
        copilot_token: "tid=1;proxy-ep=proxy.individual.githubcopilot.com".to_string(),
        expires_at: 12345,
        base_url: "https://api.individual.githubcopilot.com".to_string(),
    }
}

#[test]
fn credentials_and_models_are_stored_without_a_keyring() -> anyhow::Result<()> {
    let home = tempfile::tempdir()?;
    let original = credential("first");
    assert!(load_credentials(home.path())?.is_none());
    save_credentials(home.path(), &original, CredentialWrite::Login)?;
    assert_eq!(
        serde_json::to_value(crate::github_copilot::load(home.path())?)?,
        serde_json::to_value(Some(&original))?
    );
    save_models_cache(home.path(), "{\"models\":[]}")?;
    assert_eq!(
        load_models_cache(home.path())?,
        Some("{\"models\":[]}".to_string())
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let dir = storage_dir(home.path());
        assert_eq!(fs::metadata(&dir)?.permissions().mode() & 0o777, 0o700);
        for filename in [CREDENTIALS_FILE, MODELS_FILE] {
            assert_eq!(
                fs::metadata(dir.join(filename))?.permissions().mode() & 0o777,
                0o600
            );
        }
    }

    assert!(crate::github_copilot::logout(home.path())?);
    assert!(crate::github_copilot::load(home.path())?.is_none());
    assert!(load_models_cache(home.path())?.is_none());
    assert!(!crate::github_copilot::logout(home.path())?);
    Ok(())
}

#[test]
fn refresh_does_not_replace_another_login() -> anyhow::Result<()> {
    let home = tempfile::tempdir()?;
    save_credentials(home.path(), &credential("first"), CredentialWrite::Login)?;
    save_credentials(home.path(), &credential("second"), CredentialWrite::Login)?;
    let error = save_credentials(
        home.path(),
        &credential("first"),
        CredentialWrite::RefreshIfGithubToken("first"),
    )
    .expect_err("refresh of previous account must fail");
    assert!(error.to_string().contains("login changed"));
    assert_eq!(
        load_credentials(home.path())?.unwrap().github_token,
        "second"
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn rejects_world_readable_credentials_and_symlinks() -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let home = tempfile::tempdir()?;
    save_credentials(home.path(), &credential("first"), CredentialWrite::Login)?;
    let path = storage_dir(home.path()).join(CREDENTIALS_FILE);
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644))?;
    assert_eq!(
        load_credentials(home.path())
            .err()
            .expect("world-readable file must be rejected")
            .kind(),
        io::ErrorKind::PermissionDenied
    );
    fs::remove_file(&path)?;
    std::os::unix::fs::symlink(home.path().join("other-file"), &path)?;
    assert!(load_credentials(home.path()).is_err());
    Ok(())
}

#[test]
fn rejects_invalid_or_oversized_cache() -> anyhow::Result<()> {
    let home = tempfile::tempdir()?;
    assert!(save_models_cache(home.path(), &"x".repeat(MAX_MODELS_BYTES as usize + 1)).is_err());
    assert!(load_models_cache(home.path())?.is_none());
    Ok(())
}
