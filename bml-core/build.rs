//! Link the IKOS analyzer statically when the `ikos-static` feature is on.
//!
//! Inputs (build-time environment):
//! - `BML_IKOS_BUILD_DIR` (optional): the fork's cmake build tree, configured
//!   with `-DIKOS_DISABLE_APRON=ON`. Defaults to the `ikos` submodule's
//!   `build-llvm18-noapron` tree (see doc/ikos-setup.md for the cmake
//!   invocation). APRON drags in the GPL-licensed PPL bridge, which must
//!   not be linked into bml; the apron-* domains are simply unavailable in
//!   static builds.
//! - `BML_LLVM_CONFIG` (optional): llvm-config of the LLVM 18 the fork was
//!   built against. Defaults probe the common install prefixes.
//!
//! License-driven linking choices (everything static EXCEPT):
//! - GMP/GMPXX are LGPL: keep them dynamic.
//! - PPL is GPL: excluded entirely (no-APRON build, see above).
//!
//! Boost (BSL), TBB (Apache-2.0), LLVM (Apache-2.0 + exception) are linked
//! statically; sqlite3 symbols come from rusqlite's bundled build.

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    println!("cargo::rerun-if-env-changed=BML_IKOS_BUILD_DIR");
    println!("cargo::rerun-if-env-changed=BML_LLVM_CONFIG");

    if std::env::var_os("CARGO_FEATURE_IKOS_STATIC").is_none() {
        return;
    }

    let build_dir = std::env::var_os("BML_IKOS_BUILD_DIR").map_or_else(
        || {
            // Default: the ikos submodule's APRON-free build tree.
            let manifest =
                PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("cargo sets this"));
            let default = manifest
                .parent()
                .expect("bml-core sits in the workspace root")
                .join("ikos/build-llvm18-noapron");
            assert!(
                default.exists(),
                "ikos-static: {} not found. Build it (git submodule update --init && cmake; \
                 see doc/ikos-setup.md) or point BML_IKOS_BUILD_DIR at an existing tree \
                 configured with -DIKOS_DISABLE_APRON=ON",
                default.display()
            );
            default
        },
        PathBuf::from,
    );

    // The IKOS static libraries (cmake targets ikos-analyzer-core,
    // ikos-llvm-to-ar, ikos-ar).
    for (subdir, lib) in [
        ("analyzer", "ikos-analyzer-core"),
        ("frontend/llvm", "ikos-llvm-to-ar"),
        ("ar", "ikos-ar"),
    ] {
        let dir = build_dir.join(subdir);
        let archive = dir.join(format!("lib{lib}.a"));
        assert!(
            archive.exists(),
            "ikos-static: {} not found; build the `ikos-analyzer` cmake target in {}",
            archive.display(),
            build_dir.display()
        );
        println!("cargo::rustc-link-search=native={}", dir.display());
        println!("cargo::rustc-link-lib=static={lib}");
    }

    // Refuse APRON-enabled builds: libap_ppl is GPL.
    let cmake_cache = build_dir.join("analyzer/CMakeCache.txt").exists();
    let cache_path = if cmake_cache {
        build_dir.join("analyzer/CMakeCache.txt")
    } else {
        build_dir.join("CMakeCache.txt")
    };
    if let Ok(cache) = std::fs::read_to_string(&cache_path) {
        assert!(
            !cache.contains("APRON_LIB:FILEPATH=/") || cache.contains("IKOS_DISABLE_APRON:BOOL=ON"),
            "ikos-static: {} was configured with APRON; reconfigure with -DIKOS_DISABLE_APRON=ON (APRON pulls in the GPL-licensed PPL bridge)",
            build_dir.display()
        );
    }

    // LLVM 18, static. The component set mirrors the fork's analyzer
    // CMakeLists (`passes` is for the in-process --mem2reg pipeline).
    let llvm_config = std::env::var_os("BML_LLVM_CONFIG").map_or_else(
        || {
            const CANDIDATES: &[&str] = &[
                "/opt/homebrew/opt/llvm@18/bin/llvm-config",
                "/usr/local/opt/llvm@18/bin/llvm-config",
                "/usr/lib/llvm-18/bin/llvm-config",
            ];
            CANDIDATES
                .iter()
                .map(Path::new)
                .find(|p| p.exists())
                .map(Path::to_path_buf)
                .expect("ikos-static: no LLVM 18 llvm-config found; set BML_LLVM_CONFIG")
        },
        PathBuf::from,
    );
    let llvm = |args: &[&str]| -> String {
        let out = Command::new(&llvm_config)
            .args(args)
            .output()
            .unwrap_or_else(|e| panic!("ikos-static: {} failed: {e}", llvm_config.display()));
        assert!(
            out.status.success(),
            "ikos-static: llvm-config {args:?} failed"
        );
        String::from_utf8(out.stdout).expect("llvm-config output not utf-8")
    };
    println!(
        "cargo::rustc-link-search=native={}",
        llvm(&["--libdir"]).trim()
    );
    for flag in llvm(&[
        "--link-static",
        "--libs",
        "core",
        "ipo",
        "irreader",
        "passes",
        "support",
        "transformutils",
    ])
    .split_whitespace()
    {
        let name = flag
            .strip_prefix("-l")
            .unwrap_or_else(|| panic!("ikos-static: unexpected llvm-config --libs entry {flag}"));
        println!("cargo::rustc-link-lib=static={name}");
    }
    for flag in llvm(&["--link-static", "--system-libs"]).split_whitespace() {
        // System libraries stay dynamic (-lm -lz -lzstd -lcurses -lxml2 ...).
        if let Some(name) = flag.strip_prefix("-l") {
            println!("cargo::rustc-link-lib={name}");
        }
    }
    // Homebrew's shared lib dir (zstd and friends) is not on the default
    // linker search path.
    for dir in ["/opt/homebrew/lib", "/usr/local/lib"] {
        if Path::new(dir).exists() {
            println!("cargo::rustc-link-search=native={dir}");
        }
    }

    // Boost (static, BSL license). The set mirrors what ikos-analyzer links:
    // filesystem + thread and their internal dependencies.
    let boost_dir = ["/opt/homebrew/opt/boost/lib", "/usr/local/opt/boost/lib"]
        .iter()
        .map(Path::new)
        .find(|p| p.exists())
        .expect("ikos-static: boost not found (brew install boost)");
    println!("cargo::rustc-link-search=native={}", boost_dir.display());
    for lib in [
        "boost_filesystem",
        "boost_thread",
        "boost_atomic",
        "boost_chrono",
        "boost_container",
        "boost_date_time",
    ] {
        println!("cargo::rustc-link-lib=static={lib}");
    }

    // TBB (static, Apache-2.0).
    let tbb_dir = ["/opt/homebrew/opt/tbb/lib", "/usr/local/opt/tbb/lib"]
        .iter()
        .map(Path::new)
        .find(|p| p.exists())
        .expect("ikos-static: tbb not found (brew install tbb)");
    println!("cargo::rustc-link-search=native={}", tbb_dir.display());
    println!("cargo::rustc-link-lib=static=tbb");

    // GMP stays DYNAMIC: LGPL.
    let gmp_dir = ["/opt/homebrew/opt/gmp/lib", "/usr/local/opt/gmp/lib"]
        .iter()
        .map(Path::new)
        .find(|p| p.exists())
        .expect("ikos-static: gmp not found (brew install gmp)");
    println!("cargo::rustc-link-search=native={}", gmp_dir.display());
    println!("cargo::rustc-link-lib=dylib=gmpxx");
    println!("cargo::rustc-link-lib=dylib=gmp");

    // C++ runtime.
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os == "macos" {
        println!("cargo::rustc-link-lib=c++");
    } else {
        println!("cargo::rustc-link-lib=stdc++");
    }
}
