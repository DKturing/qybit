//! 查找占用播放设备节点的进程：扫描 `/proc/<pid>/fd` 中指向 `/dev/snd/pcmC{卡}D{设备}p`
//! 的符号链接，读取 `/proc/<pid>/comm`。无权读取的进程跳过；只用于报错提示。

use std::path::Path;

pub fn find(card: u32, device: u32) -> Vec<String> {
    let node = format!("/dev/snd/pcmC{card}D{device}p");
    let mut out = Vec::new();
    let Ok(procs) = std::fs::read_dir("/proc") else {
        return out;
    };
    for p in procs.flatten() {
        let name = p.file_name();
        let Some(pid) = name
            .to_str()
            .filter(|s| s.bytes().all(|b| b.is_ascii_digit()))
        else {
            continue;
        };
        let Ok(fds) = std::fs::read_dir(p.path().join("fd")) else {
            continue;
        };
        let holds = fds
            .flatten()
            .any(|fd| std::fs::read_link(fd.path()).is_ok_and(|t| t == Path::new(&node)));
        if holds {
            let comm = std::fs::read_to_string(p.path().join("comm")).unwrap_or_default();
            out.push(format!("{pid} {}", comm.trim()));
        }
    }
    out
}
