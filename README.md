# Claude Code 自动续跑监听器（Windows / macOS）

一个无需修改 Claude 客户端的轻量监听程序。它直接读取 Claude 本地会话文件，判断当前回合是否因为输出长度或明确的“尚未完成”状态而停止，然后自动回到目标窗口，发送续跑提示词。

当前主版本是基于 **Tauri（Rust + 系统自带 WebView）** 的原生安装版：启动快、内存占用低、白色亮色界面。Windows 提供 `.exe` / `.msi` 安装程序，macOS 提供 `.dmg`。仓库根目录的 Python 版本（`app.pyw` / `app_web.pyw`）为历史实现，见文末“历史实现”。

## 支持的客户端

同时兼容三种 Claude 客户端，它们都会写入相同的 `~/.claude/projects/<项目>/<会话ID>.jsonl` 会话文件，因此判断逻辑通用，区别只在发送方式：

- **Claude Code（VS Code 插件）**：通过命令面板执行 `Claude Code: Focus input`，聚焦输入框后发送。
- **Claude CLI（终端）**：识别常见终端窗口，直接把提示词输入到正在运行的 `claude` 命令行。Windows 上支持 Windows Terminal、PowerShell、cmd、conhost、Alacritty、WezTerm、Hyper、Tabby；macOS 上支持 Terminal、iTerm2、Warp、Alacritty、kitty、WezTerm、Hyper、Tabby、Ghostty。
- **Claude Desktop（独立桌面应用）**：识别 Claude 桌面客户端窗口（Windows 上进程名 `claude.exe`，macOS 上应用名 `Claude`），激活窗口后直接把提示词输入到消息框并回车。

平台差异：Windows 通过 Win32 API 枚举窗口并模拟按键；macOS 通过 `osascript`（System Events）驱动，续跑提示词经剪贴板 + Cmd-V 输入（对非 ASCII 文本更可靠）。macOS 首次使用需在“系统设置 → 隐私与安全性 → 辅助功能”中授权本应用。

在“客户端类型”下拉框里可选择 `自动`、`Claude Code（VS Code 插件）`、`Claude CLI（终端）` 或 `Claude Desktop（桌面应用）`。自动模式下会优先使用已绑定/前台窗口，否则优先 VS Code，再退回终端 / 桌面应用。

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

1. 在 VS Code 中使用 Claude Code、在终端里运行 `claude` CLI，或打开 Claude Desktop 桌面应用。
2. 启动“Claude 自动续跑监听器”。
3. 程序默认选择最近更新的 Claude 会话。
4. 按需在“客户端类型”里选择自动 / VS Code / 终端 / 桌面应用。目标窗口保持“自动查找”即可；如果同时开了多个窗口，请手动选择，或点击“3 秒后绑定当前窗口”并切换到目标窗口。
5. 保持默认的“智能”模式，点击“开始监听”。
6. 可先点“立即分析一次”查看当前会话状态，也可以用“测试发送”验证窗口自动化。
7. 多开时可为每个会话各绑一个窗口，逐个“加入监听列表”后一起监听（见“多会话同时监听”）。

## 三种判断模式

- **安全：只处理 max_tokens**：仅在会话明确记录 `stop_reason=max_tokens` 时续跑，误触发概率最低。
- **智能：截断 + 明确未完成语句**：除 `max_tokens` 外，还识别“尚未完成”“待完成”、未闭合代码块等信号。默认推荐。为降低误触发：未完成信号只在回复**结尾部分**匹配（回复中间的“接下来我来改 X”这类叙述不算数），且一旦回复出现完成标记（如“任务已完成/测试通过”），即便有零散的未完成措辞也不续跑。
- **严格：直到检测到完成标记**：每次 `end_turn` 都继续，直到回复出现“任务已完成”“全部完成”“测试通过”或 `[[AUTO_CONTINUE_DONE]]` 等完成标记。此模式务必设置较小的最大续跑次数。

不论选哪种模式，以下两类信号都会触发续跑（见下）。

## API 错误自动续跑

当 Claude 因 API 错误（如 502、524、429、连接中断、overloaded 等）而中断时，程序会自动发送一次续跑。判定规则很关键：

- 只有当 API 错误是会话记录里的**最后一个事件**、且文件已静默一段时间，才判定为“未恢复的错误”并续跑。
- 因为 Claude Code 遇到这类错误时会自行重试；如果重试成功，后面会写入新的 assistant 消息，程序据此判断错误已恢复，**不会**误触发。
- 这一行为在安全 / 智能 / 严格三种模式下都生效。

## 意外断流检测（没报错但没做完就停了）

有时上游不是以报错方式中断，而是**流被悄悄切断**——最后一条 assistant 消息既没有正常结束标记（`end_turn` / `max_tokens`），也没有 API 错误记录，任务就停在半路。程序会捕捉这种情况：

- 当会话最后停在“有用户提问、有部分回复、但没有任何结束标记、也不是在执行工具”时，判定为断流。
- 因为正常的流式输出中途也没有结束标记，为避免把“还在生成”误判为断流，该检测只在文件**静默满“断线判定”秒数**（默认 60 秒）后才触发，并且在安全 / 智能 / 严格三种模式下都生效。
- 触发后发送一次续跑提示词，让 Claude 从断点接着做。

## 让循环能正常收尾

默认续跑提示词已包含一句：**“如果确认已全部完成，请在回复最后单独输出一行 `[[AUTO_CONTINUE_DONE]]`。”** 一旦 Claude 输出该标记，完成检测即判定任务结束，不再续跑。这样即便在严格 / 断流场景下，也有一个明确的“得到肯定答案后停止”的终止条件。

## 确认完成模式（可选，任何判断模式下都生效）

在“监听设置”里打开“确认完成模式”后：**只要回合正常停止且回复没有明确表示任务完成，就会追问一次“是否已完成”，直到回复明确确认完成（出现完成标记/完成语句且结尾没有未完成措辞）才停。** 不区分智能 / 严格模式，也不依赖具体模型的措辞——最可靠的收尾方式是让续跑提示词要求“完成时输出 `[[AUTO_CONTINUE_DONE]]`”。异常情况（API 错误 / 截断 / 断流）仍按各自规则续跑。追问次数受“最大续跑次数”上限兜底，不会无限循环。

## 自定义关键字触发（可选）

在“监听设置”里打开“自定义关键字触发”，填入若干关键字（逗号或换行分隔，如 `已暂停, waiting for, 等待确认`）。开启后，只要回复中命中任一关键字且没有出现完成标记，就判定为“任务未真正完成”，触发一次续跑。用于捕捉内置规则识别不到的异常终止 / 中途停下场景。

## 多会话同时监听

多开场景下，可为每个会话各自绑定一个目标窗口，同时监听：

1. 在会话卡片选好会话与目标窗口后，点“加入监听列表”，即把该“会话 ↔ 窗口”配对加入列表。
2. 逐个添加多个配对，每个配对各自独立记录续跑次数、冷却与已处理状态，互不干扰。
3. 点“开始监听”后所有配对一起运行；每行可单独“移除”。
4. 如果一个配对都没加就直接点“开始监听”，程序会用当前单选会话建立一个隐式配对，保持原来的单会话用法。

## 工作原理

1. 监听 `%USERPROFILE%\.claude\projects\<项目>\<会话ID>.jsonl`。
2. 等待会话文件静默一段时间，避免 Claude 仍在写入时误判。
3. 静默满“断线判定”秒数后，若回合没有任何正常结束标记也没有 API 错误，即按“意外断流”处理并续跑；无需等待进程退出（也会额外对照 `~/.claude/sessions/*.json` 的进程状态作为佐证）。
4. 读取最后一个用户回合之后的 assistant 记录及 `stop_reason`。
5. 需要续跑时，激活目标窗口（VS Code / 终端 / Claude 桌面应用）。
6. 按客户端类型投递续跑提示词：
   - VS Code（Claude Code 插件）：打开命令面板，执行 `Claude Code: Focus input`，输入提示词并按 Enter。
   - 终端（Claude CLI）：直接在 `claude` 进程等待输入的终端里键入提示词并按 Enter。
   - Claude Desktop（桌面应用）：激活窗口后输入框即获得焦点，直接键入提示词并按 Enter。

这比 OCR 或固定鼠标坐标稳定，不依赖 Claude 面板的位置和窗口分辨率。

## 防止死循环

- 每个监听配对独立计数，默认最多自动续跑 12 次，达到上限后该配对自动停止。
- 同一个停止事件（含 API 错误）只处理一次。
- 两次发送之间有冷却时间，每个配对各自计时。
- 默认不会对普通的完整回答无条件发送“继续”。
- 不会点击或自动批准 Claude Code 的权限确认弹窗。
- 发送后会校验是否真正送达：程序在发送前后对比会话文件，如果 6 秒内文件没有增长/更新，则判定“未检测到会话更新”，日志会明确提示“可能没有真正输入到 Claude”，并释放该回合以便下次重试——不再把“执行了按键动作”当成“发送成功”。

## 轮询与心跳日志

- 监听线程每 **1 秒**轮询一次每个配对的会话文件；检测到回合停止后，会等满“静默等待”（默认 7 秒）才判断是否续跑，避免 Claude 仍在写入时误判。
- “监听设置”里可打开“打印心跳日志”开关。开启后，监听时会为每个配对约每 3 秒打印一条日志，内容包含：会话当前状态（生成中 / 已停止 / 执行工具 / API 错误）、已静默秒数、以及目标窗口是否找到。用于排查“为什么触发了续跑 / 为什么没触发”。

## 配置和日志

保存在：

```text
Windows: %APPDATA%\ClaudeAutoContinue\config.json / monitor.log
macOS:   ~/Library/Application Support/ClaudeAutoContinue/config.json / monitor.log
```

## 自动更新

应用内置了 Tauri 官方更新器：控制区的“检查更新”按钮会查询 GitHub Releases 的最新版本，发现新版时用应用内弹窗提示，确认后自动下载、安装并重启。启动约 3 秒后也会静默检查一次（无新版时不打扰）。

更新包由一对 ed25519 密钥签名，防止被篡改：

- 公钥写在 `desktop/src-tauri/tauri.conf.json` 的 `plugins.updater.pubkey`。
- 私钥存为仓库的 **Repository secret** `TAURI_SIGNING_PRIVATE_KEY`（不进仓库；`updater_key` 已在 `.gitignore` 中）。
- CI 打包时用私钥签名，并生成随 Release 一起发布的 `latest.json` 更新清单；`tauri.conf.json` 的 `updater.endpoints` 指向该清单。

注意：这套更新签名与操作系统的代码签名（SmartScreen / Gatekeeper）是两回事，只用于校验更新包完整性，不消除首次安装时的系统拦截提示。验证自动更新需要至少两个版本：先装一个较低版本，再发布一个更高版本，低版本才能检测并升级到它。

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
- Claude Code 或 VS Code 若修改命令名称，需要同步调整 `automation/windows.rs`、`automation/macos.rs` 中的命令面板文字（`Claude Code: Focus input`）。
- macOS 版通过 `osascript` 驱动，需授予辅助功能/自动化权限；且未签名，首次打开需手动放行（见上文安装说明）。
- API 错误续跑依赖会话文件的静默判定：若 Claude Code 正在快速重试，程序会等其静默后才判断是否真正未恢复，因此存在数秒延迟属正常。

## 历史实现（Python 版）

根目录保留了早期的 Python 版本，作为参考与无 WebView2 环境的备用方案：

- `app.pyw`：tkinter 原生界面版。
- `app_web.pyw`：pywebview + Element Plus 版（界面较重，启动偏慢）。
- `core.py` / `windows_automation.py`：Python 版的分析与窗口自动化逻辑。
- `tests/`：Python 单元测试，可用 `python -m unittest discover -s tests -v` 运行。

这些文件不再作为主要发布形态，功能已在 `desktop/` 的 Rust 版中完整实现。
