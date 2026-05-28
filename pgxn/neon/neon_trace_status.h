/*-------------------------------------------------------------------------
 *
 * neon_trace_status.h
 *	  Per-backend trace state container (feat-034 path α / path β output).
 *
 * PostgreSQL's PgBackendStatus struct (src/include/utils/backend_status.h)
 * is part of the upstream `vendor/postgres-v*` subtree which we are
 * forbidden from modifying (fork discipline · see top-level CLAUDE.md
 * "Fork / 双 remote 协作纪律"). So instead of extending PgBackendStatus,
 * the neon extension owns a parallel `NeonTraceStatus[MaxBackends]`
 * array in its own shared memory segment, indexed by the same slot
 * that backend_status.c uses (`MyProcPid` for the writer, PID lookup
 * for the reader). This:
 *
 *	- keeps the upstream submodule untouched (fork-safe)
 *	- still lets the `neon_stat_activity` SQL-level view JOIN on PID with
 *	  pg_stat_activity to expose trace_id / span_id / sampled
 *	- carries the same per-query lifecycle (overwritten by
 *	  post_parse_analyze_hook, cleared by ExecutorEnd_hook so subsequent
 *	  untagged queries don't leak the prior trace_id)
 *
 * Concurrency: each slot is guarded by a single LWLock from the
 * "neon_trace_status" tranche. The writer is always the owning backend
 * itself, so contention is between writer (current backend) and reader
 * (another backend's `neon_stat_activity` SRF). Each slot also carries
 * a u64 monotonic "stamp" so the reader can detect torn reads even
 * without taking the lock for low-frequency dashboards.
 *
 *-------------------------------------------------------------------------
 */
#ifndef NEON_TRACE_STATUS_H
#define NEON_TRACE_STATUS_H

#include "postgres.h"

#include "trace_context.h"

/* Max size of a tracestate string we are willing to remember per slot. */
#define NEON_TRACESTATE_MAX 256

typedef struct NeonTraceStatus
{
	uint64		stamp;			/* even = idle, odd = mid-write (seq-lock) */
	bool		valid;			/* true if tc carries a real trace */
	struct trace_context tc;	/* W3C TraceContext (v00 wire) */
	char		tracestate[NEON_TRACESTATE_MAX];	/* NUL-terminated; "" if absent */
	int			pid;			/* MyProcPid of the owning backend */
} NeonTraceStatus;

/* Stage 2 hook: declare shmem need. */
extern void NeonTraceStatusShmemRequest(void);

/* Stage 3 hook: allocate + initialise shmem. */
extern void NeonTraceStatusShmemInit(void);

/*
 * Called from the post_parse_analyze_hook (path α) AFTER a query was
 * analysed: writes the extracted traceparent into this backend's slot.
 * No-op if `tc` is NULL.
 */
extern void neon_trace_status_set(const struct trace_context *tc,
								  const char *tracestate);

/*
 * Called from ExecutorEnd_hook: clears this backend's slot so a
 * subsequent untagged query is NOT reported with the prior trace_id.
 */
extern void neon_trace_status_clear(void);

/*
 * Read another backend's slot by PID. Returns true on hit; *out and
 * *tracestate_out_buf (NULL-tolerated) are filled. `tracestate_buf_len`
 * is the caller's buffer size for the tracestate copy.
 */
extern bool neon_trace_status_get_by_pid(int pid,
										 struct trace_context *out,
										 char *tracestate_out_buf,
										 size_t tracestate_buf_len);

/*
 * Walk all valid slots, calling visitor(slot_copy, ctx) for each. The
 * walker takes per-slot shared LWLocks one at a time so the SRF can
 * stream rows without holding a global lock.
 */
typedef void (*neon_trace_status_visitor) (const NeonTraceStatus *slot,
										   void *ctx);

extern void neon_trace_status_iterate(neon_trace_status_visitor visit,
									  void *ctx);

/*
 * Snapshot of the current backend's slot (used by walproposer path β to
 * propagate the backend's trace_id into the START_WAL_PUSH command).
 * Returns true if this backend currently has a valid trace_context.
 */
extern bool neon_trace_status_current(struct trace_context *out,
									  char *tracestate_buf,
									  size_t tracestate_buf_len);

#endif							/* NEON_TRACE_STATUS_H */
