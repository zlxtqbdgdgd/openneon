//! W3C TraceContext (traceparent / tracestate) 解析、生成、透传——proxy 侧 Rust 实现。
//!
//! 落到 feat-065：proxy 是流量必经路径。客户端带 `traceparent` 时走 path α（透传，
//! 不重新决策采样，spec §3.2），不带时走 path β（proxy entry 自生 trace_id，ADR-0011
//! ODD 承诺）。`tracestate` 里加 `neon=root=app|proxy` 区分起源。
//!
//! 这个模块**只做数据**：解析 / 生成 / 校验 / tracestate 标记。把它注入到 Postgres
//! startup options 槽位（`-c neon.traceparent=...`）由 `proxy/mod.rs` 调用；
//! 跟 `tracing::Span` / OTel SDK 的接驳由 `pglb/handshake.rs` 调用。

use std::str::FromStr;

use opentelemetry::trace::{SpanContext, SpanId, TraceFlags, TraceId, TraceState};

use crate::pqproto::StartupMessageParams;

/// W3C TraceContext 版本号 (`00` 为当前唯一版本)。
const TRACEPARENT_VERSION: &str = "00";

/// Postgres startup options 槽位用的 GUC 名 —— proxy → compute 透传 trace_id 用。
/// 跟 feat-033 C 侧 `PgBackendStatus.trace_context` schema 对齐。
pub const TRACEPARENT_GUC: &str = "neon.traceparent";

/// 与 traceparent 配套的 tracestate GUC。
pub const TRACESTATE_GUC: &str = "neon.tracestate";

/// vendor 标记 key：W3C tracestate vendor name。
///
/// 完整 entry 形如 `neon=app` (path α) / `neon=proxy` (path β)。
///
/// 注意：之前考虑过 `neon=root=app` 这种嵌套 key=value，但 OTel TraceState 严格
/// 校验 value 不能含 `=` / `,`（W3C spec §3.3.1.4 value grammar），所以最终用
/// 单字 value `app` / `proxy` 直接表达起源。
pub const NEON_VENDOR_KEY: &str = "neon";

/// trace 起源 —— path α/β 分流的判据 (issue #28)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TraceRoot {
    /// path α：上游 app 已带 traceparent，proxy 只透传。
    App,
    /// path β：proxy entry 自生 trace_id（ADR-0011 ODD 承诺）。
    Proxy,
}

impl TraceRoot {
    pub fn as_neon_value(self) -> &'static str {
        match self {
            TraceRoot::App => "app",
            TraceRoot::Proxy => "proxy",
        }
    }
}

/// 解析出的 traceparent + tracestate + 起源。
#[derive(Debug, Clone)]
pub struct TraceContext {
    pub span_context: SpanContext,
    pub root: TraceRoot,
}

impl TraceContext {
    /// 序列化成 W3C traceparent header 字符串：`00-<trace_id>-<span_id>-<flags>`。
    pub fn to_traceparent(&self) -> String {
        format!(
            "{}-{}-{}-{:02x}",
            TRACEPARENT_VERSION,
            self.span_context.trace_id(),
            self.span_context.span_id(),
            self.span_context.trace_flags().to_u8(),
        )
    }

    /// 序列化成 tracestate header 字符串。永远把 `neon=app|proxy` 放第一位，
    /// 其他 vendor 追加在后（按 W3C spec §3.3.1.3 "新条目放最前"）。
    pub fn to_tracestate(&self) -> String {
        let neon_entry = format!("{}={}", NEON_VENDOR_KEY, self.root.as_neon_value());
        let upstream = self.span_context.trace_state().header();
        if upstream.is_empty() {
            neon_entry
        } else {
            // 移除上游里可能已经存在的 neon= entry，避免重复。
            let filtered: Vec<&str> = upstream
                .split(',')
                .map(str::trim)
                .filter(|e| !e.is_empty() && !e.starts_with(&format!("{NEON_VENDOR_KEY}=")))
                .collect();
            if filtered.is_empty() {
                neon_entry
            } else {
                format!("{},{}", neon_entry, filtered.join(","))
            }
        }
    }
}

/// 严格按 W3C TraceContext spec §3.2 校验并解析 traceparent。
///
/// 格式 (ASCII)：`<2hex version>-<32hex trace_id>-<16hex span_id>-<2hex flags>`。
/// 长度固定 55 字节。trace_id / span_id 不能全 0。version 必须是 `00`。
///
/// 返回 None 表示校验失败 —— 调用方应当走 path β 自生（issue #28 验收门 "校验失败 → path β"）。
///
/// `parse_traceparent_with_state(s, "")` 的薄包装；tests 与未来可能的 HTTP / WebSocket 入口共用。
#[cfg(test)]
pub fn parse_traceparent(s: &str) -> Option<SpanContext> {
    parse_traceparent_with_state(s, "")
}

/// 同 [`parse_traceparent`] 但带 tracestate 一起解析。tracestate 解析失败不影响 traceparent
/// 成立（W3C spec §3.3.1.5 "tracestate MUST be tolerant"）。
pub fn parse_traceparent_with_state(traceparent: &str, tracestate: &str) -> Option<SpanContext> {
    // spec §3.2.2.1: 严格长度 55 字节
    if traceparent.len() != 55 {
        return None;
    }
    let parts: Vec<&str> = traceparent.split('-').collect();
    if parts.len() != 4 {
        return None;
    }
    let (version, trace_id_hex, span_id_hex, flags_hex) =
        (parts[0], parts[1], parts[2], parts[3]);

    if version.len() != 2 || trace_id_hex.len() != 32 || span_id_hex.len() != 16 || flags_hex.len() != 2 {
        return None;
    }
    // version 必须是 "00" (spec §3.2.2.2 forwards-compat: 未来版本暂时拒绝)
    if version != TRACEPARENT_VERSION {
        return None;
    }
    // 全部必须是合法 hex
    if !trace_id_hex.bytes().all(|b| b.is_ascii_hexdigit())
        || !span_id_hex.bytes().all(|b| b.is_ascii_hexdigit())
        || !flags_hex.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return None;
    }
    // trace_id / span_id 全 0 是非法的 (spec §3.2.2.3 "MUST be discarded")
    if trace_id_hex == "00000000000000000000000000000000" {
        return None;
    }
    if span_id_hex == "0000000000000000" {
        return None;
    }

    let trace_id = TraceId::from_hex(trace_id_hex).ok()?;
    let span_id = SpanId::from_hex(span_id_hex).ok()?;
    let flags_u8 = u8::from_str_radix(flags_hex, 16).ok()?;
    let trace_flags = TraceFlags::new(flags_u8);

    let trace_state = TraceState::from_str(tracestate).unwrap_or_default();

    // proxy 是远端 (proxy 接到的 traceparent 来自上游)，因此 is_remote=true
    Some(SpanContext::new(
        trace_id,
        span_id,
        trace_flags,
        true,
        trace_state,
    ))
}

/// 从 Postgres startup packet params 抽出 traceparent —— path α 入口。
///
/// 优先级：
/// 1. `options=-c neon.traceparent=...`（feat-065 §3 选定 schema，跟 feat-033 GUC 同名）
/// 2. 退化看 `_pq_.neon_traceparent`（PG 协议扩展槽位，未来兼容）
///
/// 解析成功 → path α；未找到 / 校验失败 → 调用方走 path β。
pub fn extract_from_startup(params: &StartupMessageParams) -> Option<TraceContext> {
    let mut traceparent: Option<String> = None;
    let mut tracestate: Option<String> = None;

    // 1. options 槽位
    if let Some(options) = params.options_raw() {
        let opts: Vec<&str> = options.collect();
        let mut i = 0;
        while i < opts.len() {
            // 处理 "-c name=value" / "-cname=value"
            let candidate: Option<&str> = if opts[i] == "-c" {
                i += 1;
                opts.get(i).copied()
            } else if let Some(rest) = opts[i].strip_prefix("-c") {
                Some(rest)
            } else {
                None
            };
            if let Some(kv) = candidate
                && let Some((k, v)) = kv.split_once('=')
            {
                match k {
                    TRACEPARENT_GUC => traceparent = Some(v.to_string()),
                    TRACESTATE_GUC => tracestate = Some(v.to_string()),
                    _ => {}
                }
            }
            i += 1;
        }
    }

    // 2. 直接 startup param（个别 driver 可能 bypass options）
    if traceparent.is_none()
        && let Some(v) = params.get(TRACEPARENT_GUC)
    {
        traceparent = Some(v.to_string());
    }
    if tracestate.is_none()
        && let Some(v) = params.get(TRACESTATE_GUC)
    {
        tracestate = Some(v.to_string());
    }

    let tp = traceparent?;
    let ts = tracestate.unwrap_or_default();
    let span_context = parse_traceparent_with_state(&tp, &ts)?;
    Some(TraceContext {
        span_context,
        root: TraceRoot::App,
    })
}

/// 从当前 tracing span 派生一个新的 [`TraceContext`]（path β 入口）—— proxy entry 自生。
///
/// 用法：在 handshake.rs accept TCP 后立即调用；返回的 trace_id 就是 proxy span 的 trace_id。
/// 通过 `tracing::Span::current()` 把 tracing-opentelemetry layer 已经分配的 span_context
/// 抽出来，把 root 标成 `proxy`。
pub fn from_current_span() -> Option<TraceContext> {
    use opentelemetry::trace::TraceContextExt;
    use tracing_opentelemetry::OpenTelemetrySpanExt;

    let otel_context = tracing::Span::current().context();
    let span_ref = otel_context.span();
    let sc = span_ref.span_context();
    if !sc.is_valid() {
        return None;
    }
    Some(TraceContext {
        span_context: sc.clone(),
        root: TraceRoot::Proxy,
    })
}

/// 给 startup options 注入 `-c neon.traceparent=... -c neon.tracestate=...`，
/// **追加**到既有 `options` 后面（不破坏原 client 传上来的 GUC，spec friendly）。
///
/// 这是 proxy → compute 的 GUC 注入 (issue #29)。返回 (new_options_value, applied)。
pub fn inject_into_options(existing: Option<&str>, tc: &TraceContext) -> String {
    let injected = format!(
        "-c {}={} -c {}={}",
        TRACEPARENT_GUC,
        tc.to_traceparent(),
        TRACESTATE_GUC,
        tc.to_tracestate(),
    );
    match existing {
        Some(prev) if !prev.is_empty() => format!("{prev} {injected}"),
        _ => injected,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_tp() -> &'static str {
        "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01"
    }

    #[test]
    fn parse_valid_traceparent() {
        let sc = parse_traceparent(sample_tp()).expect("should parse");
        assert_eq!(format!("{}", sc.trace_id()), "0af7651916cd43dd8448eb211c80319c");
        assert_eq!(format!("{}", sc.span_id()), "b7ad6b7169203331");
        assert!(sc.trace_flags().is_sampled());
        assert!(sc.is_remote());
    }

    #[test]
    fn reject_wrong_length() {
        assert!(parse_traceparent("00-tooshort").is_none());
        assert!(parse_traceparent(&format!("{}-extra", sample_tp())).is_none());
    }

    #[test]
    fn reject_unsupported_version() {
        let s = "ff-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01";
        assert!(parse_traceparent(s).is_none());
    }

    #[test]
    fn reject_all_zero_trace_id() {
        let s = "00-00000000000000000000000000000000-b7ad6b7169203331-01";
        assert!(parse_traceparent(s).is_none());
    }

    #[test]
    fn reject_all_zero_span_id() {
        let s = "00-0af7651916cd43dd8448eb211c80319c-0000000000000000-01";
        assert!(parse_traceparent(s).is_none());
    }

    #[test]
    fn reject_non_hex() {
        let s = "00-zaf7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01";
        assert!(parse_traceparent(s).is_none());
    }

    #[test]
    fn roundtrip_traceparent_format() {
        let sc = parse_traceparent(sample_tp()).unwrap();
        let tc = TraceContext {
            span_context: sc,
            root: TraceRoot::App,
        };
        assert_eq!(tc.to_traceparent(), sample_tp());
    }

    #[test]
    fn tracestate_app_root() {
        let sc = parse_traceparent(sample_tp()).unwrap();
        let tc = TraceContext {
            span_context: sc,
            root: TraceRoot::App,
        };
        assert_eq!(tc.to_tracestate(), "neon=app");
    }

    #[test]
    fn tracestate_proxy_root() {
        let sc = parse_traceparent(sample_tp()).unwrap();
        let tc = TraceContext {
            span_context: sc,
            root: TraceRoot::Proxy,
        };
        assert_eq!(tc.to_tracestate(), "neon=proxy");
    }

    #[test]
    fn tracestate_preserves_upstream_vendors() {
        let sc_base = parse_traceparent(sample_tp()).unwrap();
        // W3C tracestate value 不能含 '=' 或 ','，所以用合法的 vendor=value 形式
        let upstream_ts = TraceState::from_str("vendor1=foo,vendor2=bar").unwrap();
        let sc = SpanContext::new(
            sc_base.trace_id(),
            sc_base.span_id(),
            sc_base.trace_flags(),
            true,
            upstream_ts,
        );
        let tc = TraceContext {
            span_context: sc,
            root: TraceRoot::App,
        };
        let ts = tc.to_tracestate();
        assert!(ts.starts_with("neon=app,"), "neon entry must come first: {ts}");
        assert!(ts.contains("vendor1=foo"));
        assert!(ts.contains("vendor2=bar"));
    }

    #[test]
    fn tracestate_replaces_existing_neon_entry() {
        // 上游错误地塞了一个 neon= entry —— proxy 必须用自己的覆盖掉
        let sc_base = parse_traceparent(sample_tp()).unwrap();
        let upstream_ts = TraceState::from_str("neon=evil,other=ok").unwrap();
        let sc = SpanContext::new(
            sc_base.trace_id(),
            sc_base.span_id(),
            sc_base.trace_flags(),
            true,
            upstream_ts,
        );
        let tc = TraceContext {
            span_context: sc,
            root: TraceRoot::App,
        };
        let ts = tc.to_tracestate();
        assert!(ts.contains("neon=app"));
        assert!(!ts.contains("evil"));
        assert!(ts.contains("other=ok"));
    }

    #[test]
    fn extract_from_startup_options_dash_c_split() {
        let mut params = StartupMessageParams::default();
        params.insert(
            "options",
            "-c neon.traceparent=00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01 -c statement_timeout=5s",
        );
        let tc = extract_from_startup(&params).expect("should extract path α");
        assert_eq!(tc.root, TraceRoot::App);
        assert_eq!(
            format!("{}", tc.span_context.trace_id()),
            "0af7651916cd43dd8448eb211c80319c"
        );
    }

    #[test]
    fn extract_from_startup_options_dash_c_joined() {
        // -cname=value 不带空格也是 PG 合法格式
        let mut params = StartupMessageParams::default();
        params.insert(
            "options",
            "-cneon.traceparent=00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01",
        );
        let tc = extract_from_startup(&params).expect("should extract path α");
        assert_eq!(tc.root, TraceRoot::App);
    }

    #[test]
    fn extract_from_startup_with_tracestate() {
        let mut params = StartupMessageParams::default();
        params.insert(
            "options",
            "-c neon.traceparent=00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01 -c neon.tracestate=app=upstream",
        );
        let tc = extract_from_startup(&params).expect("should extract path α");
        assert_eq!(tc.span_context.trace_state().get("app"), Some("upstream"));
    }

    #[test]
    fn extract_from_startup_missing_returns_none() {
        let mut params = StartupMessageParams::default();
        params.insert("options", "-c statement_timeout=5s");
        assert!(extract_from_startup(&params).is_none());
    }

    #[test]
    fn extract_from_startup_malformed_traceparent_returns_none() {
        // 注意：这里直接断言 None —— 让调用方 fallback 到 path β
        let mut params = StartupMessageParams::default();
        params.insert("options", "-c neon.traceparent=garbage");
        assert!(extract_from_startup(&params).is_none());
    }

    #[test]
    fn inject_into_options_empty() {
        let sc = parse_traceparent(sample_tp()).unwrap();
        let tc = TraceContext {
            span_context: sc,
            root: TraceRoot::Proxy,
        };
        let out = inject_into_options(None, &tc);
        assert!(out.contains("-c neon.traceparent="));
        assert!(out.contains("-c neon.tracestate=neon=proxy"));
    }

    #[test]
    fn inject_into_options_appends_to_existing() {
        let sc = parse_traceparent(sample_tp()).unwrap();
        let tc = TraceContext {
            span_context: sc,
            root: TraceRoot::App,
        };
        let out = inject_into_options(Some("-c statement_timeout=5s"), &tc);
        assert!(out.starts_with("-c statement_timeout=5s "));
        assert!(out.contains("-c neon.traceparent="));
    }

    #[test]
    fn tracestate_lenient_on_malformed() {
        // tracestate 解析失败也不应阻塞 traceparent
        let sc = parse_traceparent_with_state(sample_tp(), "###bad###");
        assert!(sc.is_some());
    }
}
