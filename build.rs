fn main() {
    println!("cargo:rerun-if-changed=assets/loglens.rc");
    println!("cargo:rerun-if-changed=assets/icon.ico");
    embed_resource::compile("assets/loglens.rc", embed_resource::NONE).manifest_optional().unwrap();
}
