/*-------------------------------------------------------------------------
 *
 * trace_context.h
 *	  W3C TraceContext (traceparent header v00) serializer / parser.
 *
 * Zero external dependencies (stdint / stdbool / stddef only). Designed to
 * be linked from:
 *	  - libpq pagestream client (feat-033)
 *	  - SQLCommenter injector (feat-034)
 *	  - safekeeper / pageserver C glue (feat-035 / feat-065)
 *
 * The on-the-wire format is the W3C Trace Context "traceparent" header,
 * version 00:
 *
 *	  00-<32 hex trace_id>-<16 hex parent_id>-<2 hex flags>
 *
 * Reference: https://www.w3.org/TR/trace-context/#traceparent-header
 *
 *-------------------------------------------------------------------------
 */
#ifndef TRACE_CONTEXT_H
#define TRACE_CONTEXT_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

/*
 * Wire-format byte length of a v00 traceparent value: "00-...-...-..".
 * 2 + 1 + 32 + 1 + 16 + 1 + 2 = 55 bytes (no trailing NUL).
 */
#define TRACE_CONTEXT_WIRE_LEN 55

/*
 * Minimum buffer size to pass to trace_context_serialize(); includes the
 * trailing NUL terminator.
 */
#define TRACE_CONTEXT_BUF_SIZE (TRACE_CONTEXT_WIRE_LEN + 1)

/* Sampled flag bit (W3C §3.2.2.4 "trace-flags"). */
#define TRACE_CONTEXT_FLAG_SAMPLED 0x01

/*
 * Decoded W3C traceparent v00. trace_id / parent_id are stored big-endian
 * (network byte order = hex order), so memcmp on the byte arrays matches
 * lexicographic compare of the hex form.
 */
struct trace_context
{
	uint8_t		version;			/* always 0x00 for v00 */
	uint8_t		trace_id[16];		/* 128-bit trace id */
	uint8_t		parent_id[8];		/* 64-bit parent span id */
	uint8_t		trace_flags;
};

/*
 * Parse a traceparent header value. Returns true on success and fills *out.
 *
 * Strict W3C v00 validation:
 *	  - input must be exactly TRACE_CONTEXT_WIRE_LEN bytes (caller must
 *		ensure NUL-terminated string of that length, or pass a buffer of
 *		at least that length; we read TRACE_CONTEXT_WIRE_LEN bytes and
 *		check that input[TRACE_CONTEXT_WIRE_LEN] is '\0' to reject longer)
 *	  - version field must be "00" (other versions are rejected here;
 *		spec §3.2.2.1 says forward-compat parsers may accept, but we are
 *		a fresh implementation and per ADR-0010 we only emit/accept v00)
 *	  - hex digits may be lower- or upper-case on input; the decoded
 *		bytes are case-independent
 *	  - trace_id must not be all zero (spec §3.2.2.2)
 *	  - parent_id must not be all zero (spec §3.2.2.3)
 *	  - the three dashes must be at the exact offsets
 *
 * On failure, *out is left in an unspecified state.
 */
extern bool trace_context_parse(const char *input, struct trace_context *out);

/*
 * Serialize a trace_context into buf. Returns the number of bytes written
 * not counting the trailing NUL (i.e. TRACE_CONTEXT_WIRE_LEN on success),
 * or -1 if buflen is too small. The output is lowercase hex per W3C
 * §3.2.2 "all hex characters MUST be lowercase".
 *
 * Caller must pass buflen >= TRACE_CONTEXT_BUF_SIZE.
 *
 * Note: serialize does NOT validate that trace_id / parent_id are non-zero
 * — that is the caller's responsibility (typically the OTel SDK ensures
 * this). serialize always writes version byte as "00".
 */
extern int	trace_context_serialize(const struct trace_context *in,
									char *buf, size_t buflen);

#endif							/* TRACE_CONTEXT_H */
