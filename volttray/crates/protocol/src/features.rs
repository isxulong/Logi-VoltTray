//! 内嵌 feature 注册表（fixtures/features.json）。
//!
//! 两个用途：
//! 1. P1-1 白名单复核：feature 的 kind 必须是 query 才允许编码；
//! 2. P1-6 帧级脱敏：`sensitive_params` 标记响应参数中的敏感字节偏移
//!    （如序列号），导出 / 录制时按偏移遮蔽。

use std::sync::OnceLock;

const FEATURES_JSON: &str = include_str!("../../../fixtures/features.json");

/// feature 类别。除 Query 外一律不允许出现在命令编码路径。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FeatureKind {
    Query,
    Pair,
    Set,
    Firmware,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FeatureInfo {
    pub id: u16,
    pub name: String,
    pub kind: FeatureKind,
    /// 响应参数中的敏感字节偏移（P1-6）。
    #[serde(default)]
    pub sensitive_params: Vec<u8>,
}

#[derive(Debug, serde::Deserialize)]
struct RegistryFile {
    features: Vec<FeatureInfo>,
}

/// 已解析的注册表（进程内单例）。
#[derive(Debug)]
pub struct FeatureRegistry {
    infos: Vec<FeatureInfo>,
}

static REGISTRY: OnceLock<FeatureRegistry> = OnceLock::new();

pub fn registry() -> &'static FeatureRegistry {
    REGISTRY.get_or_init(|| {
        let file: RegistryFile =
            serde_json::from_str(FEATURES_JSON).expect("fixtures/features.json must be valid JSON");
        FeatureRegistry {
            infos: file.features,
        }
    })
}

impl FeatureRegistry {
    pub fn get(&self, id: u16) -> Option<&FeatureInfo> {
        self.infos.iter().find(|f| f.id == id)
    }

    pub fn kind_of(&self, id: u16) -> Option<FeatureKind> {
        self.get(id).map(|f| f.kind)
    }

    /// 未登记的 feature 一律视为非 query（白名单失败关闭）。
    pub fn is_query(&self, id: u16) -> bool {
        self.kind_of(id) == Some(FeatureKind::Query)
    }

    /// 敏感参数偏移；未登记或无敏感字段返回空切片。
    pub fn sensitive_offsets(&self, id: u16) -> &[u8] {
        match self.get(id) {
            Some(info) => &info.sensitive_params,
            None => &[],
        }
    }

    pub fn name_of(&self, id: u16) -> Option<&str> {
        self.get(id).map(|f| f.name.as_str())
    }

    pub fn len(&self) -> usize {
        self.infos.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_loads_and_classifies() {
        let r = registry();
        assert!(r.len() >= 8, "registry should cover PRD appendix features");
        assert!(r.is_query(0x0000));
        assert!(r.is_query(0x1004));
        assert!(!r.is_query(0x1B04), "pair-kind feature must be rejected");
    }

    #[test]
    fn sensitive_offsets_registered() {
        let r = registry();
        assert!(
            !r.sensitive_offsets(0x0007).is_empty(),
            "serial feature must declare sensitive offsets"
        );
        assert!(r.sensitive_offsets(0x1004).is_empty());
    }
}
