//! Link the IKOS analyzer statically when the `ikos-static` feature is on.
//!
//! Inputs (build-time environment):
//! - `BML_IKOS_BUILD_DIR` (optional): the fork's cmake build tree, configured
//!   with `-DIKOS_DISABLE_APRON=ON`. When unset, build.rs uses the `ikos`
//!   submodule's `build-llvm18-noapron` tree and BUILDS IT ON DEMAND (cmake
//!   configure + `cmake --build --target ikos-analyzer`) if its static
//!   libraries are missing -- a one-time C++ build that needs cmake and the
//!   LLVM 18 toolchain on the machine. A caller-supplied directory is taken
//!   as-is and never auto-built. APRON drags in the GPL-licensed PPL bridge,
//!   which must not be linked into bml; the apron-* domains are simply
//!   unavailable in static builds.
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

    // llvm-config of the LLVM 18 the fork is built against. Needed both to
    // configure the on-demand IKOS cmake build and to emit the LLVM link flags.
    let llvm_config = find_llvm_config();

    // The IKOS cmake build tree. By default this is the `ikos` submodule's
    // APRON-free tree, which build.rs builds on demand if its static libraries
    // are missing. A caller-supplied BML_IKOS_BUILD_DIR is taken as-is.
    let build_dir = if let Some(dir) = std::env::var_os("BML_IKOS_BUILD_DIR") {
        let dir = PathBuf::from(dir);
        assert!(
            dir.exists(),
            "ikos-static: BML_IKOS_BUILD_DIR={} does not exist; point it at a tree \
             configured with -DIKOS_DISABLE_APRON=ON (see doc/ikos-setup.md)",
            dir.display()
        );
        dir
    } else {
        let src = workspace_root().join("ikos");
        let dir = src.join("build-llvm18-noapron");
        ensure_ikos_built(&src, &dir, &llvm_config);
        dir
    };

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
    let boost_dir = ["/opt/homebrew/opt/boost/lib", "/usr/local/opt/boost/lib", "/usr/lib64", "/usr/lib"]
        .iter()
        .map(Path::new)
        .find(|p| p.exists() && p.join("libboost_filesystem.a").exists())
        .expect("ikos-static: boost not found (install boost-devel)");
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

    // TBB (Apache-2.0). Prefer static but fall back to dynamic.
    let tbb_dir = if let Some(d) = ["/opt/homebrew/opt/tbb/lib", "/usr/local/opt/tbb/lib"]
        .iter()
        .map(Path::new)
        .find(|p| p.exists())
    {
        d.to_path_buf()
    } else {
        PathBuf::from("/usr/lib64")
    };
    println!("cargo::rustc-link-search=native={}", tbb_dir.display());
    if tbb_dir.join("libtbb.a").exists() {
        println!("cargo::rustc-link-lib=static=tbb");
    } else {
        println!("cargo::rustc-link-lib=dylib=tbb");
    }

    // GMP stays DYNAMIC: LGPL.
    let gmp_dir = ["/opt/homebrew/opt/gmp/lib", "/usr/local/opt/gmp/lib", "/usr/lib64", "/usr/lib"]
        .iter()
        .map(Path::new)
        .find(|p| p.exists() && p.join("libgmp.so").exists())
        .expect("ikos-static: gmp not found (install gmp-devel)");
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

/// The workspace root (parent of the `bml-core` package directory).
fn workspace_root() -> PathBuf {
    PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("cargo sets this"))
        .parent()
        .expect("bml-core sits in the workspace root")
        .to_path_buf()
}

/// llvm-config of the LLVM 18 the fork is built against (`BML_LLVM_CONFIG`
/// overrides the probe).
fn find_llvm_config() -> PathBuf {
    std::env::var_os("BML_LLVM_CONFIG").map_or_else(
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
                .or_else(|| {
                    std::env::split_paths(
                        &std::env::var("PATH").unwrap_or_default(),
                    )
                    .find(|dir| dir.join("llvm-config-18").exists())
                    .map(|dir| dir.join("llvm-config-18"))
                })
                .expect("ikos-static: no LLVM 18 llvm-config found; set BML_LLVM_CONFIG")
        },
        PathBuf::from,
    )
}

/// Build the IKOS analyzer's static libraries in `build_dir` if they are not
/// already there. Mirrors doc/ikos-setup.md: an APRON-free Release build (no
/// GPL PPL bridge) of the `ikos-analyzer` target. Configures once, then builds.
fn ensure_ikos_built(src: &Path, build_dir: &Path, llvm_config: &Path) {
    // Fast path: the libraries bml links already exist, so skip cmake entirely.
    // (Removing the build tree forces a rebuild; bumping the submodule does not
    // -- this build script only reruns on env-var changes, so clean it by hand.)
    if build_dir.join("analyzer/libikos-analyzer-core.a").exists() {
        return;
    }

    assert!(
        src.join("CMakeLists.txt").exists(),
        "ikos-static: IKOS source not found at {}; initialize the submodule first \
         (git submodule update --init ikos)",
        src.display()
    );

    // Surfaces during `cargo build` (build-script stdout is otherwise hidden);
    // the C++ build is slow and would otherwise look like a hang.
    println!(
        "cargo::warning=ikos-static: building the IKOS analyzer in {} \
         (one-time, several minutes)",
        build_dir.display()
    );

    if !build_dir.join("CMakeCache.txt").exists() {
        run(
            Command::new("cmake")
                .arg("-S")
                .arg(src)
                .arg("-B")
                .arg(build_dir)
                .arg("-DCMAKE_BUILD_TYPE=Release")
                .arg(format!(
                    "-DLLVM_CONFIG_EXECUTABLE={}",
                    llvm_config.display()
                ))
                .arg("-DIKOS_DISABLE_APRON=ON")
                .arg("-DCMAKE_POSITION_INDEPENDENT_CODE=ON"),
            "cmake configure",
        );
    }

    run(
        Command::new("cmake")
            .arg("--build")
            .arg(build_dir)
            .arg("-j")
            .arg("--target")
            .arg("ikos-analyzer"),
        "cmake build",
    );
}

fn run(cmd: &mut Command, what: &str) {
    let status = cmd
        .status()
        .unwrap_or_else(|e| panic!("ikos-static: failed to spawn {what} (is cmake on PATH?): {e}"));
    assert!(status.success(), "ikos-static: {what} failed ({status})");
}
