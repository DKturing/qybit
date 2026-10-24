#!/usr/bin/env python3
"""Linux 图形界面的读屏支持核对（LG2）：经 AT-SPI（与 Orca 等读屏软件相同的接口）读取 qybit-gui
的控件树并执行动作。由 scripts/gui-linux-check.sh 在 D-Bus 会话与 AT-SPI 总线中调用。

用法：gui-a11y-check.py <qybit-gui 的日志文件>
需要：python3-gi、gir1.2-atspi-2.0（pyatspi 所用的 Atspi 库）。
"""

import sys
import time
import warnings

import gi

# Atspi.Action.get_action_name 已标为过时，但 Python 绑定中替代的 get_name 与 Accessible.get_name 同名。
warnings.filterwarnings("ignore", category=DeprecationWarning)

gi.require_version("Atspi", "2.0")
from gi.repository import Atspi  # noqa: E402

LOG = sys.argv[1]
S = Atspi.StateType


def fail(msg):
    print(f"::error::读屏核对：{msg}")
    sys.exit(1)


def wait(cond, what, t=10.0):
    end = time.time() + t
    while time.time() < end:
        try:
            v = cond()
        except Exception:  # 控件树在更新中
            v = None
        if v:
            return v
        time.sleep(0.1)
    fail(f"等待超时：{what}")


def app():
    desk = Atspi.get_desktop(0)
    for i in range(desk.get_child_count()):
        a = desk.get_child_at_index(i)
        if a is not None and a.get_name() == "qybit-gui":
            return a
    return None


def walk(n, out):
    out.append(n)
    for i in range(n.get_child_count()):
        c = n.get_child_at_index(i)
        if c is not None:
            walk(c, out)
    return out


def nodes():
    a = app()
    if a is None:
        return []
    a.clear_cache()
    return walk(a, [])


def find(role, name=None, prefix=False):
    for n in nodes():
        if n.get_role_name() != role:
            continue
        nm = n.get_name()
        if name is None or nm == name or (prefix and nm.startswith(name)):
            return n
    return None


def get(role, name=None, prefix=False):
    return wait(lambda: find(role, name, prefix), f"{role}「{name}」")


def has(n, state):
    return n.get_state_set().contains(state)


def act(n, name="click"):
    a = n.get_action_iface()
    if a is None:
        fail(f"{n.get_role_name()}「{n.get_name()}」没有动作接口")
    names = [a.get_action_name(i) for i in range(a.get_n_actions())]
    if name not in names:
        fail(f"{n.get_role_name()}「{n.get_name()}」没有动作 {name}（有 {names}）")
    a.do_action(names.index(name))


def log_has(text):
    with open(LOG, encoding="utf-8") as f:
        return text in f.read()


def expect_log(text, t=10.0):
    wait(lambda: log_has(text), f"日志出现「{text}」", t)


def children(n):
    return [n.get_child_at_index(i) for i in range(n.get_child_count())]


# 1. 应用与主窗口的控件
wait(app, "AT-SPI 中出现 qybit-gui", 20)
get("frame", "QyBit")
bar = get("menu bar")
titles = [c.get_name() for c in children(bar)]
if titles != ["文件(F)", "播放(P)", "列表(L)", "查看(V)", "帮助(H)"]:
    fail(f"菜单栏为 {titles}")
for b in ["播放", "停止", "上一首", "下一首", "添加文件…"]:
    get("push button", b)
get("combo box", "输出设备")
mono = get("check box", "单声道复制到左右两路（会改变声道）")
get("check box", "浮点转整数（会改变样本）")
get("slider", "播放时的设备硬件音量（百分比）")
seek = get("slider", "播放进度（百分比）")
lst = get("list box", "播放列表")
rows = wait(lambda: [c for c in children(lst) if c.get_role_name() == "list item"], "列表行")
names = [r.get_name() for r in rows]
if len(rows) != 2 or "sim-stereo44.wav" not in names[0] or "sim-mono48.wav" not in names[1]:
    fail(f"播放列表为 {names}")
if not has(rows[0], S.FOCUSED):
    fail("第 1 行应有焦点")
log = get("log", "日志")
if log.get_child_count() == 0:
    fail("日志没有内容")
status = get("status bar", "状态栏")
if not any(c.get_name() == "就绪" for c in children(status)):
    fail(f"状态栏为 {[c.get_name() for c in children(status)]}")
if has(seek, S.ENABLED) and has(seek, S.SENSITIVE):
    fail("未播放时进度条应不可用")
print(f"控件树：菜单 {len(titles)}、播放列表 {len(rows)} 行、日志 {log.get_child_count()} 行")

# 2. 选中第 2 行：选中状态与焦点随之改变
act(get("list item", "2，", prefix=True))
wait(lambda: has(find("list item", "2，", True), S.SELECTED), "第 2 行选中")
wait(lambda: has(find("list item", "2，", True), S.FOCUSED), "焦点移到第 2 行")

# 3. 复选框
act(mono)
wait(lambda: has(find("check box", mono.get_name()), S.CHECKED), "单声道复制勾选")
act(find("check box", mono.get_name()))
wait(lambda: not has(find("check box", mono.get_name()), S.CHECKED), "单声道复制取消")

# 4. 选中第 1 行并按“播放”按钮；进度条可用后设置为 50%
act(get("list item", "1，", prefix=True))
wait(lambda: has(find("list item", "1，", True), S.SELECTED), "第 1 行选中")
act(get("push button", "播放"))
expect_log("指示灯：44.1 kHz 原生")
wait(lambda: find("list item", "正在播放：1，", True), "第 1 行标为正在播放")
seek = wait(
    lambda: (s := find("slider", "播放进度（百分比）")) and has(s, S.SENSITIVE) and s,
    "进度条可用",
)
seek.get_value_iface().set_current_value(50.0)
wait(
    lambda: find("slider", "播放进度（百分比）").get_value_iface().get_current_value() >= 49.0,
    "进度条定位到 50% 之后",
)
print("播放与定位：通过")

# 5. 菜单：打开“播放(P)”，点“停止(S)”
act(get("menu item", "播放(P)"))
menu = get("menu", "播放(P)")
items = [c.get_name() for c in children(menu)]
if "停止(S)" not in items:
    fail(f"播放菜单为 {items}")
keys = find("check menu item", "全局媒体键（窗口不在前台时也响应）(K)")
if keys is None:
    fail("全局媒体键应为可勾选的菜单项")
act(get("menu item", "停止(S)"))
expect_log("指示灯：采样率 —")
wait(lambda: find("menu", "播放(P)") is None, "菜单关闭")
print("菜单：通过")

# 6. 文件面板：路径栏的文字与光标，取消后关闭
act(get("push button", "添加文件…"))
dlg = get("dialog", "添加文件", prefix=True)
path = get("entry", "路径")
if path.get_text_iface() is None:
    fail("路径栏没有文字接口")
# Python 绑定中 Text.get_text 与 Accessible.get_text 同名：按接口类调用。
value = Atspi.Text.get_text(path, 0, -1)
caret = Atspi.Text.get_caret_offset(path)
if not value.startswith("/"):
    fail(f"路径栏文字为「{value}」")
if caret != len(value):
    fail(f"光标应在末尾：{caret} / {len(value)}")
act(get("push button", "取消"))
wait(lambda: find("dialog", "添加文件", True) is None, "文件面板关闭")
print(f"文件面板：通过（路径栏「{value}」）")

# 7. 关于对话框
act(get("menu item", "帮助(H)"))
act(get("menu item", "关于 QyBit(A)"))
get("dialog", "关于 QyBit")
act(get("push button", "确定"))
wait(lambda: find("dialog", "关于 QyBit") is None, "关于对话框关闭")
print("读屏核对通过")
