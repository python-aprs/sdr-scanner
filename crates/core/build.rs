//! Compiles the mbelib shim when the `mbelib` feature is on.

fn main() {
    #[cfg(feature = "mbelib")]
    mbelib();
}

#[cfg(feature = "mbelib")]
fn mbelib() {
    use std::path::PathBuf;
    println!("cargo:rerun-if-changed=mbelib/shim.c");
    println!("cargo:rerun-if-changed=mbelib/shim_nolib.c");
    println!("cargo:rerun-if-env-changed=MBELIB_DIR");
    // MBELIB_DIR is the install prefix (include/ and lib/ under it).
    // mbelib-neo is preferred over classic mbelib when both are there.
    let prefixes: Vec<PathBuf> = match std::env::var_os("MBELIB_DIR") {
        Some(d) => vec![PathBuf::from(d)],
        None => vec!["/usr/local".into(), "/opt/homebrew".into(), "/usr".into()],
    };
    let find = |header: &str| prefixes.iter().find(|p| p.join(header).exists());
    let mut build = cc::Build::new();
    build.warnings(true);
    let (prefix, header, lib) = if let Some(p) = find("include/mbelib-neo/mbelib.h") {
        build.define("SCANNER_MBELIB_NEO", None);
        (Some(p), "include/mbelib-neo/mbelib.h", "mbe-neo")
    } else if let Some(p) = find("include/mbelib.h") {
        (Some(p), "include/mbelib.h", "mbe")
    } else {
        (None, "", "")
    };
    match prefix {
        Some(p) => {
            println!("cargo:rerun-if-changed={}", p.join(header).display());
            build.file("mbelib/shim.c").include(p.join("include"));
            build.compile("scanner_mbe");
            // Link the static library when there is one, so the binary runs
            // without the library's folder on the loader path (mbelib-neo's
            // macOS install name is @rpath).
            let mut kind = "dylib";
            for dir in ["lib", "lib64"] {
                let d = p.join(dir);
                if d.exists() {
                    println!("cargo:rustc-link-search=native={}", d.display());
                    if d.join(format!("lib{lib}.a")).exists() {
                        kind = "static";
                    }
                }
            }
            println!("cargo:rustc-link-lib={kind}={lib}");
            if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("linux") {
                println!("cargo:rustc-link-lib=m");
            }
        }
        None => {
            // Keep the Rust side building (CI checks the feature on every
            // platform); the scanner says at start-up that it has no vocoder.
            println!(
                "cargo:warning=mbelib: no mbelib-neo or mbelib headers under {}; building \
                 without a vocoder. Install mbelib-neo or set MBELIB_DIR to its prefix.",
                prefixes
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            build.file("mbelib/shim_nolib.c");
            build.compile("scanner_mbe");
        }
    }
}
