//! 降级路由表：把"可降级错误"解析为候选 (provider, model) 列表。
//!
//! 三级路由按优先级依次生效：
//!
//! 1. `explicit`：显式模型映射，命中即只返回该唯一候选；
//! 2. `same_provider`：同提供方内的备选模型链；
//! 3. `cross_provider`：跨提供方的同模型链（仅当错误可跨提供方降级）。

use std::collections::HashMap;

use ra_core::error::RiboError;

use crate::{
  ids::{ModelId, ModelRef, ProviderId},
  model::Model,
  traits::{ProviderCatalog, ProviderErrorExt},
};

/// 降级路由表。
///
/// 由配置方构建后注入网关；查询侧（[`RouteMap::candidates`]）只读，
/// 候选的实际存在性以目录（catalog）为准。
///
/// 表达能力分层：`explicit` 是精确的 `(provider, model)` 级映射，
/// 用于表达跨模型命名体系的备选；`same_provider` / `cross_provider`
/// 是静态链式匹配，依赖各提供方共享模型命名，配置 `same_provider`
/// 链时应使用跨提供方通用的模型名。
#[derive(Debug, Clone, Default)]
pub struct RouteMap {
  /// 显式模型映射：主模型 → 目标模型（最高优先级，命中即短路）。
  explicit: HashMap<ModelRef, ModelRef>,
  /// 同提供方备选链：模型名 → 备选模型名序列（模型名级、提供方无关）。
  ///
  /// 同一份链会在两处求值：主提供方目录（二级降级）与各目标
  /// 提供方目录（三级降级的回退），即"主链 ∩ 提供方目录"；
  /// 不存在"主提供方专属链"与"目标提供方链"的区分。
  same_provider: HashMap<ModelId, Vec<ModelId>>,
  /// 跨提供方链：主提供方 → 备选提供方序列（按同模型名查找）。
  cross_provider: HashMap<ProviderId, Vec<ProviderId>>,
}

impl RouteMap {
  /// 设置显式模型映射。
  pub fn insert_explicit(&mut self, from: ModelRef, to: ModelRef) {
    self.explicit.insert(from, to);
  }

  /// 设置同提供方备选模型链。
  ///
  /// 键与值均为模型名（`ModelId`），提供方无关；链中模型是否可用
  /// 由各提供方的目录在查询时决定，因此应使用跨提供方通用的模型名。
  pub fn insert_same_provider(&mut self, model: ModelId, chain: Vec<ModelId>) {
    self.same_provider.insert(model, chain);
  }
  /// 设置跨提供方备选提供方链。
  pub fn insert_cross_provider(
    &mut self,
    provider: ProviderId,
    chain: Vec<ProviderId>,
  ) {
    self.cross_provider.insert(provider, chain);
  }

  /// 解析降级候选列表。
  ///
  /// 处理流程：
  ///
  /// 1. 错误不可降级 → 返回空列表（调用方不应降级）；
  /// 2. 显式映射命中且目标存在于目录 → 只返回该唯一候选；
  /// 3. 依序收集同提供方备选链中在目录存在的模型；
  /// 4. 错误可跨提供方降级时，再遍历跨提供方链：目标提供方有
  ///    同名模型则直命中；否则把同一份备选链投到目标提供方
  ///    目录中求值（"主链 ∩ 目标目录"）。
  ///
  /// 重复与自我映射的候选会被剔除（见 [`push_candidate`]）。
  ///
  /// # 参数
  ///
  /// - `primary`：主候选的模型描述；
  /// - `err`：触发降级的错误；
  /// - `catalog`：提供方目录，用于校验候选实际存在。
  pub fn candidates(
    &self,
    primary: &Model,
    err: &RiboError,
    catalog: &dyn ProviderCatalog,
  ) -> Vec<Candidate> {
    if !err.is_degradable() {
      return Vec::new();
    }

    let primary_ref: ModelRef = (primary.provider.clone(), primary.id.clone());
    let mut out: Vec<Candidate> = Vec::new();

    // ===== 一级：显式模型映射 =====
    // 命中即短路；目标缺失或映射到自身时告警并落到后续层级。
    if let Some((tp, tm)) = self.explicit.get(&primary_ref) {
      if tp == &primary.provider && tm == &primary.id {
        tracing::warn!(provider = %tp,
                  model = %tm,
                  "explicit route maps model to itself, ignoring");
      } else if let Some(m) = catalog.model(tp, tm) {
        out.push(Candidate {
          provider: tp.clone(),
          model: m,
        });
        return out;
      } else {
        tracing::warn!(
          target_provider = %tp,
          target_model = %tm,
          "explicit route target missing in catalog, falling through to chains"
        );
      }
    }

    // ===== 二级：同提供方备选链 =====
    // 把模型名级的备选链（same_provider，提供方无关）投到主提供方
    // 目录中求值。
    if let Some(chain) = self.same_provider.get(&primary.id) {
      for m in chain {
        if let Some(m) = catalog.model(&primary.provider, m) {
          push_candidate(primary, &mut out, primary.provider.clone(), m);
        }
      }
    }

    // ===== 三级：跨提供方链 =====
    // 仅当错误能通过换提供方解决（限流、鉴权、配额）时启用。
    // 两条命中路径：目标提供方有同名模型则直命中；否则把同一份
    // 模型名级备选链投到目标提供方目录求值（"主链 ∩ 目标目录"）。
    // same_provider 与提供方无关，不存在"主/目标提供方专属链"之分；
    // 若目标提供方使用完全不同的模型命名体系，链式匹配大概率落空
    // ——这是静态映射的职责边界，跨命名体系的精确备选应经
    // explicit 显式映射表达（优先级最高、命中即返回）。
    if err.is_cross_provider_degradable()
      && let Some(providers) = self.cross_provider.get(&primary.provider)
    {
      for p in providers {
        // 路径一：同模型名直命中
        if let Some(m) = catalog.model(p, &primary.id) {
          push_candidate(primary, &mut out, p.clone(), m);
          continue;
        }
        // 路径二：主链 ∩ 目标目录
        if let Some(chain) = self.same_provider.get(&primary.id) {
          for m in chain {
            if let Some(m) = catalog.model(p, m) {
              push_candidate(primary, &mut out, p.clone(), m);
            }
          }
        }
      }
    }
    out
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

/// 降级候选：一个具体的 `(提供方, 模型)` 组合。
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
  /// 候选提供方 ID。
  pub provider: ProviderId,
  /// 候选模型描述。
  pub model: Model,
}

#[cfg(test)]
mod tests {
  use std::sync::Arc;

  use super::*;
  use crate::{
    error::{INVALID_REQUEST, RATE_LIMITED, SERVER_ERROR, pre_stream},
    ids::ModelId,
    traits::Provider,
  };

  /// 仅实现目录查询的测试目录；`provider` 查找不被候选解析使用。
  struct TestCatalog {
    entries: HashMap<ProviderId, Vec<Model>>,
  }

  impl TestCatalog {
    fn new(entries: Vec<(&str, Vec<Model>)>) -> Self {
      Self {
        entries: entries
          .into_iter()
          .map(|(pid, models)| (provider(pid), models))
          .collect(),
      }
    }
  }

  impl ProviderCatalog for TestCatalog {
    fn provider(&self, _id: &ProviderId) -> Option<Arc<dyn Provider>> {
      None
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

  fn provider(raw: &str) -> ProviderId {
    ProviderId::parse(raw).expect("测试提供方 ID 合法")
  }

  fn model(provider_raw: &str, id: &str) -> Model {
    Model::new(
      provider(provider_raw),
      ModelId::parse(id).expect("测试模型 ID 合法"),
      id,
      "http://stub.local",
    )
  }

  fn candidate(provider_raw: &str, model_id: &str) -> Candidate {
    Candidate {
      provider: provider(provider_raw),
      model: model(provider_raw, model_id),
    }
  }

  /// 场景：错误不可降级（参数错误）。
  ///
  /// 预期：返回空候选列表，调用方不应触发降级。
  #[test]
  fn not_degradable_error_returns_empty() {
    let route = RouteMap::default();
    let catalog = TestCatalog::new(vec![("p1", vec![model("p1", "m1")])]);
    let err = pre_stream(INVALID_REQUEST, "bad request");

    let out = route.candidates(&model("p1", "m1"), &err, &catalog);
    assert!(out.is_empty());
  }

  /// 场景：显式映射命中且目标存在于目录。
  ///
  /// 预期：只返回显式候选，同提供方链与跨提供方链均被短路。
  #[test]
  fn explicit_hit_short_circuits_chains() {
    let mut route = RouteMap::default();
    route.insert_explicit(
      (provider("p1"), ModelId::parse("m1").unwrap()),
      (provider("p2"), ModelId::parse("m2").unwrap()),
    );
    route.insert_same_provider(
      ModelId::parse("m1").unwrap(),
      vec![ModelId::parse("m3").unwrap()],
    );
    route.insert_cross_provider(provider("p1"), vec![provider("p2")]);

    let catalog = TestCatalog::new(vec![
      ("p1", vec![model("p1", "m1"), model("p1", "m3")]),
      ("p2", vec![model("p2", "m2")]),
    ]);
    let err = pre_stream(RATE_LIMITED, "rate limited");

    assert_eq!(
      route.candidates(&model("p1", "m1"), &err, &catalog),
      vec![candidate("p2", "m2")]
    );
  }

  /// 场景：显式映射到自身（无效配置）。
  ///
  /// 预期：告警后落到同提供方链，而不是返回自我映射候选。
  #[test]
  fn explicit_self_mapping_falls_through() {
    let mut route = RouteMap::default();
    route.insert_explicit(
      (provider("p1"), ModelId::parse("m1").unwrap()),
      (provider("p1"), ModelId::parse("m1").unwrap()),
    );
    route.insert_same_provider(
      ModelId::parse("m1").unwrap(),
      vec![ModelId::parse("m2").unwrap()],
    );

    let catalog = TestCatalog::new(vec![(
      "p1",
      vec![model("p1", "m1"), model("p1", "m2")],
    )]);
    let err = pre_stream(SERVER_ERROR, "boom");

    assert_eq!(
      route.candidates(&model("p1", "m1"), &err, &catalog),
      vec![candidate("p1", "m2")]
    );
  }

  /// 场景：显式映射的目标不在目录中。
  ///
  /// 预期：告警后回退到后续链式匹配。
  #[test]
  fn explicit_missing_target_falls_through() {
    let mut route = RouteMap::default();
    route.insert_explicit(
      (provider("p1"), ModelId::parse("m1").unwrap()),
      (provider("p9"), ModelId::parse("missing").unwrap()),
    );
    route.insert_same_provider(
      ModelId::parse("m1").unwrap(),
      vec![ModelId::parse("m2").unwrap()],
    );

    let catalog = TestCatalog::new(vec![(
      "p1",
      vec![model("p1", "m1"), model("p1", "m2")],
    )]);
    let err = pre_stream(SERVER_ERROR, "boom");

    assert_eq!(
      route.candidates(&model("p1", "m1"), &err, &catalog),
      vec![candidate("p1", "m2")]
    );
  }

  /// 场景：同提供方链中存在目录缺失的模型。
  ///
  /// 预期：仅保留目录中实际存在的备选，且跳过主候选自身与重复项。
  #[test]
  fn same_provider_chain_is_filtered_and_deduped() {
    let mut route = RouteMap::default();
    route.insert_same_provider(
      ModelId::parse("m1").unwrap(),
      vec![
        ModelId::parse("m1").unwrap(),
        ModelId::parse("m2").unwrap(),
        ModelId::parse("m2").unwrap(),
        ModelId::parse("m3").unwrap(),
        ModelId::parse("missing").unwrap(),
      ],
    );

    let catalog = TestCatalog::new(vec![(
      "p1",
      vec![model("p1", "m1"), model("p1", "m2"), model("p1", "m3")],
    )]);
    let err = pre_stream(SERVER_ERROR, "boom");

    assert_eq!(
      route.candidates(&model("p1", "m1"), &err, &catalog),
      vec![candidate("p1", "m2"), candidate("p1", "m3")]
    );
  }

  /// 场景：跨提供方错误且目标提供方存在同名模型。
  ///
  /// 预期：直命中目标提供方的同模型候选。
  #[test]
  fn cross_provider_direct_same_model() {
    let mut route = RouteMap::default();
    route.insert_cross_provider(provider("p1"), vec![provider("p2")]);

    let catalog = TestCatalog::new(vec![
      ("p1", vec![model("p1", "m1")]),
      ("p2", vec![model("p2", "m1")]),
    ]);
    let err = pre_stream(RATE_LIMITED, "rate limited");

    assert_eq!(
      route.candidates(&model("p1", "m1"), &err, &catalog),
      vec![candidate("p2", "m1")]
    );
  }

  /// 场景：跨提供方目标没有同名模型，但存在主链中的备选模型。
  ///
  /// 预期：把模型名级备选链投到目标目录求值（“主链 ∩ 目标目录”）。
  #[test]
  fn cross_provider_chain_intersection() {
    let mut route = RouteMap::default();
    route.insert_same_provider(
      ModelId::parse("m1").unwrap(),
      vec![ModelId::parse("m2").unwrap()],
    );
    route.insert_cross_provider(provider("p1"), vec![provider("p2")]);

    let catalog = TestCatalog::new(vec![
      ("p1", vec![model("p1", "m1"), model("p1", "m2")]),
      ("p2", vec![model("p2", "m2")]),
    ]);
    let err = pre_stream(RATE_LIMITED, "rate limited");

    assert_eq!(
      route.candidates(&model("p1", "m1"), &err, &catalog),
      vec![candidate("p1", "m2"), candidate("p2", "m2")]
    );
  }

  /// 场景：服务端错误只允许同提供方降级，不允许跨提供方。
  ///
  /// 预期：即使配置了跨提供方链，也不产生目标提供方候选。
  #[test]
  fn cross_provider_disabled_for_server_error() {
    let mut route = RouteMap::default();
    route.insert_cross_provider(provider("p1"), vec![provider("p2")]);

    let catalog = TestCatalog::new(vec![
      ("p1", vec![model("p1", "m1")]),
      ("p2", vec![model("p2", "m1")]),
    ]);
    let err = pre_stream(SERVER_ERROR, "boom");

    assert!(
      route
        .candidates(&model("p1", "m1"), &err, &catalog)
        .is_empty()
    );
  }

  /// 场景：跨提供方目标既无同名模型，也未配置主链备选。
  ///
  /// 预期：不产生候选，覆盖“主链 ∩ 目标目录”缺失的分支。
  #[test]
  fn cross_provider_without_same_model_or_chain_returns_empty() {
    let mut route = RouteMap::default();
    route.insert_cross_provider(provider("p1"), vec![provider("p2")]);

    let catalog = TestCatalog::new(vec![
      ("p1", vec![model("p1", "m1")]),
      ("p2", vec![model("p2", "m2")]),
    ]);
    let err = pre_stream(RATE_LIMITED, "rate limited");

    assert!(
      route
        .candidates(&model("p1", "m1"), &err, &catalog)
        .is_empty()
    );
  }

  /// 场景：主链中的部分模型在目标提供方目录中不存在。
  ///
  /// 预期：仅保留目标目录存在的备选，缺失模型被跳过。
  #[test]
  fn cross_provider_chain_skips_models_missing_in_target() {
    let mut route = RouteMap::default();
    route.insert_same_provider(
      ModelId::parse("m1").unwrap(),
      vec![ModelId::parse("m2").unwrap(), ModelId::parse("m3").unwrap()],
    );
    route.insert_cross_provider(provider("p1"), vec![provider("p2")]);

    let catalog = TestCatalog::new(vec![
      (
        "p1",
        vec![model("p1", "m1"), model("p1", "m2"), model("p1", "m3")],
      ),
      ("p2", vec![model("p2", "m2")]),
    ]);
    let err = pre_stream(RATE_LIMITED, "rate limited");

    assert_eq!(
      route.candidates(&model("p1", "m1"), &err, &catalog),
      vec![
        candidate("p1", "m2"),
        candidate("p1", "m3"),
        candidate("p2", "m2")
      ]
    );
  }

  /// 场景：测试目录的辅助查询方法。
  ///
  /// 预期：`provider` 返回 None；`find_model` 命中与未命中分支正确；
  /// `providers` 列出全部已注册提供方。
  #[test]
  fn catalog_helpers_expose_registered_entries() {
    let catalog = TestCatalog::new(vec![("p1", vec![model("p1", "m1")])]);
    let pid = provider("p1");

    assert!(catalog.provider(&pid).is_none());
    assert_eq!(
      catalog.find_model(&ModelId::parse("m1").unwrap()),
      Some((pid.clone(), model("p1", "m1")))
    );
    assert!(
      catalog
        .find_model(&ModelId::parse("missing").unwrap())
        .is_none()
    );
    assert_eq!(catalog.providers(), vec![pid]);
  }
}
