# pgxs/neon/Makefile


MODULE_big = neon
# 注: 本 OBJS 列表保持字母序。rebase 上游 neon 时, 若上游也在
# relsize_cache.o ~ unstable_extensions.o 区间加新 .o, trace_context.o 这
# 一行可能产生上下文冲突 — 解冲突时把它按字母序保留在 r 与 u 之间即可。
OBJS = \
	$(WIN32RES) \
	communicator.o \
	communicator_process.o \
	extension_server.o \
	file_cache.o \
	hll.o \
	libpagestore.o \
	logical_replication_monitor.o \
	neon.o \
	neon_lwlsncache.o \
	neon_pgversioncompat.o \
	neon_perf_counters.o \
	neon_trace_status.o \
	neon_utils.o \
	neon_walreader.o \
	pagestore_smgr.o \
	relsize_cache.o \
	sqlcommenter.o \
	trace_context.o \
	unstable_extensions.o \
	walproposer.o \
	walproposer_pg.o \
	neon_ddl_handler.o \
	walsender_hooks.o \
	$(NEON_CARGO_ARTIFACT_TARGET_DIR)/libcommunicator.a

PG_CPPFLAGS = -I$(libpq_srcdir)
SHLIB_LINK_INTERNAL = $(libpq)
SHLIB_LINK = -lcurl

UNAME_S := $(shell uname -s)
ifeq ($(UNAME_S), Darwin)
    SHLIB_LINK += -framework Security -framework CoreFoundation -framework SystemConfiguration

    # Link against object files for the current macOS version, to avoid spurious linker warnings.
    MACOSX_DEPLOYMENT_TARGET := $(shell xcrun --sdk macosx --show-sdk-version)
    export MACOSX_DEPLOYMENT_TARGET
endif

EXTENSION = neon
DATA = \
	neon--1.0.sql \
	neon--1.0--1.1.sql \
	neon--1.1--1.2.sql \
	neon--1.2--1.3.sql \
	neon--1.3--1.4.sql \
	neon--1.4--1.5.sql \
	neon--1.5--1.6.sql \
	neon--1.6--1.7.sql \
	neon--1.7--1.8.sql \
	neon--1.8--1.7.sql \
	neon--1.7--1.6.sql \
	neon--1.6--1.5.sql \
	neon--1.5--1.4.sql \
	neon--1.4--1.3.sql \
	neon--1.3--1.2.sql \
	neon--1.2--1.1.sql \
	neon--1.1--1.0.sql
PGFILEDESC = "neon - cloud storage for PostgreSQL"

EXTRA_CLEAN = \
	libwalproposer.a \
	test/trace_context_test \
	test/sqlcommenter_test

WALPROP_OBJS = \
	$(WIN32RES) \
	walproposer.o \
	neon_utils.o \
	walproposer_compat.o

# libcommunicator.a is built by cargo from the Rust sources under communicator/
# subdirectory. `cargo build` also generates communicator_bindings.h.
communicator_process.o: communicator/communicator_bindings.h
file_cache.o: communicator/communicator_bindings.h

$(NEON_CARGO_ARTIFACT_TARGET_DIR)/libcommunicator.a communicator/communicator_bindings.h &:
	(cd $(srcdir)/communicator && cargo build $(CARGO_BUILD_FLAGS) $(CARGO_PROFILE))

# Force `cargo build` every time. Some of the Rust sources might have
# changed.
.PHONY: $(NEON_CARGO_ARTIFACT_TARGET_DIR)/libcommunicator.a communicator/communicator_bindings.h

.PHONY: walproposer-lib
walproposer-lib: CPPFLAGS += -DWALPROPOSER_LIB
walproposer-lib: libwalproposer.a;

.PHONY: libwalproposer.a
libwalproposer.a: $(WALPROP_OBJS)
	$(RM) $@
	$(AR) $(AROPT) $@ $^

# needs vars:
# FIND_TYPEDEF pointing to find_typedef
# INDENT pointing to pg_bsd_indent
# PGINDENT_SCRIPT pointing to pgindent (be careful with PGINDENT var name:
#   pgindent will pick it up as pg_bsd_indent path).
.PHONY: pgindent
pgindent:
	+@ echo top_srcdir=$(top_srcdir) top_builddir=$(top_builddir) srcdir=$(srcdir)
	$(FIND_TYPEDEF) . > neon.typedefs
	INDENT=$(INDENT) $(PGINDENT_SCRIPT) --typedefs neon.typedefs $(srcdir)/*.c $(srcdir)/*.h

PG_CONFIG = pg_config
PGXS := $(shell $(PG_CONFIG) --pgxs)
include $(PGXS)

# ---------------------------------------------------------------------------
# Standalone unit tests for trace_context.{h,c}
#
# trace_context 是 W3C TraceContext (traceparent v00) 的 serializer/parser,
# 零外部依赖 (不 #include postgres.h)。可以脱离 PGXS / pg_install 直接用
# 系统 gcc 编出来跑单测。后续 feat-033 / 034 / 035 / 065 在 libpq /
# libpagestore / proxy 各自 wire-up 时 #include "trace_context.h" 复用。
#
# 注: 本测试是独立 target, 不挂入 PGXS 的 check / installcheck 依赖链
# (避免引入 running-DB 依赖)。CI / 开发机必须显式调:
#     make -C pgxn/neon check-trace-context
# ---------------------------------------------------------------------------

TRACE_CONTEXT_CC ?= cc
TRACE_CONTEXT_CFLAGS ?= -std=c11 -Wall -Wextra -Werror -O0 -g

test/trace_context_test: trace_context.c trace_context.h test/test_trace_context.c
	$(TRACE_CONTEXT_CC) $(TRACE_CONTEXT_CFLAGS) -I. \
		trace_context.c test/test_trace_context.c -o $@

.PHONY: check-trace-context
check-trace-context: test/trace_context_test
	./test/trace_context_test

# ---------------------------------------------------------------------------
# Standalone unit tests for sqlcommenter.{h,c} (feat-034/#37).
#
# Same independence rationale as check-trace-context: sqlcommenter.c is
# linked from the backend extension AND walproposer, so we keep its tests
# free of postgres.h / running-DB dependency. Run via:
#     make -C pgxn/neon check-sqlcommenter
# ---------------------------------------------------------------------------

test/sqlcommenter_test: sqlcommenter.c sqlcommenter.h trace_context.c \
		trace_context.h test/test_sqlcommenter.c
	$(TRACE_CONTEXT_CC) $(TRACE_CONTEXT_CFLAGS) -I. \
		trace_context.c sqlcommenter.c test/test_sqlcommenter.c -o $@

.PHONY: check-sqlcommenter
check-sqlcommenter: test/sqlcommenter_test
	./test/sqlcommenter_test

# ---------------------------------------------------------------------------
# Standalone check target for the dynamic probe whitelist/denylist schema
# (feat-067 anchor A0b · 跟平行 PR #40 (A0a feat-033) check-trace-context
# 同级 / 同位置 / 同前缀 · 跨 anchor 测试入口纪律统一 · R2/N1)。
#
# 用法:
#     make -C pgxn/neon check-probes
#
# 跑全套 fixture 校验 (合法 + 非法) · 走仓根 scripts/validate_whitelist.py。
# 依赖: python3 + PyYAML + jsonschema (pip install pyyaml jsonschema)。
#
# 不依赖 PGXS / pg_install / cargo · CI 在装好 python 依赖的环境里直接跑。
# ---------------------------------------------------------------------------

.PHONY: check-probes
check-probes:
	bash $(srcdir)/probes/tests/run_tests.sh
