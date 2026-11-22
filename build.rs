//! 构建脚本：Windows 下把应用图标嵌入 exe。

/// 入口：Windows 嵌图标。
fn main() {
    println!("cargo:rerun-if-changed=assets/app.ico");
    println!("cargo:rerun-if-changed=assets/rupix.rc");
    // 目标是 Windows 时才嵌入 ICO；其他平台无事可做，避免影响跨平台构建。
    #[cfg(target_os = "windows")]
    {
        // rc 内相对路径按 rc 所在目录解析，即 `assets/app.ico`。
        // 图标编译失败直接中断构建（构建脚本按惯例用 unwrap，图标缺失不应静默发布；
        // 无资源编译器的环境按 NotAttempted 放行，见 embed-resource 文档）。
        embed_resource::compile("assets/rupix.rc", embed_resource::NONE)
            .manifest_optional()
            .unwrap();
    }
}
