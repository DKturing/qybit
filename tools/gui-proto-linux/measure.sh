#!/usr/bin/env bash
# LG0 原型测量：在已运行的显示服务器（DISPLAY 或 WAYLAND_DISPLAY）上运行一个原型程序，
# 记录首帧时间、加载的共享库、空闲 CPU（30 秒，刷新/静止）、内存与绘制/提交耗时。
# 用法：measure.sh <程序路径> <输出目录>
set -u
bin=$1
out=$2
name=$(basename "$bin")
mkdir -p "$out"

cpu_ticks() { awk '{print $14 + $15}' "/proc/$1/stat"; }
mem() { grep -E 'VmRSS|VmHWM' "/proc/$1/status" | tr -s ' \t' ' ' | paste -sd ';'; }

run_idle() { # $1 = 模式名，$2 = QY_PROTO_STATIC 值
  QY_PROTO_STATIC=$2 "$bin" 2>"$out/$name-$1.log" &
  local pid=$!
  sleep 3
  if ! kill -0 $pid 2>/dev/null; then echo "$name $1：程序已退出"; cat "$out/$name-$1.log"; return; fi
  if [ "$1" = refresh ]; then
    grep -o '/[^ ]*\.so[^ ]*' "/proc/$pid/maps" | sort -u >"$out/$name-libs.txt"
  fi
  local t0 t1 hz
  hz=$(getconf CLK_TCK)
  t0=$(cpu_ticks $pid)
  sleep 30
  t1=$(cpu_ticks $pid)
  echo "$name $1：30 秒 CPU $(( (t1 - t0) * 1000 / hz )) ms；$(mem $pid)"
  kill -TERM $pid; wait $pid 2>/dev/null
  grep 首帧 "$out/$name-$1.log"
}

run_idle refresh 0
run_idle static 1
QY_PROTO_BENCH=1 "$bin" 2>"$out/$name-bench.log"
sed "s/^/$name 测量：/" "$out/$name-bench.log"
echo "$name 运行时加载的共享库：$(wc -l <"$out/$name-libs.txt" 2>/dev/null || echo 0) 个"
sed 's/^/  /' "$out/$name-libs.txt" 2>/dev/null
