#!/usr/bin/env bash
# Linux 图形界面核对（模拟设备，`qybit-gui --features sim`）：在 Xvfb 中用 xdotool 操作，
# 核对状态栏与指示灯（模拟设备模式下写到标准错误）、拖动调整顺序、记住设置与恢复、XDND 拖放，
# 并保存截图。走真实核心、自有解码、pack 与渲染循环，只模拟设备与时间；没有声音输出。
#
# 用法：gui-linux-check.sh <qybit-gui> <xdnd-send 或 -> <输出目录> [<xim-commit 或 ->]
# 第 4 个参数为测试用输入法（tools/gui-proto-linux 的 xim-commit）时另核对路径栏的输入法输入（LG2）。
# 环境变量 QY_A11Y_CHECK=1 时另在独立的 D-Bus 会话中核对读屏支持（LG2，gui-a11y-session.sh）。
# 需要：Xvfb、xdotool、ImageMagick（import）、中文字体。坐标按 96 DPI（缩放 1）的布局。
set -euo pipefail
gui=$(realpath "$1")
xdnd=$2
out=$(realpath -m "$3")
xim=${4:--}
mkdir -p "$out"
export HOME="$out/home"
# 设置文件按 XDG 规范写到 $XDG_CONFIG_HOME（GitHub runner 设置了该变量），一并指向临时目录。
export XDG_CONFIG_HOME="$HOME/.config"
rm -rf "$HOME"
mkdir -p "$HOME"
export DISPLAY=:93
Xvfb :93 -screen 0 1280x800x24 >/dev/null 2>&1 &
xvfb=$!
trap 'kill $xvfb 2>/dev/null || true; pkill -x qybit-gui 2>/dev/null || true; pkill -x xim-commit 2>/dev/null || true' EXIT
sleep 1

fail() {
  echo "::error::$*"
  exit 1
}
shot() { import -window root "$out/$1.png"; }
win() { xdotool search --sync --classname qybit-gui | head -1; }
# 等待日志出现某一行（最多 $2 秒）。
expect() {
  local pat=$1 t=${2:-10}
  for _ in $(seq $((t * 10))); do
    grep -q -- "$pat" "$log" && return 0
    sleep 0.1
  done
  fail "等待超时：$pat（日志见 $log）"
}
start() {
  log="$out/$1.log"
  QY_GUI_SIM=ok QY_GUI_SIM_FILES=$2 "$gui" >"$log" 2>&1 &
  sleep 2
  W=$(win)
}
click() { xdotool mousemove --window "$W" "$1" "$2" click 1; }
dclick() { xdotool mousemove --window "$W" "$1" "$2" click --repeat 2 --delay 80 1; }
row() { echo $((174 + 22 * $1)); }

# 1. 播放、指示灯、单声道被拒与明确开启的单声道复制
start run1 stereo44,mono48,short
shot 01-ready
# 没有读屏软件（没有会话总线或辅助技术未启用）时读屏支持不激活。
if grep -q "读屏支持：已启用" "$log"; then fail "没有读屏软件时读屏支持不应激活"; fi
click 40 44
expect "指示灯：44.1 kHz 原生"
sleep 1
shot 02-playing
dclick 200 "$(row 1)"
expect "指示灯：48 kHz 未打开"
shot 03-mono-rejected
click 14 73
dclick 200 "$(row 1)"
expect "指示灯：48 kHz 原生"
expect "声道已改变"
shot 04-mono-duplicated
click 120 44
expect "指示灯：采样率 —"

# 2. 菜单、设备下拉框、文件面板、关于
click 90 11
sleep 0.3
shot 05-menu
xdotool key Escape
click 600 44
sleep 0.3
shot 06-devices
xdotool key Escape
click 360 44
sleep 0.5
shot 07-files
xdotool key Escape
click 270 11
sleep 0.2
click 300 40
sleep 0.3
shot 08-about
xdotool key Return

# 3. 拖动第 1 行到列表末尾，开启“记住”，退出后读设置文件核对顺序
click 200 "$(row 0)"
sleep 0.5
xdotool mousemove --window "$W" 200 "$(row 0)" mousedown 1 \
  mousemove --window "$W" 200 $(($(row 0) + 10)) sleep 0.1 \
  mousemove --window "$W" 200 $(($(row 2) + 15)) sleep 0.1 \
  mousemove --window "$W" 200 $(($(row 2) + 16))
sleep 0.3
shot 09-drag
xdotool mouseup 1
sleep 0.5
if [ "$xdnd" != - ]; then
  "$xdnd" "$W" "/tmp/qybit-gui-sim/sim-short.wav" | tee "$out/xdnd.txt"
  grep -q "XdndFinished：成功 = 1" "$out/xdnd.txt" || fail "拖放未完成"
  sleep 0.5
fi
xdotool key alt+f
sleep 0.2
xdotool key m
sleep 0.5
xdotool key alt+f
sleep 0.2
xdotool key x
sleep 1
pgrep -x qybit-gui && fail "退出命令后程序仍在运行"
cfg="$HOME/.config/qybit/qybit-gui.cfg"
[ -f "$cfg" ] || fail "没有保存设置文件"
grep '^file=' "$cfg" | sed 's|.*/||' >"$out/order.txt"
want="sim-mono48.wav sim-short.wav sim-stereo44.wav"
[ "$xdnd" != - ] && want="$want sim-short.wav"
[ "$(tr '\n' ' ' <"$out/order.txt" | sed 's/ $//')" = "$want" ] ||
  fail "顺序不符：$(tr '\n' ' ' <"$out/order.txt")，应为 $want"
grep -qE '^(mono|float)' "$cfg" && fail "设置文件不应保存两个非严格选项"

# 4. 再次启动：恢复播放列表
start run2 ""
expect "已恢复上次的播放列表"
shot 10-restored
xdotool key alt+f
sleep 0.2
xdotool key x
sleep 1

# 5. 输入法（LG2）：测试用输入法在 z 键时提交“音乐目录”，其他键原样送回。在文件面板路径栏输入
#    “$HOME/” + z + “/a.wav”后回车：文件加入列表（退出后从设置文件核对路径）；输入法只在路径栏
#    有焦点时获得焦点，并收到光标位置。
if [ "$xim" != - ]; then
  mkdir -p "$HOME/音乐目录"
  cp /tmp/qybit-gui-sim/sim-short.wav "$HOME/音乐目录/a.wav"
  "$xim" qytest 音乐目录 >"$out/xim.log" 2>&1 &
  sleep 0.5
  export XMODIFIERS=@im=qytest
  start run3 ""
  unset XMODIFIERS
  expect "输入法：已连接 qytest（光标处）"
  if grep -q "获得焦点" "$out/xim.log"; then fail "路径栏没有焦点时输入法不应获得焦点"; fi
  click 360 44
  sleep 0.5
  xdotool key Tab
  sleep 0.3
  grep -q "获得焦点" "$out/xim.log" || fail "路径栏有焦点时输入法应获得焦点（$out/xim.log）"
  xdotool key End
  xdotool key --delay 5 --repeat 300 BackSpace
  xdotool type --delay 20 "$HOME/"
  xdotool key z
  sleep 0.2
  xdotool type --delay 20 "/a.wav"
  sleep 0.3
  shot 11-ime-path
  expect "输入法提交：音乐目录"
  grep -q "光标位置" "$out/xim.log" || fail "输入法没有收到光标位置"
  xdotool key Return
  sleep 1
  grep -q "失去焦点" "$out/xim.log" || fail "面板关闭后输入法应失去焦点"
  shot 12-ime-added
  xdotool key alt+f
  sleep 0.2
  xdotool key x
  sleep 1
  grep -qF "file=$HOME/音乐目录/a.wav" "$cfg" ||
    fail "经输入法输入的路径没有加入列表（设置文件中的文件：$(grep '^file=' "$cfg" | tr '\n' ' ')）"
fi
# 6. 读屏支持（LG2）：在独立的 D-Bus 会话中经 AT-SPI 读取控件树并执行动作。
if [ "${QY_A11Y_CHECK:-0}" = 1 ]; then
  dbus-run-session -- "$(dirname "$(realpath "$0")")/gui-a11y-session.sh" "$gui" "$out" ||
    fail "读屏核对失败（日志 $out/a11y.log）"
fi
echo "Linux 图形界面核对通过（截图与日志在 $out）"
