//! USR tracing 注入 cornerstone（feat-008 §3.3(c)）。
//!
//! 提供一个可被 4 组件（pageserver / safekeeper / compute / proxy）**共用**的机制，
//! 把 USR 三件套（+ 可选 endpoint/project）注入到 OpenTelemetry span attribute 出口，
//! attribute key 用 `openneon.usr.*` canonical namespace。
//!
//! 两种用法：
//! 1. [`UsrLayer`] —— 注册到 tracing subscriber，对**每个新 span** 自动把 resolver 解析出的
//!    USR 写入该 span 的 OTel attribute（适合 4 组件 bin 入口一键 wiring，feat-009/010 §3.2(b)）。
//! 2. [`record_usr`] —— 在已知 USR 的代码点手动调用，把 USR 写到**当前 span**（适合
//!    pageserver GetPage / WAL ingest 等已有显式 span 的热点路径，feat-008 §3.3(c)）。
//!
//! feat-009 safekeeper / feat-010 compute/proxy **复用本 cornerstone**，不各自重写 attribute 注入。
//!
//! 注：USR 字段取值的 string 化形式（`shard_id` 4 char hex 等）由 `utils::usr` 定义，
//! 本模块只持有已 string 化的 [`UsrContext`]，避免 `tracing-utils` ↔ `utils` 依赖环。

use std::sync::Arc;
use std::sync::Once;

use opentelemetry::KeyValue;
use tracing::Subscriber;
use tracing::span;
use tracing_opentelemetry::OpenTelemetrySpanExt;
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;

/// USR canonical attribute key（跟 `utils::usr::attr` 保持一致；此处复制常量以打破依赖环）。
pub const ATTR_TENANT_ID: &str = "openneon.usr.tenant_id";
pub const ATTR_TIMELINE_ID: &str = "openneon.usr.timeline_id";
pub const ATTR_SHARD_ID: &str = "openneon.usr.shard_id";
pub const ATTR_ENDPOINT_ID: &str = "openneon.usr.endpoint_id";
pub const ATTR_PROJECT_ID: &str = "openneon.usr.project_id";
/// feat-039/#2 USR pattern 第 8 维 metric label —— 仅 compute 侧 resolver 填充
/// (OQ4: pageserver/safekeeper/proxy 共享进程不贴,避免误标其他 endpoint)。
pub const ATTR_WARMING_UP: &str = "openneon.usr.warming_up";

/// USR_LABEL_NAMES 常量列表 (feat-039/#2 USR pattern · 第 8 维加 warming_up)。
///
/// 跟 Prometheus + OTel + tracing 三 channel 自动同步: 各 channel exporter 都从这里
/// 拿 label name 列表,一处改全链路飘起。
pub const USR_LABEL_NAMES: &[&str] = &[
    ATTR_TENANT_ID,
    ATTR_TIMELINE_ID,
    ATTR_SHARD_ID,
    ATTR_ENDPOINT_ID,
    ATTR_PROJECT_ID,
    ATTR_WARMING_UP,
];

/// 一份已 string 化的 USR 上下文快照（resolver 输出）。
///
/// 各字段都是 [`Option<String>`]：缺失字段不注入（fail-safe，OTel collector 端 backward compat）。
/// `shard_id` 多 shard 场景填逗号分隔的升序列表（feat-009 §4.1）。
///
/// `warming_up`: feat-039/#2 第 8 维。仅 compute 侧 resolver 填充 `Some(true/false)`,
/// 其它组件 (pageserver / safekeeper / proxy 共享进程) 保持 `None` 不贴 label,
/// 避免误标其他 endpoint (OQ4)。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UsrContext {
    pub tenant_id: Option<String>,
    pub timeline_id: Option<String>,
    pub shard_id: Option<String>,
    pub endpoint_id: Option<String>,
    pub project_id: Option<String>,
    /// feat-039/#2: 冷启 warming_up 子相位 flag。`Some(true)` 表示状态机尚未退出
    /// (mcp baseline 算法应排除该 sample);`Some(false)` 表示已暖完;`None` 表示
    /// 进程未启用 warming_up tracking (非 compute 组件)。
    pub warming_up: Option<bool>,
}

impl UsrContext {
    /// 把上下文转成 OTel `KeyValue` 列表，仅含非空字段。
    pub fn as_key_values(&self) -> Vec<KeyValue> {
        let mut out = Vec::with_capacity(6);
        if let Some(v) = &self.tenant_id {
            out.push(KeyValue::new(ATTR_TENANT_ID, v.clone()));
        }
        if let Some(v) = &self.timeline_id {
            out.push(KeyValue::new(ATTR_TIMELINE_ID, v.clone()));
        }
        if let Some(v) = &self.shard_id {
            out.push(KeyValue::new(ATTR_SHARD_ID, v.clone()));
        }
        if let Some(v) = &self.endpoint_id {
            out.push(KeyValue::new(ATTR_ENDPOINT_ID, v.clone()));
        }
        if let Some(v) = &self.project_id {
            out.push(KeyValue::new(ATTR_PROJECT_ID, v.clone()));
        }
        if let Some(v) = &self.warming_up {
            // bool → "true" / "false" string,跟 Prometheus label 习惯一致 (label value 永远是 string)
            out.push(KeyValue::new(ATTR_WARMING_UP, v.to_string()));
        }
        out
    }

    pub fn is_empty(&self) -> bool {
        self.tenant_id.is_none()
            && self.timeline_id.is_none()
            && self.shard_id.is_none()
            && self.endpoint_id.is_none()
            && self.project_id.is_none()
            && self.warming_up.is_none()
    }
}

/// 把一份 [`UsrContext`] 注入到**当前** tracing span 的 OpenTelemetry attribute。
///
/// 用于已有显式 span 的热点路径（pageserver GetPage / WAL ingest / compaction，feat-008 §3.3(c)）。
/// 空上下文是 no-op（fail-safe，不 panic）。
pub fn record_usr(usr: &UsrContext) {
    if usr.is_empty() {
        return;
    }
    let span = tracing::Span::current();
    for kv in usr.as_key_values() {
        span.set_attribute(kv.key, kv.value);
    }
}

/// 把一组 USR `KeyValue`（如 `utils::usr::UsrAttributes::usr_attributes()` 的输出）注入当前 span。
///
/// 让 pageserver 侧可以直接传 `tenant_shard_id.usr_attributes()` 的结果，不必先构造 [`UsrContext`]。
pub fn record_usr_attributes(attrs: Vec<KeyValue>) {
    if attrs.is_empty() {
        return;
    }
    let span = tracing::Span::current();
    for kv in attrs {
        span.set_attribute(kv.key, kv.value);
    }
}

/// resolver closure 类型：每次新 span 创建时被调用，返回当下进程/请求维度的 USR 快照。
///
/// 典型实现：从 `ComputeSpec` / `ProxyConfig` / safekeeper `shard_map_cache` 取值（feat-009/010）。
/// 用 `Arc` snapshot 实现 < 100ns 的 hot-path 取值（feat-010 §5）。
pub type UsrResolver = Arc<dyn Fn() -> UsrContext + Send + Sync + 'static>;

/// 4 组件共用的 USR tracing layer。
///
/// 在每个新 span 创建时把 resolver 解析出的 USR 写入该 span 的 OpenTelemetry attribute
/// （通过 mutate `tracing_opentelemetry::OtelData` 的 `SpanBuilder`）。子 span 会继承 parent
/// 的 OTel context，因此 USR 自然 propagate。
///
/// **layer 注册顺序**：本 layer 的 `on_new_span` 依赖 `tracing-opentelemetry` 的
/// `OpenTelemetryLayer` 已把 [`tracing_opentelemetry::OtelData`] 放进 span extension，
/// 因此**必须在 `OpenTelemetryLayer` 之后**注册（即在 `Registry::with()` 链里排在它后面）。
pub struct UsrLayer {
    resolver: UsrResolver,
}

impl UsrLayer {
    pub fn new(resolver: UsrResolver) -> Self {
        Self { resolver }
    }
}

impl<S> Layer<S> for UsrLayer
where
    S: Subscriber + for<'span> LookupSpan<'span>,
{
    fn on_new_span(&self, _attrs: &span::Attributes<'_>, id: &span::Id, ctx: Context<'_, S>) {
        let usr = (self.resolver)();
        if usr.is_empty() {
            return;
        }
        let Some(span) = ctx.span(id) else {
            return;
        };
        let mut extensions = span.extensions_mut();
        // OpenTelemetryLayer 已在它自己的 on_new_span 里插入 OtelData；本 layer 排在其后，
        // 把 USR attribute 追加到同一个 SpanBuilder。
        if let Some(otel_data) = extensions.get_mut::<tracing_opentelemetry::OtelData>() {
            let kvs = usr.as_key_values();
            match otel_data.builder.attributes.as_mut() {
                Some(existing) => existing.extend(kvs),
                None => otel_data.builder.attributes = Some(kvs),
            }
        } else {
            // OtelData 缺失：通常是 layer 注册顺序错了（UsrLayer 排在 OpenTelemetryLayer 之前），
            // 或没启用 OTel 出口。此时 USR attribute 会被静默丢弃 —— 这是隐蔽的数据质量问题，
            // 必须给运行时可见性。用 Once 防止每个 span 都刷屏，只告警一次。
            static WARN_ONCE: Once = Once::new();
            WARN_ONCE.call_once(|| {
                tracing::warn!(
                    "USR layer 未取到 OtelData，USR attribute 未注入；\
                     请检查 layer 注册顺序（UsrLayer 必须排在 OpenTelemetryLayer 之后），\
                     或确认 OTel 出口已启用。此告警只打印一次。"
                );
            });
        }
    }
}

/// 构造一个 [`UsrLayer`]（feat-008 cornerstone 对外接口 · feat-009/010 调用方用）。
///
/// `resolver` 在每个新 span 上被调用，返回当前 USR 上下文快照。
pub fn usr_layer<F>(resolver: F) -> UsrLayer
where
    F: Fn() -> UsrContext + Send + Sync + 'static,
{
    UsrLayer::new(Arc::new(resolver))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// feat-039/#2: USR_LABEL_NAMES 包含 warming_up 第 8 维。
    #[test]
    fn usr_label_names_contains_warming_up() {
        assert!(USR_LABEL_NAMES.contains(&ATTR_WARMING_UP));
        // 命名 canonical: openneon.usr.* 前缀。
        for name in USR_LABEL_NAMES {
            assert!(
                name.starts_with("openneon.usr."),
                "label `{name}` 不符合 canonical 命名"
            );
        }
    }

    /// feat-039/#2: warming_up 字段不为 None 时正确序列化为 "true"/"false" string。
    #[test]
    fn usr_context_serializes_warming_up_as_string() {
        let ctx_warming = UsrContext {
            warming_up: Some(true),
            ..Default::default()
        };
        let kvs = ctx_warming.as_key_values();
        let warm_kv = kvs
            .iter()
            .find(|kv| kv.key.as_str() == ATTR_WARMING_UP)
            .expect("warming_up KV 必须出现");
        assert_eq!(warm_kv.value.as_str(), "true");

        let ctx_warm = UsrContext {
            warming_up: Some(false),
            ..Default::default()
        };
        let kvs = ctx_warm.as_key_values();
        let warm_kv = kvs
            .iter()
            .find(|kv| kv.key.as_str() == ATTR_WARMING_UP)
            .expect("warming_up=false KV 必须出现");
        assert_eq!(warm_kv.value.as_str(), "false");
    }

    /// feat-039/#2 OQ4: warming_up=None 时不注入 label (pageserver/safekeeper/proxy 走这条)。
    #[test]
    fn usr_context_omits_warming_up_when_none() {
        let ctx = UsrContext {
            tenant_id: Some("tenant_x".to_string()),
            warming_up: None,
            ..Default::default()
        };
        let kvs = ctx.as_key_values();
        assert!(
            !kvs.iter().any(|kv| kv.key.as_str() == ATTR_WARMING_UP),
            "warming_up=None 时不应注入 label"
        );
        // tenant_id 应仍正常注入。
        assert!(kvs.iter().any(|kv| kv.key.as_str() == ATTR_TENANT_ID));
    }

    /// feat-039/#2: warming_up=Some 单独存在时 is_empty=false。
    #[test]
    fn usr_context_warming_up_alone_not_empty() {
        let ctx = UsrContext {
            warming_up: Some(true),
            ..Default::default()
        };
        assert!(!ctx.is_empty());
    }

    /// 5 维全部 None 时 is_empty=true (无 warming_up tag)。
    #[test]
    fn usr_context_default_is_empty() {
        let ctx = UsrContext::default();
        assert!(ctx.is_empty());
        assert!(ctx.as_key_values().is_empty());
    }
}
