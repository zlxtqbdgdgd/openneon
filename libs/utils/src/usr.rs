//! USR (Unified Service Resource) 字段命名规范的单一事实源（feat-008 cornerstone）。
//!
//! 本模块定义 openneon 跨 4 组件（pageserver / safekeeper / compute / proxy）在
//! metric / log / tracing 出口侧统一注入的 USR 三件套 +扩展字段的 **attribute namespace**、
//! 取值 string 化形式，以及把 Neon 内部 ID 类型转成 OpenTelemetry attribute 的 trait。
//!
//! 所有需要打 USR 标签的位置都必须经由 [`UsrAttributes`] / 这里的常量取字段名，
//! 不允许各处自己拼字符串 —— 防止命名漂移（详 overview §10.2.3 / feat-010 §3.2(d) CI grep guard）。
//!
//! # 两层关系厘清（R11 §3.1）
//!
//! - **数据层 Key**（`pageserver_api::key::Key`，18 byte opaque binary）：pageserver 内部
//!   存储排序用，对 agent 不可见，**不**用作 USR。
//! - **agent 层 USR**（本模块）：metric / log / trace 跨组件 JOIN 用的高层 ID 的 string
//!   形式 —— `(tenant_id, timeline_id, shard_id)` 三件套 + 可选 `endpoint_id` / `project_id`。
//!
//! USR 的 string 化复用 Neon 已有类型的 `Display`：`TenantId` / `TimelineId` 是 32 char
//! lowercase hex，`ShardIndex` 是 `<index><count>` 4 char hex（如 `0204` = 4 shard 中的第 3 个，
//! `0000` = unsharded），跟 layer file name convention 解耦但取值一致。

use crate::id::{TenantId, TenantTimelineId, TimelineId};
use crate::shard::{ShardIndex, TenantShardId};

/// 紧急回滚 env flag：置 `true` 时 4 组件出口侧完全不注入 USR 标签，
/// 退化到上游 baseline 形态（feat-008 §8 / feat-010 §8 回滚策略）。
pub const USR_TAGGING_DISABLED_ENV: &str = "USR_TAGGING_ENABLED";

/// feat-010 §8：置 `true` 时 4 组件退化到上游 legacy 字段命名（`endpoint` 等），
/// 紧急 unblock 已部署用户 dashboard。
pub const USR_NAMING_LEGACY_ENV: &str = "OPENNEON_USR_NAMING_LEGACY";

/// USR OTel attribute 的统一 namespace 前缀。**禁止**写 `tenant-id` / `tenantId` 等漂移形式。
pub const USR_NAMESPACE: &str = "openneon.usr.";

/// USR attribute / log record field 的 canonical key 名（OTel dot 分隔）。
///
/// 这些常量是 feat-008 cornerstone 锁定的 schema，feat-009 / feat-010 / feat-031 全部复用，
/// 不另起命名。一旦发布**不可改**（dashboard / log query 已依赖）。
pub mod attr {
    /// `openneon.usr.tenant_id` —— 32 char lowercase hex。
    pub const TENANT_ID: &str = "openneon.usr.tenant_id";
    /// `openneon.usr.timeline_id` —— 32 char lowercase hex。
    pub const TIMELINE_ID: &str = "openneon.usr.timeline_id";
    /// `openneon.usr.shard_id` —— `<index><count>` 4 char hex，多 shard 逗号分隔。
    pub const SHARD_ID: &str = "openneon.usr.shard_id";
    /// `openneon.usr.endpoint_id` —— compute / proxy 强制，pageserver / safekeeper 经 mapping pull。
    pub const ENDPOINT_ID: &str = "openneon.usr.endpoint_id";
    /// `openneon.usr.project_id` —— compute / proxy 强制，optional field。
    pub const PROJECT_ID: &str = "openneon.usr.project_id";
}

/// Prometheus metric label 的 canonical 名（用 underscore，跟 OTel attribute 去掉 namespace 前缀对应）。
pub mod label {
    pub const TENANT_ID: &str = "tenant_id";
    pub const TIMELINE_ID: &str = "timeline_id";
    pub const SHARD_ID: &str = "shard_id";
    pub const ENDPOINT_ID: &str = "endpoint_id";
    pub const PROJECT_ID: &str = "project_id";
}

/// USR 字段缺失时的占位取值（fail-safe：不阻塞出口、不 panic）。
pub const USR_UNKNOWN: &str = "unknown";

/// unsharded tenant 的 shard_id 占位（`<index=0><count=0>` hex）。
pub const SHARD_ID_UNSHARDED: &str = "0000";

/// 把 Neon 内部 ID 类型投影成 OpenTelemetry USR attribute 三件套。
///
/// 所有 metric / log / tracing 出口侧统一调 [`UsrAttributes::usr_attributes`]，
/// 避免每处自己拼 attribute string，防命名漂移（overview §10.2.3）。
pub trait UsrAttributes {
    /// 返回该对象对应的 USR `KeyValue` 列表，attribute key 用 [`attr`] 里的 canonical 常量。
    fn usr_attributes(&self) -> Vec<opentelemetry::KeyValue>;
}

impl UsrAttributes for TenantId {
    fn usr_attributes(&self) -> Vec<opentelemetry::KeyValue> {
        vec![opentelemetry::KeyValue::new(
            attr::TENANT_ID,
            self.to_string(),
        )]
    }
}

impl UsrAttributes for TenantShardId {
    fn usr_attributes(&self) -> Vec<opentelemetry::KeyValue> {
        vec![
            opentelemetry::KeyValue::new(attr::TENANT_ID, self.tenant_id.to_string()),
            opentelemetry::KeyValue::new(attr::SHARD_ID, shard_id_str(&self.to_index())),
        ]
    }
}

impl UsrAttributes for (TenantShardId, TimelineId) {
    fn usr_attributes(&self) -> Vec<opentelemetry::KeyValue> {
        let mut v = self.0.usr_attributes();
        v.push(opentelemetry::KeyValue::new(
            attr::TIMELINE_ID,
            self.1.to_string(),
        ));
        v
    }
}

impl UsrAttributes for (TenantTimelineId, ShardIndex) {
    fn usr_attributes(&self) -> Vec<opentelemetry::KeyValue> {
        vec![
            opentelemetry::KeyValue::new(attr::TENANT_ID, self.0.tenant_id.to_string()),
            opentelemetry::KeyValue::new(attr::TIMELINE_ID, self.0.timeline_id.to_string()),
            opentelemetry::KeyValue::new(attr::SHARD_ID, shard_id_str(&self.1)),
        ]
    }
}

/// 统一的 `shard_id` string 化形式：`<index><count>` 4 char hex（复用 [`ShardIndex`] 的 `Display`）。
///
/// 例：`0000`（unsharded）/ `0204`（4 shard 中第 3 个）。
/// 所有组件（含 feat-009 safekeeper / feat-010 compute/proxy）都用本函数，
/// 防止出现 `shard-0` / `0` / `00` / `shard_index` 等漂移格式。
pub fn shard_id_str(idx: &ShardIndex) -> String {
    idx.to_string()
}

/// 多 shard 场景（如 safekeeper WAL stream 跨多 shard）的 `shard_id` 多值表示：
/// 升序、逗号分隔的单值列表（OTel attribute 是 string type，不用 array；agent JOIN 用 LIKE 匹配）。
///
/// 详 feat-009 §3.2(d) / §4.1。空列表降级到 [`SHARD_ID_UNSHARDED`]。
pub fn shard_ids_str<I>(indices: I) -> String
where
    I: IntoIterator<Item = ShardIndex>,
{
    let mut v: Vec<ShardIndex> = indices.into_iter().collect();
    if v.is_empty() {
        return SHARD_ID_UNSHARDED.to_string();
    }
    v.sort();
    v.dedup();
    v.iter()
        .map(|i| i.to_string())
        .collect::<Vec<_>>()
        .join(",")
}

/// 读取 [`USR_TAGGING_DISABLED_ENV`] / [`USR_NAMING_LEGACY_ENV`]，判断是否启用 USR 注入。
///
/// fail-safe：env 未设时默认启用；显式设为 `false` / legacy 模式时禁用。
pub fn usr_tagging_enabled() -> bool {
    if let Ok(v) = std::env::var(USR_TAGGING_DISABLED_ENV) {
        if v.eq_ignore_ascii_case("false") || v == "0" {
            return false;
        }
    }
    if let Ok(v) = std::env::var(USR_NAMING_LEGACY_ENV) {
        if v.eq_ignore_ascii_case("true") || v == "1" {
            return false;
        }
    }
    true
}
