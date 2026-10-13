//! QyBit Linux ALSA 严格输出宿主（M2 T3）。
//!
//! - [`pcm`]：与 alsa-lib 对应的最小 PCM 操作集合（trait），真实实现与假实现共用；
//! - [`api`]：PCM 到 `qy_player` 短写渲染循环的适配（xrun/挂起/设备消失的错误映射）；
//! - [`backend`]：只接受 `hw:` 设备，按候选格式设置参数、读回并核对，不一致即换下一个或报错；
//! - [`attempt`]：协商记录（请求、alsa-lib 返回、读回值、核对结果）；
//! - [`matrix`]：设备格式矩阵与 JSON/CSV 报告；
//! - [`dump`]：从 `snd_pcm_dump_hw_setup` 取精确采样率与有效位；
//! - `linux`（仅 Linux）：alsa crate 真实绑定、设备枚举、占用进程查找、实时调度；
//! - `testing`（`test-support`）：errno 层的假 PCM。
//!
//! 软件路径检查与虚拟回环都不证明真实 DAC 输入逐位一致。

pub mod api;
pub mod attempt;
pub mod backend;
pub mod chmap;
pub mod dump;
pub mod loopback;
pub mod matrix;
pub mod pcm;

#[cfg(target_os = "linux")]
pub mod linux;

#[cfg(any(test, feature = "test-support"))]
pub mod testing;

pub use backend::AlsaBackend;

/// 设备名检查：只接受 `hw:`，否则返回原因。
pub fn pcm_name_check(device: &str) -> Result<(), String> {
    qy_pcm_format::alsa::check_device_name(device)
        .map(|_| ())
        .map_err(|e| format!("设备名 {device:?} 被拒绝：{e}"))
}

#[cfg(test)]
mod tests;

/// 在真实设备上运行格式矩阵与顺序换曲，生成报告（不写文件）。临时 WAV 写入 `work_dir`。
#[cfg(target_os = "linux")]
pub fn device_matrix(
    device: &str,
    work_dir: &std::path::Path,
) -> Result<matrix::MatrixReport, String> {
    use linux::RealOpener;
    use linux::priority::RtPriority;
    use matrix::{MatrixReport, backend_version, matrix_items, run_matrix, run_sequential};

    qy_pcm_format::alsa::check_device_name(device).map_err(|e| format!("设备名被拒绝：{e}"))?;
    let mut res = qy_player::HostResources::new().map_err(|e| format!("资源分配失败: {e:?}"))?;
    std::fs::create_dir_all(work_dir).map_err(|e| e.to_string())?;
    let entries = run_matrix(&matrix_items(), &mut res, work_dir, |item| {
        let mut b = AlsaBackend::new(device, RealOpener, RtPriority::default());
        b.forced = Some(item.format);
        b
    })
    .map_err(|e| e.to_string())?;
    let mut backend = AlsaBackend::new(device, RealOpener, RtPriority::default());
    let sequential =
        run_sequential(&entries, &mut res, work_dir, &mut backend).map_err(|e| e.to_string())?;
    Ok(MatrixReport {
        device: device.to_string(),
        backend: backend_version(),
        entries,
        sequential,
    })
}
