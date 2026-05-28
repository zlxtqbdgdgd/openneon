-- feat-034 path α/β · neon_stat_activity view 烟雾测试
--
-- 适用于 pg_regress 或 psql 手测. 假设已 CREATE EXTENSION neon VERSION '1.8'
-- 并 ALTER ENABLE 了 post_parse_analyze hook (来自 _PG_init).
--
-- 8 case fixture 的 SQL 形态化版本; 期望输出在 .expected/ 同名文件.

\set TP1 '''00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01'''
\set TS_APP '''neon%3Droot%3Dapp'''

-- ---- case 2: 标准 path α --------------------------------------------------
SELECT 1 AS query_with_traceparent
  /*traceparent='00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01',tracestate='neon%3Droot%3Dapp'*/;

SELECT trace_id, span_id, trace_flags, sampled, tracestate
FROM neon_stat_activity
WHERE pid = pg_backend_pid();

-- ---- case 3: multi-KV ------------------------------------------------------
SELECT 1 AS query_multi_kv
  /*traceparent='00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01',action='checkout',controller='checkout%2Fpay'*/;

SELECT trace_id, tracestate FROM neon_stat_activity WHERE pid = pg_backend_pid();

-- ---- case 4: lenient version=99 -------------------------------------------
SELECT 1 AS query_v99
  /*traceparent='99-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01'*/;

SELECT trace_id FROM neon_stat_activity WHERE pid = pg_backend_pid();

-- ---- case 5: URL-encoded value --------------------------------------------
SELECT 1 AS query_urlenc
  /*traceparent='00%2D0af7651916cd43dd8448eb211c80319c%2Db7ad6b7169203331%2D01'*/;

SELECT trace_id FROM neon_stat_activity WHERE pid = pg_backend_pid();

-- ---- case 6: 双引号非法 → silent skip, query 仍 OK ------------------------
SELECT 1 AS query_double_quoted
  /*traceparent="00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01"*/;

-- 期望: trace_id 与 case 5 相同 (上一次成功设置的还在 slot 里)
-- OR · 如果在 backend lifecycle 里 ExecutorEnd 已经 clear, 这里返 0 行.
-- 这取决于 case 顺序 — 这是一致性测试.

-- ---- case 7: 字符串字面量 slash-star --------------------------------------
SELECT '/*literal_not_comment*/' AS query_literal;

-- 不应触发 trace 提取
SELECT count(*) AS expect_zero
FROM neon_stat_activity
WHERE pid = pg_backend_pid() AND tracestate = 'neon=app';
-- 注: 上一行可能会被它自己的 'neon=app' 命中 ⇒ 这正是预期边界 case;
-- 注释里没 KV 但字符串内有像 KV 的, 应该 0 — 这正是 string-literal 隔离测试.

-- ---- case 8: per-query 覆盖 (需要 feat-065 共栈, 本 PR 范围只测 lifecycle)
-- query 8a: 无 traceparent
SELECT 1 AS untagged;
-- view 应该是 0 行 (因 ExecutorEnd 清了 slot, 而 untagged query 没设)
SELECT count(*) AS expect_zero_when_untagged
FROM neon_stat_activity
WHERE pid = pg_backend_pid();

-- query 8b: 重新带上 traceparent
SELECT 1 AS retagged
  /*traceparent='00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01'*/;
SELECT trace_id FROM neon_stat_activity WHERE pid = pg_backend_pid();
