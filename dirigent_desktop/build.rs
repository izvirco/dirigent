use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::{Path, PathBuf};

const LILEX_VERSION: &str = "2.700";
const LILEX_SHA256: &str = "34310edb4ba749f772d42b88e3eec4a6318527e75ceeaa96cfc556ee35366fbc";
const LILEX_FACES: &[&str] = &["Regular", "Italic", "Medium", "MediumItalic", "SemiBold", "SemiBoldItalic"];

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    if let Err(e) = lilex(&out) {
        panic!("Lilex: {e}\nSet DIRIGENT_LILEX_DIR to a directory with Lilex-<Face>.ttf to build offline.");
    }
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        windows_resources(&out);
    }
}

fn lilex(out: &Path) -> Result<(), String> {
    println!("cargo:rerun-if-env-changed=DIRIGENT_LILEX_DIR");
    let dir = out.join("lilex");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let face = |name: &str| dir.join(format!("Lilex-{name}.ttf"));
    if let Ok(src) = std::env::var("DIRIGENT_LILEX_DIR") {
        for name in LILEX_FACES {
            let from = Path::new(&src).join(format!("Lilex-{name}.ttf"));
            // Removed first: `copy` keeps the source's mode, read-only from a nix store.
            let _ = std::fs::remove_file(face(name));
            std::fs::copy(&from, face(name)).map_err(|e| format!("{}: {e}", from.display()))?;
        }
    } else if !LILEX_FACES.iter().all(|name| face(name).exists()) {
        let url = format!("https://github.com/mishamyrt/Lilex/releases/download/{LILEX_VERSION}/Lilex.zip");
        let curl = std::process::Command::new("curl").args(["-fsSL", &url]).output().map_err(|e| format!("run curl: {e}"))?;
        if !curl.status.success() {
            return Err(format!("download {url}: {}", String::from_utf8_lossy(&curl.stderr).trim()));
        }
        let zip = curl.stdout;
        let hash: String = Sha256::digest(&zip).iter().map(|b| format!("{b:02x}")).collect();
        if hash != LILEX_SHA256 {
            return Err(format!("{url} has sha256 {hash}, expected {LILEX_SHA256}"));
        }
        let mut archive = zip::ZipArchive::new(std::io::Cursor::new(zip)).map_err(|e| e.to_string())?;
        for name in LILEX_FACES {
            let mut bytes = vec![];
            let mut file = archive.by_name(&format!("ttf/Lilex-{name}.ttf")).map_err(|e| format!("Lilex-{name}.ttf: {e}"))?;
            file.read_to_end(&mut bytes).map_err(|e| e.to_string())?;
            std::fs::write(face(name), bytes).map_err(|e| e.to_string())?;
        }
    }
    let faces: String = LILEX_FACES.iter().map(|name| format!("    include_bytes!({:?}),\n", face(name))).collect();
    std::fs::write(out.join("lilex.rs"), format!("&[\n{faces}]\n")).map_err(|e| e.to_string())
}

fn windows_resources(out: &Path) {
    println!("cargo:rerun-if-env-changed=DIRIGENT_BUILD_VERSION");
    let version = std::env::var("DIRIGENT_BUILD_VERSION").unwrap_or_else(|_| std::env::var("CARGO_PKG_VERSION").unwrap());
    let (name, icon) = if version.contains("-nightly.") {
        ("Dirigent Nightly", "assets/dirigent_nightly.ico")
    } else {
        ("Dirigent", "assets/dirigent.ico")
    };
    let icon = Path::new(&std::env::var("CARGO_MANIFEST_DIR").unwrap()).join(icon);
    println!("cargo:rerun-if-changed={}", icon.display());
    let mut parts = version.split(['.', '-', '+']).map(|p| p.parse::<u16>().unwrap_or(0));
    let numeric: Vec<String> = (0..4).map(|_| parts.next().unwrap_or(0).to_string()).collect();
    let numeric = numeric.join(",");
    // UTF-8: rc.exe reads the file in the ANSI code page otherwise, mangling non-ASCII paths.
    let rc = format!(
        r#"#pragma code_page(65001)
1 ICON "{icon}"
1 VERSIONINFO
FILEVERSION {numeric}
PRODUCTVERSION {numeric}
FILEOS 0x40004L
FILETYPE 0x1L
BEGIN
    BLOCK "StringFileInfo"
    BEGIN
        BLOCK "040904b0"
        BEGIN
            VALUE "FileDescription", "{name}\0"
            VALUE "FileVersion", "{version}\0"
            VALUE "ProductName", "{name}\0"
            VALUE "ProductVersion", "{version}\0"
            VALUE "OriginalFilename", "Dirigent.exe\0"
        END
    END
    BLOCK "VarFileInfo"
    BEGIN
        VALUE "Translation", 0x0409, 1200
    END
END
"#,
        icon = icon.display().to_string().replace('\\', "\\\\"),
    );
    let path = out.join("dirigent.rc");
    std::fs::write(&path, rc).unwrap();
    embed_resource::compile(&path, embed_resource::NONE).manifest_optional().unwrap();
}
