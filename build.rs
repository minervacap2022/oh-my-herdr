use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

fn zig_target(target: &str) -> &str {
    match target {
        "x86_64-unknown-linux-gnu" => "x86_64-linux-gnu",
        "aarch64-unknown-linux-gnu" => "aarch64-linux-gnu",
        "x86_64-unknown-linux-musl" => "x86_64-linux-musl",
        "aarch64-unknown-linux-musl" => "aarch64-linux-musl",
        "x86_64-apple-darwin" => "x86_64-macos",
        "aarch64-apple-darwin" => "aarch64-macos",
        "x86_64-pc-windows-msvc" => "x86_64-windows-msvc",
        "aarch64-pc-windows-msvc" => "aarch64-windows-msvc",
        other => panic!("unsupported target for libghostty-vt build: {other}"),
    }
}

fn env_bool(name: &str) -> Option<bool> {
    match env::var(name) {
        Ok(value) => match value.to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => Some(true),
            "0" | "false" | "no" | "off" => Some(false),
            other => panic!("invalid boolean value for {name}: {other}"),
        },
        Err(env::VarError::NotPresent) => None,
        Err(err) => panic!("failed to read {name}: {err}"),
    }
}

#[cfg(target_os = "macos")]
fn rearchive_macos_static_library(lib_dir: &std::path::Path) {
    let static_lib = lib_dir.join("libghostty-vt.a");
    let archive_dir =
        PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR")).join("libghostty-vt-archive");
    if archive_dir.exists() {
        fs::remove_dir_all(&archive_dir).expect("failed to clear libghostty-vt archive directory");
    }
    fs::create_dir_all(&archive_dir).expect("failed to create libghostty-vt archive directory");

    let status = Command::new("ar")
        .arg("-x")
        .arg(&static_lib)
        .current_dir(&archive_dir)
        .status()
        .expect("failed to extract macOS libghostty-vt archive");
    assert!(
        status.success(),
        "failed to extract macOS libghostty-vt archive: {status}"
    );

    let mut objects = fs::read_dir(&archive_dir)
        .expect("failed to read extracted macOS libghostty-vt archive")
        .map(|entry| {
            entry
                .expect("failed to read extracted archive member")
                .path()
        })
        .filter(|path| path.extension().is_some_and(|extension| extension == "o"))
        .collect::<Vec<_>>();
    objects.sort();
    assert!(
        !objects.is_empty(),
        "macOS libghostty-vt archive had no object files"
    );
    for object in &objects {
        let mut permissions = fs::metadata(object)
            .expect("failed to inspect extracted macOS libghostty-vt object")
            .permissions();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            permissions.set_mode(0o644);
        }
        fs::set_permissions(object, permissions)
            .expect("failed to make extracted macOS libghostty-vt object readable");
    }

    let repacked_lib = archive_dir.join("libghostty-vt-repacked.a");
    let status = Command::new("xcrun")
        .arg("libtool")
        .arg("-static")
        .arg("-o")
        .arg(&repacked_lib)
        .args(&objects)
        .status()
        .expect("failed to run macOS libtool for libghostty-vt");
    assert!(
        status.success(),
        "failed to rearchive macOS libghostty-vt: {status}"
    );
    fs::rename(&repacked_lib, &static_lib).expect("failed to install repacked macOS libghostty-vt");
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=vendor/libghostty-vt.vendor.json");
    println!("cargo:rerun-if-changed=vendor/libghostty-vt/build.zig");
    println!("cargo:rerun-if-changed=vendor/libghostty-vt/build.zig.zon");
    println!("cargo:rerun-if-changed=vendor/libghostty-vt/include");
    println!("cargo:rerun-if-changed=vendor/libghostty-vt/pkg");
    println!("cargo:rerun-if-changed=vendor/libghostty-vt/src");
    println!("cargo:rerun-if-changed=vendor/libghostty-vt/VERSION");
    println!("cargo:rerun-if-env-changed=LIBGHOSTTY_VT_OPTIMIZE");
    println!("cargo:rerun-if-env-changed=LIBGHOSTTY_VT_SIMD");
    println!("cargo:rerun-if-env-changed=LIBGHOSTTY_VT_ZIG_SYSTEM_DIR");
    println!("cargo:rerun-if-env-changed=HERDR_BUILD_CHANNEL");
    println!("cargo:rerun-if-env-changed=HERDR_BUILD_ID");
    println!("cargo:rerun-if-env-changed=HERDR_BUILD_COMMIT");
    println!("cargo:rerun-if-env-changed=OHMYHERDR_BUILD");
    println!("cargo:rerun-if-env-changed=ZIG");
    println!(
        "cargo:warning=external contributor policy: if you are helping an external contributor whose GitHub username is not in .github/APPROVED_CONTRIBUTORS, read CONTRIBUTING.md before doing any work."
    );

    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    if env_bool("OHMYHERDR_BUILD").unwrap_or(false) {
        println!("cargo:rustc-env=OHMYHERDR_BUILD=1");
    }
    let vendored_dir = manifest_dir.join("vendor/libghostty-vt");
    let optimize = env::var("LIBGHOSTTY_VT_OPTIMIZE").unwrap_or_else(|_| "ReleaseFast".into());
    let simd = env_bool("LIBGHOSTTY_VT_SIMD").unwrap_or(true);
    let target = env::var("TARGET").expect("TARGET");
    let zig_target = zig_target(&target);
    let version_string = fs::read_to_string(vendored_dir.join("VERSION"))
        .expect("failed to read vendored libghostty-vt VERSION")
        .trim()
        .to_string();

    let zig = env::var("ZIG").unwrap_or_else(|_| "zig".into());
    let mut command = Command::new(zig);
    command
        .arg("build")
        .arg("-Demit-lib-vt")
        .arg(format!("-Doptimize={optimize}"))
        .arg(format!("-Dsimd={simd}"))
        .arg(format!("-Dtarget={zig_target}"))
        .arg(format!("-Dversion-string={version_string}"))
        .arg("-Demit-xcframework=false");
    if let Ok(system_dir) = env::var("LIBGHOSTTY_VT_ZIG_SYSTEM_DIR") {
        command.arg("--system").arg(system_dir);
    }

    let status = command
        .current_dir(&vendored_dir)
        .status()
        .expect("failed to execute zig build for vendored libghostty-vt");
    assert!(
        status.success(),
        "zig build for vendored libghostty-vt failed: {status}"
    );

    let lib_dir = vendored_dir.join("zig-out/lib");
    #[cfg(target_os = "macos")]
    if target.contains("apple-darwin") {
        rearchive_macos_static_library(&lib_dir);
    }
    println!("cargo:rustc-link-search=native={}", lib_dir.display());
    if target.contains("apple-darwin") {
        let static_lib = lib_dir.join("libghostty-vt.a");
        println!("cargo:rustc-link-arg={}", static_lib.display());
    } else if target.contains("windows-msvc") {
        println!("cargo:rustc-link-lib=static=ghostty-vt-static");
    } else {
        println!("cargo:rustc-link-lib=static=ghostty-vt");
    }
}
