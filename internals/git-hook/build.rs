//! Git Hook 构建脚本
//!
//! 在编译期初始化 [monk](https://docs.rs/monk) 工具链，用于管理工作区的 git hooks。
//! 通过 `CARGO_MANIFEST_DIR` 推导工作区根目录并切换当前目录后调用 `monk::init()`。

fn main() {
  let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
  let workspace_root = std::path::PathBuf::from(manifest_dir)
    .parent() // 如果 hooks 在 crates/hooks，则到 crates
    .unwrap()
    .parent() // 到 workspace 根
    .unwrap()
    .to_path_buf();
  // 切换到 workspace 根目录
  std::env::set_current_dir(&workspace_root).unwrap();
  monk::init();
}
