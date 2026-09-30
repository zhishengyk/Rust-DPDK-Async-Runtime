use std::{env, path::PathBuf, process::Command};
fn pkg(arg: &str) -> String {
    let output = Command::new("pkg-config")
        .args(["--static", arg, "libdpdk"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "libdpdk not found; run scripts/setup.sh"
    );
    String::from_utf8(output.stdout).unwrap()
}
fn main() {
    println!("cargo:rerun-if-changed=shim.c");
    println!("cargo:rerun-if-changed=shim.h");
    println!("cargo:rerun-if-env-changed=PKG_CONFIG_PATH");
    let flags = pkg("--cflags");
    let mut cc = cc::Build::new();
    cc.file("shim.c").flag("-std=gnu11").opt_level(3);
    for flag in flags.split_whitespace() {
        cc.flag(flag);
    }
    cc.compile("dpdk_shim");
    bindgen::Builder::default()
        .header("shim.h")
        .allowlist_function("w_.*")
        .allowlist_type("w_.*")
        .layout_tests(false)
        .generate()
        .unwrap()
        .write_to_file(PathBuf::from(env::var("OUT_DIR").unwrap()).join("bindings.rs"))
        .unwrap();
    // Native link metadata is propagated through rlibs to both binaries.
    let libs = pkg("--libs");
    let mut seen = std::collections::HashSet::new();
    for flag in libs.split_whitespace() {
        if let Some(path) = flag.strip_prefix("-L") {
            println!("cargo:rustc-link-search=native={path}");
        }
        if let Some(name) = flag
            .strip_prefix("-l:librte_")
            .and_then(|s| s.strip_suffix(".a"))
        {
            if seen.insert(format!("rte_{name}")) {
                println!("cargo:rustc-link-lib=static:+whole-archive=rte_{name}");
            }
        }
    }
    for flag in libs.split_whitespace() {
        if let Some(name) = flag
            .strip_prefix("-l")
            .filter(|s| !s.starts_with(':') && !s.starts_with("rte_"))
        {
            if seen.insert(name.to_owned()) {
                println!("cargo:rustc-link-lib={name}");
            }
        }
    }
}
