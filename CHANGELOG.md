# 更新日志

格式参考 Keep a Changelog：`Added / Changed / Fixed`，日期为 tag 日期。

## [Unreleased]

### Added

- 新增 Windows 命令行录屏 `rivix record`：支持按窗口标题 / HWND、进程 PID / 名称或单显示器区域捕获，提供时长、MP4 / GIF、码率、帧率、输出路径与复制参数；复用现有 WGC 与 FFmpeg 管道，拒绝覆盖旧文件。
- 手动录屏悬浮条在「暂停 / 停止」按钮组左侧新增「取消」，立即停止并丢弃本次视频，不保存、不提示保存成功。

### Fixed

- 修复命令行录屏静止画面只写入首帧、视频时长远短于 `--duration` 的问题；无新帧时按目标帧率重复最后一帧，并从首帧到达后开始计时。

后续规划见 [docs/ROADMAP.md](docs/ROADMAP.md)。

## [0.4.0] - 2026-11-27

### Added

- 新增 **长截图**：`Insert` 唤起截图，按住左键拖框时按 `L` 立即进入准备态，点击「开始滚动截屏」后再手动向下滚动；`Ctrl + Enter` 完成、`Shift + Esc` 取消，悬浮条同步显示当前尺寸。
- 长截图复用录屏的屏幕源选择、持续推流、裁剪与排除抓屏浮层，最终沿用普通截图的复制、预览和标注流程。

### Fixed

- 统一长截图与录屏准备面板：两行左右对齐，上排比例/尺寸预设，下排尺寸及操作按钮；分别保留蓝色和红色开始按钮。

- `L` 不再注册全局热键，框选阶段的功能键统一要求按住左键，关闭标注窗口也不会恢复抢键。
- 长截图改用有界连续帧队列并提高采样频率，允许更小重叠区域，减少快速连续滚动时断拼。
- 长图容量由 3200 万提高至 1.28 亿像素；容量或扩容失败时保留已拼接内容并提示分段截图。

## [0.3.0] - 2026-11-23

### Added

- 正式更名为 **Rivix · 望川**，启用全新「川」字品牌图标与托盘/设置/标注界面图标。
- 新增 **FFmpeg 驱动录屏**：支持比例/分辨率微调、NVENC 硬件加速（自动回退 `libx264`）、分段防崩溃 MP4 与 GIF 输出，以及 WASAPI 系统音频/麦克风混音。
- 新增**桌面置顶贴图（Pin）**、**冻结帧二维码识别**、**离线 RapidOCR（CUDA）**与**快门音效反馈**。
- 标注编辑器支持 **`Ctrl + T` 自由变换与旋转**、**`Ctrl + A` 全选所有元素**、**元素自由移出画布之外**，以及多行文字输入（`Enter` 换行、`Ctrl + Enter` 提交）。

### Changed

- 重构设置页、标注工具栏与跟随式迷你色卡 UI，默认静默驻留托盘、单文件绿色免安装，Windows 发布包自动附带 `ffmpeg.exe` 与许可证。
- 关于页仓库与自动检查更新地址切换为 `kelsoz/Rivix`。

## [0.2.1] - 2026-11-01

### Added

- 设置页「截图」新增两个截图行为开关：「截图后自动复制到剪贴板」（默认开，截完直接 `Ctrl+V` 可用）、
  「截图后打开编辑窗」（默认关）；开关与热键、保存目录等设置一起存于 `rupix-config.json`
- 新增 `src/toast.rs` 截图保存提示：不弹编辑窗时在屏幕右下角闪一枚胶囊（1.6 秒自动消失、
  不抢焦点、经 `WDA_EXCLUDEFROMCAPTURE` 排除出抓屏），交代「已保存（并复制）」与文件名
- 截图收尾统一到 `app::finish_capture`（区域框选与全屏两条路径同一处判断）：
  自动保存 → 按设置复制 → 打开编辑窗或闪提示；`autosave_screenshot` 负责保存后写剪贴板
- 新增「编辑最近一张」入口（设置页截图 Tab 与托盘菜单）：截图收尾时在进程内记下最近一张
  （`RupixApp::last_preview`，不写盘），关掉「截图后打开编辑窗」后可一键重开标注；
  没有截图或文件已删时入口置灰 / 只回写状态栏提示
- 设置页新增「关于」Tab：版本号（取自 `Cargo.toml`）、简介与仓库地址，
  支持一键打开仓库与复制链接
- 设置页「通用」新增开机启动开关（`src/autostart.rs`）：Windows 写 `HKCU\...\CurrentVersion\Run`、
  macOS 写 `~/Library/LaunchAgents/com.launcher-rs.rupix.plist`、Linux 写 `~/.config/autostart/rupix.desktop`；
  只写当前用户范围，无需管理员权限；状态实时读取系统登记，与任务管理器 / 系统设置的改动保持一致
- 新增 `ci` 工作流（push `main` / PR / 手动）：`cargo fmt --check` + `cargo clippy -- -D warnings`
  → 三平台 `cargo build --release` → `--version` / `--help` 冒烟 → `cargo test --release --bins`
  → glibc 基线与 `otool -L` 信息输出

### Changed

- 配置文件统一为 exe 旁单个 `rupix-config.json`（`src/config.rs`），取代原来的 8 个单值 txt
  （保存目录 / 三个热键 / 语言 / OCR 三件套 / 两个截图行为开关）；JSON 损坏时备份为
  `rupix-config.bak.json` 并回默认值。开机启动状态仍实时读系统登记，不写入配置
- 截图默认不再自动弹出编辑窗：截完直接保存并（默认）复制，右下角闪一下提示；
  需要标注时在设置 → 截图里打开「截图后打开编辑窗」，或从托盘「打开图片编辑」进来
- 发版工作流加串行并发控制（`concurrency: release`，不取消进行中的发版），
  避免 tag 发版与手动 `latest` 发版同时跑互相覆盖附件；原发版流程里的冒烟步骤整体迁到 `ci`
- `rgpui` / `rgpui-platform` 升级到 1.6（含 Linux 浮层 visual 选择、`show=false` 隐藏启动、
  帧同步握手与截图浮层 Esc 无法取消的修复）
- 文档：重写 README（核心特点、四张实机界面截图、下载 / 使用 / FAQ），
  发版与 CI 说明拆到 [docs/RELEASING.md](docs/RELEASING.md)；同步 ROADMAP、AGENTS 的模块与配置说明
- 清理未使用的颜色预设与导出常量，`cargo clippy --all-targets -- -D warnings` 全绿

### Fixed

- 跨平台默认保存目录：非 Windows 平台此前误读 `USERPROFILE` / `OneDrive`（Windows 专有环境变量），
  现在 macOS / Linux 依次尝试 `XDG_PICTURES_DIR`、`~/Pictures`、`~/Desktop`，都不可用时回退系统临时目录
- macOS / Linux 标注文字烧录缺失：字体候选此前全是 Windows 字体，现按平台选择
  （macOS：PingFang / STHeiti / Helvetica；Linux：Noto Sans CJK / 文泉驿 / DejaVu）
- Windows 剪贴板写入失败时 `HGLOBAL` 未释放：加锁或 `SetClipboardData` 失败路径现在会 `GlobalFree`
- 截取中重复触发热键会叠加浮层：区域 / 取色浮层打开期间保持 `capturing`，浮层关闭（含 Alt+F4）
  或打开失败时统一复位，按钮与状态栏不再卡在「截取中」
- 切换界面语言后托盘菜单仍是旧语言：语言变更时同步刷新托盘提示与菜单
- OCR 通用模型兜底在模式匹配守卫里被同名绑定遮蔽，导致文件名不在已知家族表里时找不到模型
  （`find_trio_cases` 覆盖）
- 去掉库逻辑里的 `unwrap` / `expect`（复制标注图、OCR 模型三件套、主窗口创建），
  失败改为状态栏提示 / 日志输出
- Windows：开机启动开关打开后立刻又显示「已关闭」——`autostart::enabled()` 读回 REG_SZ 时
  数据带结尾 NUL（`RegGetValueW` 对 REG_SZ 连终止符一起返回），`from_utf16_lossy` 把它变成
  `'\0'` 字符后 `trim_matches('"')` 连引号都剥不掉，比对恒不相等。现在解码前先把尾部 NUL
  剥干净（`decode_reg_sz`），并补了对应单测
- Linux：区域截图看不到遮罩层与选区框。Rupix 侧把两个全屏浮层（`overlay.rs` 选区层、
  `picker.rs` 取色层）的 `window_background` 从 `Transparent` 改为 `Opaque` —— 它们铺的是
  一张冻结的整屏截图，本来就不需要窗口级透明；配合 rgpui 侧 X11 的 visual 选择修复
  （仅当「请求非 Opaque + 服务器有 32 位 visual + 合成器在场」才走 ARGB）后浮层正常上屏
- Linux：启动后设置窗口直接甩在桌面上（Windows 是隐藏到托盘）、以及设置窗口出现后先黑屏
  一段时间才出内容 —— 两条根因都在 rgpui 的 X11 后端（忽略 `WindowOptions::show == false`、
  建窗未设 `background_pixel`），Rupix 侧无改动，随 rgpui 更新生效

## [0.1.0] - 2026-09-24

首个版本：看到 → 截取 → 留存。

### Added

- 区域截图（全屏冻结帧拖拽框选，尺寸徽标，右键 / Esc 取消）
- 全屏截图（截取即进预览）与窗口截图（悬停高亮，Windows）
- 屏幕取色（放大镜 + 十字线，点击复制 HEX）
- 预览与标注（缩放 / 平移 / 矩形 / 椭圆 / 箭头 / 画笔 / 荧光笔 / 橡皮擦 / 马赛克 / 文字 / 撤销重做）
- 保存（桌面时间戳新文件）与双格式复制（图片 + 文件）
- 本地图片编辑、四全局热键、托盘常驻、命令行截图（`-f` / `-r` / `-o` / `--copy`）
- 离线 OCR（PP-OCRv5 mobile，后台识别，编号框 + 文字面板 + 落为标注）
- OCR 三处来源：内嵌包（`--features bundled-ocr-models`）/ 默认目录 / 设置页手动指定三个文件
- 中英双语、设置页 Tab（截图 / 快捷键 / 保存 / OCR 模型 / 通用）

### Changed

- 发版构建走 `size` 档（体积优先）；静态 CRT（免 MSVC 运行库）

### 发布附件

- `rupix-0.1.0-windows-x64.exe`（常规版，模型外置）
- `rupix-0.1.0-windows-x64-bundled.exe`（内嵌版，开箱即用）
