/*-------------------------------------------------------------------------
 *
 * jsonlog_ext.c
 *	  feat-036 · Neon extension 端的 PG jsonlog 字段注入 hook 实现.
 *
 * 4 个 Neon 字段:
 *   - endpoint_id  · 取自 USR GUC `neon.endpoint_id`   (feat-008)
 *   - branch_id    · 取自 USR GUC `neon.branch_id`     (feat-010)
 *   - project_id   · 取自 USR GUC `neon.project_id`    (feat-011)
 *   - trace_id     · 取自 neon_trace_status (B1 #49)   per-backend slot
 *
 * 白名单 GUC: `neon.jsonlog_extra_fields` (string · PGC_SIGHUP)
 *   - 默认 `endpoint_id,branch_id,project_id,trace_id`
 *   - 设空 = 完全退化到上游 PG jsonlog (4 字段都不输出)
 *   - 子集 = 按逗号分隔白名单逐字段输出
 *
 * design source: feat-036-L3-neon-compute-jsonlog.html §3.3 + §3.4
 *
 *-------------------------------------------------------------------------
 */
#ifdef NEON_JSONLOG_STANDALONE_TEST
/*
 * Standalone mode: don't pull PG headers; instead share defs with the
 * test driver via test_pg_shim.h (sibling header committed alongside
 * test_jsonlog_ext.c).
 */
#include <stdint.h>
#include <stdbool.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <assert.h>
typedef uint8_t uint8;
#define Assert(x) assert(x)
#define PGDLLIMPORT

typedef struct StringInfoData
{
	char	   *data;
	int			len;
	int			maxlen;
}			StringInfoData;
typedef StringInfoData *StringInfo;

extern void appendStringInfoString(StringInfo str, const char *s);
extern void appendStringInfoChar(StringInfo str, char ch);
extern void appendStringInfo(StringInfo str, const char *fmt, ...);

#define GUC_LIST_INPUT 0x0001
#define GUC_LIST_QUOTE 0x0002
typedef enum { PGC_SIGHUP, PGC_POSTMASTER } GucContext;
typedef enum { PGC_S_DEFAULT } GucSource;
typedef bool (*GucStringCheckHook) (char **newval, void **extra, GucSource source);
typedef void (*GucStringAssignHook) (const char *newval, void *extra);
typedef const char *(*GucShowHook) (void);
extern void DefineCustomStringVariable(const char *name, const char *short_desc,
									   const char *long_desc, char **valueAddr,
									   const char *bootValue, GucContext context,
									   int flags, GucStringCheckHook check_hook,
									   GucStringAssignHook assign_hook,
									   GucShowHook show_hook);
#else
#include "postgres.h"
#include "lib/stringinfo.h"
#include "utils/builtins.h"
#include "utils/guc.h"
#endif

#include "jsonlog_ext.h"

/* ----------------------------------------------------------------------
 *	GUC 状态
 * ---------------------------------------------------------------------- */

/*
 * 用户可见原始 GUC 值 (assign hook 写; 不直接消费).
 */
static char *neon_jsonlog_extra_fields_raw = NULL;

/*
 * 解析后的 bit mask · 用于 hook fast-path 决策 (避免每条日志 strcmp 解析).
 * mask = 0 时整段 hook 直接 return —— 等价于退化到上游 PG jsonlog.
 */
static uint8 neon_jsonlog_extra_fields_mask = 0;

/* Field bit masks come from jsonlog_ext.h to share with tests. */

/* Default = 全开 (跟 GUC 默认字符串保持等价). */
#define NEON_JSONLOG_DEFAULT_MASK                \
	(NEON_JSONLOG_FIELD_ENDPOINT_ID |             \
	 NEON_JSONLOG_FIELD_BRANCH_ID    |             \
	 NEON_JSONLOG_FIELD_PROJECT_ID   |             \
	 NEON_JSONLOG_FIELD_TRACE_ID)

/* ----------------------------------------------------------------------
 *	白名单解析
 * ---------------------------------------------------------------------- */

/*
 * neon_jsonlog_parse_fields_to_mask · 解析逗号分隔白名单到 bit mask.
 *
 * - 空 / NULL / "" → mask = 0 (= 退化)
 * - "endpoint_id,trace_id" → 对应位置 1
 * - 未知字段 (e.g. "foo") → 静默忽略 (forward-compat · 让运维加未来字段不破)
 * - 大小写敏感 (符合 PG GUC 字符串约定 + jsonlog 字段名 snake_case)
 * - 容忍前后空格 (e.g. " endpoint_id , trace_id " 视同 "endpoint_id,trace_id")
 *
 * 暴露 (非 static) 给单测调.
 */
uint8
neon_jsonlog_parse_fields_to_mask(const char *raw)
{
	uint8		m = 0;
	const char *p;

	if (raw == NULL)
		return 0;

	p = raw;
	while (*p != '\0')
	{
		const char *tok_start;
		const char *tok_end;
		size_t		tok_len;

		/* skip leading whitespace + commas */
		while (*p == ' ' || *p == '\t' || *p == ',')
			p++;
		if (*p == '\0')
			break;

		tok_start = p;
		while (*p != '\0' && *p != ',')
			p++;
		tok_end = p;

		/* strip trailing whitespace */
		while (tok_end > tok_start &&
			   (*(tok_end - 1) == ' ' || *(tok_end - 1) == '\t'))
			tok_end--;

		tok_len = (size_t) (tok_end - tok_start);
		if (tok_len == 0)
			continue;

		if (tok_len == 11 && memcmp(tok_start, "endpoint_id", 11) == 0)
			m |= NEON_JSONLOG_FIELD_ENDPOINT_ID;
		else if (tok_len == 9 && memcmp(tok_start, "branch_id", 9) == 0)
			m |= NEON_JSONLOG_FIELD_BRANCH_ID;
		else if (tok_len == 10 && memcmp(tok_start, "project_id", 10) == 0)
			m |= NEON_JSONLOG_FIELD_PROJECT_ID;
		else if (tok_len == 8 && memcmp(tok_start, "trace_id", 8) == 0)
			m |= NEON_JSONLOG_FIELD_TRACE_ID;
		/* else: 未知字段, 静默忽略 (forward-compat) */
	}

	return m;
}

/*
 * GUC assign hook · 解析白名单写 mask.
 */
static void
neon_jsonlog_extra_fields_assign(const char *newval, void *extra)
{
	neon_jsonlog_extra_fields_mask = neon_jsonlog_parse_fields_to_mask(newval);
}

/*
 * GUC check hook · 验证白名单格式 (unknown 字段警告但不拒, 保 forward-compat).
 * 当前实现只做空格 / 逗号容忍检查; 未来可挂格式校验.
 */
static bool
neon_jsonlog_extra_fields_check(char **newval, void **extra, GucSource source)
{
	return true;
}

/* ----------------------------------------------------------------------
 *	hook 实现
 * ---------------------------------------------------------------------- */

/*
 * neon_jsonlog_append_kv · 写 `,"key":"value"`.
 *
 * 等价于 PG 内置 appendJSONKeyValue (PG14 起就有), 但 17- 的 jsonlog.c
 * 内部使用静态 escape_json + bool 控制 (`appendJSONKeyValueFmt`), 不全版本
 * 都暴露同名. 我们用同样的转义规则手写一份: 双引号 + 反斜杠 + 控制字符
 * (0x00-0x1F) escape, 其余原样.
 *
 * 暴露 (非 static) 给单测调.
 */
void
neon_jsonlog_append_kv(StringInfo buf, const char *key, const char *value)
{
	const char *p;

	Assert(buf != NULL);
	Assert(key != NULL);
	Assert(value != NULL);

	appendStringInfoString(buf, ",\"");
	appendStringInfoString(buf, key);
	appendStringInfoString(buf, "\":\"");

	for (p = value; *p != '\0'; p++)
	{
		unsigned char c = (unsigned char) *p;

		if (c == '"')
			appendStringInfoString(buf, "\\\"");
		else if (c == '\\')
			appendStringInfoString(buf, "\\\\");
		else if (c == '\n')
			appendStringInfoString(buf, "\\n");
		else if (c == '\r')
			appendStringInfoString(buf, "\\r");
		else if (c == '\t')
			appendStringInfoString(buf, "\\t");
		else if (c < 0x20)
			appendStringInfo(buf, "\\u%04x", c);
		else
			appendStringInfoChar(buf, (char) c);
	}

	appendStringInfoChar(buf, '"');
}

/*
 * Local read of a USR GUC string. 走 GetConfigOption(missing_ok=true,
 * restrict_privileged=false). 返 NULL 表示没设置或不可见.
 */
static const char *
neon_jsonlog_read_usr_guc(const char *name)
{
#ifdef NEON_JSONLOG_STANDALONE_TEST
	/* 单测模式: 用进程级 dict, 避免链接 PG GUC */
	extern const char *neon_jsonlog_test_get_usr_guc(const char *name);
	return neon_jsonlog_test_get_usr_guc(name);
#else
	return GetConfigOption(name, true /* missing_ok */ , false);
#endif
}

/*
 * 主 hook · vendor jsonlog.c 通过 append_extra_jsonlog_fields_hook
 * 调到本函数. 暴露 (非 static) 给单测.
 */
void
jsonlog_ext_append_fields(StringInfo buf)
{
	uint8		mask = neon_jsonlog_extra_fields_mask;
	const char *val;

	if (mask == 0)
		return;					/* GUC 关 / 设空 → 退化到上游 PG jsonlog */

	if ((mask & NEON_JSONLOG_FIELD_ENDPOINT_ID) != 0)
	{
		val = neon_jsonlog_read_usr_guc("neon.endpoint_id");
		if (val != NULL && val[0] != '\0')
			neon_jsonlog_append_kv(buf, "endpoint_id", val);
	}

	if ((mask & NEON_JSONLOG_FIELD_BRANCH_ID) != 0)
	{
		val = neon_jsonlog_read_usr_guc("neon.branch_id");
		if (val != NULL && val[0] != '\0')
			neon_jsonlog_append_kv(buf, "branch_id", val);
	}

	if ((mask & NEON_JSONLOG_FIELD_PROJECT_ID) != 0)
	{
		val = neon_jsonlog_read_usr_guc("neon.project_id");
		if (val != NULL && val[0] != '\0')
			neon_jsonlog_append_kv(buf, "project_id", val);
	}

	if ((mask & NEON_JSONLOG_FIELD_TRACE_ID) != 0)
	{
		/*
		 * trace_id 走 B1 #49 引入的 neon_trace_status_snapshot_for_jsonlog
		 * weak symbol. B1 未 merge 时 symbol 为 NULL, 字段静默缺.
		 * 这是设计中的"trace_id 注入 (PgBackendStatus.trace_context 有 trace_id
		 * 时输出 · 无时 null)" — case 3 fixture 验.
		 */
		if (neon_trace_status_snapshot_for_jsonlog != NULL)
		{
			NeonTraceIdsView snap = {0};

			if (neon_trace_status_snapshot_for_jsonlog(&snap) && snap.valid)
			{
				if (snap.trace_id_hex[0] != '\0')
					neon_jsonlog_append_kv(buf, "trace_id", snap.trace_id_hex);
			}
		}
	}
}

/* ----------------------------------------------------------------------
 *	extension 加载入口
 * ---------------------------------------------------------------------- */

/*
 * jsonlog_ext_init · 在 pgxn/neon/neon.c::_PG_init 末尾调一次.
 *  1. 注册 neon.jsonlog_extra_fields GUC (PGC_SIGHUP · superuser-only-set)
 *  2. 装 hook
 */
void
jsonlog_ext_init(void)
{
	DefineCustomStringVariable(
		"neon.jsonlog_extra_fields",
		"Comma-separated whitelist of Neon-specific jsonlog fields",
		"Values: endpoint_id, branch_id, project_id, trace_id. "
		"Empty string disables all Neon fields (degrades to upstream PG jsonlog). "
		"Unknown values are silently ignored (forward-compatible).",
		&neon_jsonlog_extra_fields_raw,
		"endpoint_id,branch_id,project_id,trace_id",
		PGC_SIGHUP,
		GUC_LIST_INPUT | GUC_LIST_QUOTE,
		neon_jsonlog_extra_fields_check,
		neon_jsonlog_extra_fields_assign,
		NULL /* show hook */ );

	/*
	 * 装 hook —— vendor patch 在 jsonlog.c::write_jsonlog 末尾会调.
	 * 多个扩展不互锁: 后注册的覆盖前者 (跟 PG 标准 single-pointer hook
	 * 约定一致). Neon 内核里只有本扩展用此 hook.
	 */
	append_extra_jsonlog_fields_hook = jsonlog_ext_append_fields;
}

/* ----------------------------------------------------------------------
 *	Standalone test backdoor (NEON_JSONLOG_STANDALONE_TEST only)
 *
 *	让 test_jsonlog_ext.c 直接写 mask, 避免依赖 PG GUC subsystem.
 * ---------------------------------------------------------------------- */

#ifdef NEON_JSONLOG_STANDALONE_TEST
void
test_force_mask(uint8 m)
{
	neon_jsonlog_extra_fields_mask = m;
}
#endif

