#!/bin/bash
# A1：在 QEMU 虚拟机（Ubuntu 通用内核 + snd-aloop、snd-dummy）中运行内核 ALSA 接口的真实测试。
#
# 用途：CI runner 与很多开发机不能加载内核模块；这里启动一个只含 busybox、声卡模块与 musl 静态
# 测试程序的最小虚拟机，以 root 对每个目标架构运行：
#   1. qybit list-devices；
#   2. qy-host-alsa 中直接调用内核的单元测试（linux::，含打开每个 hw 设备、查询格式与声道映射；
#      其余单元测试用假设备与夹具，在宿主上运行）；
#   3. 设备测试（hw:CARD=Loopback,DEV=0：格式矩阵 48 项、顺序换曲、WAV/FLAC 播放）；
#   4. B 层回环矩阵（tests/loopback_aloop.rs，逐字节比对）；
#   5. 混音器读写（A2，tests/mixer.rs，snd-dummy 虚拟声卡 hw:CARD=Dummy）。
#
# QYBIT_VM_ARCH：
#   x86_64（默认）：x86_64 内核，运行 x86_64-unknown-linux-musl 程序；
#   aarch64：arm64 内核（qemu virt），运行 aarch64-unknown-linux-musl 程序，并经内核的 32 位兼容层
#            运行 armv7-unknown-linux-musleabihf 程序（不等同于 32 位 ARM 内核，见 docs/a1-kernel-alsa.md）。
# 报告写到 ${QYBIT_REPORT_DIR:-target/qybit-report}/vm-<架构>/<目标>/。
#
# 需要（Ubuntu x86_64 宿主）：qemu-system-x86（x86_64）或 qemu-system-arm（aarch64）、cpio、zstd、
# curl、最新的 apt 软件包索引（内核与 busybox-static 的版本从中读取，软件包直接从 Ubuntu 镜像下载），
# 以及 rustup 已安装对应目标。x86_64 有 /dev/kvm 写权限时用 KVM，否则用软件仿真（较慢，欠载的概率
# 更高：欠载会被如实报告为失败）。测试程序用 release 构建并保留溢出检查与 debug 断言。
# 这是虚拟声卡回环：经过真实内核 ALSA 路径，但不证明真实 DAC 输入样本逐位一致。
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
ARCH=${QYBIT_VM_ARCH:-x86_64}
WORK=${QYBIT_VM_DIR:-$ROOT/target/aloop-vm}/$ARCH
REPORT=${QYBIT_REPORT_DIR:-$ROOT/target/qybit-report}/vm-$ARCH
case "$ARCH" in
    x86_64)
        DEB_ARCH=amd64; MIRROR=http://archive.ubuntu.com/ubuntu
        TARGETS="x86_64-unknown-linux-musl"
        QEMU=qemu-system-x86_64; CONSOLE=ttyS0 ;;
    aarch64)
        DEB_ARCH=arm64; MIRROR=http://ports.ubuntu.com/ubuntu-ports
        TARGETS="aarch64-unknown-linux-musl armv7-unknown-linux-musleabihf"
        QEMU=qemu-system-aarch64; CONSOLE=ttyAMA0 ;;
    *) echo "不支持的 QYBIT_VM_ARCH：$ARCH" >&2; exit 2 ;;
esac
rm -rf "$REPORT" && mkdir -p "$WORK" "$REPORT"

for tool in $QEMU cpio zstd dpkg-deb curl apt-cache; do
    command -v "$tool" >/dev/null || { echo "未运行：缺少 $tool" >&2; exit 2; }
done

# ---- 内核、声卡模块与静态 busybox（Ubuntu 软件包，按宿主 apt 索引中的版本） ----
fetch() { # 包名 版本 → 解压到 $WORK/pkg/root
    local name=$1 ver=$2 first=${1:0:1}
    local src=$name
    case "$name" in linux-*) src=linux; first=l ;; busybox-static) src=busybox; first=b ;; esac
    curl -fsS -o "$WORK/pkg/$name.deb" "$MIRROR/pool/main/$first/$src/${name}_${ver}_$DEB_ARCH.deb"
    dpkg-deb -x "$WORK/pkg/$name.deb" "$WORK/pkg/root"
}
if [ ! -f "$WORK/pkg/ok" ] || [ ! -f "$WORK/pkg/ko/snd-dummy.ko" ]; then
    rm -rf "$WORK/pkg" && mkdir -p "$WORK/pkg/ko"
    image=$(apt-cache depends linux-image-generic | sed -n 's/.*Depends: \(linux-image-[0-9][^ ]*\)/\1/p' | head -1)
    kver=${image#linux-image-}
    pver=$(apt-cache show "$image" | sed -n 's/^Version: //p' | head -1)
    bver=$(apt-cache show busybox-static | sed -n 's/^Version: //p' | head -1)
    [ -n "$kver" ] && [ -n "$pver" ] && [ -n "$bver" ] || { echo "未运行：apt 索引中找不到内核或 busybox 版本" >&2; exit 2; }
    for p in linux-image-unsigned linux-modules linux-modules-extra; do fetch "$p-$kver" "$pver"; done
    fetch busybox-static "${bver#*:}"
    cp "$WORK/pkg/root/boot/vmlinuz-$kver" "$WORK/pkg/vmlinuz"
    for m in soundcore snd snd-timer snd-pcm snd-aloop snd-dummy; do
        f=$(find "$WORK/pkg/root/lib/modules/$kver" -name "$m.ko*" | head -1)
        [ -n "$f" ] || { echo "未运行：内核 $kver 没有模块 $m" >&2; exit 2; }
        case "$f" in
            *.zst) zstd -dqf "$f" -o "$WORK/pkg/ko/$m.ko" ;;
            *) cp "$f" "$WORK/pkg/ko/$m.ko" ;;
        esac
    done
    cp "$WORK/pkg/root/usr/bin/busybox" "$WORK/pkg/busybox"
    # 只留内核、模块与 busybox（解压的软件包约 1 GB）。
    rm -rf "$WORK/pkg/root" "$WORK"/pkg/*.deb
    echo "$kver" > "$WORK/pkg/ok"
fi
echo "内核：$(cat "$WORK/pkg/ok")（$DEB_ARCH）"

# ---- 最小根文件系统 ----
I=$WORK/initrd
rm -rf "$I" && mkdir -p "$I"/{bin,proc,sys,dev,tmp,ko,t}
cp "$WORK/pkg/busybox" "$I/bin/busybox"
for c in sh mount insmod ls cat echo poweroff mkdir; do ln -s busybox "$I/bin/$c"; done
cp "$WORK"/pkg/ko/*.ko "$I/ko/"
# 设备测试的 FLAC 夹具按编译时的路径（CARGO_MANIFEST_DIR/../qy-codec-flac/…）读取。
flac=crates/qy-codec-flac/tests/fixtures/cli_16_stereo_44k_default.flac
mkdir -p "$I$ROOT/crates/qy-host-alsa" "$I$ROOT/$(dirname $flac)" && cp "$ROOT/$flac" "$I$ROOT/$flac"

# ---- 静态测试程序 ----
cd "$ROOT"
export CARGO_PROFILE_RELEASE_DEBUG_ASSERTIONS=true CARGO_PROFILE_RELEASE_OVERFLOW_CHECKS=true
for t in $TARGETS; do
    cargo build --locked --release -p qybit --target "$t"
    cargo test --locked --release -p qy-host-alsa --target "$t" --no-run
    mkdir -p "$I/t/$t"
    cp "target/$t/release/qybit" "$I/t/$t/"
    for b in qy_host_alsa device loopback_aloop mixer; do
        cp "$(ls -t target/"$t"/release/deps/"$b"-* | grep -v '\.d$' | head -1)" "$I/t/$t/$b"
    done
done

# 软件仿真（aarch64，或 x86_64 没有 KVM）时 CPU 慢：B 层播放端用 16 个周期（160 ms）的设备缓冲，
# 留出时序余量；只影响缓冲长度，不影响比对与欠载判定。KVM 下用后端默认值。
aloop_periods=""
if [ "$ARCH" != x86_64 ] || [ ! -w /dev/kvm ]; then aloop_periods=16; fi
cat > "$I/init" <<EOF
#!/bin/sh
TARGETS="$TARGETS"
[ -n "$aloop_periods" ] && export QYBIT_ALOOP_PERIODS=$aloop_periods
EOF
cat >> "$I/init" <<'EOF'
mount -t proc proc /proc; mount -t sysfs sys /sys; mount -t devtmpfs dev /dev; mount -t tmpfs tmp /tmp
for m in soundcore snd snd-timer snd-pcm snd-aloop snd-dummy; do insmod /ko/$m.ko; done
echo "=== kernel $(cat /proc/sys/kernel/osrelease)"
cat /proc/asound/cards
export QYBIT_ALSA_DEVICE=hw:CARD=Loopback,DEV=0 QYBIT_MIXER_DEVICE=hw:CARD=Dummy RUST_BACKTRACE=1
cd /tmp
step() { name=$1; shift; echo "=== step $name"; "$@" 2>&1; echo "=== result $name $?"; }
for t in $TARGETS; do
    d=/t/$t
    export QYBIT_REPORT_DIR=/tmp/report/$t
    mkdir -p $QYBIT_REPORT_DIR
    step "$t/list-devices" $d/qybit list-devices
    step "$t/unit" $d/qy_host_alsa --nocapture --test-threads=1 linux::
    step "$t/device" $d/device --ignored --nocapture --test-threads=1
    step "$t/loopback" $d/loopback_aloop --ignored --nocapture --test-threads=1
    step "$t/mixer" $d/mixer --ignored --nocapture --test-threads=1
    for f in $QYBIT_REPORT_DIR/*; do
        [ -f "$f" ] || continue
        echo "=== begin $t/${f##*/}"; cat "$f"; echo; echo "=== end $t/${f##*/}"
    done
done
echo "=== done"
poweroff -f
EOF
chmod +x "$I/init"
(cd "$I" && find . | cpio -o -H newc 2>/dev/null | gzip -1) > "$WORK/initrd.gz"

if [ "$ARCH" = x86_64 ]; then
    if [ -w /dev/kvm ]; then machine=(-accel kvm -cpu host); else machine=(-accel tcg -cpu max); fi
else
    machine=(-M virt -cpu max)
fi
echo "虚拟机：$QEMU ${machine[*]}"
log=$REPORT/console.log
timeout "${QYBIT_VM_TIMEOUT:-5400}" "$QEMU" "${machine[@]}" -m 1024 -smp 2 -nographic -no-reboot \
    -kernel "$WORK/pkg/vmlinuz" -initrd "$WORK/initrd.gz" \
    -append "console=$CONSOLE quiet loglevel=1 panic=-1 rdinit=/init" | tr -d '\r' > "$log" || true

# 取出报告与各步骤结果。
awk -v dir="$REPORT" '
    /^=== begin / { f = dir "/" $3; sub(/\/[^\/]*$/, "", f); system("mkdir -p \"" f "\""); f = dir "/" $3; printf "" > f; next }
    /^=== end / { close(f); f = ""; next }
    f != "" { print >> f }
' "$log"
grep -E '=== kernel|^=== (result|done)' "$log" || true
grep -q '^=== done' "$log" || { echo "虚拟机未正常结束，见 $log" >&2; exit 1; }
fail=0
for t in $TARGETS; do
    for s in list-devices unit device loopback mixer; do
        grep -q "^=== result $t/$s 0\$" "$log" || { echo "失败：$t/$s（见 $log）" >&2; fail=1; }
    done
done
echo "报告：$REPORT"
exit $fail
