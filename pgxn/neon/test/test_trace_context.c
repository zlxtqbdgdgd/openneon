/*-------------------------------------------------------------------------
 *
 * test_trace_context.c
 *	  Standalone unit tests for trace_context.{h,c}.
 *
 * Build:
 *	  gcc -std=c11 -Wall -Wextra -Werror -O0 -g \
 *		  trace_context.c test_trace_context.c -o test_trace_context
 *
 * Run:
 *	  ./test_trace_context
 *
 * Exit code 0 = all PASS, non-zero = at least one FAIL.
 *
 *-------------------------------------------------------------------------
 */
#include "trace_context.h"

#include <stdio.h>
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

static void
test_parse_valid_v00(void)
{
	const char *in = "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01";
	struct trace_context tc;

	EXPECT(trace_context_parse(in, &tc), "valid v00 parses");
	EXPECT(tc.version == 0x00, "valid v00 version=0x00");

	/* trace_id: 0a f7 65 19 16 cd 43 dd 84 48 eb 21 1c 80 31 9c */
	static const uint8_t expect_trace[16] = {
		0x0a, 0xf7, 0x65, 0x19, 0x16, 0xcd, 0x43, 0xdd,
		0x84, 0x48, 0xeb, 0x21, 0x1c, 0x80, 0x31, 0x9c
	};
	EXPECT(memcmp(tc.trace_id, expect_trace, 16) == 0,
		   "valid v00 trace_id bytes");

	/* parent_id: b7 ad 6b 71 69 20 33 31 */
	static const uint8_t expect_parent[8] = {
		0xb7, 0xad, 0x6b, 0x71, 0x69, 0x20, 0x33, 0x31
	};
	EXPECT(memcmp(tc.parent_id, expect_parent, 8) == 0,
		   "valid v00 parent_id bytes");
	EXPECT(tc.trace_flags == 0x01, "valid v00 flags=0x01 (sampled)");
}

static void
test_parse_uppercase_hex_normalized(void)
{
	/* Same trace as test_parse_valid_v00 but with uppercase hex. Spec
	 * says wire MUST be lowercase, but parsers SHOULD accept upper for
	 * robustness — we accept and normalize via the decoded bytes. */
	const char *in = "00-0AF7651916CD43DD8448EB211C80319C-B7AD6B7169203331-01";
	struct trace_context tc;

	EXPECT(trace_context_parse(in, &tc), "uppercase hex accepted");
	EXPECT(tc.trace_id[0] == 0x0a && tc.trace_id[15] == 0x9c,
		   "uppercase hex decoded to same bytes");

	/* Round-trip: serialize must emit lowercase. */
	char		buf[TRACE_CONTEXT_BUF_SIZE];
	int			n = trace_context_serialize(&tc, buf, sizeof(buf));

	EXPECT(n == TRACE_CONTEXT_WIRE_LEN, "serialize returns 55");
	EXPECT(strcmp(buf,
				  "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01") == 0,
		   "round-trip normalizes to lowercase");
}

static void
test_parse_rejects_wrong_length(void)
{
	struct trace_context tc;

	/* Too short. */
	EXPECT(!trace_context_parse("00-abc", &tc),
		   "reject too-short input");

	/* Too long (extra char after the 55th). */
	const char *too_long =
		"00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01X";
	EXPECT(!trace_context_parse(too_long, &tc),
		   "reject too-long input");

	/* Exactly 55 chars but no NUL terminator at position 55 — caller
	 * must NUL-terminate; we already verify via the trailing-NUL check
	 * in the parser, so passing a 56-byte string with extra char is
	 * covered by the case above. */
}

static void
test_parse_rejects_bad_hex(void)
{
	struct trace_context tc;

	/* Non-hex char 'g' inside trace_id. */
	const char *bad_hex =
		"00-0gf7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01";
	EXPECT(!trace_context_parse(bad_hex, &tc),
		   "reject non-hex char in trace_id");

	/* Non-hex char in parent_id. */
	const char *bad_parent =
		"00-0af7651916cd43dd8448eb211c80319c-z7ad6b7169203331-01";
	EXPECT(!trace_context_parse(bad_parent, &tc),
		   "reject non-hex char in parent_id");

	/* Non-hex char in flags. */
	const char *bad_flags =
		"00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-0z";
	EXPECT(!trace_context_parse(bad_flags, &tc),
		   "reject non-hex char in flags");

	/* Wrong delimiter at dash positions. */
	const char *bad_dash =
		"00:0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01";
	EXPECT(!trace_context_parse(bad_dash, &tc),
		   "reject wrong delimiter");
}

static void
test_parse_rejects_all_zero_trace_id(void)
{
	struct trace_context tc;
	const char *all_zero_trace =
		"00-00000000000000000000000000000000-b7ad6b7169203331-01";

	EXPECT(!trace_context_parse(all_zero_trace, &tc),
		   "reject all-zero trace_id (W3C §3.2.2.2)");
}

static void
test_parse_rejects_all_zero_parent_id(void)
{
	struct trace_context tc;
	const char *all_zero_parent =
		"00-0af7651916cd43dd8448eb211c80319c-0000000000000000-01";

	EXPECT(!trace_context_parse(all_zero_parent, &tc),
		   "reject all-zero parent_id (W3C §3.2.2.3)");
}

static void
test_parse_rejects_non_v00(void)
{
	struct trace_context tc;
	const char *v99 =
		"99-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01";

	EXPECT(!trace_context_parse(v99, &tc),
		   "reject non-00 version (per ADR-0010)");

	const char *vff =
		"ff-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01";

	EXPECT(!trace_context_parse(vff, &tc),
		   "reject 0xff version (spec-reserved invalid)");
}

static void
test_parse_rejects_null(void)
{
	struct trace_context tc;

	EXPECT(!trace_context_parse(NULL, &tc), "reject NULL input");
	EXPECT(!trace_context_parse(
			   "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01",
			   NULL),
		   "reject NULL out");
}

static void
test_serialize_basic(void)
{
	struct trace_context tc = {
		.version = 0x00,
		.trace_id = {
			0x0a, 0xf7, 0x65, 0x19, 0x16, 0xcd, 0x43, 0xdd,
			0x84, 0x48, 0xeb, 0x21, 0x1c, 0x80, 0x31, 0x9c
		},
		.parent_id = {
			0xb7, 0xad, 0x6b, 0x71, 0x69, 0x20, 0x33, 0x31
		},
		.trace_flags = 0x01,
	};
	char		buf[TRACE_CONTEXT_BUF_SIZE];
	int			n = trace_context_serialize(&tc, buf, sizeof(buf));

	EXPECT(n == TRACE_CONTEXT_WIRE_LEN, "serialize returns 55");
	EXPECT(strcmp(buf,
				  "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01") == 0,
		   "serialize emits expected wire form");
	EXPECT(buf[TRACE_CONTEXT_WIRE_LEN] == '\0',
		   "serialize NUL-terminates");
}

static void
test_serialize_buffer_too_small(void)
{
	struct trace_context tc = {.version = 0};

	memset(tc.trace_id, 0xab, 16);
	memset(tc.parent_id, 0xcd, 8);
	tc.trace_flags = 0;

	char		small[TRACE_CONTEXT_WIRE_LEN];	/* one byte too small */
	int			n = trace_context_serialize(&tc, small, sizeof(small));

	EXPECT(n == -1, "serialize rejects too-small buffer");
}

static void
test_serialize_flags_high_bit(void)
{
	struct trace_context tc;

	memset(tc.trace_id, 0x11, 16);
	memset(tc.parent_id, 0x22, 8);
	tc.version = 0;
	tc.trace_flags = 0xff;

	char		buf[TRACE_CONTEXT_BUF_SIZE];

	(void) trace_context_serialize(&tc, buf, sizeof(buf));
	/* last two chars are flags hex */
	EXPECT(buf[TRACE_CONTEXT_WIRE_LEN - 2] == 'f' &&
		   buf[TRACE_CONTEXT_WIRE_LEN - 1] == 'f',
		   "serialize emits 0xff flags as 'ff'");
}

static void
test_round_trip(void)
{
	/* 32 hex chars in trace_id, 16 hex chars in parent_id. */
	const char *original =
		"00-deadbeefcafebabe1234567890abcdef-fedcba9876543210-00";
	struct trace_context tc;

	EXPECT(trace_context_parse(original, &tc), "round-trip parse");

	char		buf[TRACE_CONTEXT_BUF_SIZE];

	(void) trace_context_serialize(&tc, buf, sizeof(buf));
	EXPECT(strcmp(original, buf) == 0, "round-trip serialize matches input");
}

int
main(void)
{
	test_parse_valid_v00();
	test_parse_uppercase_hex_normalized();
	test_parse_rejects_wrong_length();
	test_parse_rejects_bad_hex();
	test_parse_rejects_all_zero_trace_id();
	test_parse_rejects_all_zero_parent_id();
	test_parse_rejects_non_v00();
	test_parse_rejects_null();
	test_serialize_basic();
	test_serialize_buffer_too_small();
	test_serialize_flags_high_bit();
	test_round_trip();

	printf("\n--- summary: %d passed, %d failed ---\n", g_pass, g_fail);
	return g_fail == 0 ? 0 : 1;
}
