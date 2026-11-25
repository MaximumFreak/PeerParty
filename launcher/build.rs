//! Builds the hook DLL for x64 and x86 and hands them, compressed, to `include_bytes!`,
//! so `peerparty.exe` is the only file anyone needs. The nested builds get their own
//! target directory inside the outer one: cargo locks per target directory, and a
//! shared one would deadlock.
//!
//! Also gives the exe its resources: the icon (drawn from `src/icon_art.rs`, the window's
//! too), a manifest (run as the user, never elevated; the Windows versions it knows),
//! and a version resource naming it PeerParty. The version is the package version plus
//! a build number, the commits since git's `v<version>` tag (`git describe`): 0.1.0.5
//! is five commits after v0.1.0. Two builds can share that number (another branch, or
//! changes not committed yet), so the version strings also carry the commit and say
//! when the working tree had changes (`v0.1.0-5-gabc1234-dirty`).

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
///
/// `PEERPARTY_HOOKS=none` builds neither: both are stubs (a launcher that starts no game,
/// or a check). `PEERPARTY_HOOK_DIR` names a folder of hooks built elsewhere
/// (`peerparty_hook64.dll`, and `peerparty_hook32.dll` if there is one), embedded as they
/// are: how a build for Linux, which can't compile the Windows DLLs, gets them. A build
/// for another OS without it embeds stubs.
const HOOK_FILES: [&str; 2] = ["peerparty_hook64.dll", "peerparty_hook32.dll"];

/// An empty hook for one that isn't embedded.
fn stub(out: &Path, key: &str) {
    let stub = out.join(format!("{}-stub.dll", key.to_ascii_lowercase()));
    std::fs::write(&stub, []).expect("writing a hook stub");
    embed(out, key, &stub);
}

/// Hands `include_bytes!` the hook DLL at `dll` uncompressed with a 24-byte header:
/// an 8-byte tag ("PPHK64" or "PPHK32" and two NULs, which the release check looks for),
/// the DLL's length and its FNV-1a hash (the name of the folder it's written out to, so
/// the launcher never hashes it at startup), then the raw DLL bytes. Keeping the DLLs
/// uncompressed avoids high-entropy blobs in `.rdata` that trigger AV heuristics.
fn embed(out: &Path, key: &str, dll: &Path) {
    let blob = out.join(format!("{}.bin", key.to_ascii_lowercase()));
    let seen = out.join(format!("{}.bin.src", key.to_ascii_lowercase()));
    let meta = std::fs::metadata(dll).unwrap_or_else(|e| panic!("{}: {e}", dll.display()));
    let stamp = format!("{} {:?} {}", meta.len(), meta.modified().ok(), dll.display());
    if !(blob.is_file() && std::fs::read_to_string(&seen).is_ok_and(|s| s == stamp)) {
        let bytes = std::fs::read(dll).unwrap_or_else(|e| panic!("{}: {e}", dll.display()));
        let tag: &[u8; 8] = if key.ends_with("32") { b"PPHK32\0\0" } else { b"PPHK64\0\0" };
        let mut out_bytes = Vec::with_capacity(24 + bytes.len());
        out_bytes.extend_from_slice(tag);
        out_bytes.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
        out_bytes.extend_from_slice(&fnv1a(&bytes).to_le_bytes());
        out_bytes.extend_from_slice(&bytes);
        std::fs::write(&blob, out_bytes).expect("writing an embedded hook");
        std::fs::write(&seen, stamp).expect("writing an embedded hook's stamp");
    }
    println!("cargo:rustc-env={key}={}", blob.display());
}

/// The same hash as `hooks::fnv1a` in the launcher.
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, &b| (h ^ b as u64).wrapping_mul(0x0100_0000_01b3))
}

/// Each language file stripped of comments and blank lines: `OUT_DIR/locales/<code>.txt`,
/// embedded directly as plain UTF-8 text.
fn locales(out: &Path) {
    let src = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap()).join("locales");
    println!("cargo:rerun-if-changed={}", src.display());
    let dir = out.join("locales");
    std::fs::create_dir_all(&dir).expect("creating the locales folder");
    for e in std::fs::read_dir(&src).expect("reading launcher/locales").flatten() {
        let p = e.path();
        if p.extension().is_none_or(|x| x != "txt") {
            continue;
        }
        println!("cargo:rerun-if-changed={}", p.display());
        let text = std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
        let mut kept = String::with_capacity(text.len());
        for line in text.lines() {
            let line = line.trim_start_matches('\u{feff}');
            let t = line.trim_start();
            if t.is_empty() || t.starts_with('#') {
                continue;
            }
            kept.push_str(line);
            kept.push('\n');
        }
        let name = p.file_stem().unwrap().to_string_lossy();
        std::fs::write(dir.join(format!("{name}.txt")), kept.as_bytes()).expect("writing a language file");
    }
}

fn main() {
    let root = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap())
        .parent()
        .unwrap()
        .to_path_buf();
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    println!("cargo:rerun-if-env-changed=PEERPARTY_HOOK_RUSTFLAGS");
    println!("cargo:rerun-if-env-changed=PEERPARTY_HOOKS");
    println!("cargo:rerun-if-env-changed=PEERPARTY_HOOK_DIR");
    // Everything the hook is built from, the LAN core and the shared protocol included.
    // Not the launcher's own sources: a change there needs no new hook (cargo rebuilds
    // the launcher itself), and watching them ran the nested build on every edit.
    // Only what the DLL is compiled from: not core's integration tests or examples,
    // which a test run edits most and the hook doesn't contain.
    for p in [
        "hook/src",
        "hook/build.rs",
        "hook/Cargo.toml",
        "core/src",
        "core/Cargo.toml",
        "protocol/src",
        "protocol/Cargo.toml",
        "vendor",
        "Cargo.toml",
        "Cargo.lock",
        ".cargo/config.toml",
    ] {
        println!(
            "cargo:rerun-if-changed={}",
            Path::new(&root).join(p).display()
        );
    }
    let windows = env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows");
    let hook_dir = env::var_os("PEERPARTY_HOOK_DIR").map(PathBuf::from).filter(|d| !d.as_os_str().is_empty());
    if env::var("PEERPARTY_HOOKS").ok().as_deref() == Some("none") {
        for (_, key) in HOOKS {
            stub(&out, key);
        }
    } else if let Some(dir) = hook_dir {
        // The folder too, so a hook added to it later is seen (a missing path would
        // rerun every build).
        println!("cargo:rerun-if-changed={}", dir.display());
        for ((_, key), file) in HOOKS.into_iter().zip(HOOK_FILES) {
            let dll = dir.join(file);
            if dll.is_file() {
                println!("cargo:rerun-if-changed={}", dll.display());
                embed(&out, key, &dll);
            } else if key == "PEERPARTY_HOOK32" {
                println!("cargo:warning=no {} in PEERPARTY_HOOK_DIR: 32-bit games won't run with this build", dll.display());
                stub(&out, key);
            } else {
                panic!("PEERPARTY_HOOK_DIR has no {}", dll.display());
            }
        }
    } else if !windows {
        println!("cargo:warning=no hook DLLs for this target (set PEERPARTY_HOOK_DIR to embed ones built for Windows): games won't get the hook");
        for (_, key) in HOOKS {
            stub(&out, key);
        }
    } else {
        build_hooks(&root, &out);
    }

    locales(&out);
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
        // The flags reach the launcher's unit-test exe too, which imports none of some of
        // them: "/DELAYLOAD ignored" (LNK4199) says only that, and rustc shows linker
        // output as warnings since 1.100.
        println!("cargo:rustc-link-arg-bins=/IGNORE:4199");
    }
}

/// Builds the hook DLLs with a nested cargo, for the exe to embed.
fn build_hooks(root: &Path, out: &Path) {
    // The outer target directory, from OUT_DIR's layout (CACHEDIR.TAG can't say: cargo
    // marks only some of its folders with it). Without the config's `..` hops: cl.exe
    // and link.exe, which the hook's build runs, can't open a file whose path as written
    // passes MAX_PATH, and those hops did once the nested build sat deeper.
    let nested = target_dir_of(out).map(|t| normalized(&t).join("hook")).unwrap_or_else(|| normalized(out).join("hook"));
    let profile = env::var("PROFILE").unwrap();
    let cargo = env::var("CARGO").unwrap();
    // PROFILE is `debug` for dev and test, and `release` for every profile that
    // inherits release, testexe included. The directory under target/ is the real
    // name (debug, release, testexe), and that is the hook build we want.
    let profile_dir = profile_dir_of(out).unwrap_or(profile);
    let x64_only = env::var("PEERPARTY_HOOKS").ok().as_deref() == Some("x64");

    let built: Vec<(&str, &str)> = HOOKS.into_iter().filter(|(triple, _)| !(x64_only && triple.starts_with("i686"))).collect();
    // Both architectures in one cargo run, so they compile side by side (one after the
    // other, each hook's single-unit link-time optimization kept most cores idle).
    let mut cmd = Command::new(&cargo);
    no_window(&mut cmd);
    cmd.current_dir(root).args(["build", "-p", "peerparty_hook"]);
    for (triple, _) in &built {
        cmd.args(["--target", triple]);
    }
    cmd.arg("--target-dir")
        .arg(&nested)
        // Outer flags are for the launcher's target; the hook gets its own from
        // .cargo/config.toml.
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .env_remove("CARGO_TARGET_DIR");
    // A release build's flags for the hook too (the static CRT, and the path remapping
    // that keeps local paths out of the DLLs), when it passes them here.
    if let Ok(flags) = env::var("PEERPARTY_HOOK_RUSTFLAGS") {
        cmd.env("CARGO_ENCODED_RUSTFLAGS", flags);
    }
    if profile_dir != "debug" {
        cmd.args(["--profile", &profile_dir]);
    }
    let status = cmd.status().expect("running cargo for the hook DLL");
    assert!(
        status.success(),
        "building peerparty_hook failed (rustup target add {}?)",
        built.iter().map(|(t, _)| *t).collect::<Vec<_>>().join(" / ")
    );
    for (triple, key) in HOOKS {
        if x64_only && triple.starts_with("i686") {
            stub(out, key);
            continue;
        }
        let dll = nested.join(triple).join(&profile_dir).join("peerparty_hook.dll");
        assert!(dll.is_file(), "{} missing", dll.display());
        embed(out, key, &dll);
    }
}

/// The `build` folder OUT_DIR is in: `<profile>/build/<pkg>-<hash>/out` (Cargo up to
/// 1.99), or `<profile>/build/<pkg>/<hash>/out` (1.100's build-dir layout).
fn build_dir_of(out: &Path) -> Option<&Path> {
    out.ancestors().skip(2).take(2).find(|p| p.file_name().and_then(|n| n.to_str()) == Some("build"))
}

/// The target directory, from `target/<profile>/build/...` or
/// `target/<triple>/<profile>/build/...` (`build_dir_of`).
fn target_dir_of(out: &Path) -> Option<PathBuf> {
    let up = build_dir_of(out)?.parent()?.parent()?;
    let triple = env::var("TARGET").ok();
    match up.file_name() {
        Some(n) if triple.as_deref() == n.to_str() => up.parent().map(Path::to_path_buf),
        _ => Some(up.to_path_buf()),
    }
}

/// `p` with its `.` and `..` components worked out, as written (no `\\?\` prefix, which
/// `canonicalize` would add and not every tool takes).
fn normalized(p: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            c => out.push(c.as_os_str()),
        }
    }
    out
}

/// The profile's output directory (`debug`, `release`, `testexe`): the folder that holds
/// `build` (`build_dir_of`).
fn profile_dir_of(out: &Path) -> Option<String> {
    build_dir_of(out)?.parent()?.file_name()?.to_str().map(str::to_string)
}

/// Commits since the package version's `v<version>` tag (0 without git or the tag),
/// and git's whole description of the build ("" without).
fn build_number(root: &Path, version: &str) -> (u32, String) {
    // Rerun when HEAD moves or a tag appears (a missing path would rerun every build).
    // Not for every change to the working tree, though `--dirty` would see it: watching
    // the launcher's sources ran this script, and the nested hook build, on every edit
    // to them. A build's "-dirty" is brought up to date by the next commit or hook change.
    // Nor git's index: `git status`, a commit and the `git describe` below rewrite it
    // whenever file times have moved, so the build after nearly any git command ran this
    // script again and relinked the exe (a minute) with nothing changed.
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
        // Reads only: the index's refreshed file times aren't written back.
        .args(["--no-optional-locks", "describe", "--tags", "--long", "--dirty", "--match", &tag])
        .output()
        .ok()
        .filter(|o| o.status.success());
    let Some(out) = out else {
        println!("cargo:warning=no git tag {tag}: peerparty.exe's build number is 0");
        return (0, String::new());
    };
    // v0.1.0-5-gabc1234
    let text = String::from_utf8_lossy(&out.stdout);
    // The whole description (tag, commits since, commit) for the hook's version presence.
    if !text.trim().is_empty() {
        println!("cargo:rustc-env=PEERPARTY_DESCRIBE={}", text.trim());
    }
    let clean = text.trim().trim_end_matches("-dirty");
    (clean.rsplitn(3, '-').nth(1).and_then(|n| n.parse().ok()).unwrap_or(0), text.trim().to_string())
}

/// The exe's icon, manifest and VERSIONINFO (the package version plus the build
/// number, and PeerParty's name), in one resource script.
fn version_resource(root: &Path, out: &Path) {
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let n = |k: &str| env::var(k).unwrap();
    let (build, describe) = build_number(root, &n("CARGO_PKG_VERSION"));
    let (major, minor, patch) = (n("CARGO_PKG_VERSION_MAJOR"), n("CARGO_PKG_VERSION_MINOR"), n("CARGO_PKG_VERSION_PATCH"));
    let nums = format!("{major},{minor},{patch},{build}");
    // The strings name the commit too (the numbers can't).
    let v = if describe.is_empty() { format!("{major}.{minor}.{patch}.{build}") } else { format!("{major}.{minor}.{patch}.{build} ({describe})") };
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
    compile_resource(&path);
}

/// rc.exe from the Windows SDK, through winresource: a build dependency only where the
/// build itself runs on Windows (build.rs is built for the building system).
#[cfg(windows)]
fn compile_resource(path: &Path) {
    winresource::WindowsResource::new()
        .set_resource_file(path.to_str().unwrap())
        .compile()
        .expect("compiling the version resource (rc.exe from the Windows SDK)");
}

/// Built for Windows elsewhere: no resource compiler here, so no icon or version info.
#[cfg(not(windows))]
fn compile_resource(_path: &Path) {
    println!("cargo:warning=peerparty.exe built off Windows has no icon or version resource");
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

/// An `.ico` with one 32-bit image per size, drawn by `icon_art`: BMP for the small
/// ones, PNG (which an icon may hold since Windows Vista) from 64 pixels up, where an
/// uncompressed bitmap was most of the exe's resources (the 256-pixel one alone 270 KB).
fn ico_file(sizes: &[usize]) -> Vec<u8> {
    let images: Vec<Vec<u8>> = sizes.iter().map(|&s| if s >= 64 { png_image(s) } else { bmp_image(s) }).collect();
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

/// A PNG of the icon at `size` (RGBA).
fn png_image(size: usize) -> Vec<u8> {
    let mut out = Vec::new();
    let mut enc = png::Encoder::new(&mut out, size as u32, size as u32);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.set_compression(png::Compression::High);
    let mut w = enc.write_header().expect("writing the icon's PNG header");
    w.write_image_data(&icon_art::icon_rgba(size)).expect("writing the icon's PNG");
    w.finish().expect("finishing the icon's PNG");
    out
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
