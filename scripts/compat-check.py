#!/usr/bin/env python3
"""E4：按旧 tag 的契约编译当前接口，发现破坏性修改。

取出 TAG 时的 `examples/qy-ext-demo`（代表一个按该 tag 编写的仓库外插件），把它对本仓库 crate 的
依赖改为指向**当前**源码树，在独立目录中编译它的库、二进制与测试。编译失败说明当前接口与 TAG
时不兼容：要么修正，要么按 docs/e4-versioning.md 的规则升级版本号并在 CHANGELOG 中标为破坏性。

当前工作区版本与 TAG 属于同一兼容系列（0.x 时次版本号相同，1.0 起主版本号相同）时，编译失败是错误
（退出码 1）；已升级到新的系列时，编译失败是已声明的破坏性修改，只作提示（退出码 0），但
CHANGELOG.md 必须有该版本的条目。

用法：scripts/compat-check.py TAG
只用标准库（tomllib 需要 Python 3.11 及以上）。
"""

import os
import re
import shutil
import subprocess
import sys
import tarfile
import io
import tomllib

ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), ".."))
DEMO = "examples/qy-ext-demo"


def run(cmd, **kw):
    print("+", " ".join(cmd), flush=True)
    return subprocess.run(cmd, check=True, **kw)


def series(version: str) -> tuple:
    """兼容系列：0.x 按 (0, 次版本号)，1.0 起按主版本号。"""
    major, minor, _patch = (int(x) for x in version.split("-")[0].split("."))
    return (0, minor) if major == 0 else (major,)


def main() -> int:
    if len(sys.argv) != 2:
        print(__doc__)
        return 2
    tag = sys.argv[1]
    work = os.path.join(ROOT, "target", "compat", tag)
    shutil.rmtree(work, ignore_errors=True)
    os.makedirs(work)
    archive = subprocess.run(
        ["git", "-C", ROOT, "archive", "--format=tar", tag, DEMO],
        check=True,
        capture_output=True,
    ).stdout
    with tarfile.open(fileobj=io.BytesIO(archive)) as t:
        t.extractall(work, filter="data")
    demo = os.path.join(work, DEMO)

    with open(os.path.join(ROOT, "Cargo.toml"), "rb") as f:
        root = tomllib.load(f)
    package = root["workspace"]["package"]
    old = tomllib.loads(
        subprocess.run(
            ["git", "-C", ROOT, "show", f"{tag}:Cargo.toml"],
            check=True,
            capture_output=True,
            text=True,
        ).stdout
    )["workspace"]["package"]["version"]
    current = package["version"]
    declared = series(current) != series(old)
    if declared:
        with open(os.path.join(ROOT, "CHANGELOG.md"), encoding="utf-8") as f:
            if f"## [{current}]" not in f.read():
                print(
                    f"版本已从 {old} 升到 {current}（新的兼容系列），但 CHANGELOG.md 没有 ## [{current}] 条目",
                    file=sys.stderr,
                )
                return 1
    paths = {
        name: os.path.join(ROOT, spec["path"])
        for name, spec in root["workspace"]["dependencies"].items()
        if isinstance(spec, dict) and "path" in spec
    }
    external = {
        name: spec
        for name, spec in root["workspace"]["dependencies"].items()
        if not (isinstance(spec, dict) and "path" in spec)
    }

    manifest = os.path.join(demo, "Cargo.toml")
    with open(manifest, encoding="utf-8") as f:
        text = f.read()

    def dep(name: str, rest: str) -> str:
        if name in paths:
            return f'{name} = {{ path = "{paths[name]}"{rest} }}'
        spec = external.get(name)
        if spec is None:
            raise SystemExit(f"{tag} 的示例依赖 {name}，当前工作区没有这个依赖")
        version = spec if isinstance(spec, str) else spec["version"]
        return f'{name} = {{ version = "{version}"{rest} }}'

    out = []
    skip_lints = False
    for line in text.splitlines():
        if skip_lints:
            if line.startswith("["):
                skip_lints = False
            else:
                continue
        if line.strip() == "[lints]":
            skip_lints = True
            continue
        m = re.match(r"^(version|edition|rust-version|license|repository)\.workspace = true$", line)
        if m:
            key = m.group(1)
            value = "0.0.0" if key == "version" else package[key]
            out.append(f'{key} = "{value}"')
            continue
        m = re.match(r"^([A-Za-z0-9_-]+)\.workspace = true$", line)
        if m:
            out.append(dep(m.group(1), ""))
            continue
        m = re.match(r"^([A-Za-z0-9_-]+) = \{ workspace = true(.*) \}$", line)
        if m:
            out.append(dep(m.group(1), m.group(2)))
            continue
        out.append(line)
    out.append("")
    out.append("# 独立于本仓库工作区编译（E4 兼容检查）。")
    out.append("[workspace]")
    with open(manifest, "w", encoding="utf-8") as f:
        f.write("\n".join(out) + "\n")
    # 外部依赖用当前工作区锁定的版本。
    shutil.copy(os.path.join(ROOT, "Cargo.lock"), os.path.join(demo, "Cargo.lock"))

    env = dict(os.environ, CARGO_TARGET_DIR=os.path.join(ROOT, "target", "compat-target"))
    try:
        run(
            ["cargo", "build", "--manifest-path", manifest, "--lib", "--bins", "--tests"],
            env=env,
        )
    except subprocess.CalledProcessError:
        if declared:
            print(
                f"\n提示：{tag}（{old}）时编写的示例不能对当前接口编译；当前版本 {current} 已声明为"
                "新的兼容系列，破坏性修改见 CHANGELOG.md。"
            )
            return 0
        print(
            f"\n兼容检查失败：{tag} 时编写的示例不能对当前接口编译（破坏性修改）。"
            "\n按 docs/e4-versioning.md：修正为兼容的写法，或升级版本号并在 CHANGELOG.md 中写明。",
            file=sys.stderr,
        )
        return 1
    print(f"兼容检查通过：{tag}（{old}）时编写的示例能对当前接口（{current}）编译。")
    return 0


if __name__ == "__main__":
    sys.exit(main())
