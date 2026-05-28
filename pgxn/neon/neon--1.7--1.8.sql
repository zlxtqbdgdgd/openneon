\echo Use "ALTER EXTENSION neon UPDATE TO '1.8'" to load this file. \quit

-- feat-034: SQLCommenter trace propagation.
--
-- Exposes per-backend W3C TraceContext state (extracted from inbound
-- query SQLCommenter blocks by the post_parse_analyze_hook) as a SQL
-- view JOIN-able with pg_stat_activity. The agent (or a human) can run:
--
--   SELECT a.pid, a.query, t.trace_id, t.tracestate
--   FROM pg_stat_activity a LEFT JOIN neon_stat_activity t USING (pid);
--
-- and find which backend is currently executing a given trace_id —
-- closing the loop from application-side OpenTelemetry trace to
-- in-database backend state (path α verification).
--
-- For path β (Neon-internal walproposer → safekeeper SQL forwarding),
-- the same trace state is injected by walproposer_pg.c via
-- sqlcommenter_inject_traceparent() before SendStartWALPush.

CREATE FUNCTION neon_get_trace_status()
RETURNS SETOF RECORD
AS 'MODULE_PATHNAME', 'neon_get_trace_status'
LANGUAGE C PARALLEL SAFE;

-- Schema mirrors NeonTraceStatus / NeonTraceStatusCtl in
-- pgxn/neon/neon_trace_status.h. trace_id / span_id are lowercase hex
-- (32 / 16 chars respectively) for direct equality match against the
-- OTel-exported W3C traceparent header.
CREATE VIEW neon_stat_activity AS
  SELECT T.pid,
         T.trace_id,
         T.span_id,
         T.trace_flags,
         T.sampled,
         T.tracestate
  FROM neon_get_trace_status() AS T (
    pid          int4,
    trace_id     text,
    span_id      text,
    trace_flags  int2,
    sampled      bool,
    tracestate   text
  );

GRANT SELECT ON neon_stat_activity TO PUBLIC;

REVOKE ALL ON FUNCTION neon_get_trace_status() FROM PUBLIC;
GRANT EXECUTE ON FUNCTION neon_get_trace_status() TO PUBLIC;
