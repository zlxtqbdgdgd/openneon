/*-------------------------------------------------------------------------
 *
 * test_jsonlog_ext.c
 *	  Standalone unit tests for feat-036 jsonlog_ext.c.
 *
 * 走 B1 sqlcommenter 引入的 anchor 模式: 不链接 PG, 不依赖 vendor 编译,
 * 自带 StringInfo / appendStringInfo* / GetConfigOption stub, 直接喂
 * 构造的 GUC 值 + buf, 验 jsonlog_ext_append_fields 拼接结果.
 *
 * 编译: cc -std=c11 -DNEON_JSONLOG_STANDALONE_TEST \
 *           -I. -o test_jsonlog_ext test_jsonlog_ext.c ../jsonlog_ext.c
 * 运行: ./test_jsonlog_ext  (0 = all green)
 *
 *-------------------------------------------------------------------------
 */
#include <assert.h>
#include <stdarg.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

/* ---- Minimal PG shim ---- */

typedef struct StringInfoData
{
	char	   *data;
	int			len;
	int			maxlen;
}			StringInfoData;
typedef StringInfoData *StringInfo;

#define Assert(x) assert(x)
#define PGDLLIMPORT

#define GUC_LIST_INPUT 0x0001
#define GUC_LIST_QUOTE 0x0002
typedef enum
{
	PGC_SIGHUP,
	PGC_POSTMASTER
}			GucContext;
typedef enum
{
	PGC_S_DEFAULT
}			GucSource;

static void
initStringInfo(StringInfo str)
{
	int			size = 1024;

	str->data = (char *) malloc(size);
	str->data[0] = '\0';
	str->len = 0;
	str->maxlen = size;
}

static void
sb_ensure(StringInfo str, int needed)
{
	if (str->len + needed + 1 >= str->maxlen)
	{
		while (str->len + needed + 1 >= str->maxlen)
			str->maxlen *= 2;
		str->data = (char *) realloc(str->data, str->maxlen);
	}
}

void
appendStringInfoString(StringInfo str, const char *s)
{
	int			n = (int) strlen(s);

	sb_ensure(str, n);
	memcpy(str->data + str->len, s, n);
	str->len += n;
	str->data[str->len] = '\0';
}

void
appendStringInfoChar(StringInfo str, char ch)
{
	sb_ensure(str, 1);
	str->data[str->len++] = ch;
	str->data[str->len] = '\0';
}

void
appendStringInfo(StringInfo str, const char *fmt,...)
{
	char		buf[64];
	va_list		ap;
	int			n;

	va_start(ap, fmt);
	n = vsnprintf(buf, sizeof(buf), fmt, ap);
	va_end(ap);
	if (n > 0)
	{
		sb_ensure(str, n);
		memcpy(str->data + str->len, buf, n);
		str->len += n;
		str->data[str->len] = '\0';
	}
}

/* GUC API stub - not used in NEON_JSONLOG_STANDALONE_TEST path */
typedef bool (*GucStringCheckHook) (char **newval, void **extra, GucSource source);
typedef void (*GucStringAssignHook) (const char *newval, void *extra);
typedef const char *(*GucShowHook) (void);

void
DefineCustomStringVariable(const char *name, const char *short_desc,
						   const char *long_desc, char **valueAddr,
						   const char *bootValue, GucContext context,
						   int flags, GucStringCheckHook check_hook,
						   GucStringAssignHook assign_hook,
						   GucShowHook show_hook)
{
	/* no-op in standalone test */
}

/* ---- USR GUC test dict ---- */

#define USR_GUC_MAX 8
static struct
{
	const char *key;
	const char *val;
}			usr_guc_dict[USR_GUC_MAX];
static int	usr_guc_n = 0;

static void
usr_guc_set(const char *k, const char *v)
{
	for (int i = 0; i < usr_guc_n; i++)
		if (strcmp(usr_guc_dict[i].key, k) == 0)
		{
			usr_guc_dict[i].val = v;
			return;
		}
	usr_guc_dict[usr_guc_n].key = k;
	usr_guc_dict[usr_guc_n].val = v;
	usr_guc_n++;
}

static void
usr_guc_clear(void)
{
	usr_guc_n = 0;
}

const char *
neon_jsonlog_test_get_usr_guc(const char *name)
{
	for (int i = 0; i < usr_guc_n; i++)
		if (strcmp(usr_guc_dict[i].key, name) == 0)
			return usr_guc_dict[i].val;
	return NULL;
}

/* ---- neon_trace_status weak symbol fake ---- */

/* When defined, jsonlog_ext.c picks it up via weak link. */

#include "jsonlog_ext.h"

/* Provide a real (non-weak) definition that jsonlog_ext.c will call. */
static bool fake_trace_valid = false;
static char fake_trace_id[33] = "";

bool
neon_trace_status_snapshot_for_jsonlog(NeonTraceIdsView *out)
{
	if (!fake_trace_valid)
		return false;
	memset(out, 0, sizeof(*out));
	out->valid = true;
	strncpy(out->trace_id_hex, fake_trace_id, 32);
	out->trace_id_hex[32] = '\0';
	return true;
}

/* Required by jsonlog_ext.c */
append_extra_jsonlog_fields_hook_type append_extra_jsonlog_fields_hook = NULL;

/* ---- Test cases ---- */

static int	passed = 0;
static int	failed = 0;

#define EXPECT_STR_EQ(label, got, want) do { \
	if (strcmp((got), (want)) == 0) { \
		printf("  PASS: %s\n", label); passed++; \
	} else { \
		printf("  FAIL: %s\n    want: %s\n    got : %s\n", label, want, got); failed++; \
	} \
} while (0)

#define EXPECT_EQ(label, got, want) do { \
	if ((got) == (want)) { \
		printf("  PASS: %s\n", label); passed++; \
	} else { \
		printf("  FAIL: %s  want=%d got=%d\n", label, (int)(want), (int)(got)); failed++; \
	} \
} while (0)

/*
 * mask parser is exposed via jsonlog_ext.c (non-static).
 * jsonlog_ext_append_fields reads from neon_jsonlog_extra_fields_mask
 * static — to drive test, we wrap the assign hook.
 */
extern uint8_t neon_jsonlog_parse_fields_to_mask(const char *raw);
extern void jsonlog_ext_append_fields(StringInfo buf);
extern void neon_jsonlog_append_kv(StringInfo buf, const char *key, const char *value);

/*
 * jsonlog_ext.c neon_jsonlog_extra_fields_mask is a static. We can't write
 * it directly. Instead, expose a helper used in tests by re-declaring via
 * a small accessor compiled into jsonlog_ext.c only under STANDALONE.
 * For now we use the assign callback via a forwarder.
 */
extern void test_force_mask(uint8_t m);

static void
test_parse_mask(void)
{
	printf("== parse_mask ==\n");
	EXPECT_EQ("empty NULL → 0", neon_jsonlog_parse_fields_to_mask(NULL), 0);
	EXPECT_EQ("empty str → 0", neon_jsonlog_parse_fields_to_mask(""), 0);
	EXPECT_EQ("whitespace-only → 0", neon_jsonlog_parse_fields_to_mask("  ,  "), 0);
	EXPECT_EQ("all 4 → 0x0F",
			  neon_jsonlog_parse_fields_to_mask("endpoint_id,branch_id,project_id,trace_id"),
			  0x0F);
	EXPECT_EQ("just trace_id → 0x08",
			  neon_jsonlog_parse_fields_to_mask("trace_id"), 0x08);
	EXPECT_EQ("endpoint_id+project_id → 0x05",
			  neon_jsonlog_parse_fields_to_mask("endpoint_id,project_id"), 0x05);
	EXPECT_EQ("with spaces → tolerant",
			  neon_jsonlog_parse_fields_to_mask(" endpoint_id , trace_id "),
			  NEON_JSONLOG_FIELD_ENDPOINT_ID | NEON_JSONLOG_FIELD_TRACE_ID);
	EXPECT_EQ("unknown field silently ignored",
			  neon_jsonlog_parse_fields_to_mask("endpoint_id,foo,bar"),
			  NEON_JSONLOG_FIELD_ENDPOINT_ID);
	EXPECT_EQ("double comma OK",
			  neon_jsonlog_parse_fields_to_mask("endpoint_id,,branch_id"),
			  NEON_JSONLOG_FIELD_ENDPOINT_ID | NEON_JSONLOG_FIELD_BRANCH_ID);
	EXPECT_EQ("case-sensitive: ENDPOINT_ID unknown",
			  neon_jsonlog_parse_fields_to_mask("ENDPOINT_ID"), 0);
}

static void
test_kv_escape(void)
{
	StringInfoData buf;

	printf("== kv_escape ==\n");

	initStringInfo(&buf);
	neon_jsonlog_append_kv(&buf, "endpoint_id", "ep-foo-123");
	EXPECT_STR_EQ("simple", buf.data, ",\"endpoint_id\":\"ep-foo-123\"");
	free(buf.data);

	initStringInfo(&buf);
	neon_jsonlog_append_kv(&buf, "branch_id", "br_with\"quote\\back");
	EXPECT_STR_EQ("quote+backslash escape",
				  buf.data, ",\"branch_id\":\"br_with\\\"quote\\\\back\"");
	free(buf.data);

	initStringInfo(&buf);
	neon_jsonlog_append_kv(&buf, "trace_id", "abc\n\r\t");
	EXPECT_STR_EQ("control char escape",
				  buf.data, ",\"trace_id\":\"abc\\n\\r\\t\"");
	free(buf.data);

	initStringInfo(&buf);
	neon_jsonlog_append_kv(&buf, "project_id", "\x01\x1f");
	EXPECT_STR_EQ("low-control escape",
				  buf.data, ",\"project_id\":\"\\u0001\\u001f\"");
	free(buf.data);
}

static void
test_hook_default_mask(void)
{
	StringInfoData buf;

	printf("== hook (default mask=0F · all 4 fields) ==\n");

	usr_guc_clear();
	usr_guc_set("neon.endpoint_id", "ep-prod-7");
	usr_guc_set("neon.branch_id", "br-main");
	usr_guc_set("neon.project_id", "proj-acme");
	fake_trace_valid = true;
	strcpy(fake_trace_id, "0123456789abcdef0123456789abcdef");
	test_force_mask(0x0F);

	initStringInfo(&buf);
	jsonlog_ext_append_fields(&buf);
	EXPECT_STR_EQ("all 4 fields output",
				  buf.data,
				  ",\"endpoint_id\":\"ep-prod-7\""
				  ",\"branch_id\":\"br-main\""
				  ",\"project_id\":\"proj-acme\""
				  ",\"trace_id\":\"0123456789abcdef0123456789abcdef\"");
	free(buf.data);
}

static void
test_hook_mask_zero_degrades(void)
{
	StringInfoData buf;

	printf("== hook (mask=0 · degrade) ==\n");

	usr_guc_clear();
	usr_guc_set("neon.endpoint_id", "ep-prod-7");
	fake_trace_valid = true;
	strcpy(fake_trace_id, "0123456789abcdef0123456789abcdef");
	test_force_mask(0);

	initStringInfo(&buf);
	jsonlog_ext_append_fields(&buf);
	EXPECT_STR_EQ("mask=0 → empty output (degrade to upstream PG jsonlog)",
				  buf.data, "");
	free(buf.data);
}

static void
test_hook_subset_mask(void)
{
	StringInfoData buf;

	printf("== hook (subset mask) ==\n");

	usr_guc_clear();
	usr_guc_set("neon.endpoint_id", "ep-prod-7");
	usr_guc_set("neon.branch_id", "br-main");
	usr_guc_set("neon.project_id", "proj-acme");
	fake_trace_valid = true;
	strcpy(fake_trace_id, "0123456789abcdef0123456789abcdef");

	/* only trace_id */
	test_force_mask(NEON_JSONLOG_FIELD_TRACE_ID);
	initStringInfo(&buf);
	jsonlog_ext_append_fields(&buf);
	EXPECT_STR_EQ("only trace_id",
				  buf.data,
				  ",\"trace_id\":\"0123456789abcdef0123456789abcdef\"");
	free(buf.data);

	/* endpoint_id + project_id */
	test_force_mask(NEON_JSONLOG_FIELD_ENDPOINT_ID | NEON_JSONLOG_FIELD_PROJECT_ID);
	initStringInfo(&buf);
	jsonlog_ext_append_fields(&buf);
	EXPECT_STR_EQ("endpoint_id + project_id",
				  buf.data,
				  ",\"endpoint_id\":\"ep-prod-7\""
				  ",\"project_id\":\"proj-acme\"");
	free(buf.data);
}

static void
test_hook_missing_guc(void)
{
	StringInfoData buf;

	printf("== hook (USR GUC empty / not set) ==\n");

	usr_guc_clear();				/* nothing set */
	fake_trace_valid = false;
	test_force_mask(0x0F);

	initStringInfo(&buf);
	jsonlog_ext_append_fields(&buf);
	EXPECT_STR_EQ("no USR GUC + no trace ctx → empty (no field falsely written)",
				  buf.data, "");
	free(buf.data);
}

static void
test_hook_partial_present(void)
{
	StringInfoData buf;

	printf("== hook (only endpoint_id set) ==\n");

	usr_guc_clear();
	usr_guc_set("neon.endpoint_id", "ep-only");
	fake_trace_valid = false;
	test_force_mask(0x0F);

	initStringInfo(&buf);
	jsonlog_ext_append_fields(&buf);
	EXPECT_STR_EQ("only endpoint_id appears (others silently missing)",
				  buf.data, ",\"endpoint_id\":\"ep-only\"");
	free(buf.data);
}

static void
test_hook_no_trace_status_symbol(void)
{
	/*
	 * NOTE: We can't unlink the weak symbol at runtime cleanly in C; this
	 * test exercises the path where neon_trace_status_snapshot_for_jsonlog
	 * is callable but returns false. The "symbol absent" case is implicitly
	 * covered by the weak-attribute fallback: if B1 #49 is not merged,
	 * the symbol resolves to NULL and the NULL check in jsonlog_ext.c
	 * short-circuits the trace_id append.
	 *
	 * Here we simulate by making fake return false (valid path).
	 */
	StringInfoData buf;

	printf("== hook (trace_status returns false) ==\n");

	usr_guc_clear();
	usr_guc_set("neon.endpoint_id", "ep-x");
	fake_trace_valid = false;
	test_force_mask(0x0F);

	initStringInfo(&buf);
	jsonlog_ext_append_fields(&buf);
	EXPECT_STR_EQ("trace_status false → trace_id omitted, endpoint_id still emitted",
				  buf.data, ",\"endpoint_id\":\"ep-x\"");
	free(buf.data);
}

int
main(void)
{
	printf("test_jsonlog_ext (feat-036)\n");
	printf("--------------------------------\n");

	test_parse_mask();
	test_kv_escape();
	test_hook_default_mask();
	test_hook_mask_zero_degrades();
	test_hook_subset_mask();
	test_hook_missing_guc();
	test_hook_partial_present();
	test_hook_no_trace_status_symbol();

	printf("--------------------------------\n");
	printf("PASS: %d  FAIL: %d\n", passed, failed);
	return failed == 0 ? 0 : 1;
}
