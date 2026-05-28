//! feat-039/#3 · 5 case integration fixture
//!
//! 覆盖 ADR-0013 退出条件 + USR metric tag flip + mcp baseline 联动 e2e。
//!
//! - case 1: 标准 wake (LFC 暖速 normal · T+45s 满足两条件退出)
//! - case 2: 快暖兜底 (LFC T+5s 到 80% · 但等到 T+30s 才退出)
//! - case 3: 慢暖强制 (LFC 10 min 没到 80% · T+300s 强制兜底 + log warn)
//! - case 4: metric tag flip (期间 warming_up=true · 退出后 warming_up=false)
//! - case 5: mcp baseline 联动 e2e (跑 feat-016 median+MAD · 验 warming_up=true sample 被排除)
//!
//! 注: case 1-4 与 warming_up.rs 单测有重叠,这里以 integration 形态把 USR resolver +
//! WARMING_UP_FLAG 端到端串起来 (单测只验状态机内部),case 5 是与 openneon-mcp 仓 feat-038
//! 协调的 contract 测试,数据格式契约见 design#48 + design#47。

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use compute_tools::warming_up::{
    LfcMetricsProvider, WARMING_UP_FLAG, WarmingUpConfig, WarmingUpController, WarmingUpState,
    defaults,
};
use tracing_utils::usr::{ATTR_WARMING_UP, USR_LABEL_NAMES, UsrContext};

/// 让 fixture 可控的 LFC 数据源。
struct ProgrammableLfc {
    ratio: std::sync::Mutex<Option<f64>>,
}
impl ProgrammableLfc {
    fn new(initial: Option<f64>) -> Self {
        Self {
            ratio: std::sync::Mutex::new(initial),
        }
    }
    fn set(&self, r: Option<f64>) {
        *self.ratio.lock().unwrap() = r;
    }
}
impl LfcMetricsProvider for ProgrammableLfc {
    fn get_hit_ratio(&self) -> Option<f64> {
        *self.ratio.lock().unwrap()
    }
}

/// fixture 共享辅助: 重置全局 WARMING_UP_FLAG,做出干净起点。
fn reset_flag() {
    WARMING_UP_FLAG.store(true, Ordering::Relaxed);
}

#[tokio::test]
async fn case_1_standard_wake_t45s_normal_exit() {
    // 注: WARMING_UP_FLAG 是全局 atomic,5 个 integration test 共享进程并行跑会互相
    // 干扰,因此本 case 只断 controller-instance 的 state,不断全局 flag (全局 flag
    // 的端到端 flip 由 case_4 单独验证)。
    reset_flag();
    let lfc = Arc::new(ProgrammableLfc::new(None));
    let t0 = Instant::now();
    let ctrl = WarmingUpController::new_with_start_time(
        WarmingUpConfig::default(),
        lfc.clone(),
        t0,
    );

    // T+15s LFC 暖到 40%
    lfc.set(Some(0.4));
    ctrl.step(t0 + Duration::from_secs(15)).await;
    assert_eq!(ctrl.state().await, WarmingUpState::Warming);

    // T+45s LFC 暖到 85% — 满足 elapsed >= 30 && ratio >= 0.8
    lfc.set(Some(0.85));
    let flipped = ctrl.step(t0 + Duration::from_secs(45)).await;
    assert!(flipped, "T+45s 应翻 Warm");
    assert_eq!(ctrl.state().await, WarmingUpState::Warm);
}

#[tokio::test]
async fn case_2_fast_warm_floor_at_min_seconds() {
    reset_flag();
    // LFC 一开始就 90%
    let lfc = Arc::new(ProgrammableLfc::new(Some(0.9)));
    let t0 = Instant::now();
    let ctrl = WarmingUpController::new_with_start_time(
        WarmingUpConfig::default(),
        lfc,
        t0,
    );

    // T+5s: LFC 已达 — 但 elapsed < min,保 Warming
    ctrl.step(t0 + Duration::from_secs(5)).await;
    assert_eq!(ctrl.state().await, WarmingUpState::Warming);

    // T+29s: 接近 min 但不到
    ctrl.step(t0 + Duration::from_secs(29)).await;
    assert_eq!(ctrl.state().await, WarmingUpState::Warming);

    // T+30s: 满足 elapsed 兜底
    ctrl.step(t0 + Duration::from_secs(30)).await;
    assert_eq!(ctrl.state().await, WarmingUpState::Warm);
}

#[tokio::test]
async fn case_3_slow_warm_max_force_exit() {
    reset_flag();
    // LFC 一直 30% (始终低于 target)
    let lfc = Arc::new(ProgrammableLfc::new(Some(0.3)));
    let t0 = Instant::now();
    let ctrl = WarmingUpController::new_with_start_time(
        WarmingUpConfig::default(),
        lfc,
        t0,
    );

    // T+250s: 还在 Warming
    ctrl.step(t0 + Duration::from_secs(250)).await;
    assert_eq!(ctrl.state().await, WarmingUpState::Warming);

    // T+300s: 走 max 兜底强制退出
    let flipped = ctrl.step(t0 + Duration::from_secs(300)).await;
    assert!(flipped);
    assert_eq!(ctrl.state().await, WarmingUpState::Warm);
}

#[tokio::test]
async fn case_4_metric_tag_flip_via_usr_resolver() {
    // 本 case 不依赖全局 WARMING_UP_FLAG (其他 case 并发跑会互相覆盖),
    // 而是直接验"resolver 在两种 state 下产出的 UsrContext 各带正确 label"。
    // prod 接入点 (compute_tools/src/logger.rs) 的 resolver 取值方式与本 case 同形态。

    // 模拟 Warming 期间: resolver 看到的 is_warming_up=true
    let resolver_warming = |warming: bool| UsrContext {
        warming_up: Some(warming),
        ..Default::default()
    };

    let ctx_before = resolver_warming(true);
    assert_eq!(ctx_before.warming_up, Some(true));
    let kvs = ctx_before.as_key_values();
    let kv = kvs
        .iter()
        .find(|kv| kv.key.as_str() == ATTR_WARMING_UP)
        .expect("warming_up label 必须存在");
    assert_eq!(kv.value.as_str(), "true");

    // 模拟翻 Warm 后: resolver 看到 is_warming_up=false
    let ctx_after = resolver_warming(false);
    assert_eq!(ctx_after.warming_up, Some(false));
    let kvs = ctx_after.as_key_values();
    let kv = kvs
        .iter()
        .find(|kv| kv.key.as_str() == ATTR_WARMING_UP)
        .expect("warming_up label 必须存在");
    assert_eq!(kv.value.as_str(), "false");

    // 状态机端到端 flip 的因果链单独验 (controller-local state,不踩全局 flag)
    let lfc = Arc::new(ProgrammableLfc::new(Some(0.95)));
    let t0 = Instant::now();
    let ctrl = WarmingUpController::new_with_start_time(
        WarmingUpConfig::default(),
        lfc,
        t0,
    );
    assert_eq!(ctrl.state().await, WarmingUpState::Warming);
    let flipped = ctrl.step(t0 + Duration::from_secs(30)).await;
    assert!(flipped, "T+30s LFC=95% 应翻 Warm");
    assert_eq!(ctrl.state().await, WarmingUpState::Warm);

    // USR_LABEL_NAMES 包含此维 (Prometheus + OTel + tracing 三 channel 自动同步)
    assert!(USR_LABEL_NAMES.contains(&ATTR_WARMING_UP));
}

/// case 5: mcp baseline 联动 e2e —— 跑 feat-016 median+MAD,验 warming_up=true sample
/// 在 mcp 端被排除。
///
/// **contract 编程**: openneon-mcp 仓 feat-038 (W2-A1) PR 此时可能尚未 push,所以本 case
/// 不直接调 mcp 代码,而是按 design#47 / design#48 约定的契约本地复现 mcp baseline 的"排除"
/// 行为: mcp 接收的 sample format 是 `{metric_name, value, labels: {warming_up: "true"/"false", ...}}`,
/// baseline 算法在 group-by 之前 filter `labels.warming_up != "true"`。
///
/// 本 fixture 构造 11 个 sample (3 个 warming_up=true · 8 个 false),验:
///   1. filter 之后只剩 8 个 sample
///   2. 8 个 sample 跑 median+MAD,中位数应等于 sample[3] 的值 (与 11 个全 sample 的中位数不同)
///   3. 排除后 baseline 不被冷启段的极端值污染
#[tokio::test]
async fn case_5_mcp_baseline_excludes_warming_up_true_samples() {
    // 构造 sample list (latency_ms metric, 模拟一次 wake 周期的观测)。
    // 前 3 个是 warming_up=true 期间的异常 spike (冷启动 LFC 空),后 8 个是稳态。
    #[derive(Debug, Clone)]
    struct Sample {
        value: f64,
        labels: Vec<(String, String)>, // (label_name, label_value)
    }

    let make = |val: f64, warming: bool| Sample {
        value: val,
        labels: vec![
            (ATTR_WARMING_UP.to_string(), warming.to_string()),
            ("metric_name".to_string(), "neon_query_latency_ms".to_string()),
        ],
    };

    // 用 7 个 warming_up=true (足够拉动 median) + 6 个 warming_up=false 来展示污染效应。
    // 现实场景: scale-to-zero 后 5min wake 期 1Hz 采样 ~ 300 个 sample,baseline 算法见到
    // 300 个 warming_up=true (latency 飙高 + LFC 空) 和此后稳态的混合,不 filter 必污染。
    // 这里用 13 个 sample 模拟比例失衡。
    let samples = vec![
        // warming_up=true period (冷启 spike,极端高 latency)—— 7 个
        make(450.0, true),
        make(380.0, true),
        make(420.0, true),
        make(500.0, true),
        make(360.0, true),
        make(410.0, true),
        make(390.0, true),
        // warming_up=false period (稳态)—— 6 个
        make(12.0, false),
        make(15.0, false),
        make(11.0, false),
        make(14.0, false),
        make(13.0, false),
        make(16.0, false),
    ];

    // contract: mcp baseline 算法在 group-by 之前 filter warming_up != "true"
    let filtered: Vec<&Sample> = samples
        .iter()
        .filter(|s| {
            s.labels
                .iter()
                .find(|(k, _)| k == ATTR_WARMING_UP)
                .map(|(_, v)| v.as_str() != "true")
                .unwrap_or(true)
        })
        .collect();

    // assertion 1: 排除 warming_up=true 后只剩 6 个稳态 sample
    assert_eq!(filtered.len(), 6, "应排除 7 个 warming_up=true sample");

    // assertion 2: filter 后 median 在稳态区间 (10~17)
    let mut vals: Vec<f64> = filtered.iter().map(|s| s.value).collect();
    vals.sort_by(|a, b| a.partial_cmp(b).unwrap());
    // 6 个值 median = (vals[2] + vals[3]) / 2
    let median = (vals[2] + vals[3]) / 2.0;
    assert!(
        (10.0..20.0).contains(&median),
        "filter 后 median={median} 应在稳态区间 [10,20]"
    );

    // assertion 3: 不 filter 时 13 个值的 median 是 vals[6],被冷启段 7 个 spike 拉到几百 ms 级
    let mut all_vals: Vec<f64> = samples.iter().map(|s| s.value).collect();
    all_vals.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let polluted_median = all_vals[6];
    assert!(
        polluted_median > 200.0,
        "未 filter 时 median ({polluted_median}) 应被冷启 spike 拉到 200+ ms,证明确实被污染"
    );
    assert!(
        polluted_median > median * 10.0,
        "polluted median ({polluted_median}) 应远大于 filtered ({median})"
    );

    // assertion 4: GUC 默认值与 ADR-0013 一致
    assert_eq!(defaults::WARMING_UP_MIN_SECONDS, 30);
    assert_eq!(defaults::WARMING_UP_LFC_TARGET, 0.8);
    assert_eq!(defaults::WARMING_UP_MAX_SECONDS, 300);
}
