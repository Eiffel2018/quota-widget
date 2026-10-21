//! Verify the Shell can read icons without starting the window or an account.
#![cfg(windows)]
use std::{ffi::c_void, os::windows::ffi::OsStrExt, ptr};

#[test]
fn executable_has_extractable_large_and_small_shell_icons() {
    #[link(name = "shell32")]
    unsafe extern "system" {
        fn ExtractIconExW(
            path: *const u16,
            index: i32,
            large: *mut *mut c_void,
            small: *mut *mut c_void,
            count: u32,
        ) -> u32;
    }
    #[link(name = "user32")]
    unsafe extern "system" {
        fn DestroyIcon(icon: *mut c_void) -> i32;
    }
    let path: Vec<u16> = std::ffi::OsStr::new(env!("CARGO_BIN_EXE_quota-widget"))
        .encode_wide()
        .chain(Some(0))
        .collect();
    let mut large = ptr::null_mut();
    let mut small = ptr::null_mut();
    // SAFETY: path is NUL-terminated and both output pointers are writable.
    // ExtractIconEx loads resources as data, without executing the program.
    let count = unsafe { ExtractIconExW(path.as_ptr(), 0, &mut large, &mut small, 1) };
    let both_present = !large.is_null() && !small.is_null();
    // SAFETY: these non-null handles are owned outputs of ExtractIconExW.
    unsafe {
        if !large.is_null() {
            DestroyIcon(large);
        }
        if !small.is_null() {
            DestroyIcon(small);
        }
    }
    assert!(
        count > 0,
        "EXE must expose a Shell icon without a live window"
    );
    assert!(
        both_present,
        "Both taskbar-sized and large Shell icons are required"
    );
}
