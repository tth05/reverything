//! Embeds the icon and a version resource generated from the version in Cargo.toml.

fn main() {
    let icon = "../../assets/reverything.ico";
    println!("cargo:rerun-if-changed={}", icon);
    let rc = version_rc(
        "reverything-service.exe",
        "Reverything index service",
        &std::path::Path::new(&std::env::var("CARGO_MANIFEST_DIR").unwrap()).join(icon),
    );
    let path = std::path::Path::new(&std::env::var("OUT_DIR").unwrap()).join("reverything.rc");
    std::fs::write(&path, rc).unwrap();
    embed_resource::compile(&path, embed_resource::NONE)
        .manifest_required()
        .unwrap();
}

/// Resource script with the icon (id 1, also loaded for the tray icon) and the version.
fn version_rc(file_name: &str, description: &str, icon: &std::path::Path) -> String {
    let version = std::env::var("CARGO_PKG_VERSION").unwrap();
    let numbers = version
        .split(['.', '-', '+'])
        .take(3)
        .map(|n| n.parse::<u16>().unwrap_or(0).to_string())
        .chain(std::iter::once("0".to_string()))
        .collect::<Vec<_>>()
        .join(",");
    let icon = icon.display().to_string().replace('\\', "/");
    format!(
        r#"1 ICON "{icon}"

1 VERSIONINFO
FILEVERSION {numbers}
PRODUCTVERSION {numbers}
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "040904B0"
    BEGIN
      VALUE "CompanyName", "tth05"
      VALUE "FileDescription", "{description}"
      VALUE "ProductName", "Reverything"
      VALUE "FileVersion", "{version}"
      VALUE "ProductVersion", "{version}"
      VALUE "OriginalFilename", "{file_name}"
      VALUE "LegalCopyright", "Copyright (c) tth05"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x409, 1200
  END
END
"#
    )
}
