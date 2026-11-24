//! 降级路由：统一 `FallbackRule` 规则列表。
//!
//! 规则表达：一条规则 = `match`（精确二元组或 provider 级通配）→ `targets`
//! （有序目标列表，元素两形态）。**按声明顺序，第一条 `match` 命中的规则
//! 生效**——这是全部优先级规则（评审拍板：声明顺序优先，无"精确优先"魔法）。
//!
//! 旧的三级路由（explicit / same_provider / cross_provider 分离表达）已删除
//!（原因：两套规则并存会引发优先级歧义）；`same_provider` 隐式链保留——它是
//! 「provider 内 models 声明序」的落点，在候选解析中先于 fallback 规则。

use std::collections::HashMap;

use ra_core::error::RiboError;

use crate::{
  ids::{ModelId, ProviderId},
  model::Model,
  traits::{ProviderCatalog, ProviderErrorExt},
};

// ============================================================================
// 规则表达（配置层映射目标）
// ============================================================================

/// 匹配条件两形态：精确二元组 / provider 级通配（model 缺失）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FallbackMatch {
  /// 匹配的提供方。
  pub provider: ProviderId,
  /// 匹配的模型；`None` = provider 级通配（该 provider 的所有 model）。
  pub model: Option<ModelId>,
}

/// 目标两形态：显式二元组（跨命名体系精确降级）/ 仅 provider
///（同名 → 目标默认两级解析）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FallbackTarget {
  /// 直接用该 `(provider, model)`。
  Model {
    provider: ProviderId,
    model: ModelId,
  },
  /// 按「同名 → 目标默认」两级解析。
  Provider(ProviderId),
}

/// 一条降级规则。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FallbackRule {
  /// 匹配条件。
  pub match_: FallbackMatch,
  /// 有序目标列表（按序尝试）。
  pub targets: Vec<FallbackTarget>,
}

// ============================================================================
// 解析结果
// ============================================================================

/// 降级落空原因（零候选时供摘要使用；互斥按序判定）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DegradeReason {
  /// 无 fallback 规则命中（或规则 targets 全被 AUTH_FAILED 跳过），
  /// 且隐式同 provider 链未产出候选。
  NoCandidatesConfigured,
  /// 候选存在但全部被窗口过滤。
  CandidatesWindowTooSmall,
  /// 候选被实际尝试过，全部失败（gateway 层在尝试耗尽时设置；
  /// `candidates` 解析期只产出前两种）。
  ChainExhausted,
}

impl DegradeReason {
  /// 摘要文本（`degraded: none (reason)` 用）。
  pub fn as_str(self) -> &'static str {
    match self {
      Self::NoCandidatesConfigured => "no_candidates_configured",
      Self::CandidatesWindowTooSmall => "candidates_window_too_small",
      Self::ChainExhausted => "chain_exhausted",
    }
  }
}

/// 候选解析结果（评审结论：reason 结构化返回，不经由错误 context）。
#[derive(Debug, Clone)]
pub struct CandidatesResult {
  /// 有序候选列表（可能为空）。
  pub candidates: Vec<Candidate>,
  /// 零候选时的原因；有候选时为 `None`。
  pub reason: Option<DegradeReason>,
}

/// 降级候选：一个具体的 `(提供方, 模型)` 组合。
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
  /// 候选提供方 ID。
  pub provider: ProviderId,
  /// 候选模型描述。
  pub model: Model,
}

// ============================================================================
// 路由表
// ============================================================================

/// 降级路由表。
///
/// 由配置方构建后注入网关；查询侧（[`RouteMap::candidates`]）只读。
/// 规则经 `insert_fallback_rules` 写入；`same_provider` 隐式链由「models 声明序」推导写入。
#[derive(Debug, Clone, Default)]
pub struct RouteMap {
  /// 有序降级规则（声明顺序 = 优先级）。
  rules: Vec<FallbackRule>,
  /// 隐式链：模型名 → 同 provider 内其余模型（声明序，不含自身）。
  same_provider: HashMap<ModelId, Vec<ModelId>>,
}

impl RouteMap {
  /// 写入降级规则列表（整体替换；规则间声明顺序即优先级）。
  pub fn insert_fallback_rules(&mut self, rules: Vec<FallbackRule>) {
    self.rules = rules;
  }

  /// 写入 隐式链（模型名 → 同 provider 其余模型声明序）。
  pub fn insert_same_provider(&mut self, model: ModelId, chain: Vec<ModelId>) {
    self.same_provider.insert(model, chain);
  }

  /// 解析降级候选列表。
  ///
  /// 处理流程：
  ///
  /// 1. 错误不可降级 → 空结果（reason `None`，调用方不降级）；
  /// 2. **隐式同 provider 链**（先于规则）：`AUTH_FAILED` 时跳过
  ///    （同 api_key 换模型必然复现 401），其余可降级错误按声明序产出
  ///    主 provider 内的备选；
  /// 3. **fallback 规则列表**：按声明顺序，第一条 `match` 命中的规则生效；
  ///    `targets` 元素按序解析——`Model` 形态直用（`AUTH_FAILED` 仍尝试，
  ///    尊重用户显式声明），`Provider` 形态走「同名 → 目标默认」两级解析
  ///    （`AUTH_FAILED` 时跳过，详见 `DegradeReason`）；
  /// 4. **窗口过滤**：`context_window < min_context_window` 的候选
  ///    滤除（`min = 0` 不过滤；调用方传「估算 × 1.1」）；
  /// 5. 零候选时按序判定 reason（互斥，见 `DegradeReason`）。
  ///
  /// 重复与自我映射的候选会被剔除（[`push_candidate`]）。
  pub fn candidates(
    &self,
    primary: &Model,
    err: &RiboError,
    catalog: &dyn ProviderCatalog,
    min_context_window: usize,
  ) -> CandidatesResult {
    if !err.is_degradable() {
      // 不可降级错误根本不应进入降级路径；reason 置 None（调用方 gate）。
      return CandidatesResult {
        candidates: Vec::new(),
        reason: None,
      };
    }
    let auth_failed = err.code() == AUTH_FAILED;
    let mut out: Vec<Candidate> = Vec::new();

    // ===== 1. 隐式同 provider 链（；AUTH_FAILED 时跳过）=====
    if !auth_failed && let Some(chain) = self.same_provider.get(&primary.id) {
      for m in chain {
        if let Some(m) = catalog.model(&primary.provider, m) {
          push_candidate(primary, &mut out, primary.provider.clone(), m);
        }
      }
    }

    // ===== 2. fallback 规则（声明序，首条命中生效）=====
    let mut rule_matched = false;
    for (index, rule) in self.rules.iter().enumerate() {
      if !rule_matches(&rule.match_, primary) {
        continue;
      }
      rule_matched = true;
      let _ = index;
      for target in &rule.targets {
        match target {
          FallbackTarget::Model { provider, model } => {
            // 显式二元组：AUTH_FAILED 仍尝试（用户意志）
            if let Some(m) = catalog.model(provider, model) {
              push_candidate(primary, &mut out, provider.clone(), m);
            }
          }
          FallbackTarget::Provider(provider) => {
            // AUTH_FAILED 跳过（本质仍是"同 key 下换模型"）
            if auth_failed {
              continue;
            }
            // 两级解析：同名 → 目标默认（目录声明第一个）
            if let Some(m) = catalog.model(provider, &primary.id) {
              push_candidate(primary, &mut out, provider.clone(), m);
            } else if let Some(prov) = catalog.provider(provider)
              && let Some(m) = prov.models().first().cloned()
            {
              push_candidate(primary, &mut out, provider.clone(), m);
            }
          }
        }
      }
      break; // 首条命中规则生效，后续规则不再评估
    }

    // ===== 3. 窗口过滤（min=0 不过滤）=====
    let mut window_filtered = false;
    if min_context_window > 0 {
      let before = out.len();
      out.retain(|c| c.model.context_window as usize >= min_context_window);
      window_filtered = before > 0 && out.is_empty();
    }

    // ===== 4. reason 判定（互斥按序）=====
    let reason = if !out.is_empty() {
      None
    } else if window_filtered {
      Some(DegradeReason::CandidatesWindowTooSmall)
    } else {
      // 无规则命中 / 规则 targets 全跳过 / 隐式链空——统一归"未配置有效候选"
      let _ = rule_matched;
      Some(DegradeReason::NoCandidatesConfigured)
    };
    CandidatesResult {
      candidates: out,
      reason,
    }
  }
}

/// 规则匹配：provider 相等，且 model 为通配（None）或精确相等。
fn rule_matches(m: &FallbackMatch, primary: &Model) -> bool {
  if m.provider != primary.provider {
    return false;
  }
  match &m.model {
    None => true, // provider 级通配
    Some(id) => id == &primary.id,
  }
}

/// 追加候选：跳过与主候选重复、以及已在结果中的重复项。
fn push_candidate(
  primary: &Model,
  out: &mut Vec<Candidate>,
  provider: ProviderId,
  model: Model,
) {
  if provider == primary.provider && model.id == primary.id {
    return;
  }
  if out
    .iter()
    .any(|c| c.provider == provider && c.model.id == model.id)
  {
    return;
  }
  out.push(Candidate { provider, model });
}

use crate::error::AUTH_FAILED;

#[cfg(test)]
mod tests {
  use std::collections::HashMap;
  use std::sync::Arc;

  use super::*;
  use crate::{
    ids::ModelId, options::ProviderOptions, stream::ProviderEventStream,
    traits::Provider,
  };
  use ra_core::error::RiboResult;

  fn provider_id(raw: &str) -> ProviderId {
    ProviderId::parse(raw).expect("测试提供方 ID 合法")
  }

  fn model(provider_raw: &str, id: &str) -> Model {
    Model::new(
      provider_id(provider_raw),
      ModelId::parse(id).expect("测试模型 ID 合法"),
      id,
      "http://stub.local",
    )
  }

  /// 测试桩 provider：models() 可用（"目标默认"解析依赖）。
  struct TestProvider {
    id: ProviderId,
    models: Vec<Model>,
  }

  #[async_trait::async_trait]
  impl Provider for TestProvider {
    fn id(&self) -> &ProviderId {
      &self.id
    }
    fn models(&self) -> &[Model] {
      &self.models
    }
    async fn stream(
      &self,
      _options: ProviderOptions<'_>,
    ) -> RiboResult<ProviderEventStream> {
      unreachable!("candidates 解析不应触发 stream")
    }
  }

  /// 可控制各模型窗口的测试目录（provider() 可用）。
  struct TestCatalog {
    entries: HashMap<ProviderId, Vec<Model>>,
  }

  impl TestCatalog {
    fn new(entries: Vec<(&str, Vec<Model>)>) -> Self {
      Self {
        entries: entries
          .into_iter()
          .map(|(pid, models)| (provider_id(pid), models))
          .collect(),
      }
    }

    fn with_windows(entries: Vec<(&str, Vec<(&str, u32)>)>) -> Self {
      Self {
        entries: entries
          .into_iter()
          .map(|(pid, models)| {
            (
              provider_id(pid),
              models
                .into_iter()
                .map(|(id, w)| model(pid, id).with_context_window(w))
                .collect(),
            )
          })
          .collect(),
      }
    }
  }

  impl ProviderCatalog for TestCatalog {
    fn provider(&self, id: &ProviderId) -> Option<Arc<dyn Provider>> {
      self.entries.get(id).map(|models| {
        Arc::new(TestProvider {
          id: id.clone(),
          models: models.clone(),
        }) as Arc<dyn Provider>
      })
    }
    fn model(&self, id: &ProviderId, model: &ModelId) -> Option<Model> {
      self
        .entries
        .get(id)?
        .iter()
        .find(|m| &m.id == model)
        .cloned()
    }
    fn find_model(&self, model: &ModelId) -> Option<(ProviderId, Model)> {
      self.entries.iter().find_map(|(pid, models)| {
        models
          .iter()
          .find(|m| &m.id == model)
          .map(|m| (pid.clone(), m.clone()))
      })
    }
    fn providers(&self) -> Vec<ProviderId> {
      self.entries.keys().cloned().collect()
    }
  }

  fn rate_limited() -> RiboError {
    crate::error::pre_stream(crate::error::RATE_LIMITED, "rate limited")
  }

  fn auth_failed() -> RiboError {
    crate::error::pre_stream(crate::error::AUTH_FAILED, "auth failed")
  }

  fn ids(result: &CandidatesResult) -> Vec<String> {
    result
      .candidates
      .iter()
      .map(|c| format!("{}/{}", c.provider, c.model.id))
      .collect()
  }

  // ===== 规则匹配语义 =====

  /// 场景：规则按声明顺序匹配——精确规则在前不被通配覆盖。
  #[test]
  fn rules_match_in_declaration_order() {
    let mut route = RouteMap::default();
    route.insert_fallback_rules(vec![
      FallbackRule {
        match_: FallbackMatch {
          provider: provider_id("zhipu"),
          model: Some(ModelId::parse("glm-4.7-flash").unwrap()),
        },
        targets: vec![FallbackTarget::Model {
          provider: provider_id("backup"),
          model: ModelId::parse("glm-4.5-flash").unwrap(),
        }],
      },
      FallbackRule {
        match_: FallbackMatch {
          provider: provider_id("zhipu"),
          model: None, // 通配
        },
        targets: vec![FallbackTarget::Provider(provider_id("other"))],
      },
    ]);
    let catalog = TestCatalog::new(vec![
      ("zhipu", vec![model("zhipu", "glm-4.7-flash")]),
      ("backup", vec![model("backup", "glm-4.5-flash")]),
      ("other", vec![model("other", "m1")]),
    ]);
    let out = route.candidates(
      &model("zhipu", "glm-4.7-flash"),
      &rate_limited(),
      &catalog,
      0,
    );
    // 精确规则命中（声明序在前），通配规则不参与
    assert_eq!(ids(&out), vec!["backup/glm-4.5-flash"]);
    assert_eq!(out.reason, None);
  }

  /// 场景：通配规则在前 → 后序精确规则 shadowed，实际命中通配。
  #[test]
  fn wildcard_first_shadows_later_precise_rule() {
    let mut route = RouteMap::default();
    route.insert_fallback_rules(vec![
      FallbackRule {
        match_: FallbackMatch {
          provider: provider_id("zhipu"),
          model: None,
        },
        targets: vec![FallbackTarget::Provider(provider_id("other"))],
      },
      FallbackRule {
        match_: FallbackMatch {
          provider: provider_id("zhipu"),
          model: Some(ModelId::parse("glm-4.7-flash").unwrap()),
        },
        targets: vec![FallbackTarget::Model {
          provider: provider_id("backup"),
          model: ModelId::parse("glm-4.5-flash").unwrap(),
        }],
      },
    ]);
    let catalog = TestCatalog::new(vec![
      ("zhipu", vec![model("zhipu", "glm-4.7-flash")]),
      ("other", vec![model("other", "m1")]),
      ("backup", vec![model("backup", "glm-4.5-flash")]),
    ]);
    let out = route.candidates(
      &model("zhipu", "glm-4.7-flash"),
      &rate_limited(),
      &catalog,
      0,
    );
    // 通配规则先命中——shadowing 行为符合"声明顺序优先"
    assert_eq!(ids(&out), vec!["other/m1"]);
  }

  /// 场景：targets 多元素按序尝试产出有序候选；混合两形态。
  #[test]
  fn targets_produce_ordered_candidates_mixed_forms() {
    let mut route = RouteMap::default();
    route.insert_fallback_rules(vec![FallbackRule {
      match_: FallbackMatch {
        provider: provider_id("p1"),
        model: Some(ModelId::parse("m1").unwrap()),
      },
      targets: vec![
        FallbackTarget::Model {
          provider: provider_id("p2"),
          model: ModelId::parse("m2").unwrap(),
        },
        FallbackTarget::Provider(provider_id("p3")), // 同名
        FallbackTarget::Provider(provider_id("p1")), // 同 provider（同名=自身，去重剔除）
      ],
    }]);
    let catalog = TestCatalog::new(vec![
      ("p1", vec![model("p1", "m1"), model("p1", "m2")]),
      ("p2", vec![model("p2", "m2")]),
      ("p3", vec![model("p3", "m1")]),
    ]);
    let out =
      route.candidates(&model("p1", "m1"), &rate_limited(), &catalog, 0);
    // 第三 target Provider(p1) 同名解析 = 失败模型自身 → 主候选去重剔除；
    // 同 provider 内其他模型由 隐式链负责（本测试未配置）
    assert_eq!(ids(&out), vec!["p2/m2", "p3/m1"]);
    assert_eq!(out.reason, None);
  }

  /// 场景：`{ provider }` 形态两级解析——同名直命中。
  #[test]
  fn provider_target_resolves_same_name_first() {
    let mut route = RouteMap::default();
    route.insert_fallback_rules(vec![FallbackRule {
      match_: FallbackMatch {
        provider: provider_id("p1"),
        model: Some(ModelId::parse("m1").unwrap()),
      },
      targets: vec![FallbackTarget::Provider(provider_id("p2"))],
    }]);
    let catalog = TestCatalog::new(vec![
      ("p1", vec![model("p1", "m1")]),
      ("p2", vec![model("p2", "m1"), model("p2", "m2")]), // 同名 m1 在前
    ]);
    let out =
      route.candidates(&model("p1", "m1"), &rate_limited(), &catalog, 0);
    assert_eq!(ids(&out), vec!["p2/m1"]);
  }

  /// 场景：`model` 含 `/`（OpenRouter 场景）的结构化匹配无歧义。
  #[test]
  fn model_id_with_slash_matches_structurally() {
    let mut route = RouteMap::default();
    route.insert_fallback_rules(vec![FallbackRule {
      match_: FallbackMatch {
        provider: provider_id("openrouter"),
        model: Some(ModelId::parse("openai/gpt-4o").unwrap()),
      },
      targets: vec![FallbackTarget::Model {
        provider: provider_id("backup"),
        model: ModelId::parse("glm-4.5-flash").unwrap(),
      }],
    }]);
    let catalog = TestCatalog::new(vec![
      ("openrouter", vec![model("openrouter", "openai/gpt-4o")]),
      ("backup", vec![model("backup", "glm-4.5-flash")]),
    ]);
    let out = route.candidates(
      &model("openrouter", "openai/gpt-4o"),
      &rate_limited(),
      &catalog,
      0,
    );
    assert_eq!(ids(&out), vec!["backup/glm-4.5-flash"]);
  }

  /// 场景：规则全落空（无命中规则）→ 零候选 + no_candidates_configured。
  #[test]
  fn no_matching_rule_yields_no_candidates_reason() {
    let route = RouteMap::default();
    let catalog = TestCatalog::new(vec![("p1", vec![model("p1", "m1")])]);
    let out =
      route.candidates(&model("p1", "m1"), &rate_limited(), &catalog, 0);
    assert!(out.candidates.is_empty());
    assert_eq!(out.reason, Some(DegradeReason::NoCandidatesConfigured));
  }

  /// 场景：不可降级错误 → 空结果且 reason 为 None（调用方 gate）。
  #[test]
  fn not_degradable_error_returns_empty_without_reason() {
    let route = RouteMap::default();
    let catalog = TestCatalog::new(vec![("p1", vec![model("p1", "m1")])]);
    let out = route.candidates(
      &model("p1", "m1"),
      &crate::error::pre_stream(crate::error::INVALID_REQUEST, "bad"),
      &catalog,
      0,
    );
    assert!(out.candidates.is_empty());
    assert_eq!(out.reason, None);
  }

  // ===== 隐式链 =====

  /// 场景：隐式同 provider 链按声明序产出（先于 fallback 规则候选）。
  #[test]
  fn implicit_same_provider_chain_first() {
    let mut route = RouteMap::default();
    route.insert_same_provider(
      ModelId::parse("m1").unwrap(),
      vec![ModelId::parse("m2").unwrap(), ModelId::parse("m3").unwrap()],
    );
    route.insert_fallback_rules(vec![FallbackRule {
      match_: FallbackMatch {
        provider: provider_id("p1"),
        model: Some(ModelId::parse("m1").unwrap()),
      },
      targets: vec![FallbackTarget::Model {
        provider: provider_id("p2"),
        model: ModelId::parse("m9").unwrap(),
      }],
    }]);
    let catalog = TestCatalog::new(vec![
      (
        "p1",
        vec![model("p1", "m1"), model("p1", "m2"), model("p1", "m3")],
      ),
      ("p2", vec![model("p2", "m9")]),
    ]);
    let out =
      route.candidates(&model("p1", "m1"), &rate_limited(), &catalog, 0);
    assert_eq!(ids(&out), vec!["p1/m2", "p1/m3", "p2/m9"]);
  }

  /// 场景：AUTH_FAILED 跳过隐式同 provider 链与 `{ provider }` 形态，
  /// 但 `{ provider, model }` 显式形态仍尝试。
  #[test]
  fn auth_failed_skips_implicit_and_provider_targets() {
    let mut route = RouteMap::default();
    route.insert_same_provider(
      ModelId::parse("m1").unwrap(),
      vec![ModelId::parse("m2").unwrap()],
    );
    route.insert_fallback_rules(vec![FallbackRule {
      match_: FallbackMatch {
        provider: provider_id("p1"),
        model: Some(ModelId::parse("m1").unwrap()),
      },
      targets: vec![
        FallbackTarget::Provider(provider_id("p2")), // AUTH 跳过
        FallbackTarget::Model {
          // 显式仍尝试
          provider: provider_id("p3"),
          model: ModelId::parse("m3").unwrap(),
        },
      ],
    }]);
    let catalog = TestCatalog::new(vec![
      ("p1", vec![model("p1", "m1"), model("p1", "m2")]),
      ("p2", vec![model("p2", "m2")]),
      ("p3", vec![model("p3", "m3")]),
    ]);
    let out = route.candidates(&model("p1", "m1"), &auth_failed(), &catalog, 0);
    assert_eq!(ids(&out), vec!["p3/m3"]);
    assert_eq!(out.reason, None);
  }

  /// 场景（回归）：AUTH_FAILED + targets 全是 `{ provider }` 形态
  /// → 全部跳过 → 规则视为未命中 → reason `no_candidates_configured`。
  #[test]
  fn auth_failed_all_provider_targets_skipped_yields_no_candidates() {
    let mut route = RouteMap::default();
    route.insert_fallback_rules(vec![FallbackRule {
      match_: FallbackMatch {
        provider: provider_id("p1"),
        model: Some(ModelId::parse("m1").unwrap()),
      },
      targets: vec![FallbackTarget::Provider(provider_id("p2"))],
    }]);
    let catalog = TestCatalog::new(vec![
      ("p1", vec![model("p1", "m1")]),
      ("p2", vec![model("p2", "m1")]),
    ]);
    let out = route.candidates(&model("p1", "m1"), &auth_failed(), &catalog, 0);
    assert!(out.candidates.is_empty());
    assert_eq!(out.reason, Some(DegradeReason::NoCandidatesConfigured));
  }

  // ===== 窗口过滤 =====

  /// 场景：窗口过滤边界——`window == min` 通过，`window < min` 滤除。
  #[test]
  fn window_filter_boundary() {
    let mut route = RouteMap::default();
    route.insert_fallback_rules(vec![FallbackRule {
      match_: FallbackMatch {
        provider: provider_id("p1"),
        model: Some(ModelId::parse("m1").unwrap()),
      },
      targets: vec![
        FallbackTarget::Model {
          provider: provider_id("p2"),
          model: ModelId::parse("m_small").unwrap(), // 1000 < 1100
        },
        FallbackTarget::Model {
          provider: provider_id("p2"),
          model: ModelId::parse("m_equal").unwrap(), // 1100 == 1100 → 通过
        },
      ],
    }]);
    let catalog = TestCatalog::with_windows(vec![(
      "p2",
      vec![("m_small", 1000), ("m_equal", 1100)],
    )]);
    let out =
      route.candidates(&model("p1", "m1"), &rate_limited(), &catalog, 1100);
    assert_eq!(ids(&out), vec!["p2/m_equal"]);
    assert_eq!(out.reason, None);
  }

  /// 场景：候选全被窗口过滤 → `candidates_window_too_small`。
  #[test]
  fn all_candidates_window_filtered_yields_reason() {
    let mut route = RouteMap::default();
    route.insert_fallback_rules(vec![FallbackRule {
      match_: FallbackMatch {
        provider: provider_id("p1"),
        model: Some(ModelId::parse("m1").unwrap()),
      },
      targets: vec![FallbackTarget::Model {
        provider: provider_id("p2"),
        model: ModelId::parse("m_small").unwrap(),
      }],
    }]);
    let catalog =
      TestCatalog::with_windows(vec![("p2", vec![("m_small", 500)])]);
    let out =
      route.candidates(&model("p1", "m1"), &rate_limited(), &catalog, 1100);
    assert!(out.candidates.is_empty());
    assert_eq!(out.reason, Some(DegradeReason::CandidatesWindowTooSmall));
  }

  /// 场景：`min_context_window = 0` 不过滤（默认行为不变）。
  #[test]
  fn zero_min_context_window_disables_filter() {
    let mut route = RouteMap::default();
    route.insert_fallback_rules(vec![FallbackRule {
      match_: FallbackMatch {
        provider: provider_id("p1"),
        model: Some(ModelId::parse("m1").unwrap()),
      },
      targets: vec![FallbackTarget::Model {
        provider: provider_id("p2"),
        model: ModelId::parse("m_tiny").unwrap(),
      }],
    }]);
    let catalog = TestCatalog::with_windows(vec![("p2", vec![("m_tiny", 1)])]);
    let out =
      route.candidates(&model("p1", "m1"), &rate_limited(), &catalog, 0);
    assert_eq!(ids(&out), vec!["p2/m_tiny"]);
  }

  // ===== 去重与防御 =====

  /// 场景：去重防环（A → B → A 不产出自我映射）。
  #[test]
  fn dedup_and_no_self_mapping() {
    let mut route = RouteMap::default();
    route.insert_fallback_rules(vec![FallbackRule {
      match_: FallbackMatch {
        provider: provider_id("p1"),
        model: Some(ModelId::parse("m1").unwrap()),
      },
      targets: vec![
        FallbackTarget::Provider(provider_id("p1")), // 自我 → 剔除
        FallbackTarget::Model {
          provider: provider_id("p2"),
          model: ModelId::parse("m1").unwrap(),
        },
        FallbackTarget::Model {
          provider: provider_id("p2"),
          model: ModelId::parse("m1").unwrap(), // 重复 → 剔除
        },
      ],
    }]);
    let catalog = TestCatalog::new(vec![
      ("p1", vec![model("p1", "m1")]),
      ("p2", vec![model("p2", "m1")]),
    ]);
    let out =
      route.candidates(&model("p1", "m1"), &rate_limited(), &catalog, 0);
    assert_eq!(ids(&out), vec!["p2/m1"]);
  }

  /// 场景：目标 provider models 为空 → 该 target 产出零候选（防御分支）。
  #[test]
  fn empty_target_provider_models_yields_nothing() {
    let mut route = RouteMap::default();
    route.insert_fallback_rules(vec![FallbackRule {
      match_: FallbackMatch {
        provider: provider_id("p1"),
        model: Some(ModelId::parse("m1").unwrap()),
      },
      targets: vec![
        FallbackTarget::Provider(provider_id("empty")), // 空目录 → 零
        FallbackTarget::Model {
          provider: provider_id("p2"),
          model: ModelId::parse("m2").unwrap(),
        },
      ],
    }]);
    let catalog = TestCatalog::new(vec![
      ("p1", vec![model("p1", "m1")]),
      ("empty", vec![]),
      ("p2", vec![model("p2", "m2")]),
    ]);
    let out =
      route.candidates(&model("p1", "m1"), &rate_limited(), &catalog, 0);
    assert_eq!(ids(&out), vec!["p2/m2"]);
  }

  // ===== shadowing 判定伪码 =====

  /// 场景：shadows 判定——通配覆盖精确、精确不覆盖通配、完全重复覆盖。
  #[test]
  fn shadows_matrix() {
    let wildcard = FallbackMatch {
      provider: provider_id("zhipu"),
      model: None,
    };
    let precise = FallbackMatch {
      provider: provider_id("zhipu"),
      model: Some(ModelId::parse("m1").unwrap()),
    };
    let other_provider = FallbackMatch {
      provider: provider_id("other"),
      model: None,
    };
    // 通配覆盖精确：是；精确不覆盖通配：否；完全重复：是；不同 provider：否
    assert!(shadows(&wildcard, &precise));
    assert!(!shadows(&precise, &wildcard));
    assert!(shadows(&wildcard, &wildcard));
    assert!(!shadows(&other_provider, &precise));
  }

  fn shadows(prev: &FallbackMatch, next: &FallbackMatch) -> bool {
    let provider_covered = prev.provider == next.provider;
    let model_covered = match (&prev.model, &next.model) {
      (None, _) => true,
      (Some(p), Some(n)) => p == n,
      (Some(_), None) => false,
    };
    provider_covered && model_covered
  }
}
