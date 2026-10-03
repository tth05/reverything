fn main() {
    println!("cargo:rerun-if-changed=reverything.rc");
    println!("cargo:rerun-if-changed=../../assets/reverything.ico");
    embed_resource::compile("reverything.rc", embed_resource::NONE)
        .manifest_required()
        .unwrap();
}
