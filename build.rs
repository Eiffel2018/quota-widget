//! Embed the existing artwork in Windows executable resources, not only HWNDs.
use std::{env, fs, path::PathBuf, process::Command};

fn resource(output: &mut Vec<u8>, kind: u16, id: u16, data: &[u8]) {
    output.extend_from_slice(&(data.len() as u32).to_le_bytes());
    output.extend_from_slice(&32u32.to_le_bytes());
    for value in [0xffff, kind, 0xffff, id] {
        output.extend_from_slice(&value.to_le_bytes());
    }
    output.extend_from_slice(&0u32.to_le_bytes()); // DataVersion
    output.extend_from_slice(&(if kind == 0 { 0u16 } else { 0x1030u16 }).to_le_bytes());
    output.extend_from_slice(&0u16.to_le_bytes()); // language-neutral
    output.extend_from_slice(&[0; 8]); // Version, Characteristics
    output.extend_from_slice(data);
    while !output.len().is_multiple_of(4) {
        output.push(0);
    }
}

fn main() {
    println!("cargo:rerun-if-changed=assets/icon.ico");
    println!("cargo:rerun-if-changed=build.rs");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let icon = include_bytes!("assets/icon.ico");
    assert_eq!(&icon[..4], &[0, 0, 1, 0], "Expected ICO artwork");
    let count = u16::from_le_bytes(icon[4..6].try_into().unwrap());
    let mut group = icon[..6].to_vec();
    let mut compiled = Vec::new();
    resource(&mut compiled, 0, 0, &[]); // required null .res header
    for i in 0..count {
        let entry = &icon[6 + usize::from(i) * 16..][..16];
        let length = u32::from_le_bytes(entry[8..12].try_into().unwrap()) as usize;
        let offset = u32::from_le_bytes(entry[12..16].try_into().unwrap()) as usize;
        resource(&mut compiled, 3, i + 1, &icon[offset..offset + length]);
        group.extend_from_slice(&entry[..4]);
        group.extend_from_slice(&1u16.to_le_bytes()); // planes
        group.extend_from_slice(&entry[6..12]);
        group.extend_from_slice(&(i + 1).to_le_bytes());
    }
    resource(&mut compiled, 14, 1, &group);
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    let res = out.join("quota-widget-icon.res");
    fs::write(&res, compiled).unwrap();
    let linked = if env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        res
    } else {
        let object = out.join("quota-widget-icon.o");
        let target = env::var("TARGET").unwrap();
        let compiler = env::var("WINDRES").unwrap_or_else(|_| {
            format!(
                "{}-windres",
                target.replace("-pc-windows-gnu", "-w64-mingw32")
            )
        });
        assert!(Command::new(compiler)
            .args(["-J", "res", "-O", "coff", "-i"])
            .arg(&res)
            .arg("-o")
            .arg(&object)
            .status()
            .expect("MinGW windres is required for GNU Windows builds")
            .success());
        object
    };
    println!("cargo:rustc-link-arg-bin=quota-widget={}", linked.display());
}
