fn main() {
    println!("cargo:rerun-if-changed=native/decode.cpp");
    println!("cargo:rerun-if-changed=native/decode.h");
    println!("cargo:rerun-if-changed=native/dng.cpp");
    println!("cargo:rerun-if-changed=native/preview.cpp");
    println!("cargo:rerun-if-changed=native/preview.h");
    println!("cargo:rerun-if-env-changed=PHOTOCATALOG_DNG_SDK");
    let sdk = std::path::PathBuf::from(
        std::env::var_os("PHOTOCATALOG_DNG_SDK")
            .expect("run scripts/fetch_dng_sdk.py and set PHOTOCATALOG_DNG_SDK"),
    );
    let source = sdk.join("dng_sdk/source");
    // Bind the actual SDK sources and headers used by this build, including local
    // changes. The download pin alone cannot identify a modified dependency cache.
    let mut sdk_files = std::fs::read_dir(&source)
        .expect("DNG SDK source directory")
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.extension()
                .is_some_and(|ext| ext == "cpp" || ext == "h")
        })
        .collect::<Vec<_>>();
    sdk_files.sort();
    let mut sdk_hash = blake3::Hasher::new();
    for path in sdk_files {
        println!("cargo:rerun-if-changed={}", path.display());
        let name = path.file_name().unwrap().to_str().unwrap().as_bytes();
        let contents = std::fs::read(&path).expect("DNG source identity");
        sdk_hash.update(&(name.len() as u64).to_le_bytes());
        sdk_hash.update(name);
        sdk_hash.update(&(contents.len() as u64).to_le_bytes());
        sdk_hash.update(&contents);
    }
    println!(
        "cargo:rustc-env=PHOTOCATALOG_DNG_SOURCE_BLAKE3={}",
        sdk_hash.finalize().to_hex()
    );
    let mut build = cc::Build::new();
    let platform = match std::env::var("CARGO_CFG_TARGET_OS").unwrap().as_str() {
        "macos" => "qMacOS",
        "windows" => "qWinOS",
        "linux" => "qLinux",
        _ => panic!("unsupported DNG target"),
    };
    build.define(platform, "1");
    build
        .opt_level(2)
        .cpp(true)
        .file("native/decode.cpp")
        .file("native/dng.cpp")
        .file("native/preview.cpp")
        .std("c++17")
        .include(&source)
        .define("qDNGUseXMP", "0")
        .define("qDNGUseLibJPEG", "1")
        .define("qDNGThreadSafe", "1")
        .define("qDNGValidate", "0")
        .define("qDNGValidateTarget", "0");
    let mut sources = std::fs::read_dir(&source)
        .expect("DNG SDK source directory")
        .map(|e| e.unwrap().path())
        .filter(|p| {
            p.extension().is_some_and(|e| e == "cpp")
                && ![
                    "dng_validate.cpp",
                    "dng_xmp_sdk.cpp",
                    "dng_update_meta.cpp",
                    "dng_jxl.cpp",
                ]
                .iter()
                .any(|x| p.file_name().unwrap() == *x)
        })
        .collect::<Vec<_>>();
    // Adobe's qDNGUseXMP=0 configuration has two unguarded XMP references in
    // dng_jxl.cpp. Keep the SDK immutable; patch only a generated build copy.
    // JXL XMP writing is disabled; raw XMP box bytes are still copied on reading.
    let mut jxl = std::fs::read_to_string(source.join("dng_jxl.cpp")).unwrap();
    for (from, to) in [
        (
            "\t\tif (includeXMP && metadata && metadata->GetXMP ())",
            "#if qDNGUseXMP\n\t\tif (includeXMP && metadata && metadata->GetXMP ())",
        ),
        ("\t\t\t} // xmp", "\t\t\t} // xmp\n#endif"),
        (
            "\t\t\tdng_xmp xmp (host.Allocator ());",
            "#if qDNGUseXMP\n\t\t\tdng_xmp xmp (host.Allocator ());\n#endif",
        ),
        (
            "\t\t\txmp.Parse (host, data.data (), count);",
            "#if qDNGUseXMP\n\t\t\txmp.Parse (host, data.data (), count);\n#endif",
        ),
    ] {
        assert_eq!(jxl.matches(from).count(), 1, "SDK patch mismatch");
        jxl = jxl.replace(from, to);
    }
    let patched =
        std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap()).join("dng_jxl.cpp");
    std::fs::write(&patched, jxl).unwrap();
    sources.push(patched);
    sources.sort();
    build.files(sources).warnings(false);
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        for name in [
            "libraw",
            "libavif",
            "libjxl",
            "libjpeg-turbo",
            "libwebp",
            "zlib",
        ] {
            let lib = vcpkg::Config::new()
                .find_package(name)
                .expect("install libraw and libavif with vcpkg (x64-windows-static-md)");
            for path in lib.include_paths {
                build.include(path);
            }
        }
    } else {
        for (name, version) in [
            ("libraw", "0.21"),
            ("libavif", "1.0"),
            ("libwebp", "1.2"),
            ("libjxl", "0.11"),
            ("libjxl_threads", "0.11"),
            ("libjpeg", "2"),
            ("zlib", "1.2"),
        ] {
            let lib = pkg_config::Config::new()
                .cargo_metadata(false)
                .atleast_version(version)
                .probe(name)
                .expect("install libraw and libavif development packages");
            for path in lib.link_paths {
                println!("cargo:rustc-link-search=native={}", path.display());
            }
            for name in lib.libs {
                // Some LibRaw pkg-config builds incorrectly publish GNU's runtime on macOS.
                if name == "stdc++"
                    && std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos")
                {
                    continue;
                }
                println!("cargo:rustc-link-lib={name}");
            }
            for path in lib.include_paths {
                build.include(path);
            }
        }
    }
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-link-lib=framework=CoreFoundation");
        println!("cargo:rustc-link-lib=framework=CoreServices");
    }
    build.compile("photocatalog_decode");
}
