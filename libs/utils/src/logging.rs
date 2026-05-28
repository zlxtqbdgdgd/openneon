use std::future::Future;
use std::pin::Pin;
use std::str::FromStr;
use std::time::Duration;

use anyhow::Context;
use metrics::{IntCounter, IntCounterVec};
use once_cell::sync::Lazy;
use strum_macros::{EnumString, VariantNames};
use tokio::time::Instant;
use tracing::{info, warn};

/// Logs a critical error, similarly to `tracing::error!`. This will:
///
/// * Emit an ERROR log message with prefix "CRITICAL:" and a backtrace.
/// * Trigger a pageable alert (via the metric below).
/// * Increment libmetrics_tracing_event_count{level="critical"}, and indirectly level="error".
/// * In debug builds, panic the process.
///
/// When including errors in the message, please use {err:?} to include the error cause and original
/// backtrace.
#[macro_export]
macro_rules! critical {
    ($($arg:tt)*) => {{
        if cfg!(debug_assertions) {
            panic!($($arg)*);
        }
        // Increment both metrics
        $crate::logging::TRACING_EVENT_COUNT_METRIC.inc_critical();
        let backtrace = std::backtrace::Backtrace::capture();
        tracing::error!("CRITICAL: {}\n{backtrace}", format!($($arg)*));
    }};
}

#[macro_export]
macro_rules! critical_timeline {
    ($tenant_shard_id:expr, $timeline_id:expr, $corruption_detected:expr, $($arg:tt)*) => {{
        if cfg!(debug_assertions) {
            panic!($($arg)*);
        }
        // Increment both metrics
        $crate::logging::TRACING_EVENT_COUNT_METRIC.inc_critical();
        $crate::logging::HADRON_CRITICAL_STORAGE_EVENT_COUNT_METRIC.inc(&$tenant_shard_id.to_string(), &$timeline_id.to_string());
        if let Some(c) = $corruption_detected.as_ref() {
            c.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        let backtrace = std::backtrace::Backtrace::capture();
        tracing::error!("CRITICAL: [tenant_shard_id: {}, timeline_id: {}] {}\n{backtrace}",
                       $tenant_shard_id, $timeline_id, format!($($arg)*));
    }};
}

/// feat-031 · audit log OTel export 的 tracing target。
///
/// 所有 audit-relevant 事件用 `tracing::info!(target: AUDIT_TARGET, ...)` emit ·
/// 让 OTel collector 端按 `target=openneon::audit` 把 audit 事件跟 ordinary trace 分流路由
/// (详 [feat-031 §3.3](https://github.com/zlxtqbdgdgd/openneon-design/blob/main/features/feat-031-L2-neon-audit-log-otel-export.html))。
/// 跟 mcp 侧 (zlxtqbdgdgd/openneon-mcp#110 `emitAuditEvent`) `target` 属性统一。
pub const AUDIT_TARGET: &str = "openneon::audit";

/// feat-031 · audit event taxonomy (`openneon.audit.event_type` 取值)。
///
/// 13 类 · 跟 feat-031 §3.2 (a) attribute schema strict 对齐 · 跨 mcp/neon 统一。
/// neon 内核侧 L2a 主要 emit `DDL_EXECUTED` (pageserver) + `COMPUTE_AUDIT_LOG_RECORD`
/// (compute_tools);其余 (plan_mode_* / confirm_token_* / g*_deny / claim_override 等)
/// 由 mcp Server 侧 emit。
pub mod audit_event_type {
    pub const G1_CROSS_PROJECT_DENY: &str = "g1_cross_project_deny";
    pub const G4_DESTRUCTIVE_DENY: &str = "g4_destructive_deny";
    pub const G9_RATE_LIMIT_DENY: &str = "g9_rate_limit_deny";
    pub const PLAN_MODE_REQUIRED: &str = "plan_mode_required";
    pub const PLAN_MODE_APPROVED: &str = "plan_mode_approved";
    pub const PLAN_MODE_REJECTED: &str = "plan_mode_rejected";
    pub const CONFIRM_TOKEN_ISSUED: &str = "confirm_token_issued";
    pub const CONFIRM_TOKEN_VERIFIED: &str = "confirm_token_verified";
    pub const CONFIRM_TOKEN_REJECTED: &str = "confirm_token_rejected";
    pub const CLAIM_OVERRIDE: &str = "claim_override";
    pub const DESTRUCTIVE_CLASSIFIED: &str = "destructive_classified";
    pub const DDL_EXECUTED: &str = "ddl_executed";
    pub const COMPUTE_AUDIT_LOG_RECORD: &str = "compute_audit_log_record";
}

/// feat-031 · emit 一条 audit event 到 `openneon::audit` tracing target。
///
/// expands 到 `tracing::info!(target: "openneon::audit", event_type, ...)` —— OtelGuard
/// 自动把它当 span export 到 OTLP collector,collector 端按 `target` 路由 audit-vs-trace
/// (详 [feat-031 §3.2 (b) + §3.3](https://github.com/zlxtqbdgdgd/openneon-design/blob/main/features/feat-031-L2-neon-audit-log-otel-export.html))。
///
/// attribute 命名空间 `openneon.audit.*` (`event_type` / `op_class` / `principal` / `outcome`
/// 为必填语义字段 · DB 字段 `db.system` / `db.statement.sha256`) · 跟 mcp 侧统一。
///
/// **PII redact (§6)**:`db.statement` 永不传全文 · 只传 `db.statement.sha256`。
/// **USR (feat-008-011 L2b)**:`openneon.usr.*` namespace 已留 hook · L2a emit 时不填
/// (缺失不算 fail · L2b ship 后 4 组件 tracing event 自动 propagate · 不 breaking)。
///
/// # 用法
/// ```ignore
/// // 必填四件套 + 可选 attribute (tracing key-value 语法 · dot key 用字符串字面量包裹)
/// audit_event!(
///     event_type = utils::logging::audit_event_type::DDL_EXECUTED,
///     op_class = "CREATE_INDEX_CONCURRENTLY",
///     principal = "agent:ab12",
///     outcome = "allow",
///     "db.system" = "postgresql",
/// );
/// // 也支持带 message 尾参
/// audit_event!(event_type = "ddl_executed", outcome = "allow", "ddl 执行完成");
/// ```
#[macro_export]
macro_rules! audit_event {
    // 仅 key-value 字段 (无尾随 message)
    ($($fields:tt)+) => {{
        tracing::info!(
            target: $crate::logging::AUDIT_TARGET,
            $($fields)+
        );
    }};
}

#[derive(EnumString, strum_macros::Display, VariantNames, Eq, PartialEq, Debug, Clone, Copy)]
#[strum(serialize_all = "snake_case")]
pub enum LogFormat {
    Plain,
    Json,
    Test,
}

impl LogFormat {
    pub fn from_config(s: &str) -> anyhow::Result<LogFormat> {
        use strum::VariantNames;
        LogFormat::from_str(s).with_context(|| {
            format!(
                "Unrecognized log format. Please specify one of: {:?}",
                LogFormat::VARIANTS
            )
        })
    }
}

pub struct TracingEventCountMetric {
    /// CRITICAL is not a `tracing` log level. Instead, we increment it in the `critical!` macro,
    /// and also emit it as a regular error. These are thus double-counted, but that seems fine.
    critical: IntCounter,
    error: IntCounter,
    warn: IntCounter,
    info: IntCounter,
    debug: IntCounter,
    trace: IntCounter,
}

// Begin Hadron: Add a HadronCriticalStorageEventCountMetric metric that is sliced by tenant_id and timeline_id
pub struct HadronCriticalStorageEventCountMetric {
    critical: IntCounterVec,
}

pub static HADRON_CRITICAL_STORAGE_EVENT_COUNT_METRIC: Lazy<HadronCriticalStorageEventCountMetric> =
    Lazy::new(|| {
        let vec = metrics::register_int_counter_vec!(
            "hadron_critical_storage_event_count",
            "Number of critical storage events, by tenant_id and timeline_id",
            &["tenant_shard_id", "timeline_id"]
        )
        .expect("failed to define metric");
        HadronCriticalStorageEventCountMetric::new(vec)
    });

impl HadronCriticalStorageEventCountMetric {
    fn new(vec: IntCounterVec) -> Self {
        Self { critical: vec }
    }

    // Allow public access from `critical!` macro.
    pub fn inc(&self, tenant_shard_id: &str, timeline_id: &str) {
        self.critical
            .with_label_values(&[tenant_shard_id, timeline_id])
            .inc();
    }
}
// End Hadron

pub static TRACING_EVENT_COUNT_METRIC: Lazy<TracingEventCountMetric> = Lazy::new(|| {
    let vec = metrics::register_int_counter_vec!(
        "libmetrics_tracing_event_count",
        "Number of tracing events, by level",
        &["level"]
    )
    .expect("failed to define metric");
    TracingEventCountMetric::new(vec)
});

impl TracingEventCountMetric {
    fn new(vec: IntCounterVec) -> Self {
        Self {
            critical: vec.with_label_values(&["critical"]),
            error: vec.with_label_values(&["error"]),
            warn: vec.with_label_values(&["warn"]),
            info: vec.with_label_values(&["info"]),
            debug: vec.with_label_values(&["debug"]),
            trace: vec.with_label_values(&["trace"]),
        }
    }

    // Allow public access from `critical!` macro.
    pub fn inc_critical(&self) {
        self.critical.inc();
    }

    fn inc_for_level(&self, level: tracing::Level) {
        let counter = match level {
            tracing::Level::ERROR => &self.error,
            tracing::Level::WARN => &self.warn,
            tracing::Level::INFO => &self.info,
            tracing::Level::DEBUG => &self.debug,
            tracing::Level::TRACE => &self.trace,
        };
        counter.inc();
    }
}

struct TracingEventCountLayer(&'static TracingEventCountMetric);

impl<S> tracing_subscriber::layer::Layer<S> for TracingEventCountLayer
where
    S: tracing::Subscriber,
{
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        self.0.inc_for_level(*event.metadata().level());
    }
}

/// Whether to add the `tracing_error` crate's `ErrorLayer`
/// to the global tracing subscriber.
///
pub enum TracingErrorLayerEnablement {
    /// Do not add the `ErrorLayer`.
    Disabled,
    /// Add the `ErrorLayer` with the filter specified by RUST_LOG, defaulting to `info` if `RUST_LOG` is unset.
    EnableWithRustLogFilter,
}

/// Where the logging should output to.
#[derive(Clone, Copy)]
pub enum Output {
    Stdout,
    Stderr,
}

pub fn init(
    log_format: LogFormat,
    tracing_error_layer_enablement: TracingErrorLayerEnablement,
    output: Output,
) -> anyhow::Result<()> {
    // We fall back to printing all spans at info-level or above if
    // the RUST_LOG environment variable is not set.
    let rust_log_env_filter = || {
        tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"))
    };

    // NB: the order of the with() calls does not matter.
    // See https://docs.rs/tracing-subscriber/0.3.16/tracing_subscriber/layer/index.html#per-layer-filtering
    use tracing_subscriber::prelude::*;
    let r = tracing_subscriber::registry();
    let r = r.with({
        let log_layer = tracing_subscriber::fmt::layer()
            .with_target(false)
            .with_ansi(false)
            .with_writer(move || -> Box<dyn std::io::Write> {
                match output {
                    Output::Stdout => Box::new(std::io::stdout()),
                    Output::Stderr => Box::new(std::io::stderr()),
                }
            });
        let log_layer = match log_format {
            LogFormat::Json => log_layer.json().boxed(),
            LogFormat::Plain => log_layer.boxed(),
            LogFormat::Test => log_layer.with_test_writer().boxed(),
        };
        log_layer.with_filter(rust_log_env_filter())
    });

    let r = r.with(
        TracingEventCountLayer(&TRACING_EVENT_COUNT_METRIC).with_filter(rust_log_env_filter()),
    );
    match tracing_error_layer_enablement {
        TracingErrorLayerEnablement::EnableWithRustLogFilter => r
            .with(tracing_error::ErrorLayer::default().with_filter(rust_log_env_filter()))
            .init(),
        TracingErrorLayerEnablement::Disabled => r.init(),
    }

    Ok(())
}

/// feat-010 USR：发一条带 USR 命名规范的 tracing event。
///
/// 4 组件（pageserver / safekeeper / compute / proxy）的 telemetry event **统一走本 macro**，
/// 而不是裸 `tracing::info!`，以保证字段命名落在 cornerstone canonical schema（`openneon.usr.*`
/// / `endpoint_id` / `tenant_id` 等），避免命名漂移（feat-010 §3.2(a) / overview §10.2.3）。
///
/// 用法：`usr_event!("connection_accepted", endpoint_id = %ep, tenant_id = %tid);`
///
/// event 统一打到 `openneon::usr` target，便于 OTel collector / 日志侧按 target 过滤。
#[macro_export]
macro_rules! usr_event {
    ($event_type:expr, $($field:tt)*) => {
        ::tracing::info!(
            target: "openneon::usr",
            event_type = $event_type,
            $($field)*
        )
    };
    ($event_type:expr) => {
        ::tracing::info!(
            target: "openneon::usr",
            event_type = $event_type,
        )
    };
}

/// 在 [`init`] 的基础上额外装配 feat-008 cornerstone 的 USR tracing layer（feat-010 §3.2(a)(b)）。
///
/// 4 个 binary（compute_ctl / proxy / local_proxy / pg_sni_router）的 bin 入口**统一调本函数**
/// 取代裸 [`init`]，传入一个 `usr_resolver` closure（从 `ComputeSpec` / `ProxyConfig` /
/// safekeeper shard_map_cache 取当前 USR 上下文快照），让所有 span 自动带上 `openneon.usr.*`
/// attribute。`usr_resolver` 在每个新 span 上被调用（用 `Arc` snapshot 实现 < 100ns 取值）。
///
/// 注：[`tracing_utils::usr::UsrLayer`] 依赖 `tracing-opentelemetry` 的 OTel 层已注入
/// `OtelData`，故本 layer 排在 fmt / error 层之后；若进程未启用 OTel 出口（无 `OtelData`），
/// 该 layer 退化为 no-op，不影响 plain / json 日志。
pub fn init_with_usr<F>(
    log_format: LogFormat,
    tracing_error_layer_enablement: TracingErrorLayerEnablement,
    output: Output,
    usr_resolver: F,
) -> anyhow::Result<()>
where
    F: Fn() -> tracing_utils::usr::UsrContext + Send + Sync + 'static,
{
    let rust_log_env_filter = || {
        tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"))
    };

    use tracing_subscriber::prelude::*;
    let r = tracing_subscriber::registry();
    let r = r.with({
        let log_layer = tracing_subscriber::fmt::layer()
            .with_target(false)
            .with_ansi(false)
            .with_writer(move || -> Box<dyn std::io::Write> {
                match output {
                    Output::Stdout => Box::new(std::io::stdout()),
                    Output::Stderr => Box::new(std::io::stderr()),
                }
            });
        let log_layer = match log_format {
            LogFormat::Json => log_layer.json().boxed(),
            LogFormat::Plain => log_layer.boxed(),
            LogFormat::Test => log_layer.with_test_writer().boxed(),
        };
        log_layer.with_filter(rust_log_env_filter())
    });

    let r = r.with(
        TracingEventCountLayer(&TRACING_EVENT_COUNT_METRIC).with_filter(rust_log_env_filter()),
    );

    // feat-008 cornerstone：4 组件共用的 USR layer，把 resolver 解析出的三件套注入每个 span。
    let usr_layer = tracing_utils::usr::usr_layer(usr_resolver);

    match tracing_error_layer_enablement {
        TracingErrorLayerEnablement::EnableWithRustLogFilter => r
            .with(tracing_error::ErrorLayer::default().with_filter(rust_log_env_filter()))
            .with(usr_layer)
            .init(),
        TracingErrorLayerEnablement::Disabled => r.with(usr_layer).init(),
    }

    Ok(())
}

/// Disable the default rust panic hook by using `set_hook`.
///
/// For neon binaries, the assumption is that tracing is configured before with [`init`], after
/// that sentry is configured (if needed). sentry will install it's own on top of this, always
/// processing the panic before we log it.
///
/// When the return value is dropped, the hook is reverted to std default hook (prints to stderr).
/// If the assumptions about the initialization order are not held, use
/// [`TracingPanicHookGuard::forget`] but keep in mind, if tracing is stopped, then panics will be
/// lost.
#[must_use]
pub fn replace_panic_hook_with_tracing_panic_hook() -> TracingPanicHookGuard {
    std::panic::set_hook(Box::new(tracing_panic_hook));
    TracingPanicHookGuard::new()
}

/// Drop guard which restores the std panic hook on drop.
///
/// Tracing should not be used when it's not configured, but we cannot really latch on to any
/// imaginary lifetime of tracing.
pub struct TracingPanicHookGuard {
    act: bool,
}

impl TracingPanicHookGuard {
    fn new() -> Self {
        TracingPanicHookGuard { act: true }
    }

    /// Make this hook guard not do anything when dropped.
    pub fn forget(&mut self) {
        self.act = false;
    }
}

impl Drop for TracingPanicHookGuard {
    fn drop(&mut self) {
        if self.act {
            let _ = std::panic::take_hook();
        }
    }
}

/// Named symbol for our panic hook, which logs the panic.
fn tracing_panic_hook(info: &std::panic::PanicHookInfo) {
    // following rust 1.66.1 std implementation:
    // https://github.com/rust-lang/rust/blob/90743e7298aca107ddaa0c202a4d3604e29bfeb6/library/std/src/panicking.rs#L235-L288
    let location = info.location();

    let msg = match info.payload().downcast_ref::<&'static str>() {
        Some(s) => *s,
        None => match info.payload().downcast_ref::<String>() {
            Some(s) => &s[..],
            None => "Box<dyn Any>",
        },
    };

    let thread = std::thread::current();
    let thread = thread.name().unwrap_or("<unnamed>");
    let backtrace = std::backtrace::Backtrace::capture();

    let _entered = if let Some(location) = location {
        tracing::error_span!("panic", %thread, location = %PrettyLocation(location))
    } else {
        // very unlikely to hit here, but the guarantees of std could change
        tracing::error_span!("panic", %thread)
    }
    .entered();

    if backtrace.status() == std::backtrace::BacktraceStatus::Captured {
        // this has an annoying extra '\n' in the end which anyhow doesn't do, but we cannot really
        // get rid of it as we cannot get in between of std::fmt::Formatter<'_>; we could format to
        // string, maybe even to a TLS one but tracing already does that.
        tracing::error!("{msg}\n\nStack backtrace:\n{backtrace}");
    } else {
        tracing::error!("{msg}");
    }

    // ensure that we log something on the panic if this hook is left after tracing has been
    // unconfigured. worst case when teardown is racing the panic is to log the panic twice.
    tracing::dispatcher::get_default(|d| {
        if let Some(_none) = d.downcast_ref::<tracing::subscriber::NoSubscriber>() {
            let location = location.map(PrettyLocation);
            log_panic_to_stderr(thread, msg, location, &backtrace);
        }
    });
}

#[cold]
fn log_panic_to_stderr(
    thread: &str,
    msg: &str,
    location: Option<PrettyLocation<'_, '_>>,
    backtrace: &std::backtrace::Backtrace,
) {
    eprintln!(
        "panic while tracing is unconfigured: thread '{thread}' panicked at '{msg}', {location:?}\nStack backtrace:\n{backtrace}"
    );
}

struct PrettyLocation<'a, 'b>(&'a std::panic::Location<'b>);

impl std::fmt::Display for PrettyLocation<'_, '_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}:{}", self.0.file(), self.0.line(), self.0.column())
    }
}

impl std::fmt::Debug for PrettyLocation<'_, '_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        <Self as std::fmt::Display>::fmt(self, f)
    }
}

/// When you will store a secret but want to make sure it won't
/// be accidentally logged, wrap it in a SecretString, whose Debug
/// implementation does not expose the contents.
#[derive(Clone, Eq, PartialEq)]
pub struct SecretString(String);

impl SecretString {
    pub fn get_contents(&self) -> &str {
        self.0.as_str()
    }
}

impl From<String> for SecretString {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl FromStr for SecretString {
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(s.to_string()))
    }
}

impl std::fmt::Debug for SecretString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[SECRET]")
    }
}

/// Logs a periodic message if a future is slow to complete.
///
/// This is performance-sensitive as it's used on the GetPage read path.
///
/// TODO: consider upgrading this to a warning, but currently it fires too often.
#[inline]
pub async fn log_slow<O>(
    name: &str,
    threshold: Duration,
    f: Pin<&mut impl Future<Output = O>>,
) -> O {
    monitor_slow_future(
        threshold,
        threshold, // period = threshold
        f,
        |MonitorSlowFutureCallback {
             ready,
             is_slow,
             elapsed_total,
             elapsed_since_last_callback: _,
         }| {
            if !is_slow {
                return;
            }
            let elapsed = elapsed_total.as_secs_f64();
            if ready {
                info!("slow {name} completed after {elapsed:.3}s");
            } else {
                info!("slow {name} still running after {elapsed:.3}s");
            }
        },
    )
    .await
}

/// Logs a periodic warning if a future is slow to complete.
#[inline]
pub async fn warn_slow<O>(
    name: &str,
    threshold: Duration,
    f: Pin<&mut impl Future<Output = O>>,
) -> O {
    monitor_slow_future(
        threshold,
        threshold, // period = threshold
        f,
        |MonitorSlowFutureCallback {
             ready,
             is_slow,
             elapsed_total,
             elapsed_since_last_callback: _,
         }| {
            if !is_slow {
                return;
            }
            let elapsed = elapsed_total.as_secs_f64();
            if ready {
                warn!("slow {name} completed after {elapsed:.3}s");
            } else {
                warn!("slow {name} still running after {elapsed:.3}s");
            }
        },
    )
    .await
}

/// Poll future `fut` to completion, invoking callback `cb` at the given `threshold` and every
/// `period` afterwards, and also unconditionally when the future completes.
#[inline]
pub async fn monitor_slow_future<F, O>(
    threshold: Duration,
    period: Duration,
    mut fut: Pin<&mut F>,
    mut cb: impl FnMut(MonitorSlowFutureCallback),
) -> O
where
    F: Future<Output = O>,
{
    let started = Instant::now();
    let mut attempt = 1;
    let mut last_cb = started;
    loop {
        // NB: use timeout_at() instead of timeout() to avoid an extra clock reading in the common
        // case where the timeout doesn't fire.
        let deadline = started + threshold + (attempt - 1) * period;
        // TODO: still call the callback if the future panics? Copy how we do it for the page_service flush_in_progress counter.
        let res = tokio::time::timeout_at(deadline, &mut fut).await;
        let now = Instant::now();
        let elapsed_total = now - started;
        cb(MonitorSlowFutureCallback {
            ready: res.is_ok(),
            is_slow: elapsed_total >= threshold,
            elapsed_total,
            elapsed_since_last_callback: now - last_cb,
        });
        last_cb = now;
        if let Ok(output) = res {
            return output;
        }
        attempt += 1;
    }
}

/// See [`monitor_slow_future`].
pub struct MonitorSlowFutureCallback {
    /// Whether the future completed. If true, there will be no more callbacks.
    pub ready: bool,
    /// Whether the future is taking `>=` the specififed threshold duration to complete.
    /// Monotonic: if true in one callback invocation, true in all subsequent onces.
    pub is_slow: bool,
    /// The time elapsed since the [`monitor_slow_future`] was first polled.
    pub elapsed_total: Duration,
    /// The time elapsed since the last callback invocation.
    /// For the initial callback invocation, the time elapsed since the [`monitor_slow_future`] was first polled.
    pub elapsed_since_last_callback: Duration,
}

#[cfg(test)]
mod tests {
    use metrics::IntCounterVec;
    use metrics::core::Opts;

    use crate::logging::{TracingEventCountLayer, TracingEventCountMetric};

    #[test]
    fn tracing_event_count_metric() {
        let counter_vec =
            IntCounterVec::new(Opts::new("testmetric", "testhelp"), &["level"]).unwrap();
        let metric = Box::leak(Box::new(TracingEventCountMetric::new(counter_vec.clone())));
        let layer = TracingEventCountLayer(metric);
        use tracing_subscriber::prelude::*;

        tracing::subscriber::with_default(tracing_subscriber::registry().with(layer), || {
            tracing::trace!("foo");
            tracing::debug!("foo");
            tracing::info!("foo");
            tracing::warn!("foo");
            tracing::error!("foo");
        });

        assert_eq!(counter_vec.with_label_values(&["trace"]).get(), 1);
        assert_eq!(counter_vec.with_label_values(&["debug"]).get(), 1);
        assert_eq!(counter_vec.with_label_values(&["info"]).get(), 1);
        assert_eq!(counter_vec.with_label_values(&["warn"]).get(), 1);
        assert_eq!(counter_vec.with_label_values(&["error"]).get(), 1);
    }
}
