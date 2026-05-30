\echo Use "ALTER EXTENSION neon UPDATE TO '1.10'" to load this file. \quit

-- feat-015: neon_perf_counters 加 endpoint_id 列.
--
-- 每行 metric 标上 endpoint_id (current_setting('neon.endpoint_id')) · 让 agent / Datadog
-- 在多 endpoint 场景区分 "这行 WAL/getpage/compute 指标来自哪个 compute endpoint".
-- 纯 SQL (view 层 current_setting · 无 C 改) · CREATE OR REPLACE 末尾追加 endpoint_id (append-only).
-- WAL/getpage/compute 指标本体由 get_perf_counters() C SRF emit (已实装) · 本 migration 只补 endpoint 维度.

CREATE OR REPLACE VIEW neon_perf_counters AS
 SELECT metric,
    bucket_le,
    value,
    current_setting('neon.endpoint_id', true) AS endpoint_id
   FROM get_perf_counters() p(metric text, bucket_le double precision, value double precision);
