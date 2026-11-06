//! 提供方注册目录：[`ProviderCatalog`] 的内存实现。

use std::{collections::HashMap, sync::Arc};

use crate::{
  ids::{ModelId, ProviderId},
  model::Model,
  traits::{Provider, ProviderCatalog},
};

/// 内存版提供方目录。
///
/// 以 `HashMap` 存储注册的提供方实例；写侧由网关注册路径保护，
/// 读侧可并发查询，并实现 [`ProviderCatalog`] 供路由与降级解析复用。
///
/// 同时维护注册顺序（`order`）：多个提供方暴露同名模型时，
/// [`find_model`](ProviderCatalog::find_model) 按注册顺序取首个命中，
/// 使主候选选择稳定可预期（先注册者优先）。
#[derive(Default)]
pub struct CatalogIndex {
  /// 已注册的提供方表（键为提供方 ID）。
  pub providers: HashMap<ProviderId, Arc<dyn Provider>>,
  /// 注册顺序（用于同名模型的主候选选择）。
  pub order: Vec<ProviderId>,
}

impl CatalogIndex {
  /// 判断指定提供方是否已注册。
  pub fn contains(&self, id: &ProviderId) -> bool {
    self.providers.contains_key(id)
  }
  /// 插入（或覆盖）一个提供方实例，键取自其自身 ID。
  ///
  /// 覆盖已有 ID 时保持原注册顺序；新 ID 追加到顺序末尾。
  pub fn insert(&mut self, provider: Arc<dyn Provider>) {
    let id = provider.id().clone();
    if self.providers.insert(id.clone(), provider).is_none() {
      self.order.push(id);
    }
  }
}

impl ProviderCatalog for CatalogIndex {
  fn provider(&self, id: &ProviderId) -> Option<Arc<dyn Provider>> {
    self.providers.get(id).cloned()
  }
  fn model(&self, id: &ProviderId, model: &ModelId) -> Option<Model> {
    self
      .providers
      .get(id)?
      .models()
      .iter()
      .find(|m| &m.id == model)
      .cloned()
  }
  fn find_model(&self, model: &ModelId) -> Option<(ProviderId, Model)> {
    // 按注册顺序逐个提供方扫描，命中首个同名模型即返回。
    self.order.iter().find_map(|pid| {
      self
        .providers
        .get(pid)?
        .models()
        .iter()
        .find(|m| &m.id == model)
        .map(|m| (pid.clone(), m.clone()))
    })
  }

  fn providers(&self) -> Vec<ProviderId> {
    self.order.clone()
  }
}

#[cfg(test)]
mod tests {
  use std::sync::Arc;

  use ra_core::error::RiboResult;

  use super::*;
  use crate::{
    context::Context,
    error::{INVALID_REQUEST, pre_stream},
    options::{ProviderOptions, StreamOptions},
    stream::ProviderEventStream,
  };

  /// 仅用于目录查询的测试提供方；`stream` 不应被调用。
  struct CatalogStub {
    id: ProviderId,
    models: Vec<Model>,
  }

  #[async_trait::async_trait]
  impl Provider for CatalogStub {
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
      Err(pre_stream(INVALID_REQUEST, "catalog stub 不应发起建流"))
    }
  }

  fn provider_id(raw: &str) -> ProviderId {
    ProviderId::parse(raw).expect("测试提供方 ID 合法")
  }

  fn model(provider: &str, id: &str) -> Model {
    Model::new(
      provider_id(provider),
      ModelId::parse(id).expect("测试模型 ID 合法"),
      id,
      "http://stub.local",
    )
    .with_context_window(8192)
    .with_max_tokens(1024)
  }

  /// 场景：插入提供方后可按 ID、模型 ID 与应用内全部模型查询。
  ///
  /// 预期：`contains` / `provider` / `model` / `find_model` 均能命中。
  #[test]
  fn indexes_provider_and_models() {
    let pid = provider_id("alpha");
    let mut catalog = CatalogIndex::default();
    catalog.insert(Arc::new(CatalogStub {
      id: pid.clone(),
      models: vec![model("alpha", "alpha-model")],
    }));

    assert!(catalog.contains(&pid));
    assert!(catalog.provider(&pid).is_some());
    assert_eq!(
      catalog.model(&pid, &ModelId::parse("alpha-model").unwrap()),
      Some(model("alpha", "alpha-model"))
    );
    let (found_pid, found_model) = catalog
      .find_model(&ModelId::parse("alpha-model").unwrap())
      .expect("应能跨提供方找到模型");
    assert_eq!(found_pid, pid);
    assert_eq!(found_model.id, ModelId::parse("alpha-model").unwrap());
    assert_eq!(catalog.providers(), vec![pid]);
  }

  /// 场景：查询不存在的提供方或模型。
  ///
  /// 预期：`provider` / `model` / `find_model` 全部返回 `None`。
  #[test]
  fn returns_none_for_unknown_entities() {
    let catalog = CatalogIndex::default();
    let pid = provider_id("missing");
    let mid = ModelId::parse("missing-model").unwrap();

    assert!(!catalog.contains(&pid));
    assert!(catalog.provider(&pid).is_none());
    assert!(catalog.model(&pid, &mid).is_none());
    assert!(catalog.find_model(&mid).is_none());
  }

  /// 场景：目标模型只存在于第二个提供方。
  ///
  /// 预期：`find_model` 遍历首个提供方未命中后继续扫描并命中第二个。
  #[test]
  fn find_model_scans_all_providers() {
    let mut catalog = CatalogIndex::default();
    catalog.insert(Arc::new(CatalogStub {
      id: provider_id("alpha"),
      models: vec![model("alpha", "alpha-model")],
    }));
    catalog.insert(Arc::new(CatalogStub {
      id: provider_id("beta"),
      models: vec![model("beta", "beta-model")],
    }));

    let (pid, found) = catalog
      .find_model(&ModelId::parse("beta-model").unwrap())
      .expect("应扫描到第二个提供方");

    assert_eq!(pid, provider_id("beta"));
    assert_eq!(found.id, ModelId::parse("beta-model").unwrap());
  }

  /// 场景：两个提供方暴露同名模型。
  ///
  /// 预期：`find_model` 按注册顺序返回首个注册者，主候选选择稳定可预期。
  #[test]
  fn find_model_prefers_earlier_registration() {
    let mut catalog = CatalogIndex::default();
    catalog.insert(Arc::new(CatalogStub {
      id: provider_id("alpha"),
      models: vec![model("alpha", "shared-model")],
    }));
    catalog.insert(Arc::new(CatalogStub {
      id: provider_id("beta"),
      models: vec![model("beta", "shared-model")],
    }));

    let mid = ModelId::parse("shared-model").unwrap();
    let (pid, found) =
      catalog.find_model(&mid).expect("同名模型应命中首个注册者");

    assert_eq!(pid, provider_id("alpha"));
    assert_eq!(found.provider, provider_id("alpha"));
  }

  /// 场景：测试桩的建流方法。
  ///
  /// 预期：返回 INVALID_REQUEST，便于覆盖测试桩自身的函数。
  #[tokio::test]
  async fn catalog_stub_stream_returns_error() {
    let stub = CatalogStub {
      id: provider_id("alpha"),
      models: vec![model("alpha", "alpha-model")],
    };
    let model = model("alpha", "alpha-model");
    let context = Context::default();
    let options = StreamOptions::default();

    let err = stub
      .stream(ProviderOptions {
        model: &model,
        context: &context,
        options: &options,
      })
      .await
      .err()
      .expect("测试桩不应发起建流");

    assert_eq!(err.code(), INVALID_REQUEST);
  }
}
