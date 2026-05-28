/*-------------------------------------------------------------------------
 *
 * sqlcommenter.h
 *	  SQLCommenter v1 lexer / injector for trace propagation through SQL
 *	  text comments (feat-034).
 *
 * SQLCommenter is the Google-originated convention of appending a
 * machine-readable `/ *key='value',key='value'* /` block (no spaces in
 * the actual wire form; the spaces here are only to keep this enclosing
 * C block comment well-formed) to the END of a SQL statement so
 * middleware can extract correlation IDs without changing the wire
 * protocol. Adopted by sqlcommenter-python, Datadog DBM, OpenTelemetry
 * SQL instrumentation, sqlcommenter-ruby. Spec:
 *	  https://google.github.io/sqlcommenter/spec/
 *
 * Format summary (W3C TraceContext + sqlcommenter):
 *	  SELECT ... / *traceparent='00-{32hex}-{16hex}-{2hex}',tracestate='neon=root=proxy',action='checkout'* /
 *	  (spaces around the slash-star delimiters are only for the
 *	  surrounding C comment; the wire format has no spaces there.)
 *
 * Rules implemented here:
 *	  - Block comments only (slash-star/star-slash); line comments
 *	    (`-- ...`) are NOT touched because the spec only defines
 *	    block-comment form.
 *	  - The KV block must be the LAST trailing comment of the statement
 *	    (sqlcommenter spec §"Output prepending or appending"). We scan
 *	    from end of input backwards: strip trailing whitespace, then look
 *	    for a star-slash terminator and matching slash-star opener. Any
 *	    non-comment content after a candidate block disqualifies it.
 *	  - KV pairs are `key='value'` separated by `,`. Whitespace around `,`
 *	    and `=` is tolerated.
 *	  - Values are URL-encoded per sqlcommenter spec (percent-encoding).
 *	    A %XX hex pair decodes to the byte (0x00..0xff); invalid escapes
 *	    cause the value to be rejected silently.
 *	  - Per spec §"Value serialization", the value must be wrapped in
 *	    SINGLE quotes. Double quotes are NOT accepted.
 *	  - SQL string literals containing the bytes slash-star are NOT
 *	    mistaken for comments because we only look at the TRAILING
 *	    comment block (any SQL after the candidate star-slash would
 *	    disqualify it).
 *
 * Zero external dependencies (stdint / stdbool / stddef / string only) so
 * this module can be unit-tested standalone (no postgres.h required) and
 * linked from both the backend extension (path α hook) and walproposer
 * (path β injector).
 *
 *-------------------------------------------------------------------------
 */
#ifndef SQLCOMMENTER_H
#define SQLCOMMENTER_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#include "trace_context.h"

/*
 * Scan the trailing block-comment of `sql` (NUL-terminated) for a
 * `traceparent='...'` KV pair, URL-decode the value, validate it against
 * W3C v00 grammar via trace_context_parse_lenient(), and fill *out.
 *
 * Returns:
 *	  true  - a valid traceparent was extracted (out filled)
 *	  false - sql is NULL / no trailing comment / no traceparent key /
 *	          value not in single quotes / URL-decode failure /
 *	          traceparent not parseable. In all failure cases *out is
 *	          unspecified and callers MUST treat extraction as a silent
 *	          no-op (do not error the query).
 *
 * The caller may pass NULL for `tracestate_out`. If non-NULL and a
 * `tracestate=` KV is present, a freshly-malloc'd NUL-terminated
 * URL-decoded copy is written to *tracestate_out (caller frees with
 * free()). If absent or invalid, *tracestate_out is set to NULL.
 *
 * This function never modifies `sql`.
 */
extern bool sqlcommenter_extract_traceparent(const char *sql,
											 struct trace_context *out,
											 char **tracestate_out);

/*
 * Build a new SQL string equal to `sql` plus a trailing SQLCommenter
 * block carrying the serialized traceparent and (optionally) tracestate.
 *
 * Output shape (no leading whitespace inside the comment, single quotes
 * around values per spec, lowercase hex per W3C §3.2.2):
 *	  <sql> / *traceparent='00-{32hex}-{16hex}-{2hex}'* /
 *	  <sql> / *traceparent='00-...',tracestate='neon=root=proxy'* /
 *	  (spaces around the slash-star delimiters are only for this
 *	  surrounding C comment; the wire output is contiguous.)
 *
 * Memory: returned buffer is malloc'd (caller must free()). NULL on any
 * input error (sql NULL, tc NULL, tc invalid for v00 serialization).
 *
 * If `sql` already ends with a SQLCommenter comment that contains a
 * traceparent KV, this function REPLACES that block rather than appending
 * a second one (avoids double-tagging when path β fires on a query that
 * was already tagged by path α upstream).
 *
 * `tracestate` may be NULL (omits the tracestate KV). When non-NULL it
 * must be a NUL-terminated C string consisting of printable ASCII; bytes
 * outside `0x20..0x7e` and the single-quote / backslash bytes are
 * percent-encoded by the injector so the output stays parseable.
 */
extern char *sqlcommenter_inject_traceparent(const char *sql,
											 const struct trace_context *tc,
											 const char *tracestate);

#endif							/* SQLCOMMENTER_H */
