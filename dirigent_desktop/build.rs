//! Packages platform resources and optional bundled font assets.

use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
};

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
    embed_commit_id();
    embed_release_identity();
    embed_windows_icon();
    bundle_lilex();
}

fn embed_release_identity() {
    println!("cargo:rerun-if-env-changed=DIRIGENT_RELEASE_VERSION");
    println!("cargo:rerun-if-env-changed=DIRIGENT_UPDATE_CHANNEL");
    println!("cargo:rerun-if-env-changed=DIRIGENT_UPDATE_TARGET");

    let version = env::var("DIRIGENT_RELEASE_VERSION").unwrap_or_else(|_| {
        env::var("CARGO_PKG_VERSION").expect("Cargo must set CARGO_PKG_VERSION")
    });
    let channel = env::var("DIRIGENT_UPDATE_CHANNEL").unwrap_or_else(|_| "stable".into());
    let target = env::var("DIRIGENT_UPDATE_TARGET").unwrap_or_else(|_| {
        match (
            env::var("CARGO_CFG_TARGET_ARCH").as_deref(),
            env::var("CARGO_CFG_TARGET_OS").as_deref(),
        ) {
            (Ok("x86_64"), Ok("windows")) => "x86_64-pc-windows-msvc".into(),
            (Ok("x86_64"), Ok("linux")) => "x86_64-arch-linux".into(),
            (arch, os) => format!("{}-{}", arch.unwrap_or("unknown"), os.unwrap_or("unknown")),
        }
    });
    println!("cargo:rustc-env=DIRIGENT_RELEASE_VERSION={version}");
    println!("cargo:rustc-env=DIRIGENT_UPDATE_CHANNEL={channel}");
    println!("cargo:rustc-env=DIRIGENT_UPDATE_TARGET={target}");
}

fn git_text(manifest_dir: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(manifest_dir)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .filter(|text| !text.is_empty())
}

fn embed_commit_id() {
    println!("cargo:rerun-if-env-changed=DIRIGENT_COMMIT_ID");
    let manifest_dir = PathBuf::from(
        env::var_os("CARGO_MANIFEST_DIR").expect("Cargo must set CARGO_MANIFEST_DIR"),
    );
    for git_path in [
        git_text(&manifest_dir, &["rev-parse", "--git-path", "HEAD"]),
        git_text(&manifest_dir, &["symbolic-ref", "-q", "HEAD"]).and_then(|reference| {
            git_text(&manifest_dir, &["rev-parse", "--git-path", &reference])
        }),
        git_text(&manifest_dir, &["rev-parse", "--git-path", "packed-refs"]),
    ]
    .into_iter()
    .flatten()
    {
        println!("cargo:rerun-if-changed={git_path}");
    }

    let commit = env::var("DIRIGENT_COMMIT_ID")
        .ok()
        .or_else(|| git_text(&manifest_dir, &["rev-parse", "--short=8", "HEAD"]))
        .unwrap_or_else(|| "unknown".into());
    let commit = commit.chars().take(8).collect::<String>();
    println!("cargo:rustc-env=DIRIGENT_COMMIT_ID={commit}");
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
