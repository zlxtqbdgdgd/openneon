\echo Use "ALTER EXTENSION neon UPDATE TO '1.8'" to load this file. \quit

-- feat-014 down: 还原 neon_lfc_stats_per_relation 去掉 pages_total 列.
-- CREATE OR REPLACE 不能减列 → DROP + CREATE 还原 1.8 的 10 列定义.

DROP VIEW IF EXISTS neon_lfc_stats_per_relation;
CREATE VIEW neon_lfc_stats_per_relation AS
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
    s.last_access_ts
   FROM get_lfc_stats_per_relation() s(relfilenode oid, reltablespace oid, reldatabase oid, pages_in_cache bigint, hits bigint, misses bigint, evictions bigint, last_access_ts timestamp with time zone)
     LEFT JOIN pg_class c ON c.relfilenode = s.relfilenode AND s.reldatabase = (( SELECT pg_database.oid
           FROM pg_database
          WHERE pg_database.datname = current_database()));
