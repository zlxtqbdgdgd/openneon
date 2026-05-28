//! W3C TraceContext (`traceparent` v00) — Rust side, mirror of pgxn/neon/trace_context.{h,c}.
//!
//! 同名 C 库见 `pgxn/neon/trace_context.h`（feat-033 anchor 引入）；本模块只是 Rust
//! 侧的对应实现，**线协议字节级一致**，让 compute (C 端) 发出的 traceparent KV 能被
//! safekeeper / pageserver 原样解出来，再原样转发出去。
//!
//! 边界：本模块只处理 W3C §3.2 `traceparent`。配套的 `tracestate` (W3C §3.3) 是另一个
//! sibling KV，由调用方各自携带（一般作为命令额外的 `tracestate '...'` KV），不进
//! `TraceContext` struct。
//!
//! 使用场景（feat-035 三段串联）：
//! - 段 1 compute → SK: `START_WAL_PUSH (proto_version '4', traceparent '00-...')`,
//!   trace_id 源 = `PgBackendStatus.trace_context` (C 侧, walproposer 注入)；
//! - 段 2 SK → pageserver: `START_REPLICATION PHYSICAL X/X (traceparent '...')`,
//!   trace_id 源 = 当前 OTel span (上游 GetPage@LSN 触发的 catch-up 链路); 无父
//!   span 时 random_root + `tracestate=neon=root=pageserver-walreceiver`；
//! - 段 3 SK → SK recovery: `START_REPLICATION PHYSICAL X/X (term='N', traceparent '...',
//!   tracestate 'neon=root=safekeeper-recovery')`, 自生 trace_id。
//!
//! sampling 决策 **只透传不重新决策**（ADR-0010 Q3 head-based）。

use opentelemetry::trace::TraceContextExt;
use rand::RngCore;
use tracing_opentelemetry::OpenTelemetrySpanExt;

/// W3C v00 wire 字节长度："00-<32 hex>-<16 hex>-<2 hex>" = 55 bytes (不含 NUL)。
pub const WIRE_LEN: usize = 55;

/// W3C §3.2.2.5 trace_flags bits。
pub const FLAG_SAMPLED: u8 = 0x01;
pub const FLAG_RANDOM: u8 = 0x02;

/// W3C TraceContext v00 解码后。`trace_id` / `parent_id` 都按 big-endian (网络字节序 =
/// 16/8 进制顺序) 存放，对应 C 侧 struct trace_context 字段语义一致。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TraceContext {
    pub version: u8,
    pub trace_id: [u8; 16],
    pub parent_id: [u8; 8],
    pub trace_flags: u8,
}

/// 解析错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ParseError {
    #[error("traceparent length must be {WIRE_LEN} bytes, got {0}")]
    BadLength(usize),
    #[error("traceparent dash at wrong offset")]
    BadDelimiter,
    #[error("traceparent contains non-hex byte")]
    BadHex,
    #[error("traceparent trace_id is all zero (W3C §3.2.2.2)")]
    AllZeroTraceId,
    #[error("traceparent parent_id is all zero (W3C §3.2.2.3)")]
    AllZeroParentId,
    #[error("traceparent version is 0xff (W3C reserved invalid)")]
    InvalidVersion,
    #[error("traceparent version not 00 (use parse_lenient for forward-compat)")]
    NonV00,
}

fn hex_nibble(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(10 + (c - b'a')),
        b'A'..=b'F' => Some(10 + (c - b'A')),
        _ => None,
    }
}

fn hex_decode(src: &[u8], dst: &mut [u8]) -> Result<(), ParseError> {
    if src.len() != 2 * dst.len() {
        return Err(ParseError::BadHex);
    }
    for i in 0..dst.len() {
        let hi = hex_nibble(src[2 * i]).ok_or(ParseError::BadHex)?;
        let lo = hex_nibble(src[2 * i + 1]).ok_or(ParseError::BadHex)?;
        dst[i] = (hi << 4) | lo;
    }
    Ok(())
}

fn hex_encode(src: &[u8], dst: &mut [u8]) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for (i, b) in src.iter().enumerate() {
        dst[2 * i] = HEX[(b >> 4) as usize];
        dst[2 * i + 1] = HEX[(b & 0x0f) as usize];
    }
}

fn parse_common(input: &[u8]) -> Result<TraceContext, ParseError> {
    if input.len() != WIRE_LEN {
        return Err(ParseError::BadLength(input.len()));
    }
    // dashes at 2 / 35 / 52
    if input[2] != b'-' || input[35] != b'-' || input[52] != b'-' {
        return Err(ParseError::BadDelimiter);
    }
    let mut version = [0u8; 1];
    hex_decode(&input[0..2], &mut version)?;
    let mut trace_id = [0u8; 16];
    hex_decode(&input[3..35], &mut trace_id)?;
    let mut parent_id = [0u8; 8];
    hex_decode(&input[36..52], &mut parent_id)?;
    let mut flags = [0u8; 1];
    hex_decode(&input[53..55], &mut flags)?;

    if trace_id.iter().all(|&b| b == 0) {
        return Err(ParseError::AllZeroTraceId);
    }
    if parent_id.iter().all(|&b| b == 0) {
        return Err(ParseError::AllZeroParentId);
    }
    Ok(TraceContext {
        version: version[0],
        trace_id,
        parent_id,
        trace_flags: flags[0],
    })
}

impl TraceContext {
    /// 严格 v00 解析。
    pub fn parse(input: &str) -> Result<Self, ParseError> {
        let tc = parse_common(input.as_bytes())?;
        if tc.version != 0x00 {
            return Err(ParseError::NonV00);
        }
        Ok(tc)
    }

    /// W3C §3.2.2.3 forward-compat lenient 解析（接受 0x00..=0xfe，仍拒 0xff）。
    /// SK / pageserver 作为 forwarder 应当用 lenient。
    pub fn parse_lenient(input: &str) -> Result<Self, ParseError> {
        let tc = parse_common(input.as_bytes())?;
        if tc.version == 0xff {
            return Err(ParseError::InvalidVersion);
        }
        Ok(tc)
    }

    /// 序列化为 55 字节小写 hex。我们 (本仓 Rust 侧) **只发 v00**——若 in-memory version
    /// 不是 0x00 也强制按 v00 出，跟 C 侧 trace_context_serialize 契约一致。
    pub fn to_wire(&self) -> String {
        let mut buf = [0u8; WIRE_LEN];
        buf[0] = b'0';
        buf[1] = b'0';
        buf[2] = b'-';
        hex_encode(&self.trace_id, &mut buf[3..35]);
        buf[35] = b'-';
        hex_encode(&self.parent_id, &mut buf[36..52]);
        buf[52] = b'-';
        const HEX: &[u8; 16] = b"0123456789abcdef";
        buf[53] = HEX[(self.trace_flags >> 4) as usize];
        buf[54] = HEX[(self.trace_flags & 0x0f) as usize];
        // 全部是 ASCII hex / '-'，from_utf8 必然成功
        String::from_utf8(buf.to_vec()).expect("traceparent is ASCII")
    }

    /// 自生 root trace_id —— 给没有上游 trace 的场景用 (lazy catch-up / SK→SK recovery)。
    /// 同时打 RANDOM flag (W3C 2023+ bit 1) 表明 trace_id 是均匀随机的。
    pub fn random_root() -> Self {
        let mut trace_id = [0u8; 16];
        let mut parent_id = [0u8; 8];
        let mut rng = rand::rng();
        // 保证非全 0
        while trace_id.iter().all(|&b| b == 0) {
            rng.fill_bytes(&mut trace_id);
        }
        while parent_id.iter().all(|&b| b == 0) {
            rng.fill_bytes(&mut parent_id);
        }
        Self {
            version: 0x00,
            trace_id,
            parent_id,
            trace_flags: FLAG_SAMPLED | FLAG_RANDOM,
        }
    }

    /// 16 进制 trace_id（用作 tracing span field、log correlation）。
    pub fn trace_id_hex(&self) -> String {
        let mut buf = [0u8; 32];
        hex_encode(&self.trace_id, &mut buf);
        String::from_utf8(buf.to_vec()).expect("hex")
    }

    /// 16 进制 span_id (parent_id 在 W3C 里就是 caller 的 span_id)。
    pub fn span_id_hex(&self) -> String {
        let mut buf = [0u8; 16];
        hex_encode(&self.parent_id, &mut buf);
        String::from_utf8(buf.to_vec()).expect("hex")
    }

    /// 从当前 tracing span (经 `tracing-opentelemetry` 桥) 抓 trace_id + span_id。
    ///
    /// 用于段 2 (SK → pageserver) 出口：pageserver walreceiver 启动时若**真的**有
    /// 上游 GetPage@LSN 触发的 span（compute → pageserver feat-065 已经把 trace
    /// 接进来了），就把这个 trace 继续往 SK 透。无父 span 时返回 `None`，调用方
    /// 走 `random_root()` 自生根并附加 `tracestate=neon=root=pageserver-walreceiver`。
    pub fn from_current_span() -> Option<Self> {
        let span = tracing::Span::current();
        let ctx = span.context();
        let span_ref = ctx.span();
        let sc = span_ref.span_context();
        if !sc.is_valid() {
            return None;
        }
        let trace_id_bytes = sc.trace_id().to_bytes();
        let span_id_bytes = sc.span_id().to_bytes();
        if trace_id_bytes.iter().all(|&b| b == 0)
            || span_id_bytes.iter().all(|&b| b == 0)
        {
            return None;
        }
        let flags = if sc.is_sampled() { FLAG_SAMPLED } else { 0 };
        Some(Self {
            version: 0x00,
            trace_id: trace_id_bytes,
            parent_id: span_id_bytes,
            trace_flags: flags,
        })
    }

    /// 段 2 出口辅助：当前 span 有 trace 就承接，没有就自生 root。返回
    /// `(traceparent_wire, Option<tracestate>)` —— tracestate 仅在自生时设。
    ///
    /// `root_marker` 例如 `"neon=ps-walreceiver"` /
    /// `"neon=sk-recovery"`。
    pub fn current_or_root(root_marker: &str) -> (Self, Option<String>) {
        match Self::from_current_span() {
            Some(tc) => (tc, None),
            None => (Self::random_root(), Some(root_marker.to_string())),
        }
    }
}

/// W3C `tracestate` (§3.3) 是另一个 sibling header。Neon 自己往里写一个 `neon=root=...`
/// 标记上游来源 —— 段 2 (pageserver-walreceiver) / 段 3 (safekeeper-recovery) 这种
/// "本地自生 root" 的链路要打这个标，便于 trace UI 知道这是哪一段新生的根。
///
/// 当 SK 转发上游 (compute) 进来的 trace 时, root 字段不要覆盖, 整段 tracestate 原样
/// 透传 (forwarder 纪律, W3C §3.3.1 "MUST preserve").
pub const NEON_ROOT_PAGESERVER_WALRECEIVER: &str = "neon=ps-walreceiver";
pub const NEON_ROOT_SAFEKEEPER_RECOVERY: &str = "neon=sk-recovery";
pub const NEON_ROOT_WALPROPOSER: &str = "neon=walproposer";

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";

    #[test]
    fn parse_sample() {
        let tc = TraceContext::parse(SAMPLE).unwrap();
        assert_eq!(tc.version, 0);
        assert_eq!(tc.trace_flags, 0x01);
        assert_eq!(tc.trace_id_hex(), "4bf92f3577b34da6a3ce929d0e0e4736");
        assert_eq!(tc.span_id_hex(), "00f067aa0ba902b7");
        assert_eq!(tc.to_wire(), SAMPLE);
    }

    #[test]
    fn reject_bad_length() {
        assert!(matches!(
            TraceContext::parse("00-1234"),
            Err(ParseError::BadLength(_))
        ));
    }

    #[test]
    fn reject_all_zero_trace_id() {
        let v = "00-00000000000000000000000000000000-00f067aa0ba902b7-01";
        assert_eq!(TraceContext::parse(v), Err(ParseError::AllZeroTraceId));
    }

    #[test]
    fn reject_all_zero_parent_id() {
        let v = "00-4bf92f3577b34da6a3ce929d0e0e4736-0000000000000000-01";
        assert_eq!(TraceContext::parse(v), Err(ParseError::AllZeroParentId));
    }

    #[test]
    fn reject_bad_delimiter() {
        let v = "00x4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
        assert_eq!(TraceContext::parse(v), Err(ParseError::BadDelimiter));
    }

    #[test]
    fn strict_rejects_v01() {
        let v = "01-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
        assert_eq!(TraceContext::parse(v), Err(ParseError::NonV00));
    }

    #[test]
    fn lenient_accepts_v01_vfe_rejects_vff() {
        let v01 = "01-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
        let vfe = "fe-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
        let vff = "ff-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
        assert!(TraceContext::parse_lenient(v01).is_ok());
        assert!(TraceContext::parse_lenient(vfe).is_ok());
        assert_eq!(
            TraceContext::parse_lenient(vff),
            Err(ParseError::InvalidVersion)
        );
    }

    #[test]
    fn random_root_is_well_formed() {
        let tc = TraceContext::random_root();
        // 应该能 round-trip 解析回来
        let wire = tc.to_wire();
        let parsed = TraceContext::parse(&wire).unwrap();
        assert_eq!(parsed.trace_id, tc.trace_id);
        assert_eq!(parsed.parent_id, tc.parent_id);
        assert_eq!(parsed.trace_flags, FLAG_SAMPLED | FLAG_RANDOM);
        assert_eq!(parsed.version, 0);
    }

    #[test]
    fn random_root_unique() {
        let a = TraceContext::random_root();
        let b = TraceContext::random_root();
        assert_ne!(a.trace_id, b.trace_id);
    }

    #[test]
    fn case_insensitive_hex() {
        let upper = "00-4BF92F3577B34DA6A3CE929D0E0E4736-00F067AA0BA902B7-01";
        let lower = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
        let u = TraceContext::parse(upper).unwrap();
        let l = TraceContext::parse(lower).unwrap();
        assert_eq!(u, l);
    }

    #[test]
    fn current_or_root_self_generates_when_no_parent() {
        // 测试线程没有活跃 OTel span → 走 random_root 路径。
        let (tc, ts) = TraceContext::current_or_root(NEON_ROOT_PAGESERVER_WALRECEIVER);
        assert_eq!(
            ts.as_deref(),
            Some(NEON_ROOT_PAGESERVER_WALRECEIVER),
            "无父 span 必须打 root marker"
        );
        assert_eq!(tc.trace_flags & FLAG_RANDOM, FLAG_RANDOM);
        assert!(tc.trace_id.iter().any(|&b| b != 0));
    }
}
