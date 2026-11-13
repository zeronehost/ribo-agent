//! 插件 storage：进程内 / 插件级 / 不持久化的键值存储（设计文档 18_ §6.2）。
//!
//! 语义边界：
//!
//! - **进程内**：重启即丢失，持久化归 Step C；
//! - **插件级**：每个插件一个 [`Storage`] 实例（挂
//!   `Arc<PluginShared>`，跨 Stateless Store 存活），互不可见；
//! - **不持久化**：无落盘路径。
//!
//! ACL 由 [`Storage::new`] 注入（`acl.storage` 为 false 时 get/put 一律
//! 返回 [`StorageError::Denied`]），经 host function 入口映射为
//! `forbidden:storage` trap → `plugin.plugin_forbidden`（不计熔断）。

use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};

use crate::acl::PluginAcl;
use crate::config::PluginRuntimeConfig;

// ===== DoS 边界（v0.8.3 P1-1）：单条 key / value 长度上限 =====
// `StoreLimits` 只约束 guest 线性内存，**不约束 host 堆上的 Vec 分配**——
// 恶意 guest 传巨大 len 即可在不触碰沙箱的前提下打爆 host。长度判定
// 必须在读取点（`read_guest_bytes_limited`，第一道）先做；本模块的检查
// 是**第二道防线**，覆盖非 host function 路径。

/// key 长度上限（字节）。
pub const MAX_KEY_LEN: usize = 256;
/// value 长度上限（字节，256KB）。远小于默认总配额 1MB，
/// 保证「单次写入」不会先于「总配额」把 host 内存打爆。
pub const MAX_VALUE_LEN: usize = 256 * 1024;

/// storage 内部错误（跨层契约的一部分：经 §6.4 trap 前缀表映射为
/// `storage_full` / `storage_bad_key` / `forbidden:storage` /
/// `storage_internal:{stage}`；对外最终收敛为 `plugin.plugin_*`）。
///
/// **不含 NotFound 变体**——「key 不存在」是正常态，由
/// `get` 的 `Ok(None)` 承载（v0.8.2 P0-1 跨层契约）。
#[derive(Debug)]
pub enum StorageError {
  /// 仅 put：容量超限（单条 value 超 [`MAX_VALUE_LEN`] 或总配额用尽）
  /// → `storage_full` trap → `plugin.plugin_exec_failed`。
  Full,
  /// get/put：key 格式非法或超 [`MAX_KEY_LEN`]
  /// → `storage_bad_key` trap → `plugin.plugin_exec_failed`。
  BadKey(String),
  /// get/put：ACL 未授权 → `forbidden:storage` trap → `plugin.plugin_forbidden`。
  Denied,
  /// 表内部错 → `storage_internal:{stage}` trap → `plugin.plugin_internal`。
  ///
  /// 当前 [`PluginTable`] 基于 HashMap，本变体尚无真实触发路径；
  /// 保留是为 §6.4 trap 前缀契约完整（Step C 持久化路径启用），
  /// host function 侧已实现对应匹配分支。
  #[allow(dead_code)] // 契约保留项：无触发路径属预期，见上注释
  Internal(String),
}

/// 单插件的键值表（HashMap + 已用字节数）。
#[derive(Debug, Default)]
struct PluginTable {
  map: HashMap<String, Vec<u8>>,
  used: usize,
}

impl PluginTable {
  fn get(&self, key: &str) -> Option<Vec<u8>> {
    self.map.get(key).cloned()
  }

  /// 写入并在总配额 `cap` 内记账；返回写入前的旧值长度供回退。
  fn put(
    &mut self,
    key: &str,
    value: Vec<u8>,
    cap: usize,
  ) -> Result<usize, StorageError> {
    let old_len = self.map.get(key).map(Vec::len).unwrap_or(0);
    // 覆盖写入按「新增净占用」计算配额
    if self.used - old_len + value.len() > cap {
      return Err(StorageError::Full);
    }
    self.used = self.used - old_len + value.len();
    self.map.insert(key.to_string(), value);
    Ok(old_len)
  }
}

/// 插件 storage 实例（进程内 / 插件级 / 不持久化）。
///
/// `Storage::get` / `put` 均为 `&self`（host function 经 owned 快照访问），
/// 内部表用 [`Mutex`] 保证可变性；锁中毒时以 `into_inner` 取回守卫
/// （HashMap 本身未被破坏，放弃读写反而扩大故障面）。
#[derive(Debug)]
pub struct Storage {
  table: Mutex<PluginTable>,
  /// 实例级配额：`min(插件覆写, 进程默认)`（只能收紧）。
  cap: usize,
  /// ACL 门禁：`PluginAcl.storage` 为 false 时 get/put 一律 Denied。
  allowed: bool,
}

impl Storage {
  /// 以进程默认配置与插件 ACL 构造（配额注入，18_ §6.2 v0.8 #15）。
  pub fn new(cfg: &PluginRuntimeConfig, acl: &PluginAcl) -> Self {
    // 插件只能收紧，不能放大进程默认
    let cap = acl
      .storage_quota_bytes
      .unwrap_or(cfg.storage_quota_bytes)
      .min(cfg.storage_quota_bytes);
    Self {
      table: Mutex::new(PluginTable::default()),
      cap,
      allowed: acl.storage,
    }
  }

  /// 读取 key（三态契约，v0.8.2 P0-1）。
  ///
  /// # Errors
  ///
  /// - ACL 未授权 → [`StorageError::Denied`]；
  /// - key 超 [`MAX_KEY_LEN`] → [`StorageError::BadKey`]；
  /// - 表内部错 → [`StorageError::Internal`]。
  pub fn get(&self, key: &str) -> Result<Option<Vec<u8>>, StorageError> {
    if !self.allowed {
      return Err(StorageError::Denied);
    }
    if key.len() > MAX_KEY_LEN {
      return Err(StorageError::BadKey(format!(
        "key exceeds {} bytes",
        MAX_KEY_LEN
      )));
    }
    // Ok(None) = key 不存在，是正常状态而非错误
    Ok(
      self
        .table
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(key),
    )
  }

  /// 写入 key/value。
  ///
  /// # Errors
  ///
  /// - ACL 未授权 → [`StorageError::Denied`]；
  /// - key 超 [`MAX_KEY_LEN`] → [`StorageError::BadKey`]；
  /// - value 超 [`MAX_VALUE_LEN`] 或总配额用尽 → [`StorageError::Full`]。
  pub fn put(&self, key: &str, value: &[u8]) -> Result<(), StorageError> {
    if !self.allowed {
      return Err(StorageError::Denied);
    }
    if key.len() > MAX_KEY_LEN {
      return Err(StorageError::BadKey(format!(
        "key exceeds {} bytes",
        MAX_KEY_LEN
      )));
    }
    if value.len() > MAX_VALUE_LEN {
      return Err(StorageError::Full);
    }
    self
      .table
      .lock()
      .unwrap_or_else(PoisonError::into_inner)
      .put(key, value.to_vec(), self.cap)
      .map(|_| ())
  }
}

#[cfg(test)]
mod tests {
  use std::sync::Arc;

  use super::*;
  use crate::acl::PluginAcl;

  /// 构造开启 storage 能力的 Storage（默认配额）。
  fn storage_with_quota(quota: Option<usize>) -> Storage {
    let mut acl = PluginAcl {
      storage: true,
      ..PluginAcl::default()
    };
    acl.storage_quota_bytes = quota;
    Storage::new(&PluginRuntimeConfig::default(), &acl)
  }

  /// 场景：三态契约——Ok(None) 表示 key 不存在（正常态）；写入后可读回。
  #[test]
  fn get_returns_none_for_missing_key_and_some_after_put() {
    let storage = storage_with_quota(None);

    // Ok(None)：key 不存在是正常状态，不是错误
    assert!(matches!(storage.get("count:a"), Ok(None)));

    storage.put("count:a", b"1").expect("put should succeed");
    assert_eq!(
      storage.get("count:a").expect("get should succeed"),
      Some(b"1".to_vec())
    );
  }

  /// 场景：key / value 长度上限（第二道防线）。
  #[test]
  fn oversized_key_or_value_is_rejected() {
    let storage = storage_with_quota(None);

    // key 恰好 256B 通过、257B 拒绝（BadKey）
    let key_ok = "k".repeat(MAX_KEY_LEN);
    let key_bad = "k".repeat(MAX_KEY_LEN + 1);
    assert!(matches!(storage.get(&key_ok), Ok(None)));
    assert!(matches!(
      storage.get(&key_bad),
      Err(StorageError::BadKey(_))
    ));
    storage
      .put(&key_ok, b"v")
      .expect("boundary key should pass");
    assert!(matches!(
      storage.put(&key_bad, b"v"),
      Err(StorageError::BadKey(_))
    ));

    // value 恰好 256KB 通过、256KB+1 拒绝（Full，容量类语义）
    let val_ok = vec![0u8; MAX_VALUE_LEN];
    let val_bad = vec![0u8; MAX_VALUE_LEN + 1];
    storage
      .put("v", &val_ok)
      .expect("boundary value should pass");
    assert!(matches!(
      storage.put("v2", &val_bad),
      Err(StorageError::Full)
    ));
  }

  /// 场景：总配额（默认 1MB）——累计写入超限返回 Full，覆盖写入按净增计。
  #[test]
  fn total_quota_is_enforced() {
    let storage = storage_with_quota(Some(1024));

    // 两次 512B 写入恰好用满 1KB
    storage.put("a", &[0u8; 512]).expect("first half");
    storage.put("b", &[0u8; 512]).expect("second half");

    // 再写任何新 key 都超配额
    assert!(matches!(storage.put("c", b"x"), Err(StorageError::Full)));

    // 覆盖写入：同 key 等量替换不新增占用，应成功
    storage
      .put("a", &[0u8; 512])
      .expect("in-place overwrite should pass");
    // 同 key 缩小写入释放配额，新 key 即可写入
    storage.put("a", &[0u8; 256]).expect("shrink should pass");
    storage
      .put("c", &[0u8; 256])
      .expect("freed quota allows new key");
  }

  /// 场景：插件配额覆写只能收紧（min(覆写, 进程默认)）。
  #[test]
  fn acl_quota_override_can_only_shrink() {
    // 覆写 1KB < 进程默认 1MB：生效
    let s1 = storage_with_quota(Some(1024));
    assert!(matches!(s1.put("k", &[0u8; 2048]), Err(StorageError::Full)));

    // 覆写 8MB > 进程默认 1MB：被 min 收紧回 1MB（不允许放大）
    let s2 = storage_with_quota(Some(8 << 20));
    assert!(matches!(
      s2.put("k", &[0u8; 2 << 20]),
      Err(StorageError::Full)
    ));
  }

  /// 场景：ACL 未授权时 get/put 一律 Denied。
  #[test]
  fn denied_when_acl_disallows_storage() {
    let acl = PluginAcl::default(); // storage = false
    let storage = Storage::new(&PluginRuntimeConfig::default(), &acl);
    assert!(matches!(storage.get("k"), Err(StorageError::Denied)));
    assert!(matches!(storage.put("k", b"v"), Err(StorageError::Denied)));
  }

  /// 场景：`Storage` 满足 Send + Sync（作为 `Arc<Storage>` 放进
  /// `HostState` / `HostStateSnapshot` 的前提，18_ §0.4 纪律 2）。
  #[test]
  fn storage_is_send_and_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Storage>();
    assert_send_sync::<Arc<Storage>>();
  }
}
