use std::{env, fs, path::PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=../dirigent_desktop/asset/icon.ico");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let icon = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap())
        .join("../dirigent_desktop/asset/icon.ico")
        .to_string_lossy()
        .replace('\\', "\\\\");
    let resource = PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("launcher.rc");
    fs::write(&resource, format!("1 ICON \"{icon}\"\n")).unwrap();
    embed_resource::compile(&resource, embed_resource::NONE)
        .manifest_required()
        .expect("failed to embed launcher icon");
}
