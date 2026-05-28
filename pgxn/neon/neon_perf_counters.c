/*-------------------------------------------------------------------------
 *
 * neon_perf_counters.c
 *	  Collect statistics about Neon I/O
 *
 * Each backend has its own set of counters in shared memory.
 *
 *-------------------------------------------------------------------------
 */
#include "postgres.h"

#include <math.h>
#include <stdio.h>

#include "funcapi.h"
#include "miscadmin.h"
#include "storage/proc.h"
#include "storage/bufmgr.h"
#include "storage/shmem.h"
#include "utils/builtins.h"
#include "utils/jsonb.h"
#include "utils/pg_lsn.h"
#include "utils/timestamp.h"

#include "neon.h"
#include "neon_perf_counters.h"
#include "walproposer.h"

/*
 * feat-012: gate for emitting the histogram bucket jsonb output of
 * neon_get_perf_counters_histograms(). When off, that function returns no
 * rows, so the neon_perf_counters_histograms view degrades to empty and
 * clients fall back to the sum/count columns of neon_perf_counters. The
 * existing neon_perf_counters / neon_backend_perf_counters views are not
 * affected by this flag.
 */
bool		neon_perf_counters_emit_buckets = true;

/*
 * feat-013: rollback flag for the neon_safekeeper_lsn view. When off, the
 * neon_get_safekeeper_lsns() SRF returns no rows (view degrades to empty);
 * clients fall back to the aggregate lag from pg_stat_replication.
 */
bool		neon_safekeeper_lsn_view_enabled = true;

/*
 * feat-015: per-group feature flags for the widened neon_perf_counters fields.
 * Each group can be rolled back independently; when off, that group's metric
 * rows are simply not emitted.
 */
bool		neon_perf_counters_wal_extra = true;
bool		neon_perf_counters_getpage_extra = true;
bool		neon_perf_counters_compute_resource = true;

/*
 * feat-015: read compute CPU%/RSS from the cgroup v2 interface.
 *
 * Fail-honest: on any error (cgroup v1, not containerized, file missing,
 * parse failure) the corresponding out-param is left untouched and the caller
 * emits NULL. We deliberately do NOT fabricate a 0 value.
 *
 * user_share / system_share are NOT a current/recent CPU utilization. They are
 * the fraction (in percent) of all CPU time the cgroup has consumed SINCE ITS
 * CREATION that went to user vs. system mode: user_usec / usage_usec and
 * system_usec / usage_usec from cpu.stat (both cumulative-lifetime counters).
 * cpu.stat exposes only cumulative usec, so a single read cannot yield an
 * instantaneous percentage without a previous sample; we deliberately surface
 * the cumulative split (named *_share, not *_pct, to avoid implying "current
 * load"). A precise per-second rate is left to the agent/history seam
 * (feat-064), consistent with the design's "view does not compute rate" rule.
 */
static bool
read_cgroup_v2_memory_rss(int64 *rss_bytes)
{
	FILE	   *f = fopen("/sys/fs/cgroup/memory.current", "r");
	long long	val;

	if (f == NULL)
		return false;
	if (fscanf(f, "%lld", &val) != 1)
	{
		fclose(f);
		return false;
	}
	fclose(f);
	*rss_bytes = (int64) val;
	return true;
}

static bool
read_cgroup_v2_cpu_share(double *user_share, double *system_share)
{
	FILE	   *f = fopen("/sys/fs/cgroup/cpu.stat", "r");
	char		key[64];
	long long	val;
	long long	usage_usec = -1;
	long long	user_usec = -1;
	long long	system_usec = -1;

	if (f == NULL)
		return false;

	while (fscanf(f, "%63s %lld", key, &val) == 2)
	{
		if (strcmp(key, "usage_usec") == 0)
			usage_usec = val;
		else if (strcmp(key, "user_usec") == 0)
			user_usec = val;
		else if (strcmp(key, "system_usec") == 0)
			system_usec = val;
	}
	fclose(f);

	if (usage_usec <= 0 || user_usec < 0 || system_usec < 0)
		return false;

	*user_share = 100.0 * (double) user_usec / (double) usage_usec;
	*system_share = 100.0 * (double) system_usec / (double) usage_usec;
	return true;
}

/* BEGIN_HADRON */
databricks_metrics *databricks_metrics_shared;

Size
DatabricksMetricsShmemSize(void)
{
	return sizeof(databricks_metrics);
}

void
DatabricksMetricsShmemInit(void)
{
	bool		found;

	databricks_metrics_shared =
		ShmemInitStruct("Databricks counters",
						DatabricksMetricsShmemSize(),
						&found);
	Assert(found == IsUnderPostmaster);
	if (!found)
	{
		pg_atomic_init_u32(&databricks_metrics_shared->index_corruption_count, 0);
		pg_atomic_init_u32(&databricks_metrics_shared->data_corruption_count, 0);
		pg_atomic_init_u32(&databricks_metrics_shared->internal_error_count, 0);
		pg_atomic_init_u32(&databricks_metrics_shared->ps_corruption_detected, 0);
	}
}
/* END_HADRON */

neon_per_backend_counters *neon_per_backend_counters_shared;

void
NeonPerfCountersShmemRequest(void)
{
	Size size;
#if PG_MAJORVERSION_NUM < 15
	/* Hack: in PG14 MaxBackends is not initialized at the time of calling NeonPerfCountersShmemRequest function.
	 * Do it ourselves and then undo to prevent assertion failure
	 */
	Assert(MaxBackends == 0); /* not initialized yet */
	InitializeMaxBackends();
	size = mul_size(NUM_NEON_PERF_COUNTER_SLOTS, sizeof(neon_per_backend_counters));
	MaxBackends = 0;
#else
	size = mul_size(NUM_NEON_PERF_COUNTER_SLOTS, sizeof(neon_per_backend_counters));
#endif
	if (lakebase_mode) {
		size = add_size(size, DatabricksMetricsShmemSize());
	}
	RequestAddinShmemSpace(size);
}

void
NeonPerfCountersShmemInit(void)
{
	bool		found;

	neon_per_backend_counters_shared =
		ShmemInitStruct("Neon perf counters",
						mul_size(NUM_NEON_PERF_COUNTER_SLOTS,
								 sizeof(neon_per_backend_counters)),
						&found);
	Assert(found == IsUnderPostmaster);
	if (!found)
	{
		/* shared memory is initialized to zeros, so nothing to do here */
	}
}

static inline void
inc_iohist(IOHistogram hist, uint64 latency_us)
{
	int			lo = 0;
	int			hi = NUM_IO_WAIT_BUCKETS - 1;

	/* Find the right bucket with binary search */
	while (lo < hi)
	{
		int			mid = (lo + hi) / 2;

		if (latency_us < io_wait_bucket_thresholds[mid])
			hi = mid;
		else
			lo = mid + 1;
	}
	hist->wait_us_bucket[lo]++;
	hist->wait_us_sum += latency_us;
	hist->wait_us_count++;
}

static inline void
inc_qthist(QTHistogram hist, uint64 elapsed_us)
{
	int			lo = 0;
	int			hi = NUM_QT_BUCKETS - 1;

	/* Find the right bucket with binary search */
	while (lo < hi)
	{
		int			mid = (lo + hi) / 2;

		if (elapsed_us < qt_bucket_thresholds[mid])
			hi = mid;
		else
			lo = mid + 1;
	}
	hist->elapsed_us_bucket[lo]++;
	hist->elapsed_us_sum += elapsed_us;
	hist->elapsed_us_count++;
}

/*
 * Count a GetPage wait operation.
 */
void
inc_getpage_wait(uint64 latency)
{
	inc_iohist(&MyNeonCounters->getpage_hist, latency);
}

/*
 * Count an LFC read wait operation.
 */
void
inc_page_cache_read_wait(uint64 latency)
{
	inc_iohist(&MyNeonCounters->file_cache_read_hist, latency);
}

/*
 * Count an LFC write wait operation.
 */
void
inc_page_cache_write_wait(uint64 latency)
{
	inc_iohist(&MyNeonCounters->file_cache_write_hist, latency);
}


void
inc_query_time(uint64 elapsed)
{
	inc_qthist(&MyNeonCounters->query_time_hist, elapsed);
}

/*
 * Support functions for the views, neon_backend_perf_counters and
 * neon_perf_counters.
 */

typedef struct
{
	const char *name;
	bool		is_bucket;
	double		bucket_le;
	double		value;
} metric_t;

static int
io_histogram_to_metrics(IOHistogram histogram,
						metric_t *metrics,
						const char *count,
						const char *sum,
						const char *bucket)
{
	int		i = 0;
	uint64	bucket_accum = 0;

	metrics[i].name = count;
	metrics[i].is_bucket = false;
	metrics[i].value = (double) histogram->wait_us_count;
	i++;
	metrics[i].name = sum;
	metrics[i].is_bucket = false;
	metrics[i].value = (double) histogram->wait_us_sum / 1000000.0;
	i++;
	for (int bucketno = 0; bucketno < NUM_IO_WAIT_BUCKETS; bucketno++)
	{
		uint64		threshold = io_wait_bucket_thresholds[bucketno];

		bucket_accum += histogram->wait_us_bucket[bucketno];

		metrics[i].name = bucket;
		metrics[i].is_bucket = true;
		metrics[i].bucket_le = (threshold == UINT64_MAX) ? INFINITY : ((double) threshold) / 1000000.0;
		metrics[i].value = (double) bucket_accum;
		i++;
	}

	return i;
}

static int
qt_histogram_to_metrics(QTHistogram histogram,
						metric_t *metrics,
						const char *count,
						const char *sum,
						const char *bucket)
{
	int		i = 0;
	uint64	bucket_accum = 0;

	metrics[i].name = count;
	metrics[i].is_bucket = false;
	metrics[i].value = (double) histogram->elapsed_us_count;
	i++;
	metrics[i].name = sum;
	metrics[i].is_bucket = false;
	metrics[i].value = (double) histogram->elapsed_us_sum / 1000000.0;
	i++;
	for (int bucketno = 0; bucketno < NUM_QT_BUCKETS; bucketno++)
	{
		uint64		threshold = qt_bucket_thresholds[bucketno];

		bucket_accum += histogram->elapsed_us_bucket[bucketno];

		metrics[i].name = bucket;
		metrics[i].is_bucket = true;
		metrics[i].bucket_le = (threshold == UINT64_MAX) ? INFINITY : ((double) threshold) / 1000000.0;
		metrics[i].value = (double) bucket_accum;
		i++;
	}

	return i;
}

static metric_t *
neon_perf_counters_to_metrics(neon_per_backend_counters *counters)
{
#define NUM_METRICS ((2 + NUM_IO_WAIT_BUCKETS) * 3 + (2 + NUM_QT_BUCKETS) + 12)
	metric_t   *metrics = palloc((NUM_METRICS + 1) * sizeof(metric_t));
	int			i = 0;

#define APPEND_METRIC(_name) do { \
		metrics[i].name = #_name; \
		metrics[i].is_bucket = false; \
		metrics[i].value = (double) counters->_name; \
		i++; \
	} while (false)

	i += io_histogram_to_metrics(&counters->getpage_hist, &metrics[i],
								 "getpage_wait_seconds_count",
								 "getpage_wait_seconds_sum",
								 "getpage_wait_seconds_bucket");

	APPEND_METRIC(getpage_prefetch_requests_total);
	APPEND_METRIC(getpage_sync_requests_total);
	APPEND_METRIC(compute_getpage_stuck_requests_total);
	APPEND_METRIC(compute_getpage_max_inflight_stuck_time_ms);
	APPEND_METRIC(getpage_prefetch_misses_total);
	APPEND_METRIC(getpage_prefetch_discards_total);
	APPEND_METRIC(pageserver_requests_sent_total);
	APPEND_METRIC(pageserver_disconnects_total);
	APPEND_METRIC(pageserver_send_flushes_total);
	APPEND_METRIC(pageserver_open_requests);
	APPEND_METRIC(getpage_prefetches_buffered);

	APPEND_METRIC(file_cache_hits_total);

	i += io_histogram_to_metrics(&counters->file_cache_read_hist, &metrics[i],
								 "file_cache_read_wait_seconds_count",
								 "file_cache_read_wait_seconds_sum",
								 "file_cache_read_wait_seconds_bucket");
	i += io_histogram_to_metrics(&counters->file_cache_write_hist, &metrics[i],
								 "file_cache_write_wait_seconds_count",
								 "file_cache_write_wait_seconds_sum",
								 "file_cache_write_wait_seconds_bucket");

	i += qt_histogram_to_metrics(&counters->query_time_hist, &metrics[i],
								 "query_time_seconds_count",
								 "query_time_seconds_sum",
								 "query_time_seconds_bucket");

	Assert(i == NUM_METRICS);

#undef APPEND_METRIC
#undef NUM_METRICS

	/* NULL entry marks end of array */
	metrics[i].name = NULL;
	metrics[i].value = 0;

	return metrics;
}

/*
 * Write metric to three output Datums
 */
static void
metric_to_datums(metric_t *m, Datum *values, bool *nulls)
{
	values[0] = CStringGetTextDatum(m->name);
	nulls[0] = false;
	if (m->is_bucket)
	{
		values[1] = Float8GetDatum(m->bucket_le);
		nulls[1] = false;
	}
	else
	{
		values[1] = (Datum) 0;
		nulls[1] = true;
	}
	values[2] = Float8GetDatum(m->value);
	nulls[2] = false;
}

PG_FUNCTION_INFO_V1(neon_get_backend_perf_counters);
Datum
neon_get_backend_perf_counters(PG_FUNCTION_ARGS)
{
	ReturnSetInfo *rsinfo = (ReturnSetInfo *) fcinfo->resultinfo;
	Datum		values[5];
	bool		nulls[5];

	/* We put all the tuples into a tuplestore in one go. */
	InitMaterializedSRF(fcinfo, 0);

	for (int procno = 0; procno < NUM_NEON_PERF_COUNTER_SLOTS; procno++)
	{
		PGPROC	   *proc = GetPGProcByNumber(procno);
		int			pid = proc->pid;
		neon_per_backend_counters *counters = &neon_per_backend_counters_shared[procno];
		metric_t   *metrics = neon_perf_counters_to_metrics(counters);

		values[0] = Int32GetDatum(procno);
		nulls[0] = false;
		values[1] = Int32GetDatum(pid);
		nulls[1] = false;

		for (int i = 0; metrics[i].name != NULL; i++)
		{
			metric_to_datums(&metrics[i], &values[2], &nulls[2]);
			tuplestore_putvalues(rsinfo->setResult, rsinfo->setDesc, values, nulls);
		}

		pfree(metrics);
	}

	return (Datum) 0;
}

static inline void
io_histogram_merge_into(IOHistogram into, IOHistogram from)
{
	into->wait_us_count += from->wait_us_count;
	into->wait_us_sum += from->wait_us_sum;
	for (int bucketno = 0; bucketno < NUM_IO_WAIT_BUCKETS; bucketno++)
		into->wait_us_bucket[bucketno] += from->wait_us_bucket[bucketno];
}

static inline void
qt_histogram_merge_into(QTHistogram into, QTHistogram from)
{
	into->elapsed_us_count += from->elapsed_us_count;
	into->elapsed_us_sum += from->elapsed_us_sum;
	for (int bucketno = 0; bucketno < NUM_QT_BUCKETS; bucketno++)
		into->elapsed_us_bucket[bucketno] += from->elapsed_us_bucket[bucketno];
}

PG_FUNCTION_INFO_V1(neon_get_perf_counters);
Datum
neon_get_perf_counters(PG_FUNCTION_ARGS)
{
	ReturnSetInfo *rsinfo = (ReturnSetInfo *) fcinfo->resultinfo;
	Datum		values[3];
	bool		nulls[3];
	neon_per_backend_counters totals = {0};
	metric_t   *metrics;

	/* BEGIN_HADRON */
	WalproposerShmemState *wp_shmem;
	uint32 num_safekeepers;
	uint32 num_active_safekeepers;
	/* END_HADRON */

	/* We put all the tuples into a tuplestore in one go. */
	InitMaterializedSRF(fcinfo, 0);

	/* Aggregate the counters across all backends */
	for (int procno = 0; procno < NUM_NEON_PERF_COUNTER_SLOTS; procno++)
	{
		neon_per_backend_counters *counters = &neon_per_backend_counters_shared[procno];

		io_histogram_merge_into(&totals.getpage_hist, &counters->getpage_hist);
		totals.getpage_prefetch_requests_total += counters->getpage_prefetch_requests_total;
		totals.getpage_sync_requests_total += counters->getpage_sync_requests_total;
		totals.getpage_prefetch_misses_total += counters->getpage_prefetch_misses_total;
		totals.getpage_prefetch_discards_total += counters->getpage_prefetch_discards_total;
		totals.pageserver_requests_sent_total += counters->pageserver_requests_sent_total;
		totals.pageserver_disconnects_total += counters->pageserver_disconnects_total;
		totals.pageserver_send_flushes_total += counters->pageserver_send_flushes_total;
		totals.pageserver_open_requests += counters->pageserver_open_requests;
		totals.getpage_prefetches_buffered += counters->getpage_prefetches_buffered;
		totals.file_cache_hits_total += counters->file_cache_hits_total;
		totals.compute_getpage_stuck_requests_total += counters->compute_getpage_stuck_requests_total;
		totals.compute_getpage_max_inflight_stuck_time_ms = Max(
			totals.compute_getpage_max_inflight_stuck_time_ms,
			counters->compute_getpage_max_inflight_stuck_time_ms);
		io_histogram_merge_into(&totals.file_cache_read_hist, &counters->file_cache_read_hist);
		io_histogram_merge_into(&totals.file_cache_write_hist, &counters->file_cache_write_hist);
		qt_histogram_merge_into(&totals.query_time_hist, &counters->query_time_hist);
	}

	metrics = neon_perf_counters_to_metrics(&totals);
	for (int i = 0; metrics[i].name != NULL; i++)
	{
		metric_to_datums(&metrics[i], &values[0], &nulls[0]);
		tuplestore_putvalues(rsinfo->setResult, rsinfo->setDesc, values, nulls);
	}

	/*
	 * feat-015: widen neon_perf_counters with WAL / getpage / compute-resource
	 * fields, assembled across modules into the same single view. Each group is
	 * behind its own GUC; only _total / _sum / _count are exposed (no rate /
	 * mean / percentile -- the agent / history seam computes those).
	 */
	{
		WalproposerShmemState *wp_lsn_shmem = GetWalpropShmemState();

		/* --- WAL group (source: walproposer shmem, single writer) --- */
		if (neon_perf_counters_wal_extra && wp_lsn_shmem != NULL)
		{
			metric_t	m;

			m.is_bucket = false;
			m.bucket_le = 0;

			m.name = "wal_write_bytes_total";
			m.value = (double) pg_atomic_read_u64(&wp_lsn_shmem->wal_write_bytes_total);
			metric_to_datums(&m, &values[0], &nulls[0]);
			tuplestore_putvalues(rsinfo->setResult, rsinfo->setDesc, values, nulls);

			m.name = "wal_send_to_safekeeper_bytes_total";
			m.value = (double) pg_atomic_read_u64(&wp_lsn_shmem->wal_send_to_safekeeper_bytes_total);
			metric_to_datums(&m, &values[0], &nulls[0]);
			tuplestore_putvalues(rsinfo->setResult, rsinfo->setDesc, values, nulls);
		}

		/* --- getpage group (source: existing aggregated backend counters) --- */
		if (neon_perf_counters_getpage_extra)
		{
			metric_t	m;
			uint64		req_count = totals.getpage_prefetch_requests_total +
									totals.getpage_sync_requests_total;

			m.is_bucket = false;
			m.bucket_le = 0;

			/* request count = prefetch + sync getpage requests */
			m.name = "getpage_request_count_total";
			m.value = (double) req_count;
			metric_to_datums(&m, &values[0], &nulls[0]);
			tuplestore_putvalues(rsinfo->setResult, rsinfo->setDesc, values, nulls);

			/*
			 * request bytes: every getpage returns exactly one BLCKSZ page, so
			 * served-page bytes == request_count * BLCKSZ (exact identity, not
			 * an estimate).
			 */
			m.name = "getpage_request_bytes_total";
			m.value = (double) req_count * (double) BLCKSZ;
			metric_to_datums(&m, &values[0], &nulls[0]);
			tuplestore_putvalues(rsinfo->setResult, rsinfo->setDesc, values, nulls);

			/* getpage wait: reuse the in-tree getpage histogram sum/count (us) */
			m.name = "getpage_wait_us_total";
			m.value = (double) totals.getpage_hist.wait_us_sum;
			metric_to_datums(&m, &values[0], &nulls[0]);
			tuplestore_putvalues(rsinfo->setResult, rsinfo->setDesc, values, nulls);

			m.name = "getpage_wait_us_count";
			m.value = (double) totals.getpage_hist.wait_us_count;
			metric_to_datums(&m, &values[0], &nulls[0]);
			tuplestore_putvalues(rsinfo->setResult, rsinfo->setDesc, values, nulls);
		}

		/* --- compute resource group (source: cgroup v2, fail-honest NULL) --- */
		if (neon_perf_counters_compute_resource)
		{
			double		cpu_user_share = 0;
			double		cpu_system_share = 0;
			int64		rss_bytes = 0;
			bool		cpu_ok = read_cgroup_v2_cpu_share(&cpu_user_share, &cpu_system_share);
			bool		rss_ok = read_cgroup_v2_memory_rss(&rss_bytes);

			/*
			 * compute_cpu_user_share: cumulative-lifetime fraction (%) of the
			 * cgroup's total CPU time spent in user mode, NOT a current/recent
			 * utilization. See read_cgroup_v2_cpu_share().
			 */
			values[0] = CStringGetTextDatum("compute_cpu_user_share");
			nulls[0] = false;
			values[1] = (Datum) 0;
			nulls[1] = true;		/* bucket_le */
			if (cpu_ok)
			{
				values[2] = Float8GetDatum(cpu_user_share);
				nulls[2] = false;
			}
			else
				nulls[2] = true;
			tuplestore_putvalues(rsinfo->setResult, rsinfo->setDesc, values, nulls);

			/* compute_cpu_system_share: cumulative-lifetime system-mode share (%) */
			values[0] = CStringGetTextDatum("compute_cpu_system_share");
			nulls[0] = false;
			nulls[1] = true;
			if (cpu_ok)
			{
				values[2] = Float8GetDatum(cpu_system_share);
				nulls[2] = false;
			}
			else
				nulls[2] = true;
			tuplestore_putvalues(rsinfo->setResult, rsinfo->setDesc, values, nulls);

			/* compute_memory_rss_bytes */
			values[0] = CStringGetTextDatum("compute_memory_rss_bytes");
			nulls[0] = false;
			nulls[1] = true;
			if (rss_ok)
			{
				values[2] = Float8GetDatum((double) rss_bytes);
				nulls[2] = false;
			}
			else
				nulls[2] = true;
			tuplestore_putvalues(rsinfo->setResult, rsinfo->setDesc, values, nulls);
		}
	}

	if (lakebase_mode) {

		if (databricks_test_hook == TestHookCorruption) {
			ereport(ERROR,
						(errcode(ERRCODE_DATA_CORRUPTED),
						errmsg("test corruption")));
		}

		// Not ideal but piggyback our databricks counters into the neon perf counters view
		// so that we don't need to introduce neon--1.x+1.sql to add a new view.
		{
		// Keeping this code in its own block to work around the C90 "don't mix declarations and code" rule when we define
		// the `databricks_metrics` array in the next block. Yes, we are seriously dealing with C90 rules in 2025.

		// Read safekeeper status from wal proposer shared memory first.
		// Note that we are taking a mutex when reading from walproposer shared memory so that the total safekeeper count is
		// consistent with the active wal acceptors count. Assuming that we don't query this view too often the mutex should
		// not be a huge deal.
		wp_shmem = GetWalpropShmemState();
		SpinLockAcquire(&wp_shmem->mutex);
		num_safekeepers = wp_shmem->num_safekeepers;
		num_active_safekeepers = 0;
		for (int i = 0; i < num_safekeepers; i++) {
			if (wp_shmem->safekeeper_status[i] == 1) {
				num_active_safekeepers++;
			}
		}
		SpinLockRelease(&wp_shmem->mutex);
	}
	{
			metric_t databricks_metrics[] = {
				{"sql_index_corruption_count", false, 0, (double) pg_atomic_read_u32(&databricks_metrics_shared->index_corruption_count)},
				{"sql_data_corruption_count", false, 0, (double) pg_atomic_read_u32(&databricks_metrics_shared->data_corruption_count)},
				{"sql_internal_error_count", false, 0, (double) pg_atomic_read_u32(&databricks_metrics_shared->internal_error_count)},
				{"ps_corruption_detected", false, 0, (double) pg_atomic_read_u32(&databricks_metrics_shared->ps_corruption_detected)},
				{"num_active_safekeepers", false, 0.0, (double) num_active_safekeepers},
				{"num_configured_safekeepers", false, 0.0, (double) num_safekeepers},
				{NULL, false, 0, 0},
			};
			for (int i = 0; databricks_metrics[i].name != NULL; i++)
			{
				metric_to_datums(&databricks_metrics[i], &values[0], &nulls[0]);
				tuplestore_putvalues(rsinfo->setResult, rsinfo->setDesc, values, nulls);
			}
		}
		/* END_HADRON */
	}

	pfree(metrics);

	return (Datum) 0;
}

/*
 * feat-012: histogram bucket complete export (jsonb single column).
 *
 * neon_get_perf_counters() already emits, for every histogram metric, a set of
 * cumulative <name>_bucket rows carrying a bucket_le upper bound plus a
 * <name>_count / <name>_sum pair (Prometheus style). That long/EAV shape is
 * agent-unfriendly: reconstructing a single histogram requires joining many
 * rows. This function re-aggregates the same counters and, for each histogram
 * metric, emits exactly ONE row carrying the full cumulative bucket array as a
 * single jsonb value:
 *
 *   [ {"le": 1e-05, "count": 12}, ..., {"le": null, "count": 198782} ]
 *
 * Semantics (matches feat-012 design doc section 4):
 *   - "le"    = bucket upper bound, "less than or equal" (seconds).
 *   - "count" = CUMULATIVE count (monotonic non-decreasing across the array),
 *               accumulated since backend start, exactly the value already used
 *               by neon_get_perf_counters() bucket rows.
 *   - The +Inf terminal bucket uses le = null (jsonb has no native infinity).
 *   - The +Inf count equals the metric's <name>_count value (emit consistency).
 *
 * The histogram_buckets column is NULL only for the rollback path (the
 * neon.perf_counters_emit_buckets GUC is off), in which case no rows are
 * emitted at all. Non-histogram (gauge/counter) metrics are simply not present
 * in this view, which the caller reads as "not applicable".
 *
 * This reuses the in-tree IOHistogram / QTHistogram counters and adds no new
 * collector; the bucket edges stay the curated in-tree io_wait_bucket_thresholds
 * / qt_bucket_thresholds sets (no user-defined buckets, to bound cardinality).
 */
static void
emit_one_histogram(ReturnSetInfo *rsinfo, metric_t *metrics, int *idx,
				   const char *base_name)
{
	Datum		values[2];
	bool		nulls[2];
	JsonbParseState *state = NULL;
	JsonbValue *jbv;
	Jsonb	   *jb;
	int			i = *idx;
	int			nbuckets PG_USED_FOR_ASSERTS_ONLY = 0;
#ifdef USE_ASSERT_CHECKING
	/*
	 * Invariant guard: this function relies on the fact that all *_bucket rows
	 * belonging to one histogram are emitted as a SINGLE contiguous run by
	 * io_histogram_to_metrics() / qt_histogram_to_metrics(). The walk below
	 * stops at the first non-bucket metric; so if a gauge/counter were ever
	 * inserted in the middle of a bucket run, the histogram would be silently
	 * truncated (and a second, bogus histogram emitted for the tail). We assert
	 * that the run we consume has exactly the curated width for this histogram:
	 * query_time uses NUM_QT_BUCKETS, every other (IO) histogram uses
	 * NUM_IO_WAIT_BUCKETS. If you add a histogram with a different bucket count,
	 * extend this mapping.
	 */
	int			expected_buckets =
		(strcmp(base_name, "query_time_seconds") == 0)
		? NUM_QT_BUCKETS
		: NUM_IO_WAIT_BUCKETS;
#endif

	pushJsonbValue(&state, WJB_BEGIN_ARRAY, NULL);

	/* Walk consecutive *_bucket entries that belong to this histogram. */
	while (metrics[i].name != NULL && metrics[i].is_bucket)
	{
		JsonbValue	key;
		JsonbValue	val;

		pushJsonbValue(&state, WJB_BEGIN_OBJECT, NULL);

		/* "le": bucket upper bound, or null for the +Inf terminal bucket. */
		key.type = jbvString;
		key.val.string.val = "le";
		key.val.string.len = strlen("le");
		pushJsonbValue(&state, WJB_KEY, &key);

		if (isinf(metrics[i].bucket_le))
		{
			val.type = jbvNull;
		}
		else
		{
			val.type = jbvNumeric;
			val.val.numeric = DatumGetNumeric(DirectFunctionCall1(
				float8_numeric, Float8GetDatum(metrics[i].bucket_le)));
		}
		pushJsonbValue(&state, WJB_VALUE, &val);

		/* "count": cumulative count up to and including this bucket. */
		key.type = jbvString;
		key.val.string.val = "count";
		key.val.string.len = strlen("count");
		pushJsonbValue(&state, WJB_KEY, &key);

		val.type = jbvNumeric;
		val.val.numeric = DatumGetNumeric(DirectFunctionCall1(
			float8_numeric, Float8GetDatum(metrics[i].value)));
		pushJsonbValue(&state, WJB_VALUE, &val);

		pushJsonbValue(&state, WJB_END_OBJECT, NULL);
		i++;
		nbuckets++;
	}

	/*
	 * Protect the contiguity invariant (see comment above): a complete bucket
	 * run must have exactly the curated number of buckets. A short run means a
	 * non-bucket metric was interleaved and silently split the histogram.
	 */
	Assert(nbuckets == expected_buckets);

	jbv = pushJsonbValue(&state, WJB_END_ARRAY, NULL);
	jb = JsonbValueToJsonb(jbv);

	values[0] = CStringGetTextDatum(base_name);
	nulls[0] = false;
	values[1] = JsonbPGetDatum(jb);
	nulls[1] = false;
	tuplestore_putvalues(rsinfo->setResult, rsinfo->setDesc, values, nulls);

	*idx = i;
}

PG_FUNCTION_INFO_V1(neon_get_perf_counters_histograms);
Datum
neon_get_perf_counters_histograms(PG_FUNCTION_ARGS)
{
	ReturnSetInfo *rsinfo = (ReturnSetInfo *) fcinfo->resultinfo;
	neon_per_backend_counters totals = {0};
	metric_t   *metrics;

	InitMaterializedSRF(fcinfo, 0);

	/* Rollback path: emit no rows, clients fall back to sum/count. */
	if (!neon_perf_counters_emit_buckets)
		return (Datum) 0;

	/* Aggregate the counters across all backends (same as neon_get_perf_counters). */
	for (int procno = 0; procno < NUM_NEON_PERF_COUNTER_SLOTS; procno++)
	{
		neon_per_backend_counters *counters = &neon_per_backend_counters_shared[procno];

		io_histogram_merge_into(&totals.getpage_hist, &counters->getpage_hist);
		io_histogram_merge_into(&totals.file_cache_read_hist, &counters->file_cache_read_hist);
		io_histogram_merge_into(&totals.file_cache_write_hist, &counters->file_cache_write_hist);
		qt_histogram_merge_into(&totals.query_time_hist, &counters->query_time_hist);
	}

	metrics = neon_perf_counters_to_metrics(&totals);

	/*
	 * Walk the metric list. Every histogram is materialized as a contiguous
	 * run of <name>_count, <name>_sum, then NUM_*_BUCKETS *_bucket rows. We key
	 * the emitted row on the histogram base name (the metric name with the
	 * "_bucket" suffix stripped) and consume the whole bucket run.
	 */
	for (int i = 0; metrics[i].name != NULL; /* advanced inside */)
	{
		if (metrics[i].is_bucket)
		{
			const char *bucket_name = metrics[i].name;
			const char *suffix = strstr(bucket_name, "_bucket");
			char	   *base_name;

			if (suffix != NULL)
				base_name = pnstrdup(bucket_name, suffix - bucket_name);
			else
				base_name = pstrdup(bucket_name);

			emit_one_histogram(rsinfo, metrics, &i, base_name);
			pfree(base_name);
		}
		else
		{
			/* scalar (count/sum/gauge/counter): not a histogram, skip. */
			i++;
		}
	}

	pfree(metrics);

	return (Datum) 0;
}

/*
 * feat-013: neon_safekeeper_lsn view backing function.
 *
 * Returns one row per safekeeper currently in the walproposer config, exposing
 * the 4 LSNs the design calls for:
 *
 *   safekeeper_id          - the safekeeper's slot index in walproposer
 *   commit_lsn             - LSN committed by quorum (Paxos safe commit)
 *   flush_lsn              - LSN this safekeeper fsync'd locally
 *   backup_lsn             - LSN uploaded to S3   (see note below: always NULL)
 *   remote_consistent_lsn  - LSN the pageserver applied & persisted
 *   reachable              - whether this safekeeper is currently active
 *   last_response_ms       - ms since the last AppendResponse mirrored, or NULL
 *
 * Data source: the walproposer process already tracks commit/flush LSN per
 * safekeeper and the pageserver remote_consistent_lsn piggybacked in each
 * safekeeper's feedback; feat-013 mirrors those into WalproposerShmemState on
 * every AppendResponse. We read that shared-memory snapshot here, issuing NO
 * new query to the safekeeper (real-time, uncached, zero safekeeper-side cost).
 *
 * backup_lsn is intentionally always NULL: it is a safekeeper-internal value
 * exposed only over the safekeeper HTTP /v1/timeline/<ttid>/status API and is
 * NOT carried in the walproposer append protocol. Reporting NULL (rather than
 * a fabricated value) is the fail-honest choice; wiring backup_lsn would
 * require a separate safekeeper query path, which the design explicitly avoids.
 *
 * When a safekeeper is not active (reachable=false) its LSN columns are NULL,
 * so a dead safekeeper yields one honest "all NULL, reachable=false" row while
 * the others return normally.
 */
PG_FUNCTION_INFO_V1(neon_get_safekeeper_lsns);
Datum
neon_get_safekeeper_lsns(PG_FUNCTION_ARGS)
{
	ReturnSetInfo *rsinfo = (ReturnSetInfo *) fcinfo->resultinfo;
	WalproposerShmemState *wp_shmem;
	uint32		num_safekeepers;
	uint8		status[MAX_SAFEKEEPERS];
	XLogRecPtr	commit_lsn[MAX_SAFEKEEPERS];
	XLogRecPtr	flush_lsn[MAX_SAFEKEEPERS];
	XLogRecPtr	rc_lsn[MAX_SAFEKEEPERS];
	TimestampTz	updated_at[MAX_SAFEKEEPERS];
	TimestampTz	now;

	InitMaterializedSRF(fcinfo, 0);

	/* Rollback path: emit no rows. */
	if (!neon_safekeeper_lsn_view_enabled)
		return (Datum) 0;

	wp_shmem = GetWalpropShmemState();

	/*
	 * Take a consistent snapshot under the existing walproposer mutex, then do
	 * all the (palloc'ing) tuple work outside the spinlock.
	 */
	SpinLockAcquire(&wp_shmem->mutex);
	num_safekeepers = wp_shmem->num_safekeepers;
	if (num_safekeepers > MAX_SAFEKEEPERS)
		num_safekeepers = MAX_SAFEKEEPERS;
	for (uint32 i = 0; i < num_safekeepers; i++)
	{
		status[i] = wp_shmem->safekeeper_status[i];
		commit_lsn[i] = wp_shmem->safekeeper_commit_lsn[i];
		flush_lsn[i] = wp_shmem->safekeeper_flush_lsn[i];
		rc_lsn[i] = wp_shmem->safekeeper_remote_consistent_lsn[i];
		updated_at[i] = wp_shmem->safekeeper_lsn_updated_at[i];
	}
	SpinLockRelease(&wp_shmem->mutex);

	now = GetCurrentTimestamp();

	for (uint32 i = 0; i < num_safekeepers; i++)
	{
		Datum		values[7];
		bool		nulls[7];
		bool		reachable = (status[i] == 1);

		memset(nulls, false, sizeof(nulls));

		/* safekeeper_id */
		values[0] = Int32GetDatum((int32) i);

		/* commit_lsn / flush_lsn / remote_consistent_lsn: NULL if unreachable */
		if (reachable)
		{
			values[1] = LSNGetDatum(commit_lsn[i]);
			values[2] = LSNGetDatum(flush_lsn[i]);
			/* remote_consistent_lsn may be 0 if no ps feedback seen yet */
			if (rc_lsn[i] != InvalidXLogRecPtr)
				values[4] = LSNGetDatum(rc_lsn[i]);
			else
				nulls[4] = true;
		}
		else
		{
			nulls[1] = true;
			nulls[2] = true;
			nulls[4] = true;
		}

		/* backup_lsn: not available via walproposer (fail-honest NULL) */
		nulls[3] = true;

		/* reachable */
		values[5] = BoolGetDatum(reachable);

		/*
		 * last_response_ms: ms since last mirrored AppendResponse, else NULL.
		 * "never responded" sentinel is DT_NOBEGIN (timestamp -infinity), NOT 0
		 * -- 0 is the valid PG epoch (2000-01-01 UTC) and a real response near
		 * that instant must not be misjudged as "never responded".
		 */
		if (!TIMESTAMP_IS_NOBEGIN(updated_at[i]))
		{
			long		secs;
			int			usecs;

			TimestampDifference(updated_at[i], now, &secs, &usecs);
			values[6] = Int64GetDatum((int64) secs * 1000 + usecs / 1000);
		}
		else
			nulls[6] = true;

		tuplestore_putvalues(rsinfo->setResult, rsinfo->setDesc, values, nulls);
	}

	return (Datum) 0;
}
