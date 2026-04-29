fn main() {
    println!("cargo:rerun-if-changed=vendor/find_all_keys_macos.c");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        cc::Build::new()
            .file("vendor/find_all_keys_macos.c")
            .compile("tg_key_scanner");
    }
}
