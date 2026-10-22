#!/usr/bin/env bash
# verify.sh — 本地推送前自检（只读，不改任何文件）
#
# 覆盖范围声明（实施令 F-B8 裁决）：本脚本覆盖「PR 门禁三件套」
# （fmt --check / clippy -D warnings / test --locked）＋ release 目标交叉编译 check
# ＋ commit trailer 契约 lint（提示模式）＋ 只读 diff 报告。
# 本脚本【不】宣称复现完整 CI 门禁：windows-latest 真机环境、winres 图标嵌入、
# protocol freeze python unittest、release/nightly workflow 行为均不在覆盖范围。
# 对账表见 docs/experience/06-release-reconciliation.md F 节（F.1 常设区 / F.2 本期对账区；人工维护，本脚本只读、不覆盖写）。
#
# 用法：bash verify.sh [--quick]        # --quick 跳过交叉编译 check
#       bash verify.sh --a4 [--strict]  # 只跑 A4 版本一致性哈希双算（不依赖 cargo，
#                                       # 供 nightly dry-run 自检调用，P1-5）；
#                                       # --strict＝fail-closed（C-06）：python3 缺席
#                                       # 或无 tomllib（<3.11 且未装 tomli）时 ::error
#                                       # 并非 0 退出，不再 SKIP 静默绿。本地默认
#                                       # 非 strict：缺席仍只提示跳过。
set -uo pipefail

cd "$(cd "$(dirname "$0")/volttray" && pwd)"

fail=0
section() { printf '\n=== %s ===\n' "$1"; }
run() {
  local label="$1"; shift
  section "$label"
  printf '$ %s\n' "$*"
  "$@"
  local rc=$?
  printf '[exit code] %s — %s\n' "$rc" "$label"
  if [ $rc -ne 0 ]; then fail=1; fi
}

# ⓪ A4 版本一致性哈希双算（P1-5：门禁最小机械子集，零判断项——两条独立
#    取数路径各自出结果，双算一致才通过；不依赖 cargo，python3 tomllib +
#    grep 文本路。沙箱 / CI ubuntu 均可跑）。
#    路径甲（结构路）：python tomllib 解析 workspace.package.version，
#    并断言每个 member crate 的 [package] 无字面 version、均为
#    version.workspace = true（fuzz 为独立 workspace，不在断言范围）。
#    路径乙（文本路）：sed 从根 Cargo.toml 抠 [workspace.package] 的
#    version 行，grep 数 member crates 里 version.workspace = true 的行数
#    并与 crate 数比对。
#    「脚本自身失效如何被发现」的自答：本函数经 --a4 --strict 形态被
#    nightly.yml verify-selfcheck job 每晚 dry-run 实跑（含 bash -n 语法
#    检查，C-06 起 fail-closed：SKIP 即 ::error 并非 0 退出），失效当晚
#    job 红、按 N-2 同法自动开单——脚本失效不依赖人来发现。
a4_check() {
  local rc=0
  section "A4 版本一致性哈希双算（版本号一处修改、全仓生效）"
  if ! command -v python3 >/dev/null 2>&1; then
    if [ "${A4_STRICT:-0}" = "1" ]; then
      printf '::error::python3 不可用，A4 双算无法执行——fail-closed（C-06：SKIP 静默绿已废止）\n'
      return 1
    fi
    printf '[提示] python3 不可用，A4 双算跳过（完整门禁以 CI 为准）\n'
    return 0
  fi
  local ver_a ver_b
  ver_a=$(python3 - <<'PYEOF'
import sys, pathlib
try:
    import tomllib
except ImportError:
    try:
        import tomli as tomllib
    except ImportError:
        print("SKIP:no-tomllib"); sys.exit(3)
root = pathlib.Path(".")
ws = tomllib.loads((root / "Cargo.toml").read_text(encoding="utf-8"))
version = ws.get("workspace", {}).get("package", {}).get("version", "")
if not version or not isinstance(version, str):
    print("ERR:no-workspace-version"); sys.exit(1)
members = sorted(p.parent for p in root.glob("crates/*/Cargo.toml"))
if not members:
    print("ERR:no-members"); sys.exit(1)
bad = []
for m in members:
    pkg = tomllib.loads((m / "Cargo.toml").read_text(encoding="utf-8")).get("package", {})
    v = pkg.get("version")
    if isinstance(v, str):
        bad.append(f"{m.name}: literal version={v}")
    elif not (isinstance(v, dict) and v.get("workspace") is True):
        bad.append(f"{m.name}: version 键缺失或非 workspace 继承")
if bad:
    print("ERR:" + ";".join(bad)); sys.exit(1)
print(version)
PYEOF
  )
  if [ "$?" -eq 3 ] || [ "${ver_a#SKIP:}" != "$ver_a" ]; then
    if [ "${A4_STRICT:-0}" = "1" ]; then
      printf '::error::python3 无 tomllib/tomli（<3.11 且未装 tomli），A4 双算无法执行——fail-closed（C-06：nightly 前提断言 python≥3.11，本分支出现即自检失效）\n'
      return 1
    fi
    printf '[提示] python3 无 tomllib/tomli（<3.11 且未装 tomli），A4 双算跳过（完整门禁以 CI / 新版 python 为准）\n'
    return 0
  fi
  if [ -z "$ver_a" ] || [ "${ver_a#ERR:}" != "$ver_a" ]; then
    printf '[fail] A4 路径甲（tomllib 结构路）：%s\n' "$ver_a"
    return 1
  fi
  ver_b=$(awk '/^\[workspace\.package\]/{s=1;next} /^\[/{s=0} s && /^version *= *"/' Cargo.toml \
    | head -1 | sed -E 's/^version *= *"([^"]+)".*/\1/')
  local n_crates n_ws
  n_crates=$(ls -d crates/*/Cargo.toml 2>/dev/null | wc -l | tr -d ' ')
  n_ws=$(grep -l '^version\.workspace *= *true' crates/*/Cargo.toml 2>/dev/null | wc -l | tr -d ' ')
  printf '[路径甲·tomllib] workspace version = %s（members=%s，全部 version.workspace=true）\n' "$ver_a" "$(ls -d crates/*/Cargo.toml | wc -l | tr -d ' ')"
  printf '[路径乙·grep 文本] workspace version = %s（crates=%s，version.workspace=true 文件数=%s）\n' "$ver_b" "$n_crates" "$n_ws"
  if [ -z "$ver_b" ]; then
    printf '[fail] A4 路径乙（grep 文本路）：未在 [workspace.package] 抠到 version 行\n'
    return 1
  fi
  if [ "$ver_a" != "$ver_b" ]; then
    printf '[fail] A4 双算不一致：路径甲=%s 路径乙=%s（版本号存在双源漂移，门禁 A4 不通过形态）\n' "$ver_a" "$ver_b"
    rc=1
  fi
  if [ "$n_crates" != "$n_ws" ]; then
    printf '[fail] A4 双算不一致：crates 数=%s ≠ version.workspace=true 文件数=%s（某 crate 版本号忘了走 workspace，门禁 A4 不通过形态）\n' "$n_crates" "$n_ws"
    rc=1
  fi
  if [ $rc -eq 0 ]; then
    printf '[ok] A4 哈希双算一致：两条独立路径均得 %s，无 crate 单独漂移\n' "$ver_a"
  fi
  return $rc
}

if [ "${1:-}" = "--a4" ]; then
  # --strict（C-06 fail-closed）：SKIP 即 ::error 并非 0 退出，供 nightly
  # verify-selfcheck 调用；本地手动跑默认非 strict。
  if [ "${2:-}" = "--strict" ]; then A4_STRICT=1; fi
  a4_check
  exit $?
fi

printf 'verify.sh：本地推送前自检（只读）。覆盖范围＝PR 门禁三件套 + A4 版本双算 + 交叉 check + trailer lint + diff 报告。\n'

# ⓪ A4 版本一致性哈希双算（见上方函数注释；python3 缺席时提示跳过不阻断）
a4_check || fail=1

# ① fmt（门禁一）
run "fmt --check" cargo fmt --all -- --check

# ② clippy（门禁二，对齐 ci.yml：--locked -D warnings）
run "clippy" cargo clippy --workspace --all-targets --locked -- -D warnings

# ③ test（门禁三，对齐 ci.yml：--locked）
# 注（R2-F3 后半）：workspace 测试在 Linux 宿主编译 hidapi 需 libudev
# 开发库；沙箱/精简 Linux 环境缺 libudev 时本步预期跳过（真实门禁以
# windows-gnu 交叉目标的 fmt/clippy/check 三件套为准）。
run "test" cargo test --workspace --locked

# ④ commit trailer 契约（N-1 裁决：lint-only 只读提示，缺项不阻断）
section "commit trailer 契约（提示模式，不阻断）"
if git rev-parse --verify HEAD >/dev/null 2>&1; then
  msg="$(git log -1 --format='%B')"
  for t in Item Verify Not-Verified; do
    if printf '%s' "$msg" | grep -q "^$t:"; then
      printf '[ok] trailer %s 存在\n' "$t"
    else
      printf '[提示] 最近一条 commit 缺少 %s: trailer（仅提示，不阻断）\n' "$t"
    fi
  done
else
  printf '[提示] 无 commit 可查\n'
fi

# ⑤ 交叉编译 check（release 产物形态；zig cc 参数映射固化，实施令第 0 步）
#    zig 0.16 不认 --target=x86_64-pc-windows-gnu 三元组，cc crate 会传它，
#    故 wrapper 过滤 --target=xxx 并统一改写为 -target x86_64-windows-gnu。
section "release check (x86_64-pc-windows-gnu, zig cc)"
if [ "${1:-}" = "--quick" ]; then
  printf '[跳过] --quick 模式\n'
elif command -v zig >/dev/null 2>&1; then
  wrapper="$(mktemp /tmp/zig-cc-XXXXXX.sh)"
  # 数组版 wrapper（R2-F1 修复）：旧版 eval 拼接引号丢失，多参数调用（如 -E /tmp/t.c）
  # 必粘连报 Unknown Clang option；bash 数组逐参传递，无引号面。
  cat > "$wrapper" <<'WEOF'
#!/usr/bin/env bash
args=()
skip_next=0
for a in "$@"; do
  case "$a" in
    --target=*) ;;
    -target) skip_next=1; continue ;;
    *) if [ "$skip_next" = 1 ]; then
         skip_next=0
         continue
       fi
       args+=("$a") ;;
  esac
done
exec zig cc -target x86_64-windows-gnu "${args[@]}"
WEOF
  chmod +x "$wrapper"
  export CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER="$wrapper"
  export CC_x86_64_pc_windows_gnu="$wrapper"
  run "release check (x86_64-pc-windows-gnu)" cargo check --workspace --release --locked --target x86_64-pc-windows-gnu
  rm -f "$wrapper"
else
  printf '[跳过] zig 不可用（交叉链为可选自检项；CI windows-latest 上 release build 承载）\n'
fi

# ⑥ 只读 diff 报告（N-1 裁决：供人对照 F 节对账表，脚本不覆盖写对账表）
section "diff 报告（只读）"
base="$(git rev-parse --verify -q origin/main >/dev/null 2>&1 && echo origin/main || echo main)"
if git rev-parse --verify -q "$base" >/dev/null 2>&1; then
  printf -- '--- 工作区未提交改动（git status --short）---\n'
  git status --short
  printf -- '--- 相对 %s 的改动统计（git diff --stat）---\n' "$base"
  git diff "$base" --stat
  printf -- '--- 红线自查：protocol/transport/device 三 crate 应零 diff ---\n'
  if git diff "$base" --quiet -- crates/protocol crates/transport crates/device; then
    printf '[ok] protocol/transport/device 零 diff\n'
  else
    printf '[警告] 三 crate 存在 diff，请人工复核！\n'
    git diff "$base" --stat -- crates/protocol crates/transport crates/device
  fi
else
  printf '[提示] 无基线分支可比对\n'
fi

# 汇总（只读结论，不自动拦截）
section "汇总"
if [ $fail -eq 0 ]; then
  printf '结果：门禁三件套全部通过。\n注意：本脚本不覆盖 Windows 真机行为与完整 CI 门禁，通过≠已验证真机。\n'
else
  printf '结果：存在未通过项（见上方 exit code），修复后再推送。\n'
fi
exit $fail
