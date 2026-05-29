/*-------------------------------------------------------------------------
 *
 * neon_trace_status.c
 *	  Per-backend trace state container in extension-owned shared memory.
 *
 * See neon_trace_status.h for the why-not-extend-PgBackendStatus
 * fork-discipline rationale.
 *
 *-------------------------------------------------------------------------
 */
#include "postgres.h"

#include <string.h>

#include "miscadmin.h"
#include "storage/ipc.h"
#include "storage/lwlock.h"
#include "storage/shmem.h"
#include "storage/proc.h"

#include "neon_trace_status.h"

/*
 * feat-033: backend-local 快缓存 THIS backend 的当前 query trace (镜像 shmem slot)。
 * getpage hot path (libpagestore.c) 经 neon_trace_status_get_my() O(1) 读自己的
 * trace, 免每请求扫 shmem / 取锁。_set 时镜像 · _clear 时失效 (语义同 shmem slot)。
 */
static struct trace_context my_trace_cache;
static bool my_trace_cache_valid = false;

/* ------------------------------------------------------------------ */
/* Shared state                                                       */
/* ------------------------------------------------------------------ */

typedef struct NeonTraceStatusCtl
{
	int			nslots;			/* == MaxBackends + auxiliary procs */
	NeonTraceStatus slots[FLEXIBLE_ARRAY_MEMBER];
} NeonTraceStatusCtl;

static NeonTraceStatusCtl *trace_ctl = NULL;
static LWLockPadded *trace_locks = NULL;
static const char *TRANCHE_NAME = "neon_trace_status";
static int	tranche_id = 0;

/* ------------------------------------------------------------------ */
/* Size accounting                                                    */
/* ------------------------------------------------------------------ */

/*
 * Compute the total per-backend slot count. We over-provision with
 * NUM_AUXILIARY_PROCS so background workers (walproposer, communicator)
 * also have a slot — path β needs the walproposer process to be able to
 * record its own injected trace_context too.
 */
static int
trace_status_nslots(void)
{
	return MaxBackends + NUM_AUXILIARY_PROCS;
}

static Size
trace_status_shmem_size(void)
{
	Size		base = offsetof(NeonTraceStatusCtl, slots);

	return add_size(base,
					mul_size(sizeof(NeonTraceStatus),
							 (Size) trace_status_nslots()));
}

void
NeonTraceStatusShmemRequest(void)
{
	RequestAddinShmemSpace(trace_status_shmem_size());
	RequestNamedLWLockTranche(TRANCHE_NAME, trace_status_nslots());
}

void
NeonTraceStatusShmemInit(void)
{
	bool		found = false;
	int			nslots = trace_status_nslots();

	trace_ctl = (NeonTraceStatusCtl *)
		ShmemInitStruct("neon_trace_status",
						trace_status_shmem_size(), &found);
	if (!found)
	{
		MemSet(trace_ctl, 0, trace_status_shmem_size());
		trace_ctl->nslots = nslots;
	}

	trace_locks = GetNamedLWLockTranche(TRANCHE_NAME);
	tranche_id = trace_locks[0].lock.tranche;
}

/* ------------------------------------------------------------------ */
/* Slot indexing                                                      */
/* ------------------------------------------------------------------ */

/*
 * Slot index = `MyProc - ProcGlobal->allProcs`. PG already maintains
 * this mapping for backend_status.c (pgstat_get_my_status_entry()).
 * We mirror the formula directly via MyProc->pgxactoff would change
 * across versions, so we use the simpler "search by MyProcPid" path on
 * the writer (only happens once per query) and direct array index on
 * the reader.
 *
 * Implementation note: a writer always knows its own PID and writes
 * into a deterministic slot derived from MyProcPid % nslots; we re-use
 * a slot when its `pid` field is 0 or equals MyProcPid, otherwise we
 * linear-probe. This is safe because the writer holds the slot for its
 * entire backend lifetime — there is never cross-backend write
 * contention on the same slot.
 */
static int
locate_or_claim_my_slot(void)
{
	int			n = trace_ctl->nslots;
	int			my_pid = MyProcPid;
	int			start = ((unsigned int) my_pid) % (unsigned int) n;

	for (int probe = 0; probe < n; probe++)
	{
		int			idx = (start + probe) % n;
		LWLock	   *lock = &trace_locks[idx].lock;

		LWLockAcquire(lock, LW_EXCLUSIVE);
		if (trace_ctl->slots[idx].pid == 0 ||
			trace_ctl->slots[idx].pid == my_pid)
		{
			trace_ctl->slots[idx].pid = my_pid;
			LWLockRelease(lock);
			return idx;
		}
		LWLockRelease(lock);
	}
	return -1;					/* full — should never happen */
}

/* ------------------------------------------------------------------ */
/* Public writers                                                     */
/* ------------------------------------------------------------------ */

void
neon_trace_status_set(const struct trace_context *tc, const char *tracestate)
{
	if (trace_ctl == NULL)
		return;
	if (tc == NULL)
		return;

	/* feat-033: 镜像到 backend-local 快缓存 (getpage 路径 O(1) 读) */
	my_trace_cache = *tc;
	my_trace_cache_valid = true;

	int			idx = locate_or_claim_my_slot();

	if (idx < 0)
		return;

	NeonTraceStatus *slot = &trace_ctl->slots[idx];
	LWLock	   *lock = &trace_locks[idx].lock;

	LWLockAcquire(lock, LW_EXCLUSIVE);
	slot->stamp++;				/* enter write: odd */
	slot->valid = true;
	slot->tc = *tc;
	if (tracestate != NULL)
	{
		strncpy(slot->tracestate, tracestate, NEON_TRACESTATE_MAX - 1);
		slot->tracestate[NEON_TRACESTATE_MAX - 1] = '\0';
	}
	else
	{
		slot->tracestate[0] = '\0';
	}
	slot->pid = MyProcPid;
	slot->stamp++;				/* leave write: even */
	LWLockRelease(lock);
}

void
neon_trace_status_clear(void)
{
	/* feat-033: 失效 backend-local 快缓存 */
	my_trace_cache_valid = false;

	if (trace_ctl == NULL)
		return;

	int			idx = locate_or_claim_my_slot();

	if (idx < 0)
		return;

	NeonTraceStatus *slot = &trace_ctl->slots[idx];
	LWLock	   *lock = &trace_locks[idx].lock;

	LWLockAcquire(lock, LW_EXCLUSIVE);
	slot->stamp++;
	slot->valid = false;
	MemSet(&slot->tc, 0, sizeof(slot->tc));
	slot->tracestate[0] = '\0';
	/* keep `pid` so we own the slot for the rest of the backend life */
	slot->stamp++;
	LWLockRelease(lock);
}

/* ------------------------------------------------------------------ */
/* Public readers                                                     */
/* ------------------------------------------------------------------ */

bool
neon_trace_status_current(struct trace_context *out,
						  char *tracestate_buf, size_t tracestate_buf_len)
{
	if (trace_ctl == NULL || out == NULL)
		return false;

	int			idx = locate_or_claim_my_slot();

	if (idx < 0)
		return false;

	NeonTraceStatus *slot = &trace_ctl->slots[idx];
	LWLock	   *lock = &trace_locks[idx].lock;
	bool		hit = false;

	LWLockAcquire(lock, LW_SHARED);
	if (slot->valid)
	{
		*out = slot->tc;
		if (tracestate_buf != NULL && tracestate_buf_len > 0)
		{
			strncpy(tracestate_buf, slot->tracestate, tracestate_buf_len - 1);
			tracestate_buf[tracestate_buf_len - 1] = '\0';
		}
		hit = true;
	}
	LWLockRelease(lock);
	return hit;
}

/*
 * feat-033: O(1) 读 THIS backend 当前 query trace (backend-local 缓存 · 无 shmem
 * 扫描/锁)。getpage hot path 用之把 client query trace 附到 pagestream 请求。
 * 命中(当前 query 有 trace)返 true + 填 *out; 否则 false。
 */
bool
neon_trace_status_get_my(struct trace_context *out)
{
	if (!my_trace_cache_valid || out == NULL)
		return false;
	*out = my_trace_cache;
	return true;
}

bool
neon_trace_status_get_by_pid(int pid, struct trace_context *out,
							 char *tracestate_out_buf,
							 size_t tracestate_buf_len)
{
	if (trace_ctl == NULL || out == NULL || pid <= 0)
		return false;

	int			n = trace_ctl->nslots;
	int			start = ((unsigned int) pid) % (unsigned int) n;

	for (int probe = 0; probe < n; probe++)
	{
		int			idx = (start + probe) % n;
		NeonTraceStatus *slot = &trace_ctl->slots[idx];
		LWLock	   *lock = &trace_locks[idx].lock;
		bool		hit = false;

		LWLockAcquire(lock, LW_SHARED);
		if (slot->pid == pid && slot->valid)
		{
			*out = slot->tc;
			if (tracestate_out_buf != NULL && tracestate_buf_len > 0)
			{
				strncpy(tracestate_out_buf, slot->tracestate,
						tracestate_buf_len - 1);
				tracestate_out_buf[tracestate_buf_len - 1] = '\0';
			}
			hit = true;
		}
		else if (slot->pid == 0)
		{
			/* probing reached an unused slot — pid not present */
			LWLockRelease(lock);
			return false;
		}
		LWLockRelease(lock);
		if (hit)
			return true;
	}
	return false;
}

void
neon_trace_status_iterate(neon_trace_status_visitor visit, void *ctx)
{
	if (trace_ctl == NULL || visit == NULL)
		return;

	int			n = trace_ctl->nslots;

	for (int idx = 0; idx < n; idx++)
	{
		NeonTraceStatus *slot = &trace_ctl->slots[idx];
		LWLock	   *lock = &trace_locks[idx].lock;
		NeonTraceStatus copy;
		bool		copied = false;

		LWLockAcquire(lock, LW_SHARED);
		if (slot->valid && slot->pid > 0)
		{
			copy = *slot;
			copied = true;
		}
		LWLockRelease(lock);
		if (copied)
			visit(&copy, ctx);
	}
}
