//! Puts the mbelib shared library's folder on the binary's rpath when
//! scanner-core links it dynamically (a prebuilt static library it can't use),
//! so `scanner` finds it at run time.

fn main() {
    println!("cargo:rerun-if-env-changed=DEP_SCANNER_MBE_RPATH");
    if let Ok(dir) = std::env::var("DEP_SCANNER_MBE_RPATH") {
        println!("cargo:rustc-link-arg=-Wl,-rpath,{dir}");
    }
}
