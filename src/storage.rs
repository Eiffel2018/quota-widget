//! One process owns writes. Atomic replacements never overwrite a newer instance.
use crate::tokens::Pricing;
use fs2::FileExt;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub dark: bool,
    pub on_top: bool,
    pub codex_path: String,
    pub position: Option<[f32; 2]>,
    pub size: [f32; 2],
    pub bucket: String,
    pub window: String,
    pub history_days: u32,
    pub hide_unchanged: bool,
    pub task_visible_rows: u32,
    pub sessions_path: String,
    pub import_enabled: bool,
    pub pricing: Pricing,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            dark: true,
            on_top: true,
            codex_path: String::new(),
            position: None,
            size: [310., 360.],
            bucket: "codex".into(),
            window: "weekly".into(),
            history_days: 30,
            hide_unchanged: true,
            task_visible_rows: 10,
            sessions_path: default_sessions().to_string_lossy().into(),
            import_enabled: true,
            pricing: Pricing::official_standard(),
        }
    }
}
pub fn home() -> PathBuf {
    std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}
pub fn default_sessions() -> PathBuf {
    std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".codex"))
        .join("sessions")
}
pub fn data_dir() -> PathBuf {
    if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(home)
            .join("quota-widget")
    } else if cfg!(target_os = "macos") {
        home().join("Library/Application Support/quota-widget")
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home().join(".config"))
            .join("quota-widget")
    }
}
pub struct DataLock {
    _file: File,
}
impl DataLock {
    pub fn acquire(dir: &Path) -> io::Result<Self> {
        fs::create_dir_all(dir)?;
        let f = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(dir.join("writer.lock"))?;
        f.try_lock_exclusive().map_err(|_| {
            io::Error::new(
                io::ErrorKind::WouldBlock,
                "Another Quota Widget owns this data directory",
            )
        })?;
        Ok(Self { _file: f })
    }
}
pub fn load_json<T: DeserializeOwned>(path: &Path) -> io::Result<T> {
    let metadata = fs::metadata(path)?;
    if metadata.len() > 256 * 1024 * 1024 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "File too large"));
    }
    serde_json::from_slice(&fs::read(path)?).map_err(io::Error::other)
}
pub fn save_json<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("Missing parent"))?;
    fs::create_dir_all(parent)?;
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    let bytes = serde_json::to_vec_pretty(value).map_err(io::Error::other)?;
    let mut f = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&tmp)?;
    f.write_all(&bytes)?;
    f.sync_all()?;
    drop(f);
    // rename over an existing file is atomic on Unix; Windows needs ReplaceFileW.
    #[cfg(not(windows))]
    fs::rename(&tmp, path)?;
    #[cfg(windows)]
    replace_windows(&tmp, path)?;
    Ok(())
}
#[cfg(windows)]
fn replace_windows(from: &Path, to: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    #[link(name = "kernel32")]
    extern "system" {
        fn MoveFileExW(existing: *const u16, new: *const u16, flags: u32) -> i32;
    }
    let a: Vec<u16> = from.as_os_str().encode_wide().chain(Some(0)).collect();
    let b: Vec<u16> = to.as_os_str().encode_wide().chain(Some(0)).collect();
    // MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH. Same-directory move.
    if unsafe { MoveFileExW(a.as_ptr(), b.as_ptr(), 1 | 8) } == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn two_writers_are_rejected_and_crash_lock_released() {
        let d = tempfile::tempdir().unwrap();
        let a = DataLock::acquire(d.path()).unwrap();
        assert!(DataLock::acquire(d.path()).is_err());
        drop(a);
        assert!(DataLock::acquire(d.path()).is_ok());
    }
    #[test]
    fn atomic_json_roundtrip() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("state.json");
        save_json(&p, &vec![1, 2]).unwrap();
        save_json(&p, &vec![3]).unwrap();
        assert_eq!(load_json::<Vec<i32>>(&p).unwrap(), vec![3]);
    }
}
