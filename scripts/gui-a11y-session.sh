#!/usr/bin/env bash
# 读屏支持核对（LG2）的会话部分：在独立的 D-Bus 会话中启动 AT-SPI 总线并标为“辅助技术已启用”
# （与运行 Orca 等读屏软件时相同），以模拟设备启动 qybit-gui，再由 gui-a11y-check.py 经 AT-SPI
# 读取控件树并执行动作。走真实核心、自有解码、pack 与渲染循环，只模拟设备与时间。
#
# 用法：dbus-run-session -- gui-a11y-session.sh <qybit-gui> <输出目录>
# 需要：at-spi2-core（at-spi-bus-launcher）、python3-gi、gir1.2-atspi-2.0；DISPLAY 指向运行中的
# X 服务器。环境变量 PYTHON 可指定带上述库的 Python（缺省 python3）。
set -euo pipefail
gui=$1
out=$2
here=$(dirname "$(realpath "$0")")
launcher=/usr/libexec/at-spi-bus-launcher
[ -x "$launcher" ] || {
  echo "::error::没有 $launcher（需要 at-spi2-core）"
  exit 1
}
# 独立的设置目录：不恢复其他核对保存的播放列表。
export HOME="$out/home-a11y"
export XDG_CONFIG_HOME="$HOME/.config"
mkdir -p "$HOME"
"$launcher" --launch-immediately >"$out/a11y-bus.log" 2>&1 &
bus=$!
trap 'kill $bus 2>/dev/null || true; pkill -x qybit-gui 2>/dev/null || true' EXIT
owner() {
  dbus-send --session --print-reply --dest=org.freedesktop.DBus /org/freedesktop/DBus \
    org.freedesktop.DBus.NameHasOwner string:org.a11y.Bus 2>/dev/null | grep -q true
}
for _ in $(seq 50); do
  owner && break
  sleep 0.1
done
owner || {
  echo "::error::AT-SPI 总线没有启动（$out/a11y-bus.log）"
  exit 1
}
dbus-send --session --print-reply --dest=org.a11y.Bus /org/a11y/bus \
  org.freedesktop.DBus.Properties.Set string:org.a11y.Status string:IsEnabled variant:boolean:true >/dev/null
QY_GUI_SIM=ok QY_GUI_SIM_FILES=stereo44,mono48 "$gui" >"$out/a11y.log" 2>&1 &
# libatspi 2.52 不认识 AccessKit 发出的新版 AddAccessible 信号签名，会大量打印 dbind 警告
# （只影响它自己的缓存，查询照常）；过滤掉。
"${PYTHON:-python3}" "$here/gui-a11y-check.py" "$out/a11y.log" 2> >(grep -v -e 'dbind-WARNING' -e '^$' >&2)
grep -q "读屏支持：已启用" "$out/a11y.log" || {
  echo "::error::日志中没有“读屏支持：已启用”"
  exit 1
}
