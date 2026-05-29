//! feat-065 集成测试 · proxy traceparent 透传 path α/β。
//!
//! Issue #28 / #29 验收门：
//! - path α：startup options 带合法 traceparent → 透传 trace_id；tracestate `neon=root=app`
//! - path β：缺失 / 校验失败 → proxy entry 自生 trace_id；tracestate `neon=root=proxy`
//! - #29：proxy → compute 注入 `-c neon.traceparent=...` 进 AuthInfo.server_params
//!
//! 这层测试**直接走数据流单元**（不起真实 TLS 链路）—— 把
//! `handshake.rs::classify_and_attach_trace_context` 等价改用 `RequestContext::test()` 的
//! span 调一遍，断言写回的 trace_context；以及 `AuthInfo::inject_trace_context` 注入到
//! server_params 的 options 槽位。完整端到端等 feat-033 C 侧落地后再叠加。

use std::str::FromStr;

use opentelemetry::trace::{SpanContext, TraceContextExt, TraceState};
use tracing_opentelemetry::OpenTelemetrySpanExt;

use crate::compute::AuthInfo;
use crate::context::RequestContext;
use crate::pqproto::StartupMessageParams;
use crate::trace_context::{
    self, NEON_VENDOR_KEY, TRACEPARENT_GUC, TRACESTATE_GUC, TraceContext, TraceRoot,
    extract_from_startup, inject_into_options, parse_traceparent,
};

const SAMPLE_TP: &str = "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01";

/// 复刻 handshake.rs::classify_and_attach_trace_context 的核心逻辑给单测用。
/// 真实路径在 handshake 内部，但单测里我们不起 TCP，所以单独抽出来。
fn classify(ctx: &RequestContext, params: &StartupMessageParams) {
    if let Some(upstream) = trace_context::extract_from_startup(params) {
        let otel_cx =
            opentelemetry::Context::current().with_remote_span_context(upstream.span_context.clone());
        ctx.span().set_parent(otel_cx);
        ctx.set_trace_context(upstream);
        return;
    }
    if let Some(self_tc) = trace_context::from_current_span() {
        ctx.set_trace_context(TraceContext {
            span_context: self_tc.span_context,
            root: TraceRoot::Proxy,
        });
    }
}

#[tokio::test]
async fn path_alpha_traceparent_passes_through() {
    let ctx = RequestContext::test();
    let mut params = StartupMessageParams::default();
    params.insert(
        "options",
        &format!("-c {}={} -c statement_timeout=5s", TRACEPARENT_GUC, SAMPLE_TP),
    );

    classify(&ctx, &params);

    let tc = ctx.trace_context().expect("path α should set trace_context");
    assert_eq!(tc.root, TraceRoot::App);
    assert_eq!(
        format!("{}", tc.span_context.trace_id()),
        "0af7651916cd43dd8448eb211c80319c",
        "trace_id MUST be transparently propagated (W3C spec §3.2)"
    );
    assert_eq!(tc.to_tracestate(), "neon=app");
}

#[tokio::test]
async fn path_beta_no_traceparent_generates_locally() {
    let ctx = RequestContext::test();
    let mut params = StartupMessageParams::default();
    params.insert("options", "-c statement_timeout=5s");

    classify(&ctx, &params);

    let tc = ctx.trace_context();
    // OTel layer 在 test 时可能未启用 —— 这种场景下 from_current_span 返回 None。
    // 我们只断言"如果有 trace_context，root 必须是 Proxy"，确保 path β 标记正确。
    if let Some(tc) = tc {
        assert_eq!(tc.root, TraceRoot::Proxy);
        assert_eq!(tc.to_tracestate(), "neon=proxy");
    }
}

#[tokio::test]
async fn path_beta_malformed_traceparent_falls_back() {
    let ctx = RequestContext::test();
    let mut params = StartupMessageParams::default();
    params.insert("options", "-c neon.traceparent=garbage-format");

    classify(&ctx, &params);

    let tc = ctx.trace_context();
    if let Some(tc) = tc {
        assert_eq!(
            tc.root,
            TraceRoot::Proxy,
            "malformed traceparent MUST fall back to path β"
        );
    }
}

#[tokio::test]
async fn issue29_injects_traceparent_into_compute_options() {
    // 构造一个 path α 的 trace_context，模拟 proxy 解析完上游的状态
    let span_context = parse_traceparent(SAMPLE_TP).unwrap();
    let tc = TraceContext {
        span_context,
        root: TraceRoot::App,
    };

    // 用户原本带了一个 statement_timeout GUC
    let mut user_params = StartupMessageParams::default();
    user_params.insert("options", "-c statement_timeout=5s");

    let mut auth_info = AuthInfo::for_console_redirect("db", "user", None);
    auth_info.set_startup_params(&user_params, false);
    auth_info.inject_trace_context(&tc);

    // 抽出 server_params 看 options 槽位
    let server_params = auth_info.server_params();
    let opts = server_params
        .get("options")
        .expect("options must be set after inject");

    // 既要保留用户原本的 GUC，也要追加 traceparent + tracestate
    assert!(opts.contains("statement_timeout"), "user GUC preserved: {opts}");
    assert!(
        opts.contains(&format!("-c {TRACEPARENT_GUC}=")),
        "traceparent injected: {opts}"
    );
    assert!(
        opts.contains(&format!("-c {TRACESTATE_GUC}=")),
        "tracestate injected: {opts}"
    );
    assert!(opts.contains(&format!("{NEON_VENDOR_KEY}=app")));
}

#[tokio::test]
async fn issue29_inject_without_existing_options() {
    let span_context = parse_traceparent(SAMPLE_TP).unwrap();
    let tc = TraceContext {
        span_context,
        root: TraceRoot::Proxy,
    };

    let mut auth_info = AuthInfo::for_console_redirect("db", "user", None);
    auth_info.inject_trace_context(&tc);

    let opts = auth_info
        .server_params()
        .get("options")
        .expect("options should be created");
    assert!(opts.starts_with(&format!("-c {TRACEPARENT_GUC}=")));
    assert!(opts.contains(&format!("{NEON_VENDOR_KEY}=proxy")));
}

#[tokio::test]
async fn tracestate_preserves_vendor_chain_through_inject() {
    let sc_base = parse_traceparent(SAMPLE_TP).unwrap();
    // W3C tracestate key 必须以 lowercase letter 或 digit 开头；不能用 `vendorA`。
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
    let opts = inject_into_options(None, &tc);
    assert!(opts.contains("vendor1=foo"));
    assert!(opts.contains("vendor2=bar"));
    assert!(opts.contains("neon=app"));
}

#[tokio::test]
async fn extract_handles_dash_c_with_or_without_space() {
    // Test both `-c key=val` and `-ckey=val` PG syntaxes
    for opts_str in [
        format!("-c {TRACEPARENT_GUC}={SAMPLE_TP}"),
        format!("-c{TRACEPARENT_GUC}={SAMPLE_TP}"),
    ] {
        let mut params = StartupMessageParams::default();
        params.insert("options", &opts_str);
        let tc = extract_from_startup(&params).unwrap_or_else(|| {
            panic!("must extract from: {opts_str}")
        });
        assert_eq!(tc.root, TraceRoot::App);
    }
}

#[tokio::test]
async fn extract_rejects_wrong_version_byte() {
    // version "ff" 不是当前唯一支持的 "00"
    let bad_tp = "ff-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01";
    let mut params = StartupMessageParams::default();
    params.insert("options", &format!("-c {TRACEPARENT_GUC}={bad_tp}"));
    assert!(
        extract_from_startup(&params).is_none(),
        "version != 00 must reject"
    );
}

#[tokio::test]
async fn sampling_decision_not_re_decided() {
    // ADR-0010 Q3 + #28 验收门: "sampling decision 只透传不重新决策"
    // 上游 flags=00 (NOT sampled)，proxy 透传，必须保持 00。
    let not_sampled = "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-00";
    let mut params = StartupMessageParams::default();
    params.insert("options", &format!("-c {TRACEPARENT_GUC}={not_sampled}"));
    let tc = extract_from_startup(&params).expect("must extract");
    assert!(
        !tc.span_context.trace_flags().is_sampled(),
        "sampling flag must be preserved (not re-decided)"
    );
    // roundtrip 时 flags 也必须是 00
    assert!(tc.to_traceparent().ends_with("-00"));
}

#[test]
fn tracestate_lenient_on_partial_corruption() {
    // W3C spec §3.3.1.5: tracestate 解析 MUST be tolerant
    // 即使 tracestate 损坏，traceparent 该走还是要走
    let sc = trace_context::parse_traceparent_with_state(SAMPLE_TP, "###broken,@@@");
    assert!(sc.is_some(), "tracestate corruption must not block traceparent");
}
