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
 * Scope note: this module handles ONLY the W3C "traceparent" header
 * (W3C §3.2). The companion "tracestate" header (W3C §3.3) is a separate
 * second HTTP / wire-level header carrying vendor-specific key-value
 * pairs; it is NOT processed here. Callers that need tracestate must
 * propagate it as a sibling field alongside the decoded trace_context.
 *
 * TODO(neon/structure): if trace-related files in pgxn/neon grow beyond
 * ~3, consider moving trace_context.{h,c} (and future trace_inject.c,
 * trace_parse_pagestream.c, ...) into a pgxn/neon/trace/ subdirectory.
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

/*
 * W3C §3.2.2.5 "Trace Flags" defined bits. The byte is treated as an
 * 8-bit bitmap with two currently-defined bits and the rest reserved.
 *
 *	  Bit 0 (0x01) = sampled.
 *	  Bit 1 (0x02) = random (W3C 2023+; signals trace-id was generated
 *					  from a uniformly random source).
 *	  Other bits   = reserved for future spec versions.
 *
 * Caller responsibilities (W3C §3.2.2.5):
 *	- As a SENDER (originating a trace_context locally): MUST set every
 *	  undefined bit to 0; only set the named constants below.
 *	- As a FORWARDER (passing through a parsed upstream value): SHOULD
 *	  preserve the entire trace_flags byte verbatim; do NOT clear or
 *	  reinterpret unknown bits.
 */
#define TRACE_CONTEXT_FLAG_SAMPLED 0x01
#define TRACE_CONTEXT_FLAG_RANDOM  0x02

/*
 * Decoded W3C traceparent v00. trace_id / parent_id are stored big-endian
 * (network byte order = hex order), so memcmp on the byte arrays matches
 * lexicographic compare of the hex form.
 *
 * The 'trace_flags' field is an opaque 8-bit bitmap; see comments on
 * TRACE_CONTEXT_FLAG_SAMPLED / TRACE_CONTEXT_FLAG_RANDOM above for
 * sender/forwarder discipline (W3C §3.2.2.5).
 */
struct trace_context
{
	uint8_t		version;			/* always 0x00 for v00 */
	uint8_t		trace_id[16];		/* 128-bit trace id */
	uint8_t		parent_id[8];		/* 64-bit parent span id */
	uint8_t		trace_flags;		/* W3C §3.2.2.5 bitmap; see flags above */
};

/*
 * Parse a traceparent header value (strict W3C v00). Returns true on
 * success and fills *out.
 *
 * Strict W3C v00 validation:
 *	  - 'input' must be a NUL-terminated C string of exactly
 *		TRACE_CONTEXT_WIRE_LEN (55) bytes followed by '\0', i.e. at
 *		least TRACE_CONTEXT_BUF_SIZE (56) readable bytes.
 *	  - version field must be "00" (strict; other versions are rejected
 *		— see trace_context_parse_lenient() for W3C §3.2.2.3 forward-
 *		compat behavior).
 *	  - hex digits may be lower- or upper-case on input; the decoded
 *		bytes are case-independent.
 *	  - trace_id must not be all zero (spec §3.2.2.2).
 *	  - parent_id must not be all zero (spec §3.2.2.3).
 *	  - the three dashes must be at the exact offsets.
 *
 * On failure, *out is left in an unspecified state.
 */
extern bool trace_context_parse(const char *input, struct trace_context *out);

/*
 * Parse a traceparent header value in W3C §3.2.2.3 forward-compat mode.
 * Returns true on success and fills *out.
 *
 * W3C §3.2.2.3 "Versioning of traceparent" says:
 *	  "Vendors MUST NOT reject a value due to an unrecognized version."
 *	  Future versions are still expected to extend the same 55-byte
 *	  prefix (version-trace_id-parent_id-flags) and only add new fields
 *	  after the flags, so a forward-compat parser can decode the prefix
 *	  even when version != 00.
 *
 * Lenient differences vs. trace_context_parse():
 *	  - Accepts any version byte in 0x00..0xfe (still rejects 0xff which
 *		is spec-reserved as an invalid version).
 *	  - Decodes the wire prefix (the first TRACE_CONTEXT_WIRE_LEN bytes)
 *		using the v00 layout; any trailing future-version fields are
 *		treated as unknown extensions and ignored.
 *	  - Still rejects NULL, length errors, non-hex chars, wrong
 *		delimiters, and all-zero trace_id / parent_id.
 *
 * Strict parsing remains the default. Wire-up paths (feat-033/034/035/
 * 065) should choose between the two APIs based on whether they are
 * acting as a sender (strict, since we only emit v00) or as a forwarder
 * receiving from an arbitrary upstream (lenient, to comply with §3.2.2.3).
 *
 * The 'input' contract is the same as trace_context_parse(): a
 * NUL-terminated C string with at least TRACE_CONTEXT_BUF_SIZE (56)
 * readable bytes.
 */
extern bool trace_context_parse_lenient(const char *input,
										struct trace_context *out);

/*
 * Serialize a trace_context into buf. Returns the number of bytes written
 * not counting the trailing NUL (i.e. TRACE_CONTEXT_WIRE_LEN on success),
 * or -1 if buflen is too small, pointers are NULL, or in->version is
 * non-zero (we only emit W3C v00 per ADR-0010). The output is lowercase
 * hex per W3C §3.2.2 "all hex characters MUST be lowercase".
 *
 * Caller must pass buflen >= TRACE_CONTEXT_BUF_SIZE.
 *
 * Note: serialize does NOT validate that trace_id / parent_id are non-zero
 * — that is the caller's responsibility (typically the OTel SDK ensures
 * this). serialize always writes version byte as "00" and rejects
 * (returns -1) when in->version != 0.
 */
extern int	trace_context_serialize(const struct trace_context *in,
									char *buf, size_t buflen);

#endif							/* TRACE_CONTEXT_H */
