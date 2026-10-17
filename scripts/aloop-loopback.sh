#!/bin/sh
# M2 T4 B 层：snd-aloop 数字回环（本地运行）。
#
# 需要：Linux、可加载 snd-aloop 内核模块（多数发行版在 linux-modules-extra 包中）；A1 起直接使用
# 内核接口，不需要 libasound。不能加载内核模块时用 scripts/aloop-vm.sh（QEMU 虚拟机）。
# Loopback 声卡不能被 PipeWire/PulseAudio/WirePlumber 占用
# （必要时临时停止：systemctl --user stop pipewire pipewire-pulse wireplumber）。
# 报告写入 ${QYBIT_REPORT_DIR:-target/qybit-report}/qybit-alsa-loopback-snd-aloop.json。
# 这是虚拟回环，不证明真实 DAC 输入样本逐位一致。
set -eu

if ! grep -q Loopback /proc/asound/cards 2>/dev/null; then
    echo "加载 snd-aloop ……"
    if ! sudo modprobe snd-aloop; then
        echo "未运行：无法加载 snd-aloop 内核模块" >&2
        exit 2
    fi
fi
cat /proc/asound/cards
cargo test --locked -p qy-host-alsa --test loopback_aloop -- --ignored --nocapture --test-threads=1
