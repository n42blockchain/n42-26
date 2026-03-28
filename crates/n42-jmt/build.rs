fn main() {
    // reth-mdbx-sys uses CharToOemBuffA from user32.dll on Windows.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        println!("cargo:rustc-link-lib=user32");
    }
}
