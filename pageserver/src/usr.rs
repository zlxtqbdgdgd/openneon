//! Pageserver 侧 USR（Unified Service Resource）注入 glue（feat-008 §3.3）。
//!
//! Pageserver 是 USR 全栈贴标的 **cornerstone 数据源**：它是 Neon multi-tenancy + sharding 的
//! 物理 boundary，`(tenant_id, timeline_id, shard_id)` 三件套在这里齐全。本模块把 pageserver
//! 已有的 [`TenantShardId`] / [`TimelineId`] 投影成 cornerstone 定义的 `openneon.usr.*` attribute，
//! 供 metric / tracing / log 出口统一调用，避免各处自己拼字符串（overview §10.2.3）。
//!
//! - tracing：在 GetPage / WAL ingest / compaction 等关键 span 内调
//!   [`record_usr_on_span`]，把三件套写到当前 span 的 OTel attribute。
//! - metric：per-timeline metric 用 [`USR_METRIC_LABELS`] 统一 label 顺序，或经
//!   `register_with_usr!`（见 `crate::metrics`）。
//!
//! 复用 `utils::usr::UsrAttributes`（单一事实源）+ `tracing_utils::usr`（共用注入原语），
//! 不重复实现（feat-008 §9 已有依赖必须复用）。

use tracing_utils::usr::UsrContext;
use utils::id::TimelineId;
use utils::shard::TenantShardId;
use utils::usr::{self, UsrAttributes};

/// Pageserver per-timeline metric 的 canonical USR label 顺序（跟 cornerstone schema 一致）。
///
/// 新增 per-timeline metric vector 时统一用本常量做 label，保证三件套齐全且命名不漂移。
pub const USR_METRIC_LABELS: &[&str] = &[usr::label::TENANT_ID, usr::label::SHARD_ID, usr::label::TIMELINE_ID];

/// 由 `(TenantShardId, TimelineId)` 构造 string 化的 [`UsrContext`]（tracing 注入用）。
pub fn usr_context(tenant_shard_id: &TenantShardId, timeline_id: &TimelineId) -> UsrContext {
    UsrContext {
        tenant_id: Some(tenant_shard_id.tenant_id.to_string()),
        timeline_id: Some(timeline_id.to_string()),
        shard_id: Some(usr::shard_id_str(&tenant_shard_id.to_index())),
        endpoint_id: None, // pageserver 不知道 endpoint（compute/proxy 才有，feat-010）
        project_id: None,  // pageserver 不知道 project（storage_controller mapping，feat-010）
    }
}

/// 把 `(TenantShardId, TimelineId)` 的 USR 三件套注入**当前** tracing span 的 OTel attribute。
///
/// 在 GetPage / WAL ingest / compaction 等已有显式 span 的入口调用即可；
/// 若 [`usr::usr_tagging_enabled`] 返回 false（紧急回滚），则 no-op。
pub fn record_usr_on_span(tenant_shard_id: &TenantShardId, timeline_id: &TimelineId) {
    if !usr::usr_tagging_enabled() {
        return;
    }
    // 经 cornerstone 的 UsrAttributes 拿 canonical KeyValue，再用共用原语写入当前 span。
    let attrs = (*tenant_shard_id, *timeline_id).usr_attributes();
    tracing_utils::usr::record_usr_attributes(attrs);
}
