#!/usr/bin/env python3
"""
校验 pgxn/neon/probes/*.yaml 是否符合 whitelist.schema.json。

用法：
    scripts/validate_whitelist.py pgxn/neon/probes/whitelist.example.yaml
    scripts/validate_whitelist.py --schema pgxn/neon/probes/whitelist.schema.json FILE...
    scripts/validate_whitelist.py --expect-fail FILE  # 期望 reject (供 CI 用)

退出码:
    0 -> 校验通过 (或 --expect-fail 模式下校验失败)
    1 -> 校验失败 (或 --expect-fail 模式下意外通过)
    2 -> 工具/依赖错误

依赖: PyYAML + jsonschema (pip install pyyaml jsonschema)
没装依赖时直接退 exit 2 · CI 必须在装好依赖的环境跑。
"""
from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path

try:
    import yaml  # type: ignore[import-not-found]
except ImportError:
    sys.stderr.write("error: 缺少 PyYAML · 跑 'pip install pyyaml jsonschema' 后重试\n")
    sys.exit(2)

try:
    import jsonschema  # type: ignore[import-not-found]
    from jsonschema import Draft7Validator
except ImportError:
    sys.stderr.write("error: 缺少 jsonschema · 跑 'pip install pyyaml jsonschema' 后重试\n")
    sys.exit(2)


DEFAULT_SCHEMA = Path(__file__).resolve().parent.parent / "pgxn" / "neon" / "probes" / "whitelist.schema.json"


def load_schema(path: Path) -> dict:
    with path.open("r", encoding="utf-8") as f:
        return json.load(f)


def load_doc(path: Path):
    with path.open("r", encoding="utf-8") as f:
        return yaml.safe_load(f)


def validate(schema: dict, doc, path: Path) -> list[str]:
    validator = Draft7Validator(schema)
    errors = []
    for err in sorted(validator.iter_errors(doc), key=lambda e: list(e.absolute_path)):
        loc = "/".join(str(p) for p in err.absolute_path) or "<root>"
        errors.append(f"{path}: {loc}: {err.message}")
    return errors


def main() -> int:
    p = argparse.ArgumentParser(description="校验 neon probes whitelist/denylist YAML")
    p.add_argument(
        "--schema",
        type=Path,
        default=DEFAULT_SCHEMA,
        help=f"JSON Schema 路径 (默认: {DEFAULT_SCHEMA})",
    )
    p.add_argument(
        "--expect-fail",
        action="store_true",
        help="期望文件被拒 · 用于负面测试 · 通过时返回 1",
    )
    p.add_argument("files", nargs="+", type=Path, help="要校验的 YAML 文件")
    args = p.parse_args()

    if not args.schema.is_file():
        sys.stderr.write(f"error: schema 文件不存在: {args.schema}\n")
        return 2

    try:
        schema = load_schema(args.schema)
    except (OSError, json.JSONDecodeError) as e:
        sys.stderr.write(f"error: 读 schema 失败: {e}\n")
        return 2

    try:
        # schema 自身合法性检查 · 早期发现 schema 笔误
        Draft7Validator.check_schema(schema)
    except jsonschema.SchemaError as e:
        sys.stderr.write(f"error: schema 自身不合法: {e.message}\n")
        return 2

    overall_ok = True

    for fpath in args.files:
        if not fpath.is_file():
            sys.stderr.write(f"error: 文件不存在: {fpath}\n")
            overall_ok = False
            continue

        try:
            doc = load_doc(fpath)
        except yaml.YAMLError as e:
            errors = [f"{fpath}: YAML 解析失败: {e}"]
        else:
            errors = validate(schema, doc, fpath)

        passed = not errors

        if args.expect_fail:
            if passed:
                sys.stderr.write(f"FAIL: {fpath} 期望被拒但通过校验\n")
                overall_ok = False
            else:
                # negative case 命中预期 · 打印第一条错误信息便于人类核对
                first = errors[0]
                print(f"ok (expected reject): {first}")
        else:
            if passed:
                print(f"ok: {fpath}")
            else:
                for e in errors:
                    sys.stderr.write(e + "\n")
                overall_ok = False

    return 0 if overall_ok else 1


if __name__ == "__main__":
    sys.exit(main())
