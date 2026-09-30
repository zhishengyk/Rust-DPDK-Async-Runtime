// 构建项目的 C shim、生成小范围 Rust 绑定，并把静态 DPDK 链接参数传给两个客户端。
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
    // Cargo 在 dpdk-sys 的清单目录运行此脚本；源码集中在根目录的 src/ 和 native/ 下。
    println!("cargo:rerun-if-changed=../../native/shim.c");
    println!("cargo:rerun-if-changed=../../native/shim.h");
    println!("cargo:rerun-if-env-changed=PKG_CONFIG_PATH");
    let flags = pkg("--cflags");
    let mut cc = cc::Build::new();
    cc.file("../../native/shim.c")
        .flag("-std=gnu11")
        .opt_level(3);
    for flag in flags.split_whitespace() {
        cc.flag(flag);
    }
    cc.compile("dpdk_shim");
    // 只绑定 w_*，不把整套 DPDK 结构和 API 引入 Rust 接口。
    bindgen::Builder::default()
        .header("../../native/shim.h")
        .allowlist_function("w_.*")
        .allowlist_type("w_.*")
        .layout_tests(false)
        .generate()
        .unwrap()
        .write_to_file(PathBuf::from(env::var("OUT_DIR").unwrap()).join("bindings.rs"))
        .unwrap();
    // 链接元数据经 rlib 传递给 A/B；静态 PMD 的注册对象需要 whole-archive 保留。
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
