//! Canonical source identities on regular and Mount Manager-free volumes.
use std::{
    fs, io,
    path::{Path, PathBuf},
};

pub fn canonicalize(path: &Path) -> io::Result<PathBuf> {
    match fs::canonicalize(path) {
        Ok(path) => Ok(path),
        #[cfg(windows)]
        Err(error) if matches!(error.raw_os_error(), Some(1 | 50)) => device_path(path),
        Err(error) => Err(error),
    }
}

#[cfg(windows)]
fn device_path(path: &Path) -> io::Result<PathBuf> {
    use std::{
        ffi::{c_void, OsString},
        os::windows::{ffi::OsStringExt, fs::OpenOptionsExt, io::AsRawHandle},
    };
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetFinalPathNameByHandleW(
            handle: *mut c_void,
            buffer: *mut u16,
            length: u32,
            flags: u32,
        ) -> u32;
    }
    // ImDisk can expose a healthy drive without Mount Manager registration.
    // Resolve its NT device identity through an actual handle; do not fall back
    // to an unchecked lexical path. GLOBALROOT makes this usable by Win32 APIs.
    let file = fs::OpenOptions::new()
        .read(true)
        .access_mode(0)
        .share_mode(7)
        .custom_flags(0x02000000)
        .open(path)?;
    let mut buffer = vec![0u16; 512];
    loop {
        // SAFETY: file owns the handle and buffer has the stated UTF-16 length.
        let length = unsafe {
            GetFinalPathNameByHandleW(
                file.as_raw_handle(),
                buffer.as_mut_ptr(),
                buffer.len() as u32,
                2,
            )
        } as usize;
        if length == 0 {
            return Err(io::Error::last_os_error());
        }
        if length >= buffer.len() {
            if length >= 32768 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Source path is too long",
                ));
            }
            buffer.resize(length + 1, 0);
            continue;
        }
        let resolved = OsString::from_wide(&buffer[..length]);
        if !resolved.to_string_lossy().starts_with("\\Device\\") {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Unsupported source device path",
            ));
        }
        let mut result = OsString::from(r"\\?\GLOBALROOT");
        result.push(resolved);
        return Ok(PathBuf::from(result));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn canonical_identity_resolves_alias_and_reopens_the_same_file() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("synthetic source.jsonl");
        fs::write(&file, b"synthetic evidence").unwrap();
        fs::create_dir(root.path().join("child")).unwrap();
        let alias = root
            .path()
            .join("child")
            .join("..")
            .join("synthetic source.jsonl");
        let resolved = canonicalize(&file).unwrap();
        assert_eq!(resolved, canonicalize(&alias).unwrap());
        assert_eq!(fs::read(resolved).unwrap(), b"synthetic evidence");
        assert!(canonicalize(root.path()).unwrap().is_dir());
        assert_eq!(
            canonicalize(&root.path().join("missing"))
                .unwrap_err()
                .kind(),
            io::ErrorKind::NotFound
        );
    }
}
