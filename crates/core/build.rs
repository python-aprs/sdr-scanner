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
            // macOS install name is @rpath). Prebuilt release archives can
            // hold LTO bitcode from a newer compiler than the local linker
            // reads, so those link the shared library instead, with an rpath.
            // MBELIB_LINK=static|dylib overrides the choice.
            println!("cargo:rerun-if-env-changed=MBELIB_LINK");
            let mut static_lib = None;
            let mut dylib_dir = None;
            for dir in ["lib", "lib64"] {
                let d = p.join(dir);
                if !d.exists() {
                    continue;
                }
                println!("cargo:rustc-link-search=native={}", d.display());
                let a = d.join(format!("lib{lib}.a"));
                if static_lib.is_none() && a.exists() {
                    static_lib = Some(a);
                }
                let shared = ["dylib", "so"]
                    .iter()
                    .any(|ext| d.join(format!("lib{lib}.{ext}")).exists());
                if dylib_dir.is_none() && shared {
                    dylib_dir = Some(d);
                }
            }
            let kind = match std::env::var("MBELIB_LINK").as_deref() {
                Ok("static") => "static",
                Ok("dylib") => "dylib",
                _ => match &static_lib {
                    Some(a) if has_bitcode(a) && dylib_dir.is_some() => {
                        println!(
                            "cargo:warning=mbelib: {} holds LTO bitcode, which the local \
                             linker may not read; linking the shared library instead",
                            a.display()
                        );
                        "dylib"
                    }
                    Some(_) => "static",
                    None => "dylib",
                },
            };
            if kind == "dylib" {
                if let Some(d) = &dylib_dir {
                    // For this crate's tests; binaries in other crates get
                    // the folder as DEP_SCANNER_MBE_RPATH (crates/cli/build.rs).
                    println!("cargo:rustc-link-arg=-Wl,-rpath,{}", d.display());
                    println!("cargo:rpath={}", d.display());
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

/// Whether an archive holds LLVM bitcode members (built with `-flto`).
#[cfg(feature = "mbelib")]
fn has_bitcode(path: &std::path::Path) -> bool {
    const RAW: &[u8] = b"BC\xC0\xDE";
    const WRAPPED: &[u8] = &[0xDE, 0xC0, 0x17, 0x0B];
    std::fs::read(path).is_ok_and(|b| b.windows(4).any(|w| w == RAW || w == WRAPPED))
}
