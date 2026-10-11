//! rust-i18n 的 `i18n!` 宏在编译时读 `locales/` 里的文件，但 rustc 不知道这层依赖，
//! 只改语言文件不会重新编译。这里告诉 Cargo：目录里有改动就重编这个 crate。

fn main() {
    println!("cargo:rerun-if-changed=locales");
}
