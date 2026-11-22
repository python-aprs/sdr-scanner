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
    println!("cargo:rerun-if-changed=sdrplay/shim_nosdk.c");
    let mut build = cc::Build::new();
    build.include("sdrplay").warnings(true);
    if include.join("sdrplay_api.h").exists() {
        println!(
            "cargo:rerun-if-changed={}",
            include.join("sdrplay_api.h").display()
        );
        build.file("sdrplay/shim.c").include(&include);
    } else {
        // Keep the Rust side building (CI checks it on every platform), but
        // never guess the vendor's struct layouts: the stand-in fails at run
        // time with a clear message.
        println!(
            "cargo:warning=sdrplay: no sdrplay_api.h in {}; building without SDRplay \
             support. Install SDRplay API 3.x or set SDRPLAY_API_INCLUDE.",
            include.display()
        );
        build.file("sdrplay/shim_nosdk.c");
    }
    build.compile("scanner_sdrplay");
    // The API library itself is opened with dlopen at run time.
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os == "linux" {
        println!("cargo:rustc-link-lib=dl");
    }
}
