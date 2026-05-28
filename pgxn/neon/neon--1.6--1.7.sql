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


-- feat-014: per-relation LFC stats.
--
-- The existing whole-instance neon_lfc_stats / neon_stat_file_cache views are
-- left untouched. This view slices the LFC by relation so an agent can answer
-- "which table's hit rate dropped / which table is hogging the cache".
--
-- The LFC keys its entries by relfilenode (not pg_class.oid), so the backing
-- function returns relfilenode and we LEFT JOIN pg_class ON relfilenode to
-- recover relname/relkind. The LEFT JOIN keeps orphan rows (a relation dropped
-- while its pages still linger in the LFC) with relname = NULL, so the agent
-- can still see them. relfilenode = 0 catalogs (nailed/shared) won't match;
-- that's expected.
--
-- IMPORTANT (cross-database safety): the LFC hash is instance-wide and the
-- backing function returns entries from EVERY database, each tagged with its
-- own reldatabase OID. pg_class, however, is database-local and only describes
-- relations in the database the view is queried from. A relfilenode value is
-- only unique within one database, so joining on relfilenode alone would let an
-- entry from another database match an unrelated current-database pg_class row
-- and report the wrong relname (cross-database name aliasing). We therefore
-- only resolve relname for entries whose reldatabase is the current database;
-- entries from other databases stay relname = NULL (treated as orphans, which
-- is fail-honest -- we cannot name another database's relation from here).
--
-- hit_rate is NULL when there are no samples (hits + misses = 0): fail-honest,
-- so the agent never mistakes "no data" for "0% hit rate".
--
-- Rollback: GUC neon.lfc_per_relation_stats (default on). When off the per-rel
-- hash is not updated and the function returns no rows (view degrades to empty).
CREATE FUNCTION get_lfc_stats_per_relation()
RETURNS SETOF RECORD
AS 'MODULE_PATHNAME', 'neon_get_lfc_stats_per_relation'
LANGUAGE C PARALLEL SAFE;

CREATE VIEW neon_lfc_stats_per_relation AS
  SELECT
    c.oid                          AS relid,
    c.relname,
    c.relnamespace::regnamespace   AS schema_name,
    c.relkind,
    s.pages_in_cache,
    CASE
      WHEN (s.hits + s.misses) > 0
        THEN s.hits::float8 / (s.hits + s.misses)
      ELSE NULL
    END                            AS hit_rate,
    s.hits,
    s.misses,
    s.evictions,
    s.last_access_ts
  FROM get_lfc_stats_per_relation() AS s (
    relfilenode    oid,
    reltablespace  oid,
    reldatabase    oid,
    pages_in_cache bigint,
    hits           bigint,
    misses         bigint,
    evictions      bigint,
    last_access_ts timestamptz
  )
  LEFT JOIN pg_class c
    ON c.relfilenode = s.relfilenode
   AND s.reldatabase = (SELECT oid FROM pg_database WHERE datname = current_database());

GRANT SELECT ON neon_lfc_stats_per_relation TO pg_monitor;

-- feat-015: widen neon_perf_counters with WAL / getpage / compute-resource
-- metrics, assembled across modules into the same view.
--
-- The neon_perf_counters / neon_backend_perf_counters views are unchanged in
-- SHAPE: they remain (metric text, bucket_le float8, value float8). The widening
-- is delivered as additional metric ROWS emitted by the backing C function
-- neon_get_perf_counters(), so no view DDL change is required and old clients
-- selecting existing metric rows are unaffected (backward compatible).
--
-- New metric rows (all numeric; only _total / _sum / _count are exposed, no
-- rate / mean / percentile -- those are left to the agent / history seam):
--   WAL group     (neon.perf_counters_wal_extra):
--     wal_write_bytes_total, wal_send_to_safekeeper_bytes_total
--   getpage group (neon.perf_counters_getpage_extra):
--     getpage_request_count_total, getpage_request_bytes_total,
--     getpage_wait_us_total, getpage_wait_us_count
--   compute group (neon.perf_counters_compute_resource), cgroup v2, NULL value
--   on read failure (fail-honest):
--     compute_cpu_user_share, compute_cpu_system_share, compute_memory_rss_bytes
--     NOTE: *_share are the cumulative-lifetime fraction (%) of the cgroup's
--     total CPU time spent in user / system mode (user_usec / usage_usec from
--     cpu.stat), NOT a current or recent CPU utilization. A per-second rate is
--     left to the agent / history seam.
--
-- Each group has its own GUC for independent rollback. This migration carries
-- no DDL; it exists only to bump the extension to 1.7 alongside the C change.
