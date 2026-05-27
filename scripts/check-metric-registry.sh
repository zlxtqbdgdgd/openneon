#!/usr/bin/env bash
# ============================================================================
# scripts/check-metric-registry.sh
#
# feat-011 · Metric Registry CI 检查(USR 套 + feat-031 audit schema 验证层)
#
# 在 openneon fork repo root 跑 · GitHub Actions workflow
# .github/workflows/metric-registry-check.yml 调用 · 本地开发也可直接跑。
#
# ────────────────────────────────────────────────────────────────────────────
# 治理模型:Datadog 式「少而严的保留集硬管 + 其余放行 + WARN」
# (对齐 Datadog Unified Service Tagging:钦定 service/env/version 等少量
#  保留标签强约束 · 其余自定义标签自由发挥不阻断)
#
# neon 是十万行老库 · 已有几百个既有合法 tracing 字段(status/url/user/value...)。
# 若把「任何不在白名单里的字段」一律判 FAIL · 会把整个存量库判违规 → CI 长红 ·
# 这是守门员把「USR 保留身份标签」和「几百个普通日志字段」混为一谈。
#
# 因此本脚本把字段分两类对待:
#   ┌─ 受治理的「USR 保留身份标签」(endpoint/tenant/timeline/shard/project)
#   │    → 硬管(FAIL):凡语义指这几个保留身份 · 却没用钦定的 `*_id` 规范形 ·
#   │       一律 FAIL(基于「概念词根 + 非 _id 后缀」的模式正则 · 不是死名单)。
#   └─ 普通字段(其余一切)
#        → 放行 + WARN:不在 tracing_known_fields 白名单只记 WARN 提示 · 不阻断。
# ────────────────────────────────────────────────────────────────────────────
#
# 把 4 组件源码(pageserver / safekeeper / compute_tools / proxy)实际 emit 的
# metric / tracing field · diff metric-registry.yaml 的期望集:
#   class 1 · 未注册 metric                   → FAIL(exit 1)
#   class 2a · USR 保留身份标签非规范写法漂移  → FAIL(exit 1 · 模式硬拦 · 见下)
#   class 2b · 未注册的普通 tracing field      → WARN(不 fail · 放行 + 提示)
#   class 3 · metric 缺 USR 三件套             → FAIL(exit 1)
#   warn    · registry stale(源码已删)        → WARN(不 fail · 允许 rollback 灵活性)
#   class 4(联动)· audit_events 缺核心 attr   → FAIL(exit 1)
#
# 退出码:0 = pass · 非 0 = 至少一类 hard violation。
# 0 副作用:不改任何源文件 · 只读 + 临时目录。
#
# 依赖:ripgrep(rg)· yq(mikefarah/yq v4)· comm/sort/grep(coreutils)。
# OQ1:register_* 宏 / tracing 宏的 grep pattern 已按 2026-05 neon baseline 实测校准
#      (pageserver/src/metrics.rs 用 register_int_counter! / _vec! / register_uint_gauge! 等)。
# ============================================================================

set -euo pipefail

# ---- 定位 repo root(脚本在 scripts/ 下)----
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$REPO_ROOT"

REGISTRY="${REGISTRY:-metric-registry.yaml}"

# 扫描的源码根(只扫 4 组件 · 跟 workflow paths filter 对齐)
SRC_DIRS=()
for d in pageserver/src safekeeper/src compute_tools/src proxy/src; do
  [[ -d "$d" ]] && SRC_DIRS+=("$d")
done
if [[ ${#SRC_DIRS[@]} -eq 0 ]]; then
  echo "FAIL · 未找到任何组件源码目录(pageserver/src 等)· 是否在 repo root?" >&2
  exit 2
fi

# ---- 依赖检查 ----
for bin in rg yq comm sort grep; do
  command -v "$bin" >/dev/null 2>&1 || { echo "FAIL · 缺依赖:$bin" >&2; exit 2; }
done
[[ -f "$REGISTRY" ]] || { echo "FAIL · 找不到 registry 文件:$REGISTRY" >&2; exit 2; }

WORKDIR="$(mktemp -d)"
trap 'rm -rf "$WORKDIR"' EXIT

FAILED=0   # 累计 hard violation · 全程不提前退出 · 一次报全所有问题

# ---- 第 0 步:registry YAML 可解析性闸门 ----
# 必须在任何 yq 读取之前显式校验:YAML 格式错时 yq 返回非 0 · set -e 下会让脚本
# 在后续读取处直接 abort(且不打印明确原因)· 或在 process substitution 里被吞 →
# CI 误报 PASS。这里先整体解析一遍并显式检查退出码 · 坏 YAML 立即报 FAIL 退出。
if ! yq -e '.' "$REGISTRY" >/dev/null 2>"$WORKDIR/yq_parse.err"; then
  echo "FAIL · registry 文件 YAML 解析失败(格式错?):$REGISTRY"
  sed 's/^/    /' "$WORKDIR/yq_parse.err"
  echo "----------------------------------------------------------------------"
  echo "FAIL · metric-registry.yaml 无法解析 · 请修正 YAML 格式"
  exit 1
fi

# ---- 第 0 步:registry schema 版本兼容 ----
REG_VERSION="$(yq -r '.version' "$REGISTRY")"
if [[ "$REG_VERSION" != "1" ]]; then
  echo "FAIL · 不支持的 registry schema version: '$REG_VERSION'(本 CI script 仅支持 version 1)"
  echo "       升级 schema 请同 PR 改 check-metric-registry.sh 兼容判定"
  FAILED=1
fi

# ---- 第 1 步:从 registry 抽 expected 集合 ----
# grep -v 在「全被过滤掉(空列表)」时返回非 0 · set -e 下会误中断 · 故加 || true。
yq -r '.metrics[].name' "$REGISTRY" 2>/dev/null | { grep -v '^null$' || true; } | sort -u > "$WORKDIR/expected_metrics.txt"

# 合法 tracing field = tracing_known_fields ∪ required_tags ∪ neon_specific_tags
{
  yq -r '.tracing_known_fields[]' "$REGISTRY" 2>/dev/null | grep -v '^null$' || true
  yq -r '.required_tags[].name' "$REGISTRY" 2>/dev/null | grep -v '^null$' || true
  yq -r '.neon_specific_tags[].name' "$REGISTRY" 2>/dev/null | grep -v '^null$' || true
} | sort -u > "$WORKDIR/known_fields.txt"

# ---- 第 2 步:grep 源码实际 emit 的 metric name ----
# register_* 宏后第一参数是 metric name(snake_case 字符串) · 调用常跨多行 ·
# 宏调用与 metric 名之间可能夹注释 / 多行空白:
#     register_int_counter!(
#         // 见 RFC-xxx
#         "pageserver_xxx_total",
#         "help text ..."
#     )
# 用 ripgrep multiline 模式:匹配 register 宏 · 跨「任意空白 + 行注释(// ...)」
# 直到第一个字符串字面量(metric name)。
# [[:space:]] 含换行(-U multiline) · (//[^\n]*\n)* 吞掉宏调用与首参之间的整行注释。
# 覆盖边界:不处理块注释 /* ... */ 夹在宏名与 metric 名之间的极端写法
# (neon baseline 未见此形;若出现需再扩 pattern)。
RG_REGISTER='register_(int_|uint_|float_)?(counter|gauge|histogram)(_vec|_pair)?!\s*\(\s*(//[^\n]*\n\s*)*"[a-z_][a-z0-9_]*"'

rg --no-heading --no-line-number --no-filename -U -o -e "$RG_REGISTER" "${SRC_DIRS[@]}" 2>/dev/null \
  | grep -oE '"[a-z_][a-z0-9_]*"[^"]*$' \
  | grep -oE '"[a-z_][a-z0-9_]*"' \
  | tr -d '"' \
  | sort -u > "$WORKDIR/actual_metrics.txt" || true

# class 1 · actual - expected = 未注册 metric
UNREGISTERED="$(comm -23 "$WORKDIR/actual_metrics.txt" "$WORKDIR/expected_metrics.txt" || true)"
if [[ -n "$UNREGISTERED" ]]; then
  echo "FAIL · class 1 · 发现未注册 metric(需同 PR 在 $REGISTRY 加 entry):"
  echo "$UNREGISTERED" | sed 's/^/    - /'
  echo "    Hint: 加 metrics: 列表项 · 含 name / component / type / unit / required_tags_subset(必含 service,env,version)/ source_file"
  echo ""
  FAILED=1
fi

# warn · expected - actual = registry stale(源码已删但 registry 未跟进)
# 仅当源码扫到了至少一条 metric 时才比对(否则 grep pattern 未命中会误报全 stale)
if [[ -s "$WORKDIR/actual_metrics.txt" ]]; then
  STALE="$(comm -13 "$WORKDIR/actual_metrics.txt" "$WORKDIR/expected_metrics.txt" || true)"
  if [[ -n "$STALE" ]]; then
    echo "WARN · registry 中 metric 已不在源码(stale · 允许 rollback · 建议后续 PR 清理):"
    echo "$STALE" | sed 's/^/    - /'
    echo ""
  fi
fi

# ---- 第 3 步:grep tracing / audit 宏里的结构化 field name ----
# tracing::info!(field = value, ...) / info!(field, ...) / audit_event!(field = ...)
# neon 大量用两种 field 写法 · 两种都要抽:
#   (a) 显式键值     tracing::info!(tenant_id = %tid, "msg")
#   (b) 简写(shorthand) tracing::info!(tenant_id, "msg")  —— 等价于 tenant_id = tenant_id
# 两种形式抽出的 field 都进同一个 actual_fields 集 · 再分别喂给:
#   · class 2a(保留身份漂移硬拦 · 用保留大写的 mixedcase 集)
#   · class 2b(普通未注册字段 WARN 放行)。
RG_TRACING='(tracing::)?(info|warn|error|debug|trace)!|audit_event!'

# 先把每条匹配行抓到临时文件 · 再分别抽 (a)(b) 两种 field 形式。
rg --no-heading --no-line-number --no-filename -e "$RG_TRACING" "${SRC_DIRS[@]}" 2>/dev/null \
  > "$WORKDIR/tracing_lines.txt" || true

{
  # (a) 键值形式:`ident =`(排除比较运算 == · 故末尾 [^=])
  grep -oE '[a-z_][a-z0-9_]*[[:space:]]*=[^=]' "$WORKDIR/tracing_lines.txt" \
    | grep -oE '^[a-z_][a-z0-9_]*' || true

  # (b) 简写形式:宏括号内逗号分隔的裸标识符(`tenant_id,` / `%tenant_id,`)。
  #     处理顺序:① 先删掉所有双引号字符串字面量(消息文本 · 否则 "wal applied"
  #     里的词会被当 field 误报)· ② 剥掉宏名及之前的前缀 · 只留「( 之后」·
  #     ③ 按逗号切成单段(避免相邻简写 field 共享逗号被吞)· ④ 只留「整段就是一个
  #     裸 snake_case 标识符(可带 ?/%/& sigil)」的段 —— 即简写 field;
  #     `key = value` 段(含 =)/ 函数调用 a.b() / 路径 a::b 因整段不止一个 ident 被排除。
  #     覆盖边界:此启发式按单行扫 · 跨多行展开的宏调用首行可能切不全 · 个别噪声
  #     token 会先过下方关键字白名单兜底;简写 field 后续按保留身份(2a)/ 普通(2b)分流。
  sed -E 's/"([^"\\]|\\.)*"//g' "$WORKDIR/tracing_lines.txt" \
    | sed -E 's/^.*(tracing::)?(info|warn|error|debug|trace)!|^.*audit_event!//' \
    | tr ',' '\n' \
    | grep -oE '^[[:space:]]*[(]?[[:space:]]*[?%&]?[a-z_][a-z0-9_]*[[:space:]]*[)]?[[:space:]]*$' \
    | grep -oE '[a-z_][a-z0-9_]*' || true
} | sort -u > "$WORKDIR/actual_fields_raw.txt"

# 过滤掉明显的非-telemetry 噪声关键字(let/const 等不会出现在此 pattern · 保险起见去常见噪声)
# 末尾追加常见 Rust 关键字 / 宏体噪声 token(true/false/Some/None/Ok/Err 等)。
grep -vxE '(let|mut|const|if|else|for|while|match|return|self|true|false|Some|None|Ok|Err|as|fn|use|pub|ref|move|async|await|dyn|impl|where|crate|super|mod)' \
  "$WORKDIR/actual_fields_raw.txt" \
  > "$WORKDIR/actual_fields.txt" || true

# ──────────────────────────────────────────────────────────────────────────
# 额外抽一遍「含大小写混合」的 field 标识符 —— 仅供 class 2a 保留身份漂移扫描用。
# 上面 actual_fields 的抽取 pattern 是 [a-z_][a-z0-9_]* · 大写会被截断
# (`endpointId` 只会抽到 `endpoint`)· 故驼峰漂移(endpointId/tenantId/shardIndex)
# 在纯小写集里抓不到。这里对同样的 (a) 键值 + (b) 简写两种形式再抽一遍 ·
# 但 pattern 放宽到 [A-Za-z_][A-Za-z0-9_]* 保留大小写 · 只喂给 2a 的正则比对 ·
# 不进 class 2b 普通字段 WARN(避免引入大写噪声)。
{
  # (a) 键值形式(保留大小写)
  grep -oE '[A-Za-z_][A-Za-z0-9_]*[[:space:]]*=[^=]' "$WORKDIR/tracing_lines.txt" \
    | grep -oE '^[A-Za-z_][A-Za-z0-9_]*' || true
  # (b) 简写形式(保留大小写)
  sed -E 's/"([^"\\]|\\.)*"//g' "$WORKDIR/tracing_lines.txt" \
    | sed -E 's/^.*(tracing::)?(info|warn|error|debug|trace)!|^.*audit_event!//' \
    | tr ',' '\n' \
    | grep -oE '^[[:space:]]*[(]?[[:space:]]*[?%&]?[A-Za-z_][A-Za-z0-9_]*[[:space:]]*[)]?[[:space:]]*$' \
    | grep -oE '[A-Za-z_][A-Za-z0-9_]*' || true
} | sort -u > "$WORKDIR/actual_fields_mixedcase.txt"

# ──────────────────────────────────────────────────────────────────────────
# class 2a · USR 保留身份标签「非规范写法」漂移 → FAIL(模式硬拦)
#
# 治理核心:endpoint / tenant / timeline / shard / project 这几个是 USR 钦定的
# 保留身份概念 · 钦定的规范形是 `<concept>_id`(snake_case)。凡 telemetry 字段名
# 里出现这些概念词根 · 却用了非 `_id` 的后缀(uuid / Id 驼峰 / uid / Index ...) ·
# 即视为漂移 · 一律 FAIL。这是基于「概念词根 + 非 _id 后缀」的【模式】判定 ·
# 不是逐个枚举的死名单 —— 任何新造的 endpoint_xxx 漂移变体都会被同一条正则拦下。
#
# 至少覆盖(大小写不敏感):
#   endpoint_uuid / endpointId / endpoint_uid
#   tenant_uuid   / tenantId
#   timeline_uuid / timelineId
#   shard_uuid    / shardId / shardIndex
#
# 豁免(不是漂移 · 按设计 §11 OQ5 放行):
#   shard_index / shard_num  —— 是 neon 既有的合法 telemetry 字段(分片序号 ·
#   非身份标识)· 钦定保留。注意:扫描范围沿用脚本「只扫 metric/tracing 宏出口」·
#   故 neon 源码里普通的 `shard_index` / `ShardIndex` 局部变量本就不在 actual_fields
#   里 · 不会被误伤;此处再对这两个名字显式白名单兜底。
#
# 正则解读(大小写不敏感 · 见下 grep -iE):
#   ^(endpoint|tenant|timeline|shard|project)  概念词根
#   _?                                          可选下划线(覆盖驼峰 endpointId)
#   (uuid|uid|id|index|idx|num|guid|key)        非规范的身份后缀候选
#   $                                           整词
# 然后在命中集合里:
#   · 把规范形 `<concept>_id`(全小写、恰好 _id 结尾)排除(那是合法的);
#   · 把豁免名 shard_index / shard_num 排除。
# 剩下的就是真漂移。
# ──────────────────────────────────────────────────────────────────────────
RESERVED_DRIFT_RE='^(endpoint|tenant|timeline|shard|project)_?(uuid|uid|id|index|idx|num|guid|key)$'
RESERVED_CANONICAL_RE='^(endpoint|tenant|timeline|shard|project)_id$'

# 命中保留概念词根 + 身份后缀 · 大小写不敏感
# 用 mixedcase 抽取集(保留大写) · 才能抓到驼峰漂移 endpointId / shardIndex 等。
grep -iE "$RESERVED_DRIFT_RE" "$WORKDIR/actual_fields_mixedcase.txt" 2>/dev/null \
  | sort -u > "$WORKDIR/reserved_hits.txt" || true

# 从命中里剔除:① 规范形 <concept>_id(全小写恰好 _id)· ② 豁免名 shard_index / shard_num
RESERVED_DRIFT="$(
  grep -vxiE "$RESERVED_CANONICAL_RE" "$WORKDIR/reserved_hits.txt" 2>/dev/null \
    | grep -vxiE '^(shard_index|shard_num)$' 2>/dev/null || true
)"
if [[ -n "$RESERVED_DRIFT" ]]; then
  echo "FAIL · class 2a · 发现 USR 保留身份标签的非规范写法(命名漂移 · 必须改源码):"
  echo "$RESERVED_DRIFT" | sed 's/^/    - /'
  echo "    Hint: endpoint/tenant/timeline/shard/project 是 USR 钦定保留身份 · 规范形必须是"
  echo "          snake_case 的 <concept>_id(如 endpoint_uuid/endpointId/endpoint_uid → endpoint_id ·"
  echo "          tenantId → tenant_id · shardId/shardIndex → shard_id)。改源码字段名而非加 registry。"
  echo "          (分片序号请用既有 shard_index / shard_num · 已豁免)"
  echo ""
  FAILED=1
fi

# ──────────────────────────────────────────────────────────────────────────
# class 2b · 未注册的普通 tracing field → WARN(放行 · 不 fail)
#
# Datadog 式治理:保留集之外的字段「自由发挥」。十万行老库存量字段(status/url/
# user/value/waiters...)不在 tracing_known_fields 白名单只是「未登记」· 不是错误 ·
# 故仅记 WARN 提示开发者「想纳管可加白名单」· 绝不阻断 CI。
# 注意:已在 class 2a 报过的保留身份漂移 · 此处剔除 · 不重复刷屏。
# ──────────────────────────────────────────────────────────────────────────
# comm 抽「actual - known」· 再用 grep -vxF -f 去掉 reserved_hits(已在 2a 报过的)。
# reserved_hits.txt 为空文件时 · grep -f 无 pattern → 不删任何行(等价全放行)· 符合预期。
comm -23 "$WORKDIR/actual_fields.txt" "$WORKDIR/known_fields.txt" 2>/dev/null \
  > "$WORKDIR/unknown_raw.txt" || true
if [[ -s "$WORKDIR/reserved_hits.txt" ]]; then
  # 有保留命中 · 从普通未注册集里剔掉(已在 2a 报过)· 避免重复刷屏
  UNKNOWN_FIELDS="$(grep -vxF -f "$WORKDIR/reserved_hits.txt" "$WORKDIR/unknown_raw.txt" 2>/dev/null || true)"
else
  # 无保留命中 · 普通未注册集原样(空 pattern 文件在部分 grep 实现下行为不一致 · 显式分支)
  UNKNOWN_FIELDS="$(cat "$WORKDIR/unknown_raw.txt")"
fi
if [[ -n "$UNKNOWN_FIELDS" ]]; then
  echo "WARN · class 2b · 发现未注册的普通 tracing field(放行 · 非保留身份标签 · 不阻断 CI):"
  echo "$UNKNOWN_FIELDS" | sed 's/^/    - /'
  echo "    Hint: 若想纳入治理 · 可加到 $REGISTRY 的 tracing_known_fields(可选 · 不强制)。"
  echo "          普通字段对齐 Datadog「保留集之外自由发挥」· 仅提示不 fail。"
  echo ""
fi

# ---- 第 4 步:每条 metric 必须含 USR 三件套(service / env / version)----
# 注意:不能用 `while ... done < <(yq ...)` process substitution —— yq 解析失败
# (YAML 格式错)时 · 子 shell 的非 0 退出码不会传播到 while · FAILED 仍 0 →
# CI 在残缺 registry 上误报 PASS。改为先把 yq 输出落临时文件并显式校验 $? · 再喂 while。
if ! yq -r '.metrics[] | [.name, (.required_tags_subset // [] | join(","))] | @tsv' "$REGISTRY" \
     > "$WORKDIR/metrics_usr.txt" 2>"$WORKDIR/metrics_usr.err"; then
  echo "FAIL · class 3 · yq 解析 $REGISTRY 的 metrics 失败(YAML 格式错?):"
  sed 's/^/    /' "$WORKDIR/metrics_usr.err"
  FAILED=1
else
  while IFS=$'\t' read -r metric tags; do
    [[ -z "$metric" || "$metric" == "null" ]] && continue
    for usr in service env version; do
      case ",$tags," in
        *",$usr,"*) ;;
        *)
          echo "FAIL · class 3 · metric '$metric' 缺 USR 三件套字段 '$usr'(required_tags_subset 必含 service/env/version)"
          FAILED=1
          ;;
      esac
    done
  done < "$WORKDIR/metrics_usr.txt"
fi

# ---- 第 5 步:audit_events 联动一致性(feat-031 · cross-mcp 同源)----
# neon 仓本地校验:每条 audit_event 的 required_attrs 必须含核心 attribute。
# (现阶段源码无 audit_event! emit · 不做 actual-vs-expected diff · 仅锁 schema 完整性)
AUDIT_CORE=(openneon.audit.event_type openneon.audit.outcome)
AUDIT_COUNT="$(yq -r '.audit_events | length' "$REGISTRY")"
if [[ "$AUDIT_COUNT" != "0" && "$AUDIT_COUNT" != "null" ]]; then
  for ((i = 0; i < AUDIT_COUNT; i++)); do
    ev="$(yq -r ".audit_events[$i].event_type" "$REGISTRY")"
    yq -r ".audit_events[$i].required_attrs[]" "$REGISTRY" | sort -u > "$WORKDIR/audit_req_$i.txt"
    for must in "${AUDIT_CORE[@]}"; do
      if ! grep -qxF "$must" "$WORKDIR/audit_req_$i.txt"; then
        echo "FAIL · class 4 · audit_event '$ev' 的 required_attrs 缺核心 attribute: $must"
        FAILED=1
      fi
    done
  done
fi

# ---- 汇总 ----
echo "----------------------------------------------------------------------"
if [[ "$FAILED" -ne 0 ]]; then
  echo "FAIL · metric-registry.yaml 跟源码 / schema 存在不一致(见上方违规清单)"
  exit 1
fi
echo "PASS · metric-registry.yaml 跟源码一致(扫描组件:${SRC_DIRS[*]})"
exit 0
