use std::{env, fs, path::PathBuf};

fn main() {
    println!("cargo:rerun-if-env-changed=DIRIGENT_UPDATE_CHANNEL");
    let channel = env::var("DIRIGENT_UPDATE_CHANNEL").unwrap_or_default();
    let icon = match channel.as_str() {
        "stable" => "stable",
        "nightly" => "nightly",
        channel if channel.starts_with("unstable") => "unstable",
        _ => "dev",
    };
    let icon_path = format!("../dirigent_desktop/asset/{icon}.ico");
    println!("cargo:rerun-if-changed={icon_path}");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let icon = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap())
        .join(icon_path)
        .to_string_lossy()
        .replace('\\', "\\\\");
    let resource = PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("launcher.rc");
    fs::write(&resource, format!("1 ICON \"{icon}\"\n")).unwrap();
    embed_resource::compile(&resource, embed_resource::NONE)
        .manifest_required()
        .expect("failed to embed launcher icon");
}
