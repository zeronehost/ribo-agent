//! 插件 ACL：URL 访问策略与插件能力声明。
//!
//! 代码库中原不存在 `UrlPolicy` / `SecurityConfig`，
//! 本模块为新建：
//!
//! - [`UrlPolicy`]：`http-proxy` host function 的出站 URL 裁决器——
//!   域名白名单 + 字面 IP 私网段拦截 + 仅允许 http/https；
//! - [`PluginAcl`]：插件被授予的能力集合（log / storage / http + 策略
//!   引用 + 配额覆写），由宿主在加载期注入，**不由插件自述**（信任链）。

use std::{collections::HashSet, sync::Arc};

use url::Host;

/// URL 策略拒绝原因（crate 内部中间错误，仅在本 crate 内流转；
/// 对外一律经 host function 入口映射为 `plugin.plugin_forbidden`）。
#[derive(Debug, thiserror::Error)]
pub enum UrlPolicyError {
  /// URL scheme 非 http/https。
  #[error("unsupported url scheme: {0}")]
  UnsupportedScheme(String),
  /// 目标为字面 IP 且命中私网 / 回环 / 链路本地 / 元数据段。
  #[error("blocked private or reserved address: {0}")]
  PrivateAddress(String),
  /// 目标域名不在白名单内。
  #[error("host not allowed by policy: {0}")]
  HostNotAllowed(String),
}

/// 出站 URL 访问策略。
///
/// 裁决规则（`check_url` 按序执行）：
///
/// 1. **scheme 白名单**：仅允许 `http` / `https`；
/// 2. **字面 IP 拦截**：IPv4 命中 RFC 1918（私网）/ RFC 3927（169.254/16，
///    含元数据端点 169.254.169.254）/ RFC 6598/ RFC 5735
///    （0/8、127/8、240/4、255.255.255.255），IPv6 命中 `::1` / `::` /
///    `fc00::/7` / `fe80::/10` 一律拒绝——**该检查先于白名单**，白名单
///    无法放行被拦截的保留段；
/// 3. **域名白名单**：host（小写化）必须显式出现在白名单中，
///    精确匹配、不支持后缀通配（防止 `evil-allow.com` 类绕过）。
///
/// # Examples
///
/// ```
/// use ra_plugins::acl::UrlPolicy;
///
/// let policy = UrlPolicy::new(["api.example.com"]);
///
/// // 白名单内域名放行
/// let ok = "https://api.example.com/v1".parse().unwrap();
/// assert!(policy.check_url(&ok).is_ok());
///
/// // 白名单外域名拒绝
/// let bad = "https://evil.com".parse().unwrap();
/// assert!(policy.check_url(&bad).is_err());
///
/// // 私网字面 IP 拒绝（即使写进白名单也拦）
/// let policy_all = UrlPolicy::new(["192.168.1.1"]);
/// let private = "http://192.168.1.1/x".parse().unwrap();
/// assert!(policy_all.check_url(&private).is_err());
/// ```
#[derive(Debug, Clone, Default)]
pub struct UrlPolicy {
  allowed_hosts: HashSet<String>,
  /// 是否显式放行 loopback。仅限 CI 本地 mock server
  /// 场景（「受控 URL」的 CI 层实现）；默认 `false`，
  /// 且**只豁免 loopback**——RFC1918 / 链路本地 / 元数据端点无条件拒绝。
  allow_loopback: bool,
}

impl UrlPolicy {
  /// 以域名白名单构造策略（条目大小写不敏感，匹配时统一小写比较）。
  pub fn new<I: Into<String>>(
    allowed_hosts: impl IntoIterator<Item = I>,
  ) -> Self {
    let hosts = allowed_hosts
      .into_iter()
      .map(|h| h.into().to_lowercase())
      .collect();
    Self {
      allowed_hosts: hosts,
      allow_loopback: false,
    }
  }

  /// 显式放行 loopback 的构造器（仅限 CI 本地 mock server 场景）。
  ///
  /// 与 [`Self::new`] 的唯一差异：字面 IP 保留段拦截对 **loopback 段**
  /// 豁免——CI 里本地 mock server 绑定 127.0.0.1，
  /// 白名单显式声明它是宿主可审计的显式配置。RFC1918 / 链路本地 /
  /// 元数据端点无论何种构造都无条件拒绝。
  pub fn allowing_loopback<I: Into<String>>(
    allowed_hosts: impl IntoIterator<Item = I>,
  ) -> Self {
    let mut this = Self::new(allowed_hosts);
    this.allow_loopback = true;
    this
  }

  /// 校验出站 URL：scheme → 字面 IP 段 → 域名白名单，逐层裁决。
  ///
  /// # Errors
  ///
  /// - scheme 非 http/https → [`UrlPolicyError::UnsupportedScheme`]；
  /// - 字面 IP 命中保留段 → [`UrlPolicyError::PrivateAddress`]；
  /// - host 不在白名单 → [`UrlPolicyError::HostNotAllowed`]。
  pub fn check_url(&self, url: &reqwest::Url) -> Result<(), UrlPolicyError> {
    // scheme 白名单
    let scheme = url.scheme();
    if scheme != "http" && scheme != "https" {
      return Err(UrlPolicyError::UnsupportedScheme(scheme.to_string()));
    }

    // 字面 IP 保留段拦截（先于白名单，不可被白名单放行）
    // 例外：构造器显式 opt-in 的 loopback（仅限 CI 本地 mock）；
    // RFC1918 / 链路本地 / 元数据端点没有豁免通道。
    let host_key = match url.host() {
      Some(Host::Ipv4(ip)) => {
        if is_blocked_ipv4(ip) && !(self.allow_loopback && ip.is_loopback()) {
          return Err(UrlPolicyError::PrivateAddress(ip.to_string()));
        }
        ip.to_string()
      }
      Some(Host::Ipv6(ip)) => {
        if is_blocked_ipv6(ip) && !(self.allow_loopback && ip.is_loopback()) {
          return Err(UrlPolicyError::PrivateAddress(ip.to_string()));
        }
        ip.to_string()
      }
      Some(Host::Domain(d)) => d.to_lowercase(),
      None => return Err(UrlPolicyError::HostNotAllowed(String::new())),
    };

    // 白名单精确匹配（大小写不敏感）
    if self.allowed_hosts.contains(&host_key) {
      Ok(())
    } else {
      Err(UrlPolicyError::HostNotAllowed(host_key))
    }
  }
}

/// 保留段统一判定入口（IPv4 + IPv6）：供 DNS 解析结果防线（SSRF
/// 第 3 层）复用——URL 层（[`UrlPolicy`]）与 IP 层共用同一份
/// 保留段清单，避免两套判定漂移。
pub(crate) fn is_blocked_ip(ip: core::net::IpAddr) -> bool {
  match ip {
    core::net::IpAddr::V4(v4) => is_blocked_ipv4(v4),
    core::net::IpAddr::V6(v6) => is_blocked_ipv6(v6),
  }
}

/// IPv4 保留段判定：RFC 1918 / 3927 / 6598 / 5735（含元数据端点段）。
fn is_blocked_ipv4(ip: core::net::Ipv4Addr) -> bool {
  let o = ip.octets();
  ip.is_loopback()               // 127/8（RFC 5735）
    || ip.is_private()           // 10/8、172.16/12、192.168/16（RFC 1918）
    || ip.is_link_local()        // 169.254/16（RFC 3927，含 169.254.169.254 元数据端点）
    || ip.is_unspecified()       // 0.0.0.0
    || o[0] == 0                 // 0/8 "this network"（RFC 5735）
    || o[0] >= 240               // 240/4 保留段 + 255.255.255.255 广播（RFC 5735）
    || (o[0] == 100 && (64..=127).contains(&o[1])) // 100.64/10 CGNAT（RFC 6598）
}

/// IPv6 保留段判定：`::1` / `::` / `fc00::/7`（ULA）/ `fe80::/10`（链路本地）。
fn is_blocked_ipv6(ip: core::net::Ipv6Addr) -> bool {
  let seg = ip.segments();
  ip.is_loopback()
    || ip.is_unspecified()
    || (seg[0] & 0xfe00) == 0xfc00 // fc00::/7 唯一本地地址
    || (seg[0] & 0xffc0) == 0xfe80 // fe80::/10 链路本地地址
}

/// 插件能力声明：宿主在加载期授予插件的能力集合。
///
/// - **信任链约束**：ACL 由宿主注入，不由插件自述；
/// - **最小权限**：[`Default`] 全部关闭，能力按需显式开启；
/// - **配额覆写**：`memory_limit_bytes` / `storage_quota_bytes` 为插件级
///   覆写，宿主用 `min(覆写, 进程默认)` 收紧（**只能收紧不能放大**）。
#[derive(Debug, Clone, Default)]
pub struct PluginAcl {
  /// 是否允许调用 `log` host function。
  pub log: bool,
  /// 是否允许调用 `storage_get` / `storage_put` host function。
  pub storage: bool,
  /// 是否允许调用 `http_proxy` host function。
  pub http: bool,
  /// http 能力的出站 URL 策略；`http = true` 时应同时提供。
  pub url_policy: Option<Arc<UrlPolicy>>,
  /// 单工具线性内存覆写（`Option<usize>`，仅收紧）。
  pub memory_limit_bytes: Option<usize>,
  /// 单插件 storage 总配额覆写（`Option<usize>`，仅收紧）。
  pub storage_quota_bytes: Option<usize>,
  /// 单工具执行超时上限覆写（毫秒；仅收紧，与进程
  /// [`crate::config::PluginRuntimeConfig::tool_timeout_ceiling`] 取
  /// min）——与内存 / storage 配额覆写对称。
  pub timeout_ceiling_ms: Option<u64>,
}

#[cfg(test)]
mod tests {
  use super::*;

  /// 场景：白名单内域名放行、白名单外拒绝（大小写不敏感）。
  #[test]
  fn whitelist_domain_matching_is_case_insensitive() {
    let policy = UrlPolicy::new(["API.Example.com"]);

    let ok = "https://api.example.com/v1".parse().unwrap();
    assert!(policy.check_url(&ok).is_ok());

    // URL host 大写也能命中（host_key 统一小写）
    let upper = "https://API.EXAMPLE.com/v1".parse().unwrap();
    assert!(policy.check_url(&upper).is_ok());

    let err = policy
      .check_url(&"https://evil.com".parse().unwrap())
      .unwrap_err();
    assert!(matches!(err, UrlPolicyError::HostNotAllowed(_)));
  }

  /// 场景：仅允许 http/https，其余 scheme 拒绝。
  #[test]
  fn non_http_schemes_are_rejected() {
    let policy = UrlPolicy::new(["api.example.com"]);
    for url in ["ftp://api.example.com/x", "file:///etc/passwd"] {
      let err = policy.check_url(&url.parse().unwrap()).unwrap_err();
      assert!(matches!(err, UrlPolicyError::UnsupportedScheme(_)), "{url}");
    }
  }

  /// 场景：私网 / 回环 / 元数据段字面 IP 一律拦截（白名单不可放行）。
  #[test]
  fn private_and_reserved_ips_are_blocked() {
    // 白名单里显式写了私网 IP 也必须拦——保留段检查先于白名单
    let policy =
      UrlPolicy::new(["192.168.1.1", "127.0.0.1", "169.254.169.254"]);
    for url in [
      "http://192.168.1.1/x",          // RFC 1918
      "http://10.0.0.1/x",             // RFC 1918
      "http://172.16.0.1/x",           // RFC 1918
      "http://127.0.0.1/x",            // 回环
      "http://169.254.169.254/latest", // 元数据端点
      "http://100.64.0.1/x",           // RFC 6598 CGNAT
      "http://0.0.0.0/x",              // RFC 5735
      "http://255.255.255.255/x",      // 广播
    ] {
      let err = policy.check_url(&url.parse().unwrap()).unwrap_err();
      assert!(matches!(err, UrlPolicyError::PrivateAddress(_)), "{url}");
    }
  }

  /// 场景：IPv6 回环 / ULA / 链路本地拦截；非保留段公网 IP 白名单命中可放行。
  #[test]
  fn ipv6_reserved_segments_are_blocked() {
    let policy = UrlPolicy::new(["2606:4700::1111"]);
    for url in [
      "http://[::1]/x",     // 回环
      "http://[::]/x",      // 未指定
      "http://[fc00::1]/x", // ULA fc00::/7
      "http://[fe80::1]/x", // 链路本地 fe80::/10
    ] {
      let err = policy.check_url(&url.parse().unwrap()).unwrap_err();
      assert!(matches!(err, UrlPolicyError::PrivateAddress(_)), "{url}");
    }
    // 公网 IPv6 在白名单内可放行
    let ok = "http://[2606:4700::1111]/x".parse().unwrap();
    assert!(policy.check_url(&ok).is_ok());
  }

  /// 场景：`allowing_loopback` 仅豁免 loopback——127.0.0.1
  /// 在白名单内放行；RFC1918 / 链路本地 / 元数据端点仍拒绝；默认
  /// `new` 构造不放行 loopback。
  #[test]
  fn allowing_loopback_exempts_only_loopback() {
    let policy = crate::acl::UrlPolicy::allowing_loopback(["127.0.0.1"]);
    let loopback: reqwest::Url =
      "http://127.0.0.1:18080/hello".parse().unwrap();
    assert!(policy.check_url(&loopback).is_ok());

    for blocked in [
      "http://192.168.1.1/x",
      "http://169.254.169.254/meta",
      "http://10.0.0.5/x",
    ] {
      let url: reqwest::Url = blocked.parse().unwrap();
      assert!(
        matches!(
          policy.check_url(&url),
          Err(UrlPolicyError::PrivateAddress(_))
        ),
        "{blocked} must stay blocked"
      );
    }

    let strict = crate::acl::UrlPolicy::new(["127.0.0.1"]);
    let loopback: reqwest::Url =
      "http://127.0.0.1:18080/hello".parse().unwrap();
    assert!(matches!(
      strict.check_url(&loopback),
      Err(UrlPolicyError::PrivateAddress(_))
    ));
  }

  /// 场景：`PluginAcl` 默认最小权限（全部能力关闭、无配额覆写）。
  #[test]
  fn plugin_acl_default_is_least_privilege() {
    let acl = PluginAcl::default();
    assert!(!acl.log);
    assert!(!acl.storage);
    assert!(!acl.http);
    assert!(acl.url_policy.is_none());
    assert!(acl.memory_limit_bytes.is_none());
    assert!(acl.storage_quota_bytes.is_none());
  }
}
