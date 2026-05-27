//! Safekeeper 侧的 shard map cache（feat-009 §3.2(a)）。
//!
//! Safekeeper 的物理模型**不分 shard**——一条 timeline 的 WAL 是单一 stream（feat-009 §2 / OQ4）。
//! 但 agent 视角需要按 shard 拆 WAL lag，才能定位是某个热点 shard 堆积还是全 shard 普涨。
//! 因此本模块在**出口侧**（metric label / tracing attribute）补 `shard_id`：定期从
//! storage_controller pull `tenant_id → shard 列表` 的映射，cache 在进程内，**不改 safekeeper
//! 物理 model**（防上游 push back，规则 P6）。
//!
//! - 30s interval refresh（fire-and-forget；不阻塞 WAL flush）。
//! - cache miss → 降级填 `unsharded`（单 shard，`shard_id = "0000"`），fail-safe，不 panic。
//! - 复用 feat-008 cornerstone 的 `utils::usr` 做 shard_id string 化（`<index><count>` hex），不另起格式。

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use arc_swap::ArcSwap;
use serde::Deserialize;
use url::Url;
use tracing::{info, warn};
use utils::id::TenantId;
use utils::shard::ShardIndex;
use utils::usr;

/// 默认刷新间隔（feat-009 §3.2(a) / §5：30s 够用，shard split 是低频运维操作）。
pub const DEFAULT_REFRESH_INTERVAL: Duration = Duration::from_secs(30);

/// 紧急 unblock env flag（feat-009 §8）：置 `true` 时完全跳过 USR shard_id 注入，
/// safekeeper 退化到上游 baseline（shard_id 出口降级 "0000"，等价于不分 shard）。
pub const USR_DISABLED_ENV: &str = "OPENNEON_SAFEKEEPER_USR_DISABLED";

/// 读取 [`USR_DISABLED_ENV`]：显式置 `true` / `1` 时禁用 safekeeper USR shard_id 注入。
pub fn usr_disabled() -> bool {
    matches!(
        std::env::var(USR_DISABLED_ENV),
        Ok(v) if v.eq_ignore_ascii_case("true") || v == "1"
    )
}

/// storage_controller `GET /v1/tenant/{tenant_id}/shards` response 的单条 shard 描述。
///
/// 只取本 feature 需要的 `shard_id`（其它字段忽略，向后兼容上游协议扩展）。
#[derive(Debug, Deserialize)]
struct ShardDescription {
    /// `<index><count>` 4 char hex，跟 cornerstone schema 一致。
    shard_id: String,
}

#[derive(Debug, Deserialize)]
struct TenantShardsResponse {
    shards: Vec<ShardDescription>,
}

/// 进程内的 tenant → shard 列表缓存。
///
/// 用 [`ArcSwap`] 做 single-writer atomic swap，read 端 0 lock（feat-009 §5 / OQ2）。
pub struct ShardMapCache {
    /// storage_controller base URL（来自 `SafeKeeperConf::hcc_base_url`）。None 时整个 cache 禁用。
    base_url: Option<Url>,
    http: reqwest::Client,
    /// tenant_id → 该 tenant 当前 active 的 shard 列表（升序）。
    map: ArcSwap<HashMap<TenantId, Vec<ShardIndex>>>,
}

impl ShardMapCache {
    pub fn new(base_url: Option<Url>) -> Arc<Self> {
        Arc::new(Self {
            base_url,
            http: reqwest::Client::new(),
            map: ArcSwap::from_pointee(HashMap::new()),
        })
    }

    /// 取某 tenant 的 shard 列表；cache miss 返回 `None`（调用方降级到 `"0000"`）。
    pub fn get(&self, tenant_id: &TenantId) -> Option<Vec<ShardIndex>> {
        self.map.load().get(tenant_id).cloned()
    }

    /// 取某 tenant 的 `shard_id` 出口取值：
    ///
    /// - 单 shard / cache miss / unsharded → [`usr::SHARD_ID_UNSHARDED`]（`"0000"`）。
    /// - 多 shard（如 WAL stream 跨多 shard）→ 逗号分隔升序列表（feat-009 §4.1 / OQ3）。
    ///
    /// 取值格式严格复用 cornerstone（`utils::usr::shard_ids_str`），防命名 / 格式漂移。
    pub fn shard_id_label(&self, tenant_id: &TenantId) -> String {
        if usr_disabled() {
            // 紧急 unblock：退化到上游 baseline 等价形态。
            return usr::SHARD_ID_UNSHARDED.to_string();
        }
        match self.get(tenant_id) {
            Some(shards) if !shards.is_empty() => usr::shard_ids_str(shards),
            _ => usr::SHARD_ID_UNSHARDED.to_string(),
        }
    }

    /// 从 storage_controller 拉一次全量 shard map，成功则 atomic swap 替换缓存。
    ///
    /// 失败时保留旧 cache（不清空、不 panic）——降级路径，fail-safe（feat-009 §3.2(a)）。
    /// `tenant_ids` 是当前进程托管的 tenant 集合（由调用方从 GlobalTimelines 提供）。
    pub async fn refresh(&self, tenant_ids: &[TenantId]) {
        let Some(base_url) = &self.base_url else {
            return; // 未配置 storage_controller，cache 始终为空，全部降级 "0000"
        };
        let mut next: HashMap<TenantId, Vec<ShardIndex>> = HashMap::new();
        for tenant_id in tenant_ids {
            match self.fetch_one(base_url, tenant_id).await {
                Ok(shards) => {
                    next.insert(*tenant_id, shards);
                }
                Err(e) => {
                    // 单个 tenant 拉取失败不影响其它 tenant；该 tenant 走降级 "0000"。
                    warn!(%tenant_id, error = %e, "shard map pull failed; falling back to unsharded");
                }
            }
        }
        self.map.store(Arc::new(next));
    }

    async fn fetch_one(
        &self,
        base_url: &Url,
        tenant_id: &TenantId,
    ) -> anyhow::Result<Vec<ShardIndex>> {
        let url = base_url.join(&format!("v1/tenant/{tenant_id}/shards"))?;
        let resp: TenantShardsResponse = self
            .http
            .get(url)
            .timeout(Duration::from_millis(200)) // feat-009 §5：first pull p99 < 200ms
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        let mut shards = Vec::with_capacity(resp.shards.len());
        for s in resp.shards {
            // 复用 cornerstone 的 ShardIndex 解析（4 char hex `<index><count>`）。
            match s.shard_id.parse::<ShardIndex>() {
                Ok(idx) => shards.push(idx),
                Err(e) => warn!(%tenant_id, shard_id = %s.shard_id, error = %e, "unparseable shard_id from storage_controller"),
            }
        }
        shards.sort();
        shards.dedup();
        Ok(shards)
    }

    /// 把本 cache 注册为进程全局实例，供没法通过参数拿到 cache 的代码点
    /// （如深层 WAL acceptor tracing span）用 [`shard_id_label_global`] 查询。
    ///
    /// 幂等：仅第一次调用生效（[`OnceLock`]）。
    pub fn install_global(self: &Arc<Self>) {
        let _ = GLOBAL_SHARD_MAP.set(self.clone());
    }

    /// 后台刷新任务：以 `interval` 周期调 [`Self::refresh`]。
    ///
    /// `tenant_provider` 每次刷新前被调用，返回当前进程托管的 tenant 集合。
    pub async fn run_refresh_loop<F>(self: Arc<Self>, interval: Duration, tenant_provider: F)
    where
        F: Fn() -> Vec<TenantId> + Send + Sync + 'static,
    {
        if self.base_url.is_none() {
            info!("shard map cache disabled (no storage_controller base url); shard_id will be \"0000\"");
            return;
        }
        let mut ticker = tokio::time::interval(interval);
        loop {
            ticker.tick().await;
            let tenant_ids = tenant_provider();
            self.refresh(&tenant_ids).await;
        }
    }
}

/// 进程全局 shard map（由 [`ShardMapCache::install_global`] 在启动时设置）。
static GLOBAL_SHARD_MAP: OnceLock<Arc<ShardMapCache>> = OnceLock::new();

/// 全局查询某 tenant 的 `shard_id` 出口取值（供深层 tracing span 等用）。
///
/// 未安装全局 cache / USR 禁用 / cache miss 时统一降级 [`usr::SHARD_ID_UNSHARDED`]（`"0000"`）。
pub fn shard_id_label_global(tenant_id: &TenantId) -> String {
    match GLOBAL_SHARD_MAP.get() {
        Some(cache) => cache.shard_id_label(tenant_id),
        None => usr::SHARD_ID_UNSHARDED.to_string(),
    }
}
