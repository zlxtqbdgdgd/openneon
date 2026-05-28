-- feat-012 downgrade: drop the histogram bucket jsonb export.
DROP VIEW IF EXISTS neon_perf_counters_histograms;
DROP FUNCTION IF EXISTS get_perf_counters_histograms() CASCADE;

-- feat-013 downgrade: drop the neon_safekeeper_lsn view.
DROP VIEW IF EXISTS neon_safekeeper_lsn;
DROP FUNCTION IF EXISTS get_safekeeper_lsns() CASCADE;

-- feat-014 downgrade: drop the per-relation LFC stats view.
DROP VIEW IF EXISTS neon_lfc_stats_per_relation;
DROP FUNCTION IF EXISTS get_lfc_stats_per_relation() CASCADE;
