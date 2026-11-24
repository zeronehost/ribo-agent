//! 权威清单（Step C B3.2，「权威 schema 来源」）：宿主侧载的
//! 信任源，guest 自述降级为交叉校验。
//!
//! **信任链问题**：Step B 的 host 侧 schema 采信 guest `list_tools()`
//! 自述——guest 声称什么工具就注册什么，恶意 guest 可谎报工具能力。
//!
//! 生产禁用理由 ②。本模块提供宿主侧权威清单：宿主在装配期读取
//! `manifest.json`（与 guest ABI 同构的 JSON），加载期逐项比对 guest
//! 自述——**不符即拒绝加载**，guest 自述从「信任源」降级为「交叉
//! 校验样本」，信任链闭合。
//!
//! # Examples
//!
//! ```
//! use ra_plugins::authority::AuthorityManifest;
//!
//! // 权威清单与 guest ABI 同构（plugin_id + tools）
//! let auth = AuthorityManifest::from_json(
//!     br#"{"plugin_id":"t","tools":[]}"#,
//! )
//! .expect("valid authority manifest");
//! assert_eq!(auth.plugin_id, "t");
//! ```

use std::collections::BTreeMap;

use ra_core::error::RiboResult;

use crate::traits::ToolManifest;
use crate::wasm::abi::parse_manifest;

/// 宿主侧权威清单：插件 ID 与工具清单的唯一信任源（B3.2）。
///
/// 结构与 guest ABI 的 manifest JSON 同构（`plugin_id` + `tools`），
/// 经 [`AuthorityManifest::from_json`] 解析——`timeout_hint_ms` 走与
/// guest 自述相同的双向钳制，保证两侧比较语义一致。
#[derive(Debug, Clone)]
pub struct AuthorityManifest {
  /// 权威 plugin_id：与 guest 自述不一致 → 拒绝加载。
  pub plugin_id: String,
  /// 权威工具清单：逐项比对名称 / schema / 超时 / 幂等 / 内存覆写。
  pub tools: Vec<ToolManifest>,
}

impl AuthorityManifest {
  /// 从 manifest JSON 解析权威清单（复用 guest ABI 的解析与钳制逻辑）。
  ///
  /// # Errors
  ///
  /// JSON 结构非法 → `plugin.plugin_exec_failed`（与 guest manifest
  /// 解析同一错误域）。
  pub fn from_json(bytes: &[u8]) -> RiboResult<Self> {
    let m = parse_manifest(bytes)?;
    Ok(Self {
      plugin_id: m.plugin_id,
      tools: m.tools,
    })
  }
}

/// 加载期权威校验：plugin_id 与工具清单逐项比对（B3.2）。
///
/// 比对规则（任一不符即拒绝加载，错误信息指明具体差异）：
///
/// 1. `plugin_id` 必须一致；
/// 2. 工具名集合必须完全一致（guest 多报 / 少报均拒绝）；
/// 3. 每个工具的 `parameters` schema、`timeout_hint`、`idempotent`、
///    `memory_limit_bytes` 必须逐字段一致。
///
/// # Errors
///
/// 任何不匹配 → `plugin.plugin_exec_failed`（错误信息含
/// `authority manifest mismatch` 前缀与差异明细）。
pub(crate) fn check_authority(
  plugin_id: &str,
  tools: &[ToolManifest],
  authority: &AuthorityManifest,
) -> RiboResult<()> {
  let mismatch = |what: String| {
    crate::error::plugin_exec_failed(
      plugin_id,
      "authority manifest",
      &format!("authority manifest mismatch: {what}"),
    )
  };

  if authority.plugin_id != plugin_id {
    return Err(mismatch(format!(
      "plugin_id `{}` declared but guest self-reports `{}`",
      authority.plugin_id, plugin_id
    )));
  }

  let guest: BTreeMap<&str, &ToolManifest> =
    tools.iter().map(|t| (t.name.as_str(), t)).collect();
  let auth: BTreeMap<&str, &ToolManifest> = authority
    .tools
    .iter()
    .map(|t| (t.name.as_str(), t))
    .collect();

  for (name, g) in &guest {
    let Some(a) = auth.get(*name) else {
      return Err(mismatch(format!(
        "tool `{name}` not declared in authority manifest"
      )));
    };
    if g.parameters != a.parameters {
      return Err(mismatch(format!(
        "tool `{name}` parameters schema mismatch"
      )));
    }
    if g.timeout_hint != a.timeout_hint {
      return Err(mismatch(format!("tool `{name}` timeout_hint mismatch")));
    }
    if g.idempotent != a.idempotent {
      return Err(mismatch(format!("tool `{name}` idempotent mismatch")));
    }
    if g.memory_limit_bytes != a.memory_limit_bytes {
      return Err(mismatch(format!(
        "tool `{name}` memory_limit_bytes mismatch"
      )));
    }
  }

  for name in auth.keys() {
    if !guest.contains_key(*name) {
      return Err(mismatch(format!(
        "tool `{name}` declared in authority manifest but guest does not provide it"
      )));
    }
  }

  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;
  use serde_json::json;

  fn tool(name: &str) -> ToolManifest {
    ToolManifest {
      name: name.to_string(),
      description: format!("{name} tool"),
      parameters: json!({ "type": "object" }),
      timeout_hint: None,
      idempotent: true,
      memory_limit_bytes: None,
    }
  }

  fn authority(plugin_id: &str, names: &[&str]) -> AuthorityManifest {
    AuthorityManifest {
      plugin_id: plugin_id.to_string(),
      tools: names.iter().map(|n| tool(n)).collect(),
    }
  }

  /// 场景：清单完全一致 → 校验通过。
  #[test]
  fn matching_authority_passes() {
    let auth = authority("t", &["echo", "greet"]);
    let tools = vec![tool("echo"), tool("greet")];
    assert!(check_authority("t", &tools, &auth).is_ok());
  }

  /// 场景：guest 多报工具（不在权威清单中）→ 拒绝。
  #[test]
  fn guest_extra_tool_is_rejected() {
    let auth = authority("t", &["echo"]);
    let tools = vec![tool("echo"), tool("hidden")];
    let err = check_authority("t", &tools, &auth).expect_err("must reject");
    assert!(
      err.message().contains("not declared in authority manifest"),
      "got: {}",
      err.message()
    );
  }

  /// 场景：权威清单声明 guest 未提供的工具 → 拒绝。
  #[test]
  fn authority_extra_tool_is_rejected() {
    let auth = authority("t", &["echo", "phantom"]);
    let tools = vec![tool("echo")];
    let err = check_authority("t", &tools, &auth).expect_err("must reject");
    assert!(
      err.message().contains("guest does not provide it"),
      "got: {}",
      err.message()
    );
  }

  /// 场景：schema 篡改（parameters 不一致）→ 拒绝。
  #[test]
  fn schema_mismatch_is_rejected() {
    let auth = AuthorityManifest {
      plugin_id: "t".to_string(),
      tools: vec![ToolManifest {
        parameters: json!({ "type": "object", "properties": {
          "x": { "type": "string" } // 与 guest 不同
        } }),
        ..tool("echo")
      }],
    };
    let tools = vec![tool("echo")];
    let err = check_authority("t", &tools, &auth).expect_err("must reject");
    assert!(
      err.message().contains("parameters schema mismatch"),
      "got: {}",
      err.message()
    );
  }

  /// 场景：plugin_id 不一致 → 拒绝。
  #[test]
  fn plugin_id_mismatch_is_rejected() {
    let auth = authority("other", &[]);
    let err = check_authority("t", &[], &auth).expect_err("must reject");
    assert!(
      err.message().contains("authority manifest mismatch"),
      "got: {}",
      err.message()
    );
  }
}
