/*-------------------------------------------------------------------------
 *
 * test_sqlcommenter.c
 *	  Standalone unit tests for sqlcommenter.{h,c}.
 *
 * Build (via top-level Makefile target):
 *	  make -C pgxn/neon check-sqlcommenter
 *
 * Direct build (for local debugging):
 *	  cc -std=c11 -Wall -Wextra -Werror -O0 -g -I. \
 *		  trace_context.c sqlcommenter.c test/test_sqlcommenter.c \
 *		  -o test/sqlcommenter_test
 *
 * Exit code 0 = all PASS, non-zero = at least one FAIL.
 *
 *-------------------------------------------------------------------------
 */
#include "sqlcommenter.h"
#include "trace_context.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static int	g_pass;
static int	g_fail;

#define EXPECT(cond, name)											\
	do {															\
		if (cond) {													\
			g_pass++;												\
			printf("ok   - %s\n", name);							\
		} else {													\
			g_fail++;												\
			printf("FAIL - %s (line %d)\n", name, __LINE__);		\
		}															\
	} while (0)

/*
 * Reference value used by several tests:
 *	  trace_id  = 0af7651916cd43dd8448eb211c80319c
 *	  parent_id = b7ad6b7169203331
 *	  flags     = 01 (sampled)
 */
static const char REF_TP[] =
"00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01";

static bool
tp_matches_reference(const struct trace_context *tc)
{
	static const uint8_t expect_trace_id[16] = {
		0x0a, 0xf7, 0x65, 0x19, 0x16, 0xcd, 0x43, 0xdd,
		0x84, 0x48, 0xeb, 0x21, 0x1c, 0x80, 0x31, 0x9c
	};
	static const uint8_t expect_parent_id[8] = {
		0xb7, 0xad, 0x6b, 0x71, 0x69, 0x20, 0x33, 0x31
	};

	if (tc->version != 0x00)
		return false;
	if (memcmp(tc->trace_id, expect_trace_id, 16) != 0)
		return false;
	if (memcmp(tc->parent_id, expect_parent_id, 8) != 0)
		return false;
	if (tc->trace_flags != 0x01)
		return false;
	return true;
}

/* ---------------- Case 1: standard traceparent ----------------------- */
static void
test_standard_traceparent(void)
{
	const char *sql =
	"SELECT 1 /*traceparent='00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01'*/";
	struct trace_context tc;
	char	   *ts = NULL;

	bool		ok = sqlcommenter_extract_traceparent(sql, &tc, &ts);

	EXPECT(ok, "extract: standard traceparent returns true");
	EXPECT(tp_matches_reference(&tc), "extract: standard traceparent decoded correctly");
	EXPECT(ts == NULL, "extract: tracestate absent leaves out NULL");
	free(ts);
}

/* ---------------- Case 2: multi-KV with tracestate ------------------- */
static void
test_multi_kv(void)
{
	const char *sql =
	"UPDATE users SET name = 'foo' WHERE id = 7 "
	"/*action='checkout',traceparent='00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01',tracestate='neon%3Dproxy'*/";
	struct trace_context tc;
	char	   *ts = NULL;

	bool		ok = sqlcommenter_extract_traceparent(sql, &tc, &ts);

	EXPECT(ok, "extract: multi-KV traceparent picked up");
	EXPECT(tp_matches_reference(&tc), "extract: multi-KV decoded correctly");
	EXPECT(ts != NULL && strcmp(ts, "neon=proxy") == 0,
		   "extract: tracestate URL-decoded");
	free(ts);
}

/* ---------------- Case 3: URL-encoded traceparent value -------------- */
static void
test_url_encoded_value(void)
{
	/* Encode '-' chars as %2D to force decoder path. */
	const char *sql =
	"SELECT * FROM t "
	"/*traceparent='00%2D0af7651916cd43dd8448eb211c80319c%2Db7ad6b7169203331%2D01'*/";
	struct trace_context tc;
	char	   *ts = NULL;

	bool		ok = sqlcommenter_extract_traceparent(sql, &tc, &ts);

	EXPECT(ok, "extract: URL-encoded value still parses");
	EXPECT(tp_matches_reference(&tc), "extract: URL-encoded value decoded correctly");
	free(ts);
}

/* ---------------- Case 4: version=99 lenient acceptance -------------- */
static void
test_lenient_future_version(void)
{
	const char *sql =
	"SELECT 1 /*traceparent='99-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01'*/";
	struct trace_context tc;

	bool		ok = sqlcommenter_extract_traceparent(sql, &tc, NULL);

	/*
	 * W3C §3.2.2.3: receivers SHOULD forward-compat across unknown
	 * versions. lenient parser accepts 0x99.
	 */
	EXPECT(ok, "extract: version=99 accepted via lenient parser");
	EXPECT(tc.version == 0x99, "extract: version=99 preserved");
}

/* ---------------- Case 5: malformed comment / no closing ------------- */
static void
test_malformed_comment(void)
{
	const char *sql_no_close =
	"SELECT 1 /*traceparent='00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01'";
	const char *sql_no_quote =
	"SELECT 1 /*traceparent=00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01*/";
	const char *sql_double_quote =
	"SELECT 1 /*traceparent=\"00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01\"*/";
	struct trace_context tc;

	EXPECT(!sqlcommenter_extract_traceparent(sql_no_close, &tc, NULL),
		   "extract: unterminated block rejected");
	EXPECT(!sqlcommenter_extract_traceparent(sql_no_quote, &tc, NULL),
		   "extract: unquoted value rejected");
	EXPECT(!sqlcommenter_extract_traceparent(sql_double_quote, &tc, NULL),
		   "extract: double-quoted value rejected (single-quote only)");
}

/* ---------------- Case 6: no comment at all -------------------------- */
static void
test_no_comment(void)
{
	const char *sql_plain = "SELECT 1";
	const char *sql_leading_only =
	"/*traceparent='00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01'*/ SELECT 1";
	struct trace_context tc;

	EXPECT(!sqlcommenter_extract_traceparent(sql_plain, &tc, NULL),
		   "extract: plain SQL returns false");
	EXPECT(!sqlcommenter_extract_traceparent(sql_leading_only, &tc, NULL),
		   "extract: LEADING (non-trailing) comment is NOT extracted "
		   "(spec mandates trailing position)");
}

/* ---------------- Case 7: trailing semicolon + whitespace ------------ */
static void
test_trailing_semicolon(void)
{
	const char *sql =
	"SELECT 1 /*traceparent='00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01'*/   ;  ";
	struct trace_context tc;

	bool		ok = sqlcommenter_extract_traceparent(sql, &tc, NULL);

	EXPECT(ok, "extract: trailing `;` and whitespace tolerated");
	EXPECT(tp_matches_reference(&tc), "extract: post-`;` decode correct");
}

/* ---------------- Case 8: SQL with slash-star inside string literal -- */
static void
test_slash_star_in_string(void)
{
	/*
	 * The slash-star inside the string literal must NOT confuse the
	 * locator — because the locator scans from the END of the input
	 * backwards looking for a star-slash, the literal cannot become a
	 * false trailing block.
	 */
	const char *sql_no_comment =
	"SELECT '/*not_a_comment*/'";
	const char *sql_with_real =
	"SELECT '/*not_a_comment*/' "
	"/*traceparent='00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01'*/";
	struct trace_context tc;

	EXPECT(!sqlcommenter_extract_traceparent(sql_no_comment, &tc, NULL),
		   "extract: /*..*/ inside string literal is NOT a sqlcommenter "
		   "(it's not the trailing token, so spec says ignore)");

	bool		ok = sqlcommenter_extract_traceparent(sql_with_real, &tc, NULL);

	EXPECT(ok, "extract: real trailing block still found despite earlier literal");
	EXPECT(tp_matches_reference(&tc), "extract: literal-then-real decode correct");
}

/* ---------------- Inject: round-trip --------------------------------- */
static void
test_inject_round_trip(void)
{
	struct trace_context tc = {0};

	memcpy(tc.trace_id,
		   "\x0a\xf7\x65\x19\x16\xcd\x43\xdd"
		   "\x84\x48\xeb\x21\x1c\x80\x31\x9c", 16);
	memcpy(tc.parent_id, "\xb7\xad\x6b\x71\x69\x20\x33\x31", 8);
	tc.trace_flags = 0x01;

	char	   *injected = sqlcommenter_inject_traceparent("SELECT 1",
														   &tc, NULL);

	EXPECT(injected != NULL, "inject: returns non-NULL");
	EXPECT(strstr(injected, "/*traceparent='") != NULL,
		   "inject: contains traceparent block");
	EXPECT(strstr(injected, REF_TP) != NULL,
		   "inject: serializes lowercase wire value");
	EXPECT(strstr(injected, "*/") != NULL, "inject: closes block");

	/* Round-trip via extractor. */
	struct trace_context tc2;

	bool		ok = sqlcommenter_extract_traceparent(injected, &tc2, NULL);

	EXPECT(ok, "inject: round-trip via extract succeeds");
	EXPECT(memcmp(tc.trace_id, tc2.trace_id, 16) == 0,
		   "inject: round-trip trace_id matches");
	EXPECT(memcmp(tc.parent_id, tc2.parent_id, 8) == 0,
		   "inject: round-trip parent_id matches");
	EXPECT(tc.trace_flags == tc2.trace_flags,
		   "inject: round-trip flags match");
	free(injected);
}

/* ---------------- Inject: with tracestate (path β: neon=proxy) - */
static void
test_inject_with_tracestate(void)
{
	struct trace_context tc = {0};

	memcpy(tc.trace_id,
		   "\x0a\xf7\x65\x19\x16\xcd\x43\xdd"
		   "\x84\x48\xeb\x21\x1c\x80\x31\x9c", 16);
	memcpy(tc.parent_id, "\xb7\xad\x6b\x71\x69\x20\x33\x31", 8);
	tc.trace_flags = 0x01;

	char	   *injected = sqlcommenter_inject_traceparent(
		"START_WAL_PUSH (proto_version '3', allow_timeline_creation 'true')",
		&tc, "neon=proxy");

	EXPECT(injected != NULL, "inject: with tracestate non-NULL");
	/*
	 * url_encode uses lowercase hex for the %XX escapes (consistent with
	 * W3C §3.2.2 "all hex characters MUST be lowercase" for trace_id /
	 * parent_id; RFC 3986 allows either case but recommends uppercase,
	 * and our decoder accepts both — we pick lowercase for symmetry).
	 */
	EXPECT(strstr(injected, "tracestate='neon%3dproxy'") != NULL,
		   "inject: tracestate URL-encoded into single-quoted value");

	/* Round-trip via extractor. */
	struct trace_context tc2;
	char	   *ts = NULL;

	bool		ok = sqlcommenter_extract_traceparent(injected, &tc2, &ts);

	EXPECT(ok, "inject: tracestate round-trip extract succeeds");
	EXPECT(ts != NULL && strcmp(ts, "neon=proxy") == 0,
		   "inject: tracestate decoded back to original");
	free(ts);
	free(injected);
}

/* ---------------- Inject: idempotent (no double-tagging) ------------- */
static void
test_inject_idempotent(void)
{
	struct trace_context tc = {0};

	memcpy(tc.trace_id,
		   "\x0a\xf7\x65\x19\x16\xcd\x43\xdd"
		   "\x84\x48\xeb\x21\x1c\x80\x31\x9c", 16);
	memcpy(tc.parent_id, "\xb7\xad\x6b\x71\x69\x20\x33\x31", 8);
	tc.trace_flags = 0x01;

	const char *already_tagged =
	"SELECT 1 /*traceparent='00-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-bbbbbbbbbbbbbbbb-01'*/";

	char	   *injected = sqlcommenter_inject_traceparent(already_tagged,
														   &tc, NULL);

	EXPECT(injected != NULL, "inject: re-tag returns non-NULL");
	{
		int			count = 0;
		const char *p = injected;

		while ((p = strstr(p, "traceparent=")) != NULL)
		{
			count++;
			p++;
		}
		EXPECT(count == 1, "inject: re-tag results in exactly ONE traceparent block");
	}
	EXPECT(strstr(injected, REF_TP) != NULL,
		   "inject: re-tag uses NEW value, not old one");
	free(injected);
}

int
main(void)
{
	test_standard_traceparent();
	test_multi_kv();
	test_url_encoded_value();
	test_lenient_future_version();
	test_malformed_comment();
	test_no_comment();
	test_trailing_semicolon();
	test_slash_star_in_string();
	test_inject_round_trip();
	test_inject_with_tracestate();
	test_inject_idempotent();

	printf("\n--- summary: %d pass, %d fail ---\n", g_pass, g_fail);
	return g_fail == 0 ? 0 : 1;
}
