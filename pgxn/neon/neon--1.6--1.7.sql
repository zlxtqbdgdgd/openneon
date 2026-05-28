\echo Use "ALTER EXTENSION neon UPDATE TO '1.7'" to load this file. \quit

-- feat-012: histogram bucket complete export (jsonb single column).
--
-- neon_perf_counters / neon_backend_perf_counters expose histograms in a
-- long/EAV shape (one *_bucket row per bucket, carrying bucket_le). That makes
-- it hard for a client/agent to reconstruct a single histogram and compute
-- p95/p99. This view emits exactly one row per histogram metric, with the full
-- cumulative bucket array as a single jsonb value:
--
--   [ {"le": 1e-05, "count": 12}, ..., {"le": null, "count": 198782} ]
--
-- "le" is the bucket upper bound (seconds); "count" is the CUMULATIVE count
-- (Prometheus semantics); the +Inf terminal bucket uses le = null. The +Inf
-- count equals the metric's *_count value. The existing neon_perf_counters /
-- neon_backend_perf_counters views are unchanged (backward compatible).
--
-- The neon.perf_counters_emit_buckets GUC (default on) rolls this back: when
-- off the function returns no rows and the view degrades to empty, with no DDL
-- change needed.
CREATE FUNCTION get_perf_counters_histograms()
RETURNS SETOF RECORD
AS 'MODULE_PATHNAME', 'neon_get_perf_counters_histograms'
LANGUAGE C PARALLEL SAFE;

CREATE VIEW neon_perf_counters_histograms AS
  SELECT P.metric_name, P.histogram_buckets
  FROM get_perf_counters_histograms() AS P (
    metric_name text,
    histogram_buckets jsonb
  );
