//! Compiles the SDRplay shim when the `sdrplay` feature is on.

fn main() {
    #[cfg(feature = "sdrplay")]
    sdrplay();
}

#[cfg(feature = "sdrplay")]
fn sdrplay() {
    use std::path::PathBuf;
    println!("cargo:rerun-if-changed=sdrplay/shim.c");
    println!("cargo:rerun-if-changed=sdrplay/scanner_sdrplay.h");
    println!("cargo:rerun-if-env-changed=SDRPLAY_API_INCLUDE");
    let include = std::env::var_os("SDRPLAY_API_INCLUDE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/usr/local/include"));
    if !include.join("sdrplay_api.h").exists() {
        panic!(
            "the sdrplay feature needs the SDRplay API 3.x header, but {} has no \
             sdrplay_api.h. Install the API from sdrplay.com, or set \
             SDRPLAY_API_INCLUDE to the folder that holds the header.",
            include.display()
        );
    }
    cc::Build::new()
        .file("sdrplay/shim.c")
        .include("sdrplay")
        .include(&include)
        .warnings(true)
        .compile("scanner_sdrplay");
    // The API library itself is opened with dlopen at run time.
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os == "linux" {
        println!("cargo:rustc-link-lib=dl");
    }
}
