fn main() {
    println!("cargo:rerun-if-changed=assets/app-stream.rc");
    println!("cargo:rerun-if-changed=assets/app-stream.ico");
    println!("cargo:rerun-if-changed=assets/app-stream.manifest");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        embed_resource::compile("assets/app-stream.rc", embed_resource::NONE)
            .manifest_required()
            .expect("Embedding App Stream icon");
    }
}
