//! feat-039 · 冷启 warming_up 状态机 (compute_tools)
//!
//! Phase C 详设: features/feat-039-L3-neon-baseline-state-warming-up.html §3.1 §3.2
//! ADR: docs/adr/0013-warming-up-state-machine-exit-conditions.md
//!
//! ## 背景
//!
//! Neon scale-to-zero 冷启动后前 N 秒 metric 异常 (LFC 空 · latency 飙高)。若 mcp baseline
//! 算法 (feat-016/017/018/038) 不排除冷启段 sample,每次 wake 都会假警报风暴,长期 baseline
//! 也被污染。本模块给所有 Neon-emit metric 加 `warming_up=true/false` tag,让 baseline 算法
//! 在 mcp 侧把 `warming_up=true` 的 sample 过滤掉。
//!
//! ## 状态机退出条件 (ADR-0013)
//!
//! - **正常退出**: `elapsed_since_resume >= min_seconds (默认 30s)` AND
//!   `lfc_hit_ratio >= lfc_target (默认 80%)`
//! - **兜底退出**: `elapsed_since_resume >= max_seconds (默认 300s)` —— 强制退出 + log warn
//!
//! 两个退出路径都把 [`WarmingUpState`] 从 [`WarmingUpState::Warming`] 翻到
//! [`WarmingUpState::Warm`],并且 flip [`WARMING_UP_FLAG`] (USR pattern 第 8 维 metric label
//! 数据源,feat-039/#2)。
//!
//! ## 实现要点
//!
//! - 1 Hz tokio timer (`spawn_warming_up_task`),复用 `tokio::time::interval`。
//! - LFC ratio 数据源走 [`LfcMetricsProvider`] trait,prod 实现通过 communicator C-FFI
//!   `callback_get_lfc_metrics` 拉数 (见 [`prod_lfc_provider`] 的 TODO);单测用 mock。
//! - 状态机生命周期挂在 `set_status(Running)` 上,而**不**新开 `ComputeStatus` enum 值
//!   (OQ4: warming_up 只是 Running 的内部子相位,对外暴露的 status 仍是 Running)。
//! - 4 case 单测覆盖: 标准 wake / 快暖兜底 / 慢暖强制 / LFC 数据源不可用。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use once_cell::sync::Lazy;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

/// 全局 warming_up flag。USR resolver / metric label inject 都读它 (feat-039/#2)。
///
/// 进程启动时为 `true` (待 [`WarmingUpController`] 启动后保持 `true`);状态机退出条件满足后
/// 翻到 `false`,直到下一次冷启 (理论上 compute_ctl 不重启则不重置 —— Neon scale-to-zero
/// 冷启进程级隔离,新一次 wake = 新进程)。
pub static WARMING_UP_FLAG: AtomicBool = AtomicBool::new(true);

/// 默认 GUC 取值 (feat-039/#3 · ADR-0013)。
pub mod defaults {
    pub const WARMING_UP_MIN_SECONDS: u64 = 30;
    pub const WARMING_UP_LFC_TARGET: f64 = 0.8;
    pub const WARMING_UP_MAX_SECONDS: u64 = 300;
}

/// warming_up 状态机的两个相位。`Running` 内部子相位,不外露 `ComputeStatus` (OQ4)。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WarmingUpState {
    /// 进程刚 wake / cold-start,baseline 算法应该排除此期间 sample。
    Warming,
    /// 状态机已退出,metric label `warming_up=false`,baseline 算法正常采纳。
    Warm,
}

/// 状态机配置 (3 GUC 注入)。
#[derive(Clone, Copy, Debug)]
pub struct WarmingUpConfig {
    /// 最小热身时间(秒)。`elapsed < min_seconds` 时即使 LFC ratio 已达标也保持 Warming。
    pub min_seconds: u64,
    /// LFC 命中率目标 [0.0, 1.0]。`min_seconds` 满足后还要 `ratio >= target` 才退出。
    pub lfc_target: f64,
    /// 兜底最大热身时间(秒)。超过强制退出并打 warn。
    pub max_seconds: u64,
}

impl Default for WarmingUpConfig {
    fn default() -> Self {
        Self {
            min_seconds: defaults::WARMING_UP_MIN_SECONDS,
            lfc_target: defaults::WARMING_UP_LFC_TARGET,
            max_seconds: defaults::WARMING_UP_MAX_SECONDS,
        }
    }
}

impl WarmingUpConfig {
    /// 合法性校验: `min <= max`,`target` 在 (0, 1] 之间。非法值 fail-open (用 default)。
    pub fn validated(self) -> Self {
        let mut v = self;
        if v.min_seconds > v.max_seconds {
            warn!(
                min = v.min_seconds,
                max = v.max_seconds,
                "warming_up_min_seconds > max_seconds · fall back to defaults"
            );
            v = Self::default();
        }
        if !(0.0 < v.lfc_target && v.lfc_target <= 1.0) {
            warn!(
                target = v.lfc_target,
                "warming_up_lfc_target out of (0,1] · fall back to defaults"
            );
            v.lfc_target = defaults::WARMING_UP_LFC_TARGET;
        }
        v
    }
}

/// LFC ratio 数据源抽象。`get_hit_ratio()` 返回 `Some(ratio)` (0.0-1.0) 或
/// `None` (数据源不可用,通常意味着 communicator 还没启动)。
///
/// prod 实现 [`prod_lfc_provider`] 在 communicator 接入完成后通过 C-FFI 拉数;
/// 单测用 [`MockLfcProvider`]。
pub trait LfcMetricsProvider: Send + Sync + 'static {
    /// 返回当前 LFC 命中率 ∈ [0.0, 1.0],数据源不可用时返回 `None`。
    fn get_hit_ratio(&self) -> Option<f64>;
}

/// 生产环境 LFC provider 的占位实现。
///
/// **TODO (feat-068 集成阶段)**: 改为通过 `compute_tools::communicator_socket_client`
/// 或直接 ffi `callback_get_lfc_metrics()` 拉 `(lfc_hits, lfc_misses)`,计算
/// `hits / (hits + misses)`。当前实现返回 `None`,状态机走 max_seconds 兜底
/// (5min 强制退出),这是 design#48 的安全 fallback 行为。
///
/// 之所以暂不在本 PR wire prod 数据源: communicator 进程与 compute_ctl 进程之间是
/// Unix domain socket 通信 (`communicator_socket_client.rs`),pull `lfc_metrics` 需要
/// 额外定义一个 RPC method (现仓库只有 `set_my_latch` / `get_lfc_metrics` 这两条
/// callback 是 communicator → C 方向,反向拉数需新增 routes)。这一步留给 feat-068
/// 阶段统一处理,避免本 slice scope 失控。
pub fn prod_lfc_provider() -> Arc<dyn LfcMetricsProvider> {
    Arc::new(NullLfcProvider)
}

/// 始终返回 `None` 的 provider —— prod 集成前 fallback。
pub struct NullLfcProvider;
impl LfcMetricsProvider for NullLfcProvider {
    fn get_hit_ratio(&self) -> Option<f64> {
        None
    }
}

/// 状态机核心控制器。
///
/// 持有 `start_time`、当前 state、配置、LFC provider。`step()` 是状态机推进函数 (纯函数,
/// 给单测吃),`spawn_warming_up_task()` 是 tokio 1Hz timer wrapper。
pub struct WarmingUpController {
    inner: Mutex<Inner>,
    config: WarmingUpConfig,
    lfc_provider: Arc<dyn LfcMetricsProvider>,
}

struct Inner {
    state: WarmingUpState,
    start_time: Instant,
}

impl WarmingUpController {
    pub fn new(config: WarmingUpConfig, lfc_provider: Arc<dyn LfcMetricsProvider>) -> Self {
        // 进程级 flag 初始化为 true (待状态机翻 false)。
        WARMING_UP_FLAG.store(true, Ordering::Relaxed);
        Self {
            inner: Mutex::new(Inner {
                state: WarmingUpState::Warming,
                start_time: Instant::now(),
            }),
            config: config.validated(),
            lfc_provider,
        }
    }

    /// 测试用构造: 显式传入 `start_time`,绕开 `Instant::now()`。
    ///
    /// 不加 `cfg(test)` 守卫: integration test crate (compute_tools/tests/) 是独立编译单元,
    /// 没法看到 `cfg(test)` 标记的 pub item。生产代码不应该直接调本 ctor (`new()` 已经覆盖
    /// `Instant::now()`),但暴露此 ctor 不会破坏 invariant —— 状态机本身对 start_time 取值
    /// 无信任假设,合算的工程取舍。
    pub fn new_with_start_time(
        config: WarmingUpConfig,
        lfc_provider: Arc<dyn LfcMetricsProvider>,
        start_time: Instant,
    ) -> Self {
        WARMING_UP_FLAG.store(true, Ordering::Relaxed);
        Self {
            inner: Mutex::new(Inner {
                state: WarmingUpState::Warming,
                start_time,
            }),
            config: config.validated(),
            lfc_provider,
        }
    }

    /// 当前状态快照。
    pub async fn state(&self) -> WarmingUpState {
        self.inner.lock().await.state
    }

    /// 状态机推进一步。入参 `now` 让单测控制时间,prod 调用方传 `Instant::now()`。
    ///
    /// 返回 `true` 表示本次 step 把状态从 Warming 翻到了 Warm (即首次"暖完"),
    /// 调用方可据此触发 metric tag flip 副作用 (USR_LABEL_NAMES 路径,feat-039/#2)。
    pub async fn step(&self, now: Instant) -> bool {
        let mut inner = self.inner.lock().await;
        if inner.state == WarmingUpState::Warm {
            return false;
        }
        let elapsed = now.saturating_duration_since(inner.start_time);
        let elapsed_secs = elapsed.as_secs();

        // 兜底退出: max_seconds 到了无论 LFC 如何强制退出。
        if elapsed_secs >= self.config.max_seconds {
            warn!(
                elapsed_secs,
                max = self.config.max_seconds,
                "warming_up 兜底退出 (max_seconds 到 · LFC 可能未达标)"
            );
            inner.state = WarmingUpState::Warm;
            WARMING_UP_FLAG.store(false, Ordering::Relaxed);
            return true;
        }

        // 正常退出条件: elapsed >= min AND ratio >= target。
        if elapsed_secs >= self.config.min_seconds {
            if let Some(ratio) = self.lfc_provider.get_hit_ratio() {
                if ratio >= self.config.lfc_target {
                    info!(
                        elapsed_secs,
                        ratio,
                        target = self.config.lfc_target,
                        "warming_up 正常退出 (LFC 达标)"
                    );
                    inner.state = WarmingUpState::Warm;
                    WARMING_UP_FLAG.store(false, Ordering::Relaxed);
                    return true;
                }
            }
        }
        false
    }
}

/// 在 tokio runtime 上启动 1 Hz 状态机推进任务。`cancel` 提供干净退出 (compute_ctl shutdown
/// 时调用 `cancel.cancel()`)。
///
/// 返回 `JoinHandle`,调用方决定是否 await (实际部署通常 fire-and-forget,因为 compute_ctl
/// shutdown 路径会 abort runtime)。
pub fn spawn_warming_up_task(
    controller: Arc<WarmingUpController>,
    cancel: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        // skip 第一个立即 tick,让 elapsed=0 时不无意义地 step 一次。
        interval.tick().await;
        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    info!("warming_up task cancelled");
                    return;
                }
                _ = interval.tick() => {
                    let flipped = controller.step(Instant::now()).await;
                    if flipped {
                        // 已 Warm,task 退出 —— 不再消耗 wake-up。
                        info!("warming_up state machine flipped to Warm · exit timer task");
                        return;
                    }
                }
            }
        }
    })
}

/// 进程级单例,compute_ctl 启动后用 [`init_global_controller`] 装入。USR resolver / metric
/// label 通过 [`is_warming_up`] 读取。
static GLOBAL_CONTROLLER: Lazy<std::sync::OnceLock<Arc<WarmingUpController>>> =
    Lazy::new(std::sync::OnceLock::new);

/// 装入进程级 controller。**仅在 compute_ctl 启动路径调用一次**;重复调用是 no-op (返回
/// 已装入的引用)。
pub fn init_global_controller(
    config: WarmingUpConfig,
    lfc_provider: Arc<dyn LfcMetricsProvider>,
) -> Arc<WarmingUpController> {
    let controller = Arc::new(WarmingUpController::new(config, lfc_provider));
    match GLOBAL_CONTROLLER.set(controller.clone()) {
        Ok(()) => controller,
        Err(_) => {
            // 已 init 过 —— 返回已装入的引用。生产环境只在 compute_ctl 启动调一次,理论
            // 不重入;test 多 case 共用进程时可能重入,这里幂等。
            GLOBAL_CONTROLLER.get().expect("just checked").clone()
        }
    }
}

/// USR resolver / metric label inject 调用入口。无 controller (未 init) 时保守返回 `true`
/// (假设 wake 中,fail-safe 让 baseline 算法排除该 sample)。
pub fn is_warming_up() -> bool {
    WARMING_UP_FLAG.load(Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 4 case 单测专用 mock,可显式 set ratio = None / Some(x)。
    struct MockLfcProvider {
        ratio: std::sync::Mutex<Option<f64>>,
    }
    impl MockLfcProvider {
        fn new(ratio: Option<f64>) -> Self {
            Self {
                ratio: std::sync::Mutex::new(ratio),
            }
        }
        fn set(&self, ratio: Option<f64>) {
            *self.ratio.lock().unwrap() = ratio;
        }
    }
    impl LfcMetricsProvider for MockLfcProvider {
        fn get_hit_ratio(&self) -> Option<f64> {
            *self.ratio.lock().unwrap()
        }
    }

    fn make_controller(
        config: WarmingUpConfig,
        provider: Arc<MockLfcProvider>,
        start_time: Instant,
    ) -> WarmingUpController {
        WarmingUpController::new_with_start_time(config, provider, start_time)
    }

    /// case 1: 标准 wake —— T+45s 时 LFC 已达 80%,状态机正常退出。
    #[tokio::test]
    async fn case_1_standard_wake() {
        let provider = Arc::new(MockLfcProvider::new(None));
        let t0 = Instant::now();
        let ctrl = make_controller(WarmingUpConfig::default(), provider.clone(), t0);

        // T+10s LFC 还没暖起来。
        assert!(!ctrl.step(t0 + Duration::from_secs(10)).await);
        assert_eq!(ctrl.state().await, WarmingUpState::Warming);

        // T+30s LFC 暖到 50% —— 还不到 target。
        provider.set(Some(0.5));
        assert!(!ctrl.step(t0 + Duration::from_secs(30)).await);
        assert_eq!(ctrl.state().await, WarmingUpState::Warming);

        // T+45s LFC 达 85% —— 两条件都满足,翻 Warm。
        provider.set(Some(0.85));
        assert!(ctrl.step(t0 + Duration::from_secs(45)).await);
        assert_eq!(ctrl.state().await, WarmingUpState::Warm);
        assert!(!is_warming_up());
    }

    /// case 2: 快暖兜底 —— T+5s LFC 已经 90%,但要等到 T+30s 才允许退出 (min_seconds 兜底)。
    #[tokio::test]
    async fn case_2_fast_warm_min_floor() {
        let provider = Arc::new(MockLfcProvider::new(Some(0.9)));
        let t0 = Instant::now();
        let ctrl = make_controller(WarmingUpConfig::default(), provider, t0);

        // T+5s LFC 已 90%,但 elapsed < min_seconds=30,不退出。
        assert!(!ctrl.step(t0 + Duration::from_secs(5)).await);
        assert_eq!(ctrl.state().await, WarmingUpState::Warming);

        // T+29s 仍未到 min,依然 Warming。
        assert!(!ctrl.step(t0 + Duration::from_secs(29)).await);
        assert_eq!(ctrl.state().await, WarmingUpState::Warming);

        // T+30s 恰好到 min + LFC 达标,翻 Warm。
        assert!(ctrl.step(t0 + Duration::from_secs(30)).await);
        assert_eq!(ctrl.state().await, WarmingUpState::Warm);
    }

    /// case 3: 慢暖强制 —— LFC 10 min 都没到 80%,T+300s max 兜底强制退出 + warn。
    #[tokio::test]
    async fn case_3_slow_warm_max_force() {
        let provider = Arc::new(MockLfcProvider::new(Some(0.4))); // 始终低于 target
        let t0 = Instant::now();
        let ctrl = make_controller(WarmingUpConfig::default(), provider, t0);

        // T+200s LFC 还在 40%。
        assert!(!ctrl.step(t0 + Duration::from_secs(200)).await);
        assert_eq!(ctrl.state().await, WarmingUpState::Warming);

        // T+299s 仍 Warming。
        assert!(!ctrl.step(t0 + Duration::from_secs(299)).await);
        assert_eq!(ctrl.state().await, WarmingUpState::Warming);

        // T+300s max 兜底,强制 Warm。
        assert!(ctrl.step(t0 + Duration::from_secs(300)).await);
        assert_eq!(ctrl.state().await, WarmingUpState::Warm);
        assert!(!is_warming_up());
    }

    /// case 4: LFC 数据源不可用 —— get_hit_ratio() 一直返回 None,只能走 max_seconds 兜底。
    #[tokio::test]
    async fn case_4_lfc_unavailable_falls_back_to_max() {
        let provider = Arc::new(MockLfcProvider::new(None));
        let t0 = Instant::now();
        let ctrl = make_controller(WarmingUpConfig::default(), provider, t0);

        // T+100s 数据源仍 None,不退出。
        assert!(!ctrl.step(t0 + Duration::from_secs(100)).await);
        assert_eq!(ctrl.state().await, WarmingUpState::Warming);

        // T+300s 走 max 兜底。
        assert!(ctrl.step(t0 + Duration::from_secs(300)).await);
        assert_eq!(ctrl.state().await, WarmingUpState::Warm);
    }

    /// case 5: 翻 Warm 后再 step 不应回退 / 不应重复触发 flip 副作用。
    #[tokio::test]
    async fn case_5_no_regression_after_warm() {
        let provider = Arc::new(MockLfcProvider::new(Some(0.95)));
        let t0 = Instant::now();
        let ctrl = make_controller(WarmingUpConfig::default(), provider, t0);

        // T+30s 翻 Warm。
        assert!(ctrl.step(t0 + Duration::from_secs(30)).await);
        // T+31s 再 step,state 不变,flipped=false。
        assert!(!ctrl.step(t0 + Duration::from_secs(31)).await);
        assert_eq!(ctrl.state().await, WarmingUpState::Warm);
    }

    /// case 6: config 非法值 (min > max / target 越界) 自动 fall back。
    #[tokio::test]
    async fn case_6_invalid_config_fallback() {
        let bad = WarmingUpConfig {
            min_seconds: 600,
            lfc_target: 1.5,
            max_seconds: 100,
        };
        let validated = bad.validated();
        assert_eq!(validated.min_seconds, defaults::WARMING_UP_MIN_SECONDS);
        assert_eq!(validated.max_seconds, defaults::WARMING_UP_MAX_SECONDS);
        assert_eq!(validated.lfc_target, defaults::WARMING_UP_LFC_TARGET);
    }

    /// case 7: WARMING_UP_FLAG 全局可见性 (USR resolver / metric label 读这个)。
    #[tokio::test]
    async fn case_7_global_flag_visibility() {
        // 重置全局 flag (注: 多 test 并行时会互相影响,这里只断本测试因果)。
        WARMING_UP_FLAG.store(true, Ordering::Relaxed);
        let provider = Arc::new(MockLfcProvider::new(Some(0.99)));
        let t0 = Instant::now();
        let ctrl = make_controller(WarmingUpConfig::default(), provider, t0);

        // 初始 true。
        assert!(is_warming_up());
        // 翻 Warm 后 false。
        ctrl.step(t0 + Duration::from_secs(30)).await;
        assert!(!is_warming_up());
    }
}
