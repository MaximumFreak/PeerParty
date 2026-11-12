//! Builds the hook DLL for x64 and x86 and hands their paths to `include_bytes!`, so
//! `peerparty.exe` is the only file anyone needs. The nested builds get their own
//! target directory inside the outer one: cargo locks per target directory, and a
//! shared one would deadlock.
//!
//! Also gives the exe its resources: the icon (drawn from `src/icon_art.rs`, the window's
//! too), a manifest (run as the user, never elevated; the Windows versions it knows),
//! and a version resource naming it PeerParty. The version is the package version plus
//! a build number, the commits since git's `v<version>` tag (`git describe`), so a
//! peerparty.exe names its commit: 0.1.0.5 is five commits after v0.1.0.

use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

/// No console window for a program the build runs: started from a build with no
/// console (an editor, a test runner), one would flash on screen. Its output is still
/// read through pipes, and what it starts shares its hidden console.
fn no_window(cmd: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    #[cfg(not(windows))]
    let _ = cmd;
}

#[path = "src/icon_art.rs"]
mod icon_art;

const HOOKS: [(&str, &str); 2] = [
    ("x86_64-pc-windows-msvc", "PEERPARTY_HOOK64"),
    ("i686-pc-windows-msvc", "PEERPARTY_HOOK32"),
];

/// `PEERPARTY_HOOKS=x64` skips the 32-bit hook (the test suite, unless a test DLL is
/// 32-bit). The exe still compiles: that half is a stub, and a 32-bit game won't run
/// with it. Unset builds both, which is what a shipping exe needs.

fn main() {
    let root = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap())
        .parent()
        .unwrap()
        .to_path_buf();
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    // The outer target directory is the ancestor cargo marked with CACHEDIR.TAG.
    let nested = out
        .ancestors()
        .find(|p| p.join("CACHEDIR.TAG").is_file())
        .map(|t| t.join("hook"))
        .unwrap_or_else(|| out.join("hook"));
    let profile = env::var("PROFILE").unwrap();
    let cargo = env::var("CARGO").unwrap();
    // PROFILE is `debug` for dev and test, and `release` for every profile that
    // inherits release, testexe included. The directory under target/ is the real
    // name (debug, release, testexe), and that is the hook build we want.
    let profile_dir = profile_dir_of(&out).unwrap_or(profile);
    let x64_only = env::var("PEERPARTY_HOOKS").ok().as_deref() == Some("x64");

    for (triple, key) in HOOKS {
        if x64_only && triple.starts_with("i686") {
            let stub = out.join("hook32-stub.dll");
            std::fs::write(&stub, []).expect("writing the 32-bit hook stub");
            println!("cargo:rustc-env={key}={}", stub.display());
            continue;
        }
        let mut cmd = Command::new(&cargo);
        no_window(&mut cmd);
        cmd.current_dir(&root)
            .args(["build", "-p", "peerparty_hook", "--target", triple])
            .arg("--target-dir")
            .arg(&nested)
            // Outer flags are for the launcher's target; the hook gets its own from
            // .cargo/config.toml.
            .env_remove("CARGO_ENCODED_RUSTFLAGS")
            .env_remove("CARGO_TARGET_DIR");
        // A release build's flags for the hook too (the static CRT, and the path
        // remapping that keeps local paths out of the DLLs), when it passes them here.
        if let Ok(flags) = env::var("PEERPARTY_HOOK_RUSTFLAGS") {
            cmd.env("CARGO_ENCODED_RUSTFLAGS", flags);
        }
        if profile_dir != "debug" {
            cmd.args(["--profile", &profile_dir]);
        }
        let status = cmd.status().expect("running cargo for the hook DLL");
        assert!(
            status.success(),
            "building peerparty_hook for {triple} failed (rustup target add {triple}?)"
        );
        let dll = nested.join(triple).join(&profile_dir).join("peerparty_hook.dll");
        assert!(dll.is_file(), "{} missing", dll.display());
        println!("cargo:rustc-env={key}={}", dll.display());
    }

    version_resource(&root, &out);
    // Where the exe's DLLs come from (see `only_system_dlls` in main.rs for why). The
    // ones it names are looked for in System32 only (0x800, LOAD_LIBRARY_SEARCH_SYSTEM32).
    // That flag doesn't reach the DLLs those load in turn, so the ones Windows doesn't
    // always take from System32 (all but its "known DLLs") are loaded on first use,
    // by which time `main` has said where DLLs may come from. A new one that a
    // dependency starts to name belongs in this list: tools/dll-search-check.ps1 says so.
    if env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        println!("cargo:rustc-link-arg-bins=/DEPENDENTLOADFLAG:0x800");
        // advapi32 is a known DLL itself, but its random number function is another
        // DLL's under its name, and that one is fetched when the name is looked up.
        for dll in ["dwmapi", "uxtheme", "opengl32", "hid", "iphlpapi", "bcrypt", "bcryptprimitives", "advapi32"] {
            println!("cargo:rustc-link-arg-bins=/DELAYLOAD:{dll}.dll");
        }
        println!("cargo:rustc-link-arg-bins=delayimp.lib");
    }
    println!("cargo:rerun-if-env-changed=PEERPARTY_HOOK_RUSTFLAGS");
    println!("cargo:rerun-if-env-changed=PEERPARTY_HOOKS");

    // Everything the hook is built from, the LAN core included.
    for p in ["hook", "core", "vendor", "Cargo.lock", ".cargo/config.toml"] {
        println!(
            "cargo:rerun-if-changed={}",
            Path::new(&root).join(p).display()
        );
    }
}

/// The profile's output directory (`debug`, `release`, `testexe`), from
/// `target/<profile>/build/<pkg>/out` or `target/<triple>/<profile>/build/<pkg>/out`.
fn profile_dir_of(out: &Path) -> Option<String> {
    let build = out.parent()?.parent()?;
    if build.file_name()?.to_str() != Some("build") {
        return None;
    }
    build.parent()?.file_name()?.to_str().map(str::to_string)
}

/// Commits since the package version's `v<version>` tag, 0 without git or the tag.
fn build_number(root: &Path, version: &str) -> u32 {
    // Rerun when HEAD moves or a tag appears (a missing path would rerun every build).
    let git = root.join(".git");
    let mut watch: Vec<PathBuf> = ["HEAD", "refs/tags", "packed-refs"].iter().map(|p| git.join(p)).collect();
    if let Ok(head) = std::fs::read_to_string(git.join("HEAD")) {
        if let Some(r) = head.trim().strip_prefix("ref: ") {
            watch.push(git.join(r));
        }
    }
    for p in watch.iter().filter(|p| p.exists()) {
        println!("cargo:rerun-if-changed={}", p.display());
    }
    let tag = format!("v{version}");
    let mut git = Command::new("git");
    no_window(&mut git);
    let out = git
        .current_dir(root)
        .args(["describe", "--tags", "--long", "--match", &tag])
        .output()
        .ok()
        .filter(|o| o.status.success());
    let Some(out) = out else {
        println!("cargo:warning=no git tag {tag}: peerparty.exe's build number is 0");
        return 0;
    };
    // v0.1.0-5-gabc1234
    let text = String::from_utf8_lossy(&out.stdout);
    // The whole description (tag, commits since, commit) for the hook's version presence.
    if !text.trim().is_empty() {
        println!("cargo:rustc-env=PEERPARTY_DESCRIBE={}", text.trim());
    }
    text.trim().rsplitn(3, '-').nth(1).and_then(|n| n.parse().ok()).unwrap_or(0)
}

/// The exe's icon, manifest and VERSIONINFO (the package version plus the build
/// number, and PeerParty's name), in one resource script.
fn version_resource(root: &Path, out: &Path) {
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let n = |k: &str| env::var(k).unwrap();
    let build = build_number(root, &n("CARGO_PKG_VERSION"));
    let (major, minor, patch) = (n("CARGO_PKG_VERSION_MAJOR"), n("CARGO_PKG_VERSION_MINOR"), n("CARGO_PKG_VERSION_PATCH"));
    let nums = format!("{major},{minor},{patch},{build}");
    let v = format!("{major}.{minor}.{patch}.{build}");
    println!("cargo:rerun-if-changed=src/icon_art.rs");
    let ico = out.join("peerparty.ico");
    std::fs::write(&ico, ico_file(&[16, 20, 24, 32, 40, 48, 64, 256])).expect("writing peerparty.ico");
    let manifest = out.join("peerparty.manifest");
    std::fs::write(&manifest, MANIFEST).expect("writing peerparty.manifest");
    // rc.exe strings: backslashes doubled.
    let rc_path = |p: &Path| p.display().to_string().replace('\\', "\\\\");
    let (ico, manifest) = (rc_path(&ico), rc_path(&manifest));
    let rc = format!(
        r#"1 ICON "{ico}"
1 24 "{manifest}"
1 VERSIONINFO
FILEVERSION {nums}
PRODUCTVERSION {nums}
FILEOS 0x40004
FILETYPE 0x1
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "040904b0"
    BEGIN
      VALUE "CompanyName", "PeerParty"
      VALUE "FileDescription", "PeerParty"
      VALUE "FileVersion", "{v}"
      VALUE "InternalName", "peerparty"
      VALUE "LegalCopyright", "MIT License"
      VALUE "OriginalFilename", "peerparty.exe"
      VALUE "ProductName", "PeerParty"
      VALUE "ProductVersion", "{v}"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x409, 1200
  END
END
"#
    );
    let path = out.join("version.rc");
    std::fs::write(&path, rc).expect("writing version.rc");
    winresource::WindowsResource::new()
        .set_resource_file(path.to_str().unwrap())
        .compile()
        .expect("compiling the version resource (rc.exe from the Windows SDK)");
}

/// Runs as the user who started it (never asks for elevation, and Windows' installer
/// detection leaves it alone), and names the Windows versions it was built for. DPI
/// awareness is set in code (`host`, and the window's own), not here.
const MANIFEST: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <assemblyIdentity type="win32" name="PeerParty.peerparty" version="1.0.0.0"/>
  <description>PeerParty</description>
  <trustInfo xmlns="urn:schemas-microsoft-com:asm.v3">
    <security>
      <requestedPrivileges>
        <requestedExecutionLevel level="asInvoker" uiAccess="false"/>
      </requestedPrivileges>
    </security>
  </trustInfo>
  <compatibility xmlns="urn:schemas-microsoft-com:compatibility.v1">
    <application>
      <supportedOS Id="{8e0f7a12-bfb3-4fe8-b9a5-48fd50a15a9a}"/>
      <supportedOS Id="{1f676c76-80e1-4239-95bb-83d0f6d0da78}"/>
      <supportedOS Id="{4a2f28e3-53b9-4441-ba9c-d69d4a4a6e38}"/>
      <supportedOS Id="{35138b9a-5d96-4fbd-8e2d-a2440225f93a}"/>
    </application>
  </compatibility>
</assembly>
"#;

/// An `.ico` with one 32-bit BMP image per size, drawn by `icon_art`.
fn ico_file(sizes: &[usize]) -> Vec<u8> {
    let images: Vec<Vec<u8>> = sizes.iter().map(|&s| bmp_image(s)).collect();
    let mut f = Vec::new();
    // ICONDIR: reserved, type 1 (icon), count.
    f.extend_from_slice(&0u16.to_le_bytes());
    f.extend_from_slice(&1u16.to_le_bytes());
    f.extend_from_slice(&(sizes.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * sizes.len();
    for (&s, img) in sizes.iter().zip(&images) {
        // ICONDIRENTRY: 256 is written as 0.
        let dim = if s >= 256 { 0 } else { s as u8 };
        f.extend_from_slice(&[dim, dim, 0, 0]);
        f.extend_from_slice(&1u16.to_le_bytes()); // planes
        f.extend_from_slice(&32u16.to_le_bytes()); // bits per pixel
        f.extend_from_slice(&(img.len() as u32).to_le_bytes());
        f.extend_from_slice(&(offset as u32).to_le_bytes());
        offset += img.len();
    }
    for img in images {
        f.extend_from_slice(&img);
    }
    f
}

/// BITMAPINFOHEADER, bottom-up BGRA pixels, then an empty AND mask (the alpha channel
/// does the masking).
fn bmp_image(size: usize) -> Vec<u8> {
    let rgba = icon_art::icon_rgba(size);
    let mask_row = size.div_ceil(32) * 4;
    let mut b = Vec::with_capacity(40 + size * size * 4 + mask_row * size);
    b.extend_from_slice(&40u32.to_le_bytes());
    b.extend_from_slice(&(size as i32).to_le_bytes());
    b.extend_from_slice(&(2 * size as i32).to_le_bytes()); // XOR + AND
    b.extend_from_slice(&1u16.to_le_bytes());
    b.extend_from_slice(&32u16.to_le_bytes());
    b.extend_from_slice(&[0u8; 24]); // BI_RGB, sizes, resolution, palette
    for y in (0..size).rev() {
        for x in 0..size {
            let i = (y * size + x) * 4;
            b.extend_from_slice(&[rgba[i + 2], rgba[i + 1], rgba[i], rgba[i + 3]]);
        }
    }
    b.resize(b.len() + mask_row * size, 0);
    b
}
