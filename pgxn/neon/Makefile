# pgxs/neon/Makefile


MODULE_big = neon
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
	neon_utils.o \
	neon_walreader.o \
	pagestore_smgr.o \
	relsize_cache.o \
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
	neon--1.7--1.6.sql \
	neon--1.6--1.5.sql \
	neon--1.5--1.4.sql \
	neon--1.4--1.3.sql \
	neon--1.3--1.2.sql \
	neon--1.2--1.1.sql \
	neon--1.1--1.0.sql
PGFILEDESC = "neon - cloud storage for PostgreSQL"

EXTRA_CLEAN = \
	libwalproposer.a

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

# ---------------------------------------------------------------------------
# feat-069 · Rust uprobe 白名单 + 符号暴露 5-case fixture
#
# 用法:
#     make -C pgxn/neon check-feat069
#
# 跑 5 case (Cargo profile / uprobe attach stage / USDT note stage /
# 白名单 denylist 静态文件 / async fn 屏障)
# 依赖: python3 + PyYAML + jsonschema (与 check-probes 同)
# ---------------------------------------------------------------------------

.PHONY: check-feat069
check-feat069:
	bash $(srcdir)/probes/tests/feat069_fixtures.sh

# ---------------------------------------------------------------------------
# feat-069-binary-wire (D4) · 3 binary stapsdt note 接入验证
#
# 用法 (Linux 物理机 · 需先 cargo build --release -p pageserver -p safekeeper -p proxy):
#     make -C pgxn/neon check-feat069-wire
#
# 通过判据: 各 binary readelf -n | grep NT_STAPSDT 数 >= A5 #48 lib.rs USDT 段定义数
# ---------------------------------------------------------------------------

.PHONY: check-feat069-wire
check-feat069-wire:
	bash $(srcdir)/probes/tests/feat069_binary_wire_check.sh
