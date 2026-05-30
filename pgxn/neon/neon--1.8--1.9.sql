\echo Use "ALTER EXTENSION neon UPDATE TO '1.9'" to load this file. \quit

-- feat-014: neon_lfc_stats_per_relation 加 pages_total 列 (relation 主 fork 总页数).
--
-- pages_total = pg_relation_size(relid) / block_size · 让 agent 一条 SQL 直接算出
-- "该表多大比例在 LFC" (pages_in_cache / pages_total) · 不必再单独 join pg_relation_size.
-- 设计 §4 列清单含 pages_total · 这里 CREATE OR REPLACE 末尾追加 (append-only · 列按名查·
-- 位置在末尾 vs 设计示意的 pages_in_cache 后·功能等价).

CREATE OR REPLACE VIEW neon_lfc_stats_per_relation AS
 SELECT c.oid AS relid,
    c.relname,
    c.relnamespace::regnamespace AS schema_name,
    c.relkind,
    s.pages_in_cache,
        CASE
            WHEN (s.hits + s.misses) > 0 THEN s.hits::double precision / (s.hits + s.misses)::double precision
            ELSE NULL::double precision
        END AS hit_rate,
    s.hits,
    s.misses,
    s.evictions,
    s.last_access_ts,
    (pg_relation_size(c.oid) / current_setting('block_size')::bigint) AS pages_total
   FROM get_lfc_stats_per_relation() s(relfilenode oid, reltablespace oid, reldatabase oid, pages_in_cache bigint, hits bigint, misses bigint, evictions bigint, last_access_ts timestamp with time zone)
     LEFT JOIN pg_class c ON c.relfilenode = s.relfilenode AND s.reldatabase = (( SELECT pg_database.oid
           FROM pg_database
          WHERE pg_database.datname = current_database()));
