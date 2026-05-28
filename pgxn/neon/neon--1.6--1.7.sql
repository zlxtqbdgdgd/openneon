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

-- feat-013: expose the safekeeper 4 LSNs to SQL via the neon_safekeeper_lsn view.
--
-- One row per safekeeper currently in the walproposer config. The walproposer
-- process mirrors each safekeeper's commit/flush LSN and the pageserver
-- remote_consistent_lsn (piggybacked in its feedback) into shared memory on
-- every AppendResponse; this function reads that snapshot with no extra query
-- to the safekeeper (real-time, uncached).
--
-- The view is implicitly scoped to this compute's timeline: walproposer only
-- talks to the safekeepers of the timeline this endpoint is bound to.
--
-- Columns:
--   safekeeper_id          int    - safekeeper slot index in walproposer
--   commit_lsn             pg_lsn - quorum-committed LSN (NULL if unreachable)
--   flush_lsn              pg_lsn - LSN fsync'd locally by this sk (NULL if unreachable)
--   backup_lsn             pg_lsn - S3 backup LSN; always NULL (not carried in
--                                   the walproposer append protocol; only the
--                                   safekeeper HTTP status API has it)
--   remote_consistent_lsn  pg_lsn - pageserver applied+persisted LSN (NULL if
--                                   unreachable or no ps feedback yet)
--   reachable              bool   - whether this safekeeper is currently active
--   last_response_ms       bigint - ms since the last mirrored AppendResponse
--
-- pg_lsn columns join directly against pg_replication_slots.confirmed_flush_lsn
-- and pg_stat_replication, e.g.
--   SELECT pg_wal_lsn_diff(MAX(commit_lsn), confirmed_flush_lsn) ...
--
-- Rollback: GUC neon.safekeeper_lsn_view_enabled (default on). When off the
-- function returns no rows and the view degrades to empty; no DDL change.
CREATE FUNCTION get_safekeeper_lsns()
RETURNS SETOF RECORD
AS 'MODULE_PATHNAME', 'neon_get_safekeeper_lsns'
LANGUAGE C PARALLEL SAFE;

CREATE VIEW neon_safekeeper_lsn AS
  SELECT P.safekeeper_id,
         P.commit_lsn,
         P.flush_lsn,
         P.backup_lsn,
         P.remote_consistent_lsn,
         P.reachable,
         P.last_response_ms
  FROM get_safekeeper_lsns() AS P (
    safekeeper_id          int,
    commit_lsn             pg_lsn,
    flush_lsn              pg_lsn,
    backup_lsn             pg_lsn,
    remote_consistent_lsn  pg_lsn,
    reachable              bool,
    last_response_ms       bigint
  );

GRANT SELECT ON neon_safekeeper_lsn TO pg_monitor;
