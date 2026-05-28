# feat-034 SQLCommenter 端到端 fixture · 8 case 剧本

参考 feat-034 #16 验收门 / sub-issue #24 §7。

每个 case 给出:
1. **client → DB** 发出的 SQL (path α 应用接入剧本里, SQL 包含 SQLCommenter)
2. **pg_stat_activity + neon_stat_activity JOIN** 期望看到的行
3. **walproposer → safekeeper 转发的 SQL** (path β 注入后的样子)
4. **safekeeper / pageserver 端解码出来的 trace_id / tracestate**

trace_id reference: `0af7651916cd43dd8448eb211c80319c`
span_id reference: `b7ad6b7169203331`
trace_flags: `01` (sampled)

---

## Case 1 — path β baseline (用户应用零改造)

**剧本**: 用户代码用裸 psycopg2 连 Neon, 完全没接 OpenTelemetry SDK.
proxy entry 在 connection 起点生成 trace_id, walproposer 内部转发时
注入. Datadog DBM 完全无法做到 (无法改 Aurora/RDS 内核).

```sql
-- client → DB:
INSERT INTO orders(id, amount) VALUES (1, 100);
```

**neon_stat_activity** (path α 视角): 因为应用没注 traceparent,
post_parse_analyze_hook 抽不到, 这行 backend 在 view 里**不出现**
(LEFT JOIN 拿到 trace_id = NULL). 这是 path β 不依赖 path α 的
基线证明.

**walproposer → safekeeper** (path β 视角):
```
START_WAL_PUSH (proto_version '3', allow_timeline_creation 'true') /*traceparent='00-{proxy-generated-32hex}-{16hex}-01',tracestate='neon%3Droot%3Dproxy'*/
```

**safekeeper 端解码**: `tracestate=neon=proxy` → "起点 = 内核, 不可信
回溯到业务 trace". RCA 报告显示 "Backend 段可信度: 仅内核".

---

## Case 2 — path α + path β 同时存在 (推荐姿势)

**剧本**: 应用接了 OTel SDK, SQL 文本里已经带了应用层 traceparent.
post_parse_analyze 抽出来写 backend slot, walproposer 转发时优先用
backend slot 的值 (而不是 proxy entry 的) → trace_id 全程一致.

```sql
-- client → DB:
SELECT pg_sleep(0.5) /*traceparent='00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01',tracestate='neon%3Droot%3Dapp'*/;
```

**neon_stat_activity** (path α):
```
 pid  | trace_id                          | span_id           | trace_flags | sampled | tracestate
------+-----------------------------------+-------------------+-------------+---------+--------------------
 1234 | 0af7651916cd43dd8448eb211c80319c | b7ad6b7169203331 |           1 | t       | neon=app
```

**walproposer → safekeeper** (path β 注入业务 trace_id):
```
START_WAL_PUSH (...) /*traceparent='00-0af7651916cd43dd8448eb211c80319c-...-01',tracestate='neon%3Droot%3Dapp'*/
```

**safekeeper 端解码**: trace_id 与 application OTel exporter 看到的同;
`tracestate=neon=app` 表示 "起点 = 业务应用, RCA 可往业务 trace 回溯".

**验收要点**: 同一个 32hex trace_id 同时出现在 (1) 应用 OTel SDK exporter
(2) pg_stat_activity JOIN neon_stat_activity (3) safekeeper rust 端
`OpenTelemetry::Context::current()`. 三处对齐 = path α/β 双向打通.

---

## Case 3 — multi-KV with `action='checkout'`

**剧本**: sqlcommenter-python 风格, 一个 SQL 同时带 traceparent + 业务
语义 KV.

```sql
SELECT id FROM cart WHERE user_id = 7
  /*traceparent='00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01',action='checkout',controller='checkout%2Fpay'*/;
```

**neon_stat_activity**: trace_id / tracestate 同 case 2, 其它 KV 当前
schema 不存 (留 feat-034 续作扩列). 测的就是"额外 KV 不会让 traceparent
解析失败".

---

## Case 4 — version=99 lenient forward-compat (W3C §3.2.2.3)

```sql
SELECT 1 /*traceparent='99-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01'*/;
```

**neon_stat_activity**: 用 lenient parser, 接受 v99, trace_id 仍正确解出.
**walproposer**: 不会重新生成 v00; 转发的注释 forwards 整个 v00 字节
(因 `trace_context_serialize` 只发 v00, 这是 Sender 纪律). NOTE: feat-034
当前 implementation forwards as v00 — 详设里说明这是 W3C §3.2.2.5
"forwarder MAY downgrade version" 的合规选择.

---

## Case 5 — URL-encoded value 解码

```sql
SELECT 1 /*traceparent='00%2D0af7651916cd43dd8448eb211c80319c%2Db7ad6b7169203331%2D01'*/;
```

**期望**: trace_id 仍解码为 `0af7651916cd43dd8448eb211c80319c` (sqlcommenter
spec 允许 value 任意 URL-encoded; 解码后再走 W3C 校验).

---

## Case 6 — malformed (双引号 不是单引号)

```sql
SELECT 1 /*traceparent="00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01"*/;
```

**期望**: post_parse_analyze 静默 skip (sqlcommenter 规范只允许单引号).
neon_stat_activity 不出现 trace_id. SQL 仍正常执行 (注释解析失败不能让
query 报错). 这是 "不破坏 PG 默认注释行为" 的关键回归.

---

## Case 7 — 字符串字面量包含 slash-star, 没有真注释

```sql
SELECT '/*not_a_comment*/' AS s;
```

**期望**: extractor 看到 sql 末尾不是 `*/`, 直接返回 false. neon_stat_activity
没行. walproposer 也不注入 trace_id (没有当前 trace, 同 case 1 但更极端).

---

## Case 8 — per-query 覆盖 connection-level (feat-065 startup option)

**剧本**: feat-065 在连接握手时通过 startup option 注了一个 connection
级 trace_id = `aaaa...`. 第一条 query 没带 traceparent → 沿用
connection-level. 第二条 query 带了 `traceparent=0af7...` → per-query 必须
覆盖.

```sql
-- query 1 (no traceparent):
SELECT 1;
-- query 2:
SELECT 2 /*traceparent='00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01'*/;
```

**neon_stat_activity** during query 1:
trace_id = `aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa` (来自 feat-065 startup).

**neon_stat_activity** during query 2:
trace_id = `0af7651916cd43dd8448eb211c80319c` (per-query 覆盖).

**之后 (query 2 ExecutorEnd 之后, query 3 起前)**:
neon_trace_status_clear() 被调用 → next query 重置回 connection-level
(具体 feat-065 集成是另一 PR; 此处只验 path α 自己的 lifecycle).

---

## 自动化验证 hook

`make -C pgxn/neon check-sqlcommenter` 跑标准 35 case 单测覆盖上述
case 1/2/3/5/6/7 的解析/注入层 (path α/β 共享的 lexer + injector).
case 4 (lenient version) 已被 trace_context_test 42 case 覆盖.
case 8 (per-query 覆盖) 需要带 running postmaster, 留 integration test
脚本 (后续 #34.x).

## 与 Datadog DBM 差异化

| 维度                          | Datadog DBM | feat-034 |
|-------------------------------|-------------|----------|
| client → DB SQLCommenter 抽取 | yes         | yes (path α) |
| DB 内部跨进程 trace_id 转发   | **no** (无法改 Aurora/RDS 内核) | **yes** (path β walproposer → safekeeper) |
| 暴露 backend trace state      | DBM agent 抓 wire-level | SQL view (`neon_stat_activity`) |
| trace_id 起点信任度标识       | 无         | `tracestate=neon=app/proxy` |
