#!/usr/bin/env bash
#
# feat-010 USR 命名漂移 CI grep guard（first-line · feat-010 §3.2(d)）。
#
# 拦截 4 组件 telemetry 出口侧出现的非 canonical USR 字段命名（如 endpoint_uuid / endpointId /
# epId / shardId 等），强制统一到 cornerstone 定义的 `openneon.usr.*` / `endpoint_id` 等命名
# （overview §10.2.3）。feat-011 metric registry CI 是 second-line 互补。
#
# allowlist：在同一行加注释 `// USR-LINT-IGNORE: <reason>` 可豁免（防误伤 struct field /
# vendored / FFI binding，feat-010 §11 OQ5）。
#
# 退出码：发现漂移命名 → 1（CI fail）；干净 → 0。

set -uo pipefail

# 只扫 telemetry 出口侧代码；不扫 tests / vendored / FFI binding（feat-010 §11 OQ5）。
#
# scope 覆盖全部 4 组件的 telemetry 出口（feat-008 cornerstone + feat-009 safekeeper +
# feat-010 compute/proxy）：pageserver / safekeeper 的 metric label + USR glue 也纳入扫描，
# 防止 shard_id / tenant_id 等在这两个组件出口侧出现漂移命名。
# PATTERNS 仅匹配 camelCase / `*_uuid` / `epId` 等不会出现在惯用 Rust 里的明确漂移形式，
# 故按 src 目录粒度扫描误伤风险低；个别误伤用行内 `// USR-LINT-IGNORE` 豁免。
SCOPE=(
  "compute_tools/src"
  "proxy/src/binary"
  "pageserver/src"
  "safekeeper/src"
  "libs/utils/src/logging.rs"
  "libs/utils/src/usr.rs"
  "libs/tracing-utils/src/usr.rs"
)

# 禁用的漂移命名（ERE）。canonical 形式见 feat-010 §4.1。
#
# 注意：故意**不**拦 `shard_index` / `shard_num` —— 它们跟上游 canonical 的
# `utils::shard::ShardIndex` 类型及其惯用变量名（`let shard_index = ...`）冲突，拦了会大面积误伤
# （feat-010 §11 OQ5）。telemetry 出口的 shard 维度统一用 attribute `openneon.usr.shard_id`，
# 由 cornerstone helper 产出，不靠本 guard 拦字段名。这里只拦不会出现在惯用 Rust 里的明确漂移形式：
# camelCase + `*_uuid` + 缩写 `epId`。
PATTERNS=(
  '\bendpoint_uuid\b'   # 旧名
  '\bendpointId\b'      # camelCase
  '\bepId\b'            # 缩写
  '\bshardId\b'         # camelCase
  '\btenantId\b'        # camelCase
  '\btenant_uuid\b'     # 旧名
  '\btimelineId\b'      # camelCase
  '\bprojectId\b'       # camelCase
  '\bproject_uuid\b'    # 旧名
)

ALLOWLIST_MARK='USR-LINT-IGNORE'

fail=0
for pat in "${PATTERNS[@]}"; do
  # -E ERE; -n line number; --include 仅 rust 源；过滤掉 allowlist 行。
  hits=$(grep -rnE --include='*.rs' "$pat" "${SCOPE[@]}" 2>/dev/null | grep -v "$ALLOWLIST_MARK" || true)
  if [ -n "$hits" ]; then
    echo "USR 命名漂移检测（见 features/feat-010 §3.2(d)）—— 模式: $pat"
    echo "$hits"
    echo "---"
    echo "请改用 canonical 命名（openneon.usr.* / endpoint_id / tenant_id / timeline_id / shard_id / project_id）。"
    echo "确为误伤（struct field / FFI / vendored）请在该行加注释: // ${ALLOWLIST_MARK}: <reason>"
    fail=1
  fi
done

if [ "$fail" -ne 0 ]; then
  exit 1
fi

echo "USR 命名 guard 通过：4 组件 telemetry 出口侧无漂移命名。"
exit 0
