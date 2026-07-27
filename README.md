# Claude Code 自动续跑监听器（Windows / macOS）

一个无需修改 Claude 客户端的轻量监听程序。它直接读取 Claude 本地会话文件，判断当前回合是否因为输出长度或明确的“尚未完成”状态而停止，然后自动回到目标窗口，发送续跑提示词。

当前主版本是基于 **Tauri（Rust + 系统自带 WebView）** 的原生安装版：启动快、内存占用低、白色亮色界面。Windows 提供 `.exe` / `.msi` 安装程序，macOS 提供 `.dmg`。仓库根目录的 Python 版本（`app.pyw` / `app_web.pyw`）为历史实现，见文末“历史实现”。

## 支持的客户端

同时兼容两种 Claude 客户端，它们都会写入相同的 `~/.claude/projects/<项目>/<会话ID>.jsonl` 会话文件，因此判断逻辑通用，区别只在发送方式：

- **Claude Code（VS Code 插件）**：通过命令面板执行 `Claude Code: Focus input`，聚焦输入框后发送。
- **Claude CLI（终端）**：识别常见终端窗口，直接把提示词输入到正在运行的 `claude` 命令行。Windows 上支持 Windows Terminal、PowerShell、cmd、conhost、Alacritty、WezTerm、Hyper、Tabby；macOS 上支持 Terminal、iTerm2、Warp、Alacritty、kitty、WezTerm、Hyper、Tabby、Ghostty。

平台差异：Windows 通过 Win32 API 枚举窗口并模拟按键；macOS 通过 `osascript`（System Events）驱动，续跑提示词经剪贴板 + Cmd-V 输入（对非 ASCII 文本更可靠）。macOS 首次使用需在“系统设置 → 隐私与安全性 → 辅助功能”中授权本应用。

在“客户端类型”下拉框里可选择 `自动`、`Claude Code（VS Code 插件）` 或 `Claude CLI（终端）`。自动模式下会优先使用已绑定/前台窗口，否则优先 VS Code，再退回终端。

## 安装与启动

推荐从 [GitHub Releases](../../releases) 下载对应平台的安装包。

**Windows（64 位）**

```text
ClaudeAutoContinue_*_x64-setup.exe   （安装程序，推荐）
ClaudeAutoContinue_*_x64_en-US.msi   （MSI 安装包，适合企业部署）
```

双击安装后，从开始菜单启动“Claude 自动续跑监听器”。Windows 10/11 通常已内置 Edge WebView2 运行时；若缺失，可从微软官网安装 “WebView2 Runtime”。首次运行若被 SmartScreen 拦截，点“更多信息 → 仍要运行”。

**macOS**

- Apple 芯片（M 系列）下载 `*_aarch64.dmg`，Intel 芯片下载 `*_x64.dmg`。
- 打开 `.dmg` 后把应用拖进“应用程序”。
- 当前为**未签名**版本，首次打开若提示“已损坏”或“无法验证开发者”，在终端运行一次：

  ```bash
  xattr -cr "/Applications/ClaudeAutoContinue.app"
  ```

  然后双击打开即可。
- 首次点“开始监听/测试发送”时，需到 系统设置 → 隐私与安全性 → **辅助功能** 勾选本应用；若仍失败，再到 隐私与安全性 → **自动化** 中允许它控制“系统事件(System Events)”。一次授权后即可正常使用。

## 推荐使用步骤

1. 在 VS Code 中使用 Claude Code，或在终端里运行 `claude` CLI。
2. 启动“Claude 自动续跑监听器”。
3. 程序默认选择最近更新的 Claude 会话。
4. 按需在“客户端类型”里选择自动 / VS Code / 终端。目标窗口保持“自动查找”即可；如果同时开了多个窗口，请手动选择，或点击“3 秒后绑定当前窗口”并切换到目标窗口。
5. 保持默认的“智能”模式，点击“开始监听”。
6. 可先点“立即分析一次”查看当前会话状态，也可以用“测试发送”验证窗口自动化。

## 三种判断模式

- **安全：只处理 max_tokens**：仅在 Claude Code 会话明确记录 `stop_reason=max_tokens` 时续跑，误触发概率最低。
- **智能：截断 + 明确未完成语句**：除 `max_tokens` 外，还识别“还需要”“下一步”“尚未完成”、未闭合代码块等信号。默认推荐。
- **严格：直到检测到完成标记**：每次 `end_turn` 都继续，直到回复出现“任务已完成”“全部完成”“测试通过”或 `[[AUTO_CONTINUE_DONE]]` 等完成标记。此模式务必设置较小的最大续跑次数。

## 工作原理

1. 监听 `%USERPROFILE%\.claude\projects\<项目>\<会话ID>.jsonl`。
2. 等待会话文件静默一段时间，避免 Claude 仍在写入时误判。
3. 对照 `~/.claude/sessions/*.json` 检查该会话的 Claude 进程是否仍存活；进程已经退出且回合没有结束记录时，按断线处理。
4. 读取最后一个用户回合之后的 assistant 记录及 `stop_reason`。
5. 需要续跑时，激活目标窗口（VS Code 或终端）。
6. 按客户端类型投递续跑提示词：
   - VS Code（Claude Code 插件）：打开命令面板，执行 `Claude Code: Focus input`，输入提示词并按 Enter。
   - 终端（Claude CLI）：直接在 `claude` 进程等待输入的终端里键入提示词并按 Enter。

这比 OCR 或固定鼠标坐标稳定，不依赖 Claude 面板的位置和窗口分辨率。

## 防止死循环

- 默认最多自动续跑 12 次。
- 同一个停止事件只处理一次。
- 两次发送之间有冷却时间。
- 默认不会对普通的完整回答无条件发送“继续”。
- 不会点击或自动批准 Claude Code 的权限确认弹窗。

## 配置和日志

保存在：

```text
Windows: %APPDATA%\ClaudeAutoContinue\config.json / monitor.log
macOS:   ~/Library/Application Support/ClaudeAutoContinue/config.json / monitor.log
```

## 项目结构与技术栈

主版本源码在 `desktop/`，Rust 逻辑完整对应原 Python 模块：

- `desktop/src-tauri/src/core.rs`：会话分析、三种判断模式、断线指纹（对应旧 `core.py`）。
- `desktop/src-tauri/src/automation.rs`：跨平台调度层，持有共享的 `WindowItem` 类型与会话存活检查；按平台委托给子模块：
  - `desktop/src-tauri/src/automation/windows.rs`：基于 `windows-sys` 的 Win32 窗口枚举/激活/按键。
  - `desktop/src-tauri/src/automation/macos.rs`：基于 `osascript`（System Events）的应用枚举/激活/按键，提示词经剪贴板 + Cmd-V 输入。
  两者暴露相同的函数集，含 VS Code 命令面板与终端 CLI 双发送路径（对应旧 `windows_automation.py`）。
- `desktop/src-tauri/src/lib.rs`：Tauri 命令层、后台监听线程、配置读写。
- `desktop/index.html` / `desktop/src/styles.css` / `desktop/src/main.ts`：手写的白色卡片式界面，不依赖任何重型组件库（打包产物约 18KB），因此启动快、不卡顿。

## 从源码构建

需要 Rust、Node.js 与 Tauri CLI。

```powershell
cd desktop
npm install
npm run tauri dev      # 开发调试
npm run tauri build    # 生成 .exe / .msi 安装包
```

产物输出到 `desktop/src-tauri/target/release/bundle/`。安装包类型在 `desktop/src-tauri/tauri.conf.json` 的 `bundle.targets` 中配置（当前为 `nsis` 与 `msi`）。

## 已知限制

- 自动发送时会短暂把目标窗口切到前台，这是为了可靠地操作输入框。
- 如果 Claude 停下来等待用户授权、选择或补充信息，程序不会绕过这些交互。
- “是否真正完成”无法仅靠停止状态做到 100% 语义判断。对必须连续完成的长任务，建议在最初的任务提示中要求 Claude 完成时输出 `[[AUTO_CONTINUE_DONE]]`，再使用严格模式。
- Claude Code 或 VS Code 若修改命令名称，需要同步调整 `automation.rs` 中的命令面板文字（`Claude Code: Focus input`）。

## 历史实现（Python 版）

根目录保留了早期的 Python 版本，作为参考与无 WebView2 环境的备用方案：

- `app.pyw`：tkinter 原生界面版。
- `app_web.pyw`：pywebview + Element Plus 版（界面较重，启动偏慢）。
- `core.py` / `windows_automation.py`：Python 版的分析与窗口自动化逻辑。
- `tests/`：Python 单元测试，可用 `python -m unittest discover -s tests -v` 运行。

这些文件不再作为主要发布形态，功能已在 `desktop/` 的 Rust 版中完整实现。
