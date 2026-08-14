//! Small shared primitives for private local descriptor files and tokens.

use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    path::Path,
};

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

use serde::Serialize;
pub(crate) fn random_token() -> io::Result<String> {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes).map_err(io::Error::other)?;
    let mut token = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        write!(&mut token, "{byte:02x}").expect("writing to a String cannot fail");
    }
    Ok(token)
}

pub(crate) fn constant_time_equal(left: &str, right: &str) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.bytes()
        .zip(right.bytes())
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        })
        == 0
}

pub(crate) fn write_private_json(path: &Path, value: &impl Serialize) -> io::Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    // A failed `create_new` means the path belongs to somebody else and must
    // remain untouched. Cleanup begins only after this process creates it.
    let mut file = options.open(path)?;
    let result = (|| {
        let bytes = serde_json::to_vec(value).map_err(io::Error::other)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        #[cfg(unix)]
        {
            let mode = file.metadata()?.permissions().mode() & 0o777;
            if mode != 0o600 {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "descriptor permissions are not private",
                ));
            }
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(path);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_is_fixed_length_and_comparison_checks_every_equal_length_byte() {
        let token = random_token().unwrap();
        assert_eq!(token.len(), 64);
        assert!(constant_time_equal(&token, &token));
        assert!(!constant_time_equal(&token, &format!("0{}", &token[1..])));
        assert!(!constant_time_equal(&token, &token[..63]));
    }

    #[test]
    fn private_json_never_overwrites_an_existing_path() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("descriptor.json");
        std::fs::write(&path, b"owned elsewhere").unwrap();

        assert!(write_private_json(&path, &serde_json::json!({"secret": true})).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"owned elsewhere");
    }
}
