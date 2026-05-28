/*-------------------------------------------------------------------------
 *
 * jsonlog_ext.h
 *	  feat-036 · PG jsonlog Neon 字段注入 hook
 *
 * PG fork (src/backend/utils/error/jsonlog.c) 在 write_jsonlog() 收尾
 * 的 '}' 之前调全局函数指针 `append_extra_jsonlog_fields_hook`,
 * 默认 NULL —— Neon extension 加载时把 jsonlog_ext_append_fields 装上去.
 *
 * 函数指针签名 (匹配 vendor/postgres-v{14..17}/src/backend/utils/error/jsonlog.c
 * 的内部 StringInfo 累加缓冲):
 *
 *   typedef void (*append_extra_jsonlog_fields_hook_type)(StringInfo buf);
 *
 * - buf 当前已经写好上游 PG 18 个标准字段, 末尾未写 '}'.
 * - hook 可以追加 0 或多个 "<COMMA><JSONKey>:<JSONValue>" 形式的字段对.
 * - hook 内部务必保证如果首字段也写, 必须自带 leading comma (vendor patch
 *   不对上游 18 字段是否曾写做判定; 实际 18 字段中 timestamp + pid + ...
 *   至少有 3 个一定写, 所以 leading comma 永远安全).
 *
 * vendor patch 落在 jsonlog.c::write_jsonlog 末尾:
 *
 *     if (append_extra_jsonlog_fields_hook != NULL)
 *         (*append_extra_jsonlog_fields_hook)(&buf);
 *     appendStringInfoChar(&buf, '}');
 *
 * Hook 默认 NULL —— 上游 PG 行为完全不变. Neon extension 加载时填上.
 *
 *-------------------------------------------------------------------------
 */
#ifndef NEON_JSONLOG_EXT_H
#define NEON_JSONLOG_EXT_H

#ifndef NEON_JSONLOG_STANDALONE_TEST
#include "lib/stringinfo.h"
#endif
/* In STANDALONE mode, StringInfo / PGDLLIMPORT are provided by shim
 * in jsonlog_ext.c (and test_jsonlog_ext.c) before this header is included. */

/* feat-036 · vendor PG jsonlog.c 暴露的 hook 函数指针类型. */
typedef void (*append_extra_jsonlog_fields_hook_type) (StringInfo buf);

/*
 * vendor 端 (PG fork jsonlog.c) 定义本符号 (默认 NULL).
 *
 * Neon extension `_PG_init` 内调 jsonlog_ext_init() 把 hook 装上去.
 *
 * standalone 单测 (test_jsonlog_ext.c) 不链接 PG, 自带定义符号,
 * 走 dlsym/弱引用模式; 见 test 文件注释.
 */
extern PGDLLIMPORT append_extra_jsonlog_fields_hook_type
			append_extra_jsonlog_fields_hook;

/*
 * Neon extension 加载入口 (在 pgxn/neon/neon.c::_PG_init 末尾调一次):
 *   - 注册 neon.jsonlog_extra_fields GUC
 *   - 设 append_extra_jsonlog_fields_hook = jsonlog_ext_append_fields
 */
extern void jsonlog_ext_init(void);

/*
 * 实际 hook 实现 —— 直接被 vendor jsonlog.c 通过函数指针调.
 * 也直接暴露给 standalone 单测使用 (绕开 vendor patch, 喂构造的 StringInfo).
 */
extern void jsonlog_ext_append_fields(StringInfo buf);

/*
 * neon_trace_status (B1 #49 引入) 暴露的 read API.
 * 本头文件用前向声明 + extern 加 weak attribute, 让本扩展在 B1 未 merge
 * 的中间状态也能编译:
 *   - 链入: 走真正 read API, trace_id 可拿
 *   - 未链入: weak symbol 解析为 NULL, 自动退化 (不输出 trace_id 字段)
 *
 * 一旦 B1 #49 merge, neon_trace_status.h 提供真定义 (struct TraceContext
 * + char tracestate[NEON_TRACE_STATE_MAX]); 这里只引用最小子集.
 */
typedef struct NeonTraceIdsView
{
	bool		valid;
	char		trace_id_hex[33];	/* 32 hex + NUL, W3C §3.2.2.2 */
	char		span_id_hex[17];	/* 16 hex + NUL, W3C §3.2.2.3 */
}			NeonTraceIdsView;

/*
 * Read-only snapshot of current backend's trace context, used purely
 * to populate jsonlog `trace_id` field. Implemented by feat-034/B1
 * (neon_trace_status.c). Weak symbol so 本扩展独立测试时可不链接.
 */
extern bool neon_trace_status_snapshot_for_jsonlog(NeonTraceIdsView *out)
			__attribute__((weak));

/* ----------------------------------------------------------------------
 *	Whitelist bit mask (公开给单测 + 集成测试; runtime user-facing 接口
 *	走 neon.jsonlog_extra_fields GUC 字符串).
 * ---------------------------------------------------------------------- */
#define NEON_JSONLOG_FIELD_ENDPOINT_ID  (1u << 0)
#define NEON_JSONLOG_FIELD_BRANCH_ID    (1u << 1)
#define NEON_JSONLOG_FIELD_PROJECT_ID   (1u << 2)
#define NEON_JSONLOG_FIELD_TRACE_ID     (1u << 3)

#endif							/* NEON_JSONLOG_EXT_H */
