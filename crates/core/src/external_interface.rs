//! One user-controlled switch shared by external observation and model routing.

use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const FILE_NAME: &str = "external-interface.json";
static SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    pub schema_version: u32,
    pub enabled: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccessError {
    Disabled,
    InvalidSettings,
}

impl AccessError {
    pub const fn code(self) -> &'static str {
        match self {
            Self::Disabled => "external_interface_disabled",
            Self::InvalidSettings => "external_interface_settings_invalid",
        }
    }
}

pub fn path_for_db(db: &Path) -> PathBuf {
    db.with_file_name(FILE_NAME)
}

pub fn settings_path() -> PathBuf {
    path_for_db(&crate::coordination::readonly_db_path())
}

pub fn read(path: &Path) -> Result<Settings, AccessError> {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            // Existing installations already expose the interface.
            return Ok(Settings {
                schema_version: 1,
                enabled: true,
            });
        }
        Err(_) => return Err(AccessError::InvalidSettings),
    };
    let mut bytes = Vec::new();
    file.take(4097)
        .read_to_end(&mut bytes)
        .map_err(|_| AccessError::InvalidSettings)?;
    if bytes.len() > 4096 {
        return Err(AccessError::InvalidSettings);
    }
    let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(&bytes);
    let settings: Settings =
        serde_json::from_slice(bytes).map_err(|_| AccessError::InvalidSettings)?;
    if settings.schema_version != 1 {
        return Err(AccessError::InvalidSettings);
    }
    Ok(settings)
}

pub fn check_paths(paths: &[PathBuf]) -> Result<(), AccessError> {
    for path in paths {
        if !read(path)?.enabled {
            return Err(AccessError::Disabled);
        }
    }
    Ok(())
}

pub fn check(db: &Path) -> Result<(), AccessError> {
    let global = settings_path();
    let selected = path_for_db(db);
    if global == selected {
        check_paths(&[global])
    } else {
        // --db must not bypass the user's global switch.
        check_paths(&[global, selected])
    }
}

pub fn write(path: &Path, enabled: bool) -> std::io::Result<Settings> {
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("Invalid settings directory"))?;
    std::fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(
        ".external-interface-{}-{}.tmp",
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let settings = Settings {
        schema_version: 1,
        enabled,
    };
    let result = (|| {
        let bytes = serde_json::to_vec_pretty(&settings).map_err(std::io::Error::other)?;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temporary, path)?;
        Ok(settings)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "gtt-interface-test-{}-{}",
                std::process::id(),
                SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
        fn path(&self) -> PathBuf {
            self.0.join(FILE_NAME)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }
    #[test]
    fn missing_settings_preserve_legacy_enabled_without_writes() {
        let fixture = Fixture::new();
        assert!(read(&fixture.path()).unwrap().enabled);
        assert!(!fixture.path().exists());
    }
    #[test]
    fn switch_persists_and_replaces_previous_value() {
        let fixture = Fixture::new();
        write(&fixture.path(), false).unwrap();
        assert_eq!(check_paths(&[fixture.path()]), Err(AccessError::Disabled));
        write(&fixture.path(), true).unwrap();
        assert!(read(&fixture.path()).unwrap().enabled);
        assert_eq!(std::fs::read_dir(&fixture.0).unwrap().count(), 1);
    }
    #[test]
    fn damaged_unknown_or_non_boolean_settings_fail_closed() {
        let fixture = Fixture::new();
        for value in [
            "",
            "{}",
            "{\"schema_version\":1,\"enabled\":1}",
            "{\"schema_version\":2,\"enabled\":true}",
            "{\"schema_version\":1,\"enabled\":true,\"other\":0}",
        ] {
            std::fs::write(fixture.path(), value).unwrap();
            assert!(matches!(
                read(&fixture.path()),
                Err(AccessError::InvalidSettings)
            ));
        }
    }
    #[test]
    fn global_off_blocks_other_ledgers_and_bom_is_supported() {
        let fixture = Fixture::new();
        let selected = fixture.0.join("custom.json");
        write(&fixture.path(), false).unwrap();
        write(&selected, true).unwrap();
        assert_eq!(
            check_paths(&[fixture.path(), selected.clone()]),
            Err(AccessError::Disabled)
        );
        std::fs::write(
            fixture.path(),
            b"\xef\xbb\xbf{\"schema_version\":1,\"enabled\":true}",
        )
        .unwrap();
        assert!(check_paths(&[fixture.path(), selected]).is_ok());
    }
}
