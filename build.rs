use std::{env, fs, path::PathBuf};

const LILEX_FONTS: &[&str] = &[
    "Lilex-Regular.ttf",
    "Lilex-Italic.ttf",
    "Lilex-Medium.ttf",
    "Lilex-MediumItalic.ttf",
    "Lilex-SemiBold.ttf",
    "Lilex-SemiBoldItalic.ttf",
    "Lilex-Bold.ttf",
    "Lilex-BoldItalic.ttf",
];

fn main() {
    embed_windows_icon();
    bundle_lilex();
}

fn embed_windows_icon() {
    const ICON_PATH: &str = "asset/icon.ico";

    println!("cargo:rerun-if-changed={ICON_PATH}");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let manifest_dir = PathBuf::from(
        env::var_os("CARGO_MANIFEST_DIR").expect("Cargo must set CARGO_MANIFEST_DIR"),
    );
    let icon_path = manifest_dir
        .join(ICON_PATH)
        .to_string_lossy()
        .replace('\\', "\\\\");
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo must set OUT_DIR"));
    let resource_path = out_dir.join("dirigent.rc");

    fs::write(&resource_path, format!("1 ICON \"{icon_path}\"\n"))
        .expect("failed to write Windows resource file");
    embed_resource::compile(&resource_path, embed_resource::NONE)
        .manifest_required()
        .expect("failed to embed Windows application icon");
}

fn bundle_lilex() {
    println!("cargo:rerun-if-env-changed=LILEX_FONT_DIR");

    if env::var_os("CARGO_FEATURE_BUNDLED_LILEX").is_none() {
        return;
    }

    let font_dir = env::var_os("LILEX_FONT_DIR")
        .map(PathBuf::from)
        .expect("the bundled-lilex feature requires LILEX_FONT_DIR to point to Lilex TTF files");
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo must set OUT_DIR"));

    for file_name in LILEX_FONTS {
        let source = font_dir.join(file_name);
        if !source.is_file() {
            panic!(
                "the bundled-lilex feature requires {}; set LILEX_FONT_DIR to the directory containing the Lilex TTF files",
                source.display()
            );
        }

        println!("cargo:rerun-if-changed={}", source.display());
        fs::copy(&source, out_dir.join(file_name)).unwrap_or_else(|error| {
            panic!(
                "failed to copy bundled font {} into Cargo's output directory: {error}",
                source.display()
            )
        });
    }
}
