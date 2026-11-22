# 本地依赖补丁

`scap-rs/` 来自 crates.io 的 `scap-rs 0.1.0`，上游为
https://github.com/launcher-rs/scap-rs，保留 MIT 许可证。

本地行为修改：Windows 捕获始终使用 `CursorCaptureSettings::WithoutCursor`。
rgpui 1.7.0 在屏幕推流中写死 `show_cursor: true`，且未提供应用层开关，
导致区域截图的冻结画面残留鼠标。此补丁同时覆盖区域截图、全屏截图与取色。
CLI 原本使用 GDI，不受影响。

HDR 修复：每次捕获按目标显示器设备名查询 Advanced Color 状态和 SDR 白电平；启用时使用 Rgba16F 捕获，由 `win/hdr.rs` 以半精度查找表还原为八位 sRGB，再交给 rgpui。区域浮层、全屏、取色和 Pin 共用该源像素；普通 SDR 屏保留八位路径。标准 SDR 输出会截断超出 SDR 白点的真实 HDR 高光，不提供 HDR 文件导出。查询失败记录警告并保留原八位兼容路径。

上游提供关闭光标的接口后，可改用该接口并移除此覆盖。
