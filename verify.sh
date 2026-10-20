#!/usr/bin/env bash
# verify.sh — 本地推送前自检（只读，不改任何文件）
#
# 覆盖范围声明（实施令 F-B8 裁决）：本脚本覆盖「PR 门禁三件套」
# （fmt --check / clippy -D warnings / test --locked）＋ release 目标交叉编译 check
# ＋ commit trailer 契约 lint（提示模式）＋ 只读 diff 报告。
# 本脚本【不】宣称复现完整 CI 门禁：windows-latest 真机环境、winres 图标嵌入、
# protocol freeze python unittest、release/nightly workflow 行为均不在覆盖范围。
# 对账表见 docs/experience/03-release-quality-gate.md F 节（人工维护，本脚本只读、不覆盖写）。
#
# 用法：bash verify.sh [--quick]   # --quick 跳过交叉编译 check
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

printf 'verify.sh：本地推送前自检（只读）。覆盖范围＝PR 门禁三件套 + 交叉 check + trailer lint + diff 报告。\n'

# ① fmt（门禁一）
run "fmt --check" cargo fmt --all -- --check

# ② clippy（门禁二，对齐 ci.yml：--locked -D warnings）
run "clippy" cargo clippy --workspace --all-targets --locked -- -D warnings

# ③ test（门禁三，对齐 ci.yml：--locked）
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
  cat > "$wrapper" <<'WEOF'
#!/bin/sh
args=""
for a in "$@"; do
  case "$a" in
    --target=*) ;;
    -target) skip_next=1; continue ;;
    *) [ "$skip_next" = 1 ] && { skip_next=0; continue; }
       args="$args \"$(printf "%s" "$a" | sed 's/"/"\\""/g')" ;;
  esac
done
eval "exec zig cc -target x86_64-windows-gnu $args"
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
