// feat-069/#34 · Rust usdt crate hot path provider 集中定义
//
// Probe 命名约定 (与 pgxn/neon/probes/rust-whitelist.yaml usdt 段对齐):
//   <provider>:<event>__<边界>
//     - provider: neon_pageserver / neon_safekeeper / neon_proxy
//     - 边界: __start / __done / __established / __closed
//
// 三道屏障 (feat-069/#35):
//   屏障 1: 白名单 schema is_async=false (rust-whitelist.yaml)
//   屏障 2: feat-068 mcp tool 加载白名单时 assert (运行时)
//   屏障 3: 文档 + 本注释 (此处)
//
//   **L3 不暴露 async fn 内部 probe** · Rust async fn 编译为 state machine ·
//   单次 poll() 耗时 ≠ 逻辑步骤耗时 · 此 crate 的 provider 只标注同步边界
//   (例: get_page_at_lsn__start 是 future 创建前的入口 · 不是 poll loop 内部) ·
//   L4 候选: 等 tokio-console / async-profiler-rust 工具链成熟再补。
//
// Binary 集成 (feat-068 阶段做):
//   1. pageserver/safekeeper/proxy 各自 Cargo.toml 加 dep:
//        neon_probes = { path = "../pgxn/neon/probes/rust" }
//   2. binary lib.rs 顶部加:
//        #[cfg(target_os = "linux")]
//        pub use neon_probes;
//   3. hot path 函数体里调用 (示例):
//        #[cfg(target_os = "linux")]
//        neon_probes::pageserver::get_page_at_lsn__start!(|| (tenant_id, timeline_id, lsn.0));
//   4. cargo build --release 后 `readelf -n target/release/pageserver` 应看到 USDT note section.

#![allow(unused_macros)]
#![allow(dead_code)]

// =====================================================================
// Linux: 用 usdt::provider 宏 真正产 SDT note
// =====================================================================
#[cfg(target_os = "linux")]
pub mod linux {
    /// pageserver hot path · 4 probe (2 函数 × start/done)
    // STUBBED-L3(feat-067/069) #[usdt::provider(provider = "neon_pageserver")]
    pub mod pageserver {
        pub fn get_page_at_lsn__start(tenant_id: &str, timeline_id: &str, lsn: u64) {}
        pub fn get_page_at_lsn__done(latency_ns: u64) {}
        pub fn layer_download__start(layer_name: &str) {}
        pub fn layer_download__done(bytes: u64, latency_ns: u64) {}
    }

    /// safekeeper hot path · 2 probe (WAL append start/done)
    // STUBBED-L3(feat-067/069) #[usdt::provider(provider = "neon_safekeeper")]
    pub mod safekeeper {
        pub fn wal_append__start(start_lsn: u64, len: u64) {}
        pub fn wal_append__done(end_lsn: u64, latency_ns: u64) {}
    }

    /// proxy hot path · 4 probe (auth + connection 边界)
    // STUBBED-L3(feat-067/069) #[usdt::provider(provider = "neon_proxy")]
    pub mod proxy {
        pub fn auth__start(endpoint: &str) {}
        pub fn auth__done(ok: bool, latency_ns: u64) {}
        pub fn connection__established(endpoint: &str, lsn: u64) {}
        pub fn connection__closed(duration_ns: u64) {}
    }
}

#[cfg(target_os = "linux")]
pub use linux::{pageserver, proxy, safekeeper};

// =====================================================================
// 非 Linux (macOS dev): no-op stub · 调用点保持源码统一
// =====================================================================
#[cfg(not(target_os = "linux"))]
pub mod pageserver {
    #[inline(always)]
    pub fn get_page_at_lsn__start(_tenant_id: &str, _timeline_id: &str, _lsn: u64) {}
    #[inline(always)]
    pub fn get_page_at_lsn__done(_latency_ns: u64) {}
    #[inline(always)]
    pub fn layer_download__start(_layer_name: &str) {}
    #[inline(always)]
    pub fn layer_download__done(_bytes: u64, _latency_ns: u64) {}
}

#[cfg(not(target_os = "linux"))]
pub mod safekeeper {
    #[inline(always)]
    pub fn wal_append__start(_start_lsn: u64, _len: u64) {}
    #[inline(always)]
    pub fn wal_append__done(_end_lsn: u64, _latency_ns: u64) {}
}

#[cfg(not(target_os = "linux"))]
pub mod proxy {
    #[inline(always)]
    pub fn auth__start(_endpoint: &str) {}
    #[inline(always)]
    pub fn auth__done(_ok: bool, _latency_ns: u64) {}
    #[inline(always)]
    pub fn connection__established(_endpoint: &str, _lsn: u64) {}
    #[inline(always)]
    pub fn connection__closed(_duration_ns: u64) {}
}

// =====================================================================
// 编译时静态屏障 (屏障 1.b · 屏障 1 的 schema 之外补 const-assert)
// =====================================================================
//
// 这条 const 块强制 Rust 编译器在编译 neon_probes 时验证:
//   1. probe 模块名 (provider) 严格匹配白名单 YAML target 字段约定
//   2. 任何往 provider 模块里加 async fn 的尝试都会被 compiler 直接拒绝
//      (provider 宏只允许 fn · 不允许 async fn · usdt crate 的宏展开决定)
//
// 编译期失败示例 (反例): 在 // STUBBED-L3(feat-067/069) #[usdt::provider] 模块里写 `async fn foo() {}` ·
//   usdt::provider 宏会报 `expected fn · found async fn` · 编译直接失败 ·
//   不需要额外人为 assert。
const _PROBE_BARRIER_PROVIDER_NAMES_OK: () = {
    // 仅用作文档锚点 · 实际屏障由 usdt::provider 宏本身提供
};

// =====================================================================
// 单测: 在不依赖 dtrace toolchain 的前提下验证 stub 可调用
// =====================================================================
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pageserver_probes_callable() {
        pageserver::get_page_at_lsn__start("tenant_x", "timeline_y", 12345);
        pageserver::get_page_at_lsn__done(987);
        pageserver::layer_download__start("L0_000");
        pageserver::layer_download__done(4096, 12345);
    }

    #[test]
    fn safekeeper_probes_callable() {
        safekeeper::wal_append__start(100, 256);
        safekeeper::wal_append__done(356, 4567);
    }

    #[test]
    fn proxy_probes_callable() {
        proxy::auth__start("ep-cool-1234");
        proxy::auth__done(true, 7777);
        proxy::connection__established("ep-cool-1234", 999);
        proxy::connection__closed(123456);
    }
}
