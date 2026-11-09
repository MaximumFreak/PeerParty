//! Builds the hook DLL for x64 and x86 and hands their paths to `include_bytes!`, so
//! `peerparty.exe` is the only file anyone needs. The nested builds get their own
//! target directory inside the outer one: cargo locks per target directory, and a
//! shared one would deadlock.
//!
//! Also gives the exe a version resource: the version number only, nothing else in its
//! file properties. The number is the package version plus a build number, the commits
//! since git's `v<version>` tag (`git describe`), so a peerparty.exe names its commit:
//! 0.1.0.5 is five commits after v0.1.0.

use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

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
    let out = Command::new("git")
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

/// A VERSIONINFO with only the file and product version: the package's plus the build
/// number.
fn version_resource(root: &Path, out: &Path) {
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let n = |k: &str| env::var(k).unwrap();
    let build = build_number(root, &n("CARGO_PKG_VERSION"));
    let (major, minor, patch) = (n("CARGO_PKG_VERSION_MAJOR"), n("CARGO_PKG_VERSION_MINOR"), n("CARGO_PKG_VERSION_PATCH"));
    let nums = format!("{major},{minor},{patch},{build}");
    let v = format!("{major}.{minor}.{patch}.{build}");
    let rc = format!(
        r#"1 VERSIONINFO
FILEVERSION {nums}
PRODUCTVERSION {nums}
FILEOS 0x40004
FILETYPE 0x1
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "040904b0"
    BEGIN
      VALUE "FileVersion", "{v}"
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
