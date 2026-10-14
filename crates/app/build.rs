//! rust-i18n 的 `i18n!` 宏在编译时读 `locales/` 里的文件，但 rustc 不知道这层依赖，
//! 只改语言文件不会重新编译。这里告诉 Cargo：目录里有改动就重编这个 crate。
//!
//! 链接时不导出 Rust 的符号（`__R` 开头的那些，Zed 也这么做）：应用不给别人链接，导出的七千多个符号只占地方，
//! 二进制小约 0.5 MB。符号还在符号表里，调试、崩溃报告不受影响。

fn main() {
    println!("cargo:rerun-if-changed=locales");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-link-arg-bins=-Wl,-unexported_symbol,__R*");
    }
}
