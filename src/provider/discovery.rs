use std::{
    env, fs, io,
    path::{Path, PathBuf},
};

use super::ProviderError;

/// Resolve the exact configured provider without inventorying unrelated PATH entries.
pub fn discover_provider(slug: &str) -> Result<PathBuf, ProviderError> {
    let filename = if cfg!(windows) {
        format!("utterpipe-{slug}.exe")
    } else {
        format!("utterpipe-{slug}")
    };
    let current_exe = env::current_exe()
        .and_then(fs::canonicalize)
        .map_err(|source| {
            ProviderError::Discovery(format!(
                "running executable could not be resolved: {source}"
            ))
        })?;

    let mut candidates = Vec::new();
    if let Some(parent) = current_exe.parent() {
        candidates.push(parent.join(&filename));
    }
    if let Some(path) = env::var_os("PATH") {
        candidates.extend(
            env::split_paths(&path)
                .filter(|directory| directory.is_absolute() && !directory.as_os_str().is_empty())
                .map(|directory| directory.join(&filename)),
        );
    }

    for candidate in candidates {
        match validate_candidate(&candidate) {
            Ok(path) => return Ok(path),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(_) => {}
        }
    }
    Err(ProviderError::Discovery(format!(
        "provider '{slug}' was not found beside Agent Speak or in an absolute PATH directory; install {filename} and run again"
    )))
}

fn validate_candidate(candidate: &Path) -> io::Result<PathBuf> {
    let canonical = fs::canonicalize(candidate)?;
    if !fs::metadata(&canonical)?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "not a regular file",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if fs::metadata(&canonical)?.permissions().mode() & 0o111 == 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "not executable",
            ));
        }
    }
    Ok(canonical)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_path_entries_are_ignored() {
        let entries: Vec<_> = env::split_paths(&std::ffi::OsString::from("relative"))
            .filter(|path| path.is_absolute() && !path.as_os_str().is_empty())
            .collect();
        assert!(entries.is_empty());
    }
}
