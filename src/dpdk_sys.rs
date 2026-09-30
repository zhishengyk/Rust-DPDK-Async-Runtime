//! 仅包含 shim.h 生成的底层 C 绑定；业务代码通过 dpdk crate 使用所有权安全的接口。
#![allow(non_camel_case_types, non_snake_case, non_upper_case_globals)]
include!(concat!(env!("OUT_DIR"), "/bindings.rs"));
