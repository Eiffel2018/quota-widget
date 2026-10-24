//! One process owns writes. Atomic replacements never overwrite a newer instance.
use crate::tokens::Pricing;
use fs2::FileExt;
use serde::{de::DeserializeOwned, Deserialize, Deserializer, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub dark: bool,
    pub language: crate::i18n::Language,
    pub on_top: bool,
    pub codex_path: String,
    pub position: Option<[f32; 2]>,
    pub history_position: Option<[f32; 2]>,
    pub tokens_position: Option<[f32; 2]>,
    pub settings_position: Option<[f32; 2]>,
    pub prediction_position: Option<[f32; 2]>,
    pub size: [f32; 2],
    #[serde(deserialize_with = "deserialize_bucket")]
    pub bucket: String,
    pub window: String,
    pub history_days: u32,
    pub hide_unchanged: bool,
    // Retained for reading older settings; the widget now scrolls by window height.
    pub task_visible_rows: u32,
    #[serde(default)]
    pub compact_layout_version: u32,
    pub font_scale_percent: u32,
    pub reset_reminder_local: String,
    pub utc_offset_minutes: i32,
    pub reset_prediction_utc: Option<i64>,
    pub reset_prediction_set_at: Option<i64>,
    pub reset_sound_enabled: bool,
    pub sessions_path: String,
    pub import_enabled: bool,
    pub pricing: Pricing,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            dark: true,
            language: crate::i18n::Language::Traditional,
            on_top: true,
            codex_path: String::new(),
            position: None,
            history_position: None,
            tokens_position: None,
            settings_position: None,
            prediction_position: None,
            size: [244., 158. * 244. / 310.],
            bucket: default_bucket(),
            window: "weekly".into(),
            history_days: 30,
            hide_unchanged: true,
            task_visible_rows: 10,
            compact_layout_version: 4,
            font_scale_percent: 100,
            reset_reminder_local: String::new(),
            utc_offset_minutes: 480,
            reset_prediction_utc: None,
            reset_prediction_set_at: None,
            reset_sound_enabled: true,
            sessions_path: default_sessions().to_string_lossy().into(),
            import_enabled: true,
            pricing: Pricing::official_standard(),
        }
    }
}
impl Settings {
    pub fn prediction_target(&self) -> Option<i64> {
        self.reset_prediction_utc
            .or_else(|| crate::display_time::parse_local(&self.reset_reminder_local, 480))
    }
    pub fn clear_prediction(&mut self) {
        self.reset_reminder_local.clear();
        self.reset_prediction_utc = None;
        self.reset_prediction_set_at = None;
    }
}
fn default_bucket() -> String {
    "codex".into()
}

/// Older settings stored the automatic selection as `Option<String>::None`.
/// Accept only that null or a string; all other fields keep their strict types.
fn deserialize_bucket<'de, D: Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    Option::<String>::deserialize(deserializer).map(|bucket| bucket.unwrap_or_else(default_bucket))
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
#[derive(Debug)]
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
    serde_json::from_slice(&read_json_bytes(path)?).map_err(io::Error::other)
}

fn read_json_bytes(path: &Path) -> io::Result<Vec<u8>> {
    let metadata = fs::metadata(path)?;
    if metadata.len() > 256 * 1024 * 1024 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "File too large"));
    }
    fs::read(path)
}

/// Preserve the exact old history bytes before the first authorized migration
/// write. A unique create-new backup never replaces any existing backup. A copy
/// error aborts the caller's replacement, leaving the original untouched.
pub fn backup_legacy_history(path: &Path) -> io::Result<PathBuf> {
    backup_legacy_file(path)
}

fn backup_legacy_file(path: &Path) -> io::Result<PathBuf> {
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::other("Missing legacy filename"))?
        .to_string_lossy();
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(io::Error::other)?
        .as_nanos();
    let backup = path.with_file_name(format!("{name}.pre-0.3.4-{stamp}.bak"));
    let mut input = File::open(path)?;
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&backup)?;
    let result = io::copy(&mut input, &mut output).and_then(|_| output.sync_all());
    drop(output);
    if let Err(error) = result {
        // This exact file was just created by us; never leave an incomplete
        // copy labeled as a usable backup. The source has not been touched.
        let _ = fs::remove_file(&backup);
        return Err(error);
    }
    Ok(backup)
}

/// Called only for an authorized save while the caller holds the data lock.
/// Revalidate the existing file on every replacement: even a caller holding
/// defaults after a failed load must not overwrite corrupt settings. Loading
/// settings itself never writes or creates a backup.
pub fn save_settings(path: &Path, settings: &Settings) -> io::Result<()> {
    match read_json_bytes(path) {
        Ok(bytes) => {
            serde_json::from_slice::<Settings>(&bytes).map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("Invalid existing settings; original preserved: {error}"),
                )
            })?;
            // Inspect the same validated bytes so a missing bucket (already
            // supported by defaults) does not trigger a legacy-null backup.
            let raw: serde_json::Value =
                serde_json::from_slice(&bytes).map_err(io::Error::other)?;
            if !raw.is_object() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Invalid existing settings; original preserved: expected a JSON object",
                ));
            }
            if raw.get("bucket").is_some_and(serde_json::Value::is_null) {
                backup_legacy_file(path).map_err(|error| {
                    io::Error::other(format!(
                        "Legacy settings backup failed; original preserved: {error}"
                    ))
                })?;
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    save_json(path, settings)
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
