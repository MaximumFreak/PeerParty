//! Embeds Windows VERSIONINFO metadata into `peerparty_hook.dll` (both x64 and x86) so
//! extracted hook DLLs carry PeerParty's product name, version, and licence metadata.

fn main() {
    #[cfg(windows)]
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let n = |k: &str| std::env::var(k).unwrap();
        let (major, minor, patch) = (n("CARGO_PKG_VERSION_MAJOR"), n("CARGO_PKG_VERSION_MINOR"), n("CARGO_PKG_VERSION_PATCH"));
        let nums = format!("{major},{minor},{patch},0");
        let ver = format!("{major}.{minor}.{patch}.0");
        let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
        let orig = if arch == "x86" { "peerparty_hook32.dll" } else { "peerparty_hook.dll" };
        let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
        let rc = format!(
            r#"1 VERSIONINFO
FILEVERSION {nums}
PRODUCTVERSION {nums}
FILEOS 0x40004
FILETYPE 0x2
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "040904b0"
    BEGIN
      VALUE "CompanyName", "PeerParty"
      VALUE "FileDescription", "PeerParty Multiplayer Hook"
      VALUE "FileVersion", "{ver}"
      VALUE "InternalName", "peerparty_hook"
      VALUE "LegalCopyright", "MIT License"
      VALUE "OriginalFilename", "{orig}"
      VALUE "ProductName", "PeerParty"
      VALUE "ProductVersion", "{ver}"
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
        std::fs::write(&path, rc).expect("writing hook version.rc");
        winresource::WindowsResource::new()
            .set_resource_file(path.to_str().unwrap())
            .compile()
            .expect("compiling hook version resource");
    }
}
