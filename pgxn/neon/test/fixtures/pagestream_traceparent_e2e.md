# feat-033 · pagestream traceparent e2e fixture (8 cases)

compute → pageserver 的 libpq pagestream（getpage 等）携带 W3C TraceContext `traceparent`，让 agent 跨进程 trace 看到 `GetPage@LSN` span 关联 client query 的 trace_id。

## 数据流闭环
client query（SQLCommenter `/*traceparent='...'*/`）→ `neon_post_parse_analyze`（neon.c）→ `sqlcommenter_extract_traceparent` → `neon_trace_status_set`（per-backend shmem + backend-local 快缓存）→ getpage 时 `neon_trace_status_get_my`（libpagestore.c send chokepoint）→ `NeonRequest.has_trace_context/trace_context` → `nm_pack_request` v4 序列化（`trace_present` flag + 55B traceparent）→ pageserver `fe_msg_trace_context` → `info_span!("pagestream_request_with_traceparent", otel.trace_id=...)`。

## 复现验证方法
1. cluster 起 v4 pagestream（`SHOW neon.protocol_version` = 4）。
2. 关 LFC 强制 getpage：endpoint `postgresql.conf` 加 `neon.file_cache_size_limit=0`（POSTMASTER GUC）+ 重启 endpoint。
3. pageserver 开 trace 级 log：`RUST_LOG="info,pageserver::page_service=trace" neon_local start`。
4. 建大表（> shared_buffers 1MB · 如 30000 行 × 100B pad）+ CHECKPOINT + 重启 endpoint（清 shared_buffers）。
5. cold 查询带已知 traceparent，grep pageserver log 的 raw query 字节看 `trace_present` flag + trace_id。

## 8 cases

| # | 场景 | SQL / 输入 | 期望 |
|---|---|---|---|
| 1 | v4 client 发 GetPage 带合法 traceparent | `SELECT ... /*traceparent='00-{32hex}-{16hex}-01'*/`（cold · LFC off） | getpage wire `trace_present=0x01` + 55B traceparent = client trace_id；pageserver `pagestream_request_with_traceparent{otel.trace_id={32hex}}` |
| 2 | client query 无 traceparent | `SELECT ...`（无 SQLCommenter） | `neon_trace_status_get_my` 返 false → `has_trace_context=0` → wire `trace_present=0x00`（v3 兼容行为 · 不带 trace） |
| 3 | v4 client GetPage traceparent version=99（未来版本） | `/*traceparent='99-{32hex}-{16hex}-01'*/` | pageserver lenient parse 跳过 + log warn · 请求正常处理（不挂） |
| 4 | trace_id 全 0（W3C 禁止） | `/*traceparent='00-{32×0}-{16hex}-01'*/` | extract 侧拒（all-zero guard）· `has_trace_context` 不置 → 不带 trace |
| 5 | parent_id 全 0 | `/*traceparent='00-{32hex}-{16×0}-01'*/` | 同上拒 |
| 6 | v3 协商（老 pageserver / `neon.protocol_version=3`） | 任意带 traceparent 的 query | `nm_pack_request` 不序列化 trace（仅 version>=4 序列化）· wire 无 trace 字段 · 老 pageserver 不受影响 |
| 7 | query 跨多 statement（trace per-query scoped） | `Q1 带 traceparent; Q2 无` | Q1 的 getpage 带 trace；Q2 前 `neon_trace_status_clear`（neon.c:1066 hook clear-then-set）→ Q2 getpage `has_trace_context=0`（不泄漏 Q1 trace） |
| 8 | 多请求类型（exists/nblocks/dbsize/slru） | 触发非 getpage 的 pagestream 请求 | 同一 send chokepoint（libpagestore.c）覆盖 · 全部按 `has_trace_context` 携带 trace |

## 已验证（2026-05-29 dev server）
case 1：30000 行 cold 扫（LFC off），pageserver 新增 log 中 **526 个 getpage** 的 raw query 字节均含 `\x01`（trace_present）+ `00-1234567890abcdef1234567890abcdef-1122334455667788-01` = client traceparent。trace 全程流到 pageserver，未在 compute/pageserver 边界断。
