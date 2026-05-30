\echo Use "ALTER EXTENSION neon UPDATE TO '1.9'" to load this file. \quit

-- feat-015 down: 还原 neon_perf_counters 去掉 endpoint_id (DROP+CREATE · CREATE OR REPLACE 不能减列).
DROP VIEW IF EXISTS neon_perf_counters;
CREATE VIEW neon_perf_counters AS
 SELECT metric,
    bucket_le,
    value
   FROM get_perf_counters() p(metric text, bucket_le double precision, value double precision);
