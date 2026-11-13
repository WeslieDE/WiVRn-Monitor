//! Compiles the compute shaders to SPIR-V (glslc, or glslangValidator as fallback). If neither tool is
//! installed the checked-in `shaders/mirror.spv` is used.
use std::path::{Path, PathBuf};
use std::process::Command;

fn compile(src: &Path, out: &Path) -> bool {
    let tries: [(&str, Vec<&str>); 2] = [
        ("glslc", vec!["--target-env=vulkan1.1", "-O", src.to_str().unwrap(), "-o", out.to_str().unwrap()]),
        ("glslangValidator", vec!["-V", "--target-env", "vulkan1.1", src.to_str().unwrap(), "-o", out.to_str().unwrap()]),
    ];
    for (tool, args) in tries {
        if Command::new(tool).args(&args).status().map(|s| s.success()).unwrap_or(false) {
            return true;
        }
    }
    false
}

fn main() {
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let root = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    for (name, src, checked_in) in [
        ("mirror.spv", root.join("shaders/mirror.comp"), Some(root.join("shaders/mirror.spv"))),
        ("fake_fov.spv", root.join("tests/fake_fov.comp"), None),
    ] {
        println!("cargo:rerun-if-changed={}", src.display());
        let dst = out.join(name);
        if !compile(&src, &dst) {
            match checked_in {
                Some(c) => {
                    std::fs::copy(&c, &dst).expect("no shader compiler and no checked-in SPIR-V");
                }
                None => std::fs::write(&dst, []).unwrap(), // test-only shader; the test skips
            }
        }
    }
}
