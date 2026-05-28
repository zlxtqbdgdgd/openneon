\echo Use "ALTER EXTENSION neon UPDATE TO '1.7'" to load this file. \quit

-- feat-034 rollback.
DROP VIEW IF EXISTS neon_stat_activity;
DROP FUNCTION IF EXISTS neon_get_trace_status();
