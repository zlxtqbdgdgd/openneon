-- feat-012 downgrade: drop the histogram bucket jsonb export.
DROP VIEW IF EXISTS neon_perf_counters_histograms;
DROP FUNCTION IF EXISTS get_perf_counters_histograms() CASCADE;

-- feat-013 downgrade: drop the neon_safekeeper_lsn view.
DROP VIEW IF EXISTS neon_safekeeper_lsn;
DROP FUNCTION IF EXISTS get_safekeeper_lsns() CASCADE;

-- feat-014 downgrade: drop the per-relation LFC stats view.
DROP VIEW IF EXISTS neon_lfc_stats_per_relation;
DROP FUNCTION IF EXISTS get_lfc_stats_per_relation() CASCADE;

-- feat-015 downgrade: no DDL to undo (the widening adds metric rows, not view
-- columns). Roll back the data by setting the per-group GUCs to off:
--   neon.perf_counters_wal_extra = off
--   neon.perf_counters_getpage_extra = off
--   neon.perf_counters_compute_resource = off
