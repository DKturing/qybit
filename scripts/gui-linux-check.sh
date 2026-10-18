#!/usr/bin/env bash
# Linux 图形界面核对（模拟设备，`qybit-gui --features sim`）：在 Xvfb 中用 xdotool 操作，
# 核对状态栏与指示灯（模拟设备模式下写到标准错误）、拖动调整顺序、记住设置与恢复、XDND 拖放，
# 并保存截图。走真实核心、自有解码、pack 与渲染循环，只模拟设备与时间；没有声音输出。
#
# 用法：gui-linux-check.sh <qybit-gui> <xdnd-send 或 -> <输出目录>
# 需要：Xvfb、xdotool、ImageMagick（import）、中文字体。坐标按 96 DPI（缩放 1）的布局。
set -euo pipefail
gui=$(realpath "$1")
xdnd=$2
out=$(realpath -m "$3")
mkdir -p "$out"
export HOME="$out/home"
rm -rf "$HOME"
mkdir -p "$HOME"
export DISPLAY=:93
Xvfb :93 -screen 0 1280x800x24 >/dev/null 2>&1 &
xvfb=$!
trap 'kill $xvfb 2>/dev/null || true; pkill -x qybit-gui 2>/dev/null || true' EXIT
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
echo "Linux 图形界面核对通过（截图与日志在 $out）"
