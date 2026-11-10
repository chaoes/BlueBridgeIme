# 蓝牙输入桥 · 桌面 PC 端

桌面端分为两部分：一个**无界面的 Rust 服务**负责蓝牙 RFCOMM、消息解析和文字注入；一个**可替换的前端外壳**（当前为 Tauri 2 + 原生 HTML/CSS/JS）只通过本地控制通道读写状态。界面技术栈可以随时更换，服务不受影响，也可以完全脱离界面运行（例如开机自启）。Linux 与 Windows 两套后端共用同一个协议与前端。通信格式和 UUID 与 [协议文档](../android/PROTOCOL.md) 一致。

## 结构

```
src-tauri/
  src/bin/bridge-service.rs   # headless 服务入口（无 Tauri、无窗口）
  src/service/linux.rs        # Linux：BlueZ Profile、控制 socket、wl-copy/uinput 注入、systemd
  src/service/client.rs       # Linux：前端侧 Unix socket 客户端
  src/service/win/            # Windows：Win32 蓝牙 RFCOMM、SendInput 注入、命名管道控制通道、注册表自启
  src/lib.rs                  # Tauri 薄壳：命令 + 状态转发
  src/protocol.rs             # 共享 JSON 行协议与会话去重
web/                          # 当前前端（原生 HTML/CSS/JS）
docs/service-api.md           # 控制协议
docs/windows.md               # Windows 后端的实现说明与实测记录
```

## 运行

两者可以独立启动、独立停止。关闭界面不会中断已建立的蓝牙会话；服务停止后界面会显示离线状态并定期重试拉起。

### Linux

需要 Rust、GTK3/WebKit2GTK 4.1 开发库、BlueZ、`wl-copy` / `wl-paste`、可访问的 `/dev/uinput`。

```bash
cd desktop/src-tauri
cargo build --release

# 只跑服务（无界面，日志在 stderr）
./target/release/bluetooth-input-bridge-service

# 跑界面；若服务未运行，界面会自动拉起同目录的服务
./target/release/bluetooth-input-desktop
```

### Windows

需要 Rust（MSVC 工具链，Visual Studio Build Tools 的「使用 C++ 的桌面开发」）、Windows 10/11 与蓝牙适配器。

```powershell
cd desktop\src-tauri
cargo build --release

# 只跑服务（无界面，日志在输出和 %APPDATA%\bluebridge-ime\service.log）
.\target\release\bluetooth-input-bridge-service.exe

# 跑界面；若服务未运行，界面会自动拉起同目录的服务
.\target\release\bluetooth-input-desktop.exe
```

Windows 下蓝牙走 Win32 蓝牙 Winsock（`AF_BTH` + `WSASetService`），文字注入走 `SendInput` + `KEYEVENTF_UNICODE`，实测能直接打进前台输入框、不碰剪贴板。控制通道是命名管道 `\\.\pipe\bluetooth-input-bridge-control`，开机自启写注册表 `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`。缘由、踩过的坑与实测记录见 [docs/windows.md](docs/windows.md)。

### Windows 安装包

不想自己编译的话，用 Release 页或 CI 产物里的安装包 `bluetooth-input-bridge-<版本>-windows-x64-setup.exe`：

1. 双击运行。安装向导依次是：欢迎 → 许可 → **安装模式**（仅为我安装，不需要管理员权限；或为所有用户安装，会弹 UAC）→ **目录选择页**（可以点「浏览」把程序装到你想要的位置）→ 开始菜单文件夹 → 安装 → 完成页（可勾选「创建桌面快捷方式」「安装后启动」）。目录页不会再因为走了 `/P`（被动）或 `/UPDATE` 分支而跳过；无人值守安装用 `setup.exe /S /D=D:\你的路径`。
2. 向导默认语言跟随系统，也可以在启动时选择中文 / English。
3. 界面是 Tauri 窗口，**不带控制台**（`src/main.rs` 的 `windows_subsystem = "windows"`）；旁边的服务 exe 是控制台程序，方便直接看日志。
3. 卸载走「应用和功能」或开始菜单里的 Uninstall；安装目录会被整体删除，日志与配置在 `%APPDATA%\bluebridge-ime`，会保留下来（重装可沿用，想彻底清掉手动删这个目录即可）。
4. 安装包本身没有代码签名，Windows 可能提示「未知发布者」，选择「更多信息 → 仍要运行」即可。

安装后开始菜单里是 `Bluetooth Input Bridge`：主程序是界面（`bluetooth-input-desktop.exe`），旁边的 `bluetooth-input-bridge-service.exe` 是界面自动拉起、也可以单独运行的无界面服务。安装包默认**不**注册开机自启——需要的话在界面里打开「开机自启」开关，它会按实际安装位置写入。

## 持续集成

仓库根目录的 [build.yml](../../.github/workflows/build.yml) 的 `desktop` 作业会构建本目录：Ubuntu 24.04 + 稳定版 Rust，安装 WebKitGTK 4.1 等依赖，执行 `cargo build --locked --release --bins` 并跑 `cargo test --locked --lib`；产物 `desktop-linux` 含两个可执行文件，推送 `v*` 标签时打包成 `.tar.gz` 发 Release。

`windows` 作业在 `windows-latest` 上执行 `cargo build --locked --release --bins` 与 `cargo test --locked --lib`，然后调用 `cargo tauri bundle --bundles nsis --ci` 生成**可自定义安装位置**的安装包（NSIS）：

- 用 `bundle` 而不是 `tauri build`：界面二进制和测试已经由前面的 `cargo build` 产出，`tauri build` 会再完整编译一遍，白费缓存和几分钟时间。
- 安装包配置在 `src-tauri/tauri.conf.json`（`bundle.targets = ["nsis"]`、`bundle.windows.nsis.installMode = "both"`、简体中文/英文双语、`customLanguageFiles` 指向 `gen/nsis/{SimpChinese,English}.nsh`），安装/卸载钩子脚本在 `src-tauri/gen/nsis/installer-hooks.nsh`；`gen/` 本来被 `.gitignore` 排除（里面是 tauri 生成的 schema），这里用嵌套的 `src-tauri/gen/.gitignore` 只把它重新包含进来，这几个文件是用 `git add -f` 首次加入跟踪的。
- 「安装位置」这一页与启动时的语言选择框由 `build.rs` 定制：它在本地 cargo registry 里找到 `tauri-bundler` 自带的 `installer.nsi`，按 `gen/nsis/*.patch.nsi`（`=== ANCHOR ===` / `=== REPLACE ===` 两段）插入 `MUI_DIRECTORYPAGE_TEXT_*`、`MUI_LANGDLL_ALWAYSSHOW`，结果写到生成的 `src-tauri/generated-nsis/installer.nsi`（已 gitignore）。tauri 模板升级后锚点若变化，`build.rs` 会打印警告并跳过该定制，不会让构建失败——届时对照 patch 文件头注释更新锚点即可。
- CI 里用 `cargo install tauri-cli` 拿 `tauri` 命令；界面二进制由普通 `cargo build` 产出，前端静态资源通过 `build.rs` 复制到 `generated-dist/` 并设 `TAURI_ENV_FRONTEND_DIST`，所以不需要 `beforeBuildCommand`，也不需要额外的前端工具链。
- 产物两个：`desktop-windows` 含两个裸 `.exe`，`desktop-windows-installer` 含 `bluetooth-input-bridge-<ref>-windows-x64-setup.exe`；打 `v*` 标签时安装包也会进 Release，并和 APK/Linux 包一起写进 `SHA256SUMS.txt`。

Windows 后端的实现说明见 [docs/windows.md](docs/windows.md)。

## 界面

- 状态卡显示服务状态、当前设备的**设备名与 MAC**（Linux 经 BlueZ `Device1.Alias`/`Name` 解析，Windows 经 `BluetoothGetDeviceInfo`；失败时回退为 MAC）。
- **服务注册**开关：控制手机能否发现本机（Linux 即时 `RegisterProfile` / `UnregisterProfile`；Windows 是 `WSASetService` 登记/删除 SDP 记录）；选择持久化在 `service.json`（Linux `~/.config/bluetooth-input-bridge/`，Windows `%APPDATA%\bluebridge-ime\`）。
- **开机自启**开关：Linux 写入并启用/停用 `~/.config/systemd/user/bluetooth-input-bridge.service`（`WantedBy=graphical-session.target`，`ExecStart` 指向当前服务二进制）；Windows 写 `HKCU\...\CurrentVersion\Run` 的 `BluetoothInputBridge` 值。重新构建或移动二进制后，重新切换一次开关即可刷新路径。
- **01 软件指引与介绍**：三步说明与「允许配对 3 分钟」。
- **02 测试输入框**：一个本地纯文本输入框，用来试打字、确认键盘与输入法正常；内容只留在窗口内，不发送、不改动剪贴板。
- **界面语言**：跟随操作系统区域设置（Linux 依次读取 `LANGUAGE`/`LC_ALL`/`LC_MESSAGES`/`LANG`，其他平台由 `sys-locale` 提供），支持中文、英文、俄语、日语、韩语；未匹配到这些语言时回退英文。前端字典在 `web/main.js`，服务层返回的错误文本目前仍是中文，会原样显示。
- **平台相关文案由后端给出**：标题栏徽标（Windows 显示 `Windows 10/11`，Linux 显示 `GNOME · Wayland`）、「服务注册」下面的说明、「配对失败去哪里开」都来自 `src/lib.rs` 的 `get_platform` 命令，前端只按返回的 i18n 键查表——各平台 webview 都是同一个 Chromium 内核，前端自己猜不出系统。
- 窗口默认 900×650、最小 720×560，所有内容在一屏内显示，无需滚动。

## 配对

首次配对前点击界面中的「允许配对 3 分钟」（服务会在 3 分钟后关闭可发现/可配对）。若系统拒绝，也可在 PC 蓝牙设置中开启，或临时运行：

```bash
bluetoothctl discoverable on
bluetoothctl pairable on
```

再从安卓端搜索本机并连接。配对完成后可以关闭可发现状态。运行期间保持服务开启，切换到目标应用并将光标放进输入框。收到手机文本后服务会向当前焦点发送粘贴快捷键。

## 输入范围与限制

Wayland 下服务不能跨应用确认目标控件实际接收了文本。`ok:true` 表示 `wl-copy` 和虚拟键盘事件发出成功，不代表目标应用一定接受粘贴。某些终端需要 Ctrl+Shift+V，密码框可能禁止粘贴；这些控件暂不适用。`/dev/uinput` 权限取决于系统配置，若无法访问，界面会显示错误。当前实现会短暂使用系统剪贴板；非纯文本剪贴板内容无法还原。

服务不持久保存输入文本，仅在当前会话中保留最多 256 条消息 ID 及其回执用于去重。

Windows 后端不经过剪贴板：它把字符事件直接送进前台窗口的焦点控件，因此密码框、终端（无需 Ctrl+Shift+V）都能正常输入，也不会改写用户的剪贴板。限制是：更高权限（管理员）的目标窗口会被系统 UIPI 拦截，此时回执会提示「目标窗口权限较高，请以普通权限运行本程序（不要提权）或切换到普通窗口」；锁屏/安全桌面、最小化窗口、焦点落在不可输入控件上等情况都会返回可读的中文错误。单条文本上限 256 个 UTF-16 码元（协议层 16 KiB 之外的另一层保护，保证在安卓 8 秒回执超时内完成）。细节见 [docs/windows.md](docs/windows.md)。

## 验证

已在 OPPO Find X8 与本机 GNOME 51 Wayland 上完成实机验证：安卓连接成功、自动发送文本、PC 端临时 GTK 输入框收到相同文字、安卓收到成功回执。本轮重构后另行验证了 headless 服务独立运行、状态查询、服务注册开关、开机自启开关（systemd `is-enabled` 往返）以及注入通道（直接调服务 API，gedit 中收到 `bridge test ✓ 中文行`），并用真实窗口截图确认 900×650 下无滚动。

```bash
cd desktop/src-tauri
cargo check
cargo test --lib
```

Windows 后端在本机（Windows 11 Pro 26200、Realtek 蓝牙适配器、MSVC 工具链）上验证：

- `cargo check --locked --all-targets`、`cargo clippy --locked --all-targets`、`cargo fmt -- --check` 均无告警，`cargo test --locked --lib` **99 项通过**（约 0.4 s，含分帧/去重/回执顺序、注入文本规划、焦点判定等纯逻辑用例）。
- 服务启动后日志显示 `SDP 已登记，RFCOMM 通道 4` 与「已发布 Bluetooth Input Bridge（0000e8b6-…）」；用 `WSALookupServiceBeginW` 按 UUID 查询本机 SDP 数据库能查到该记录，服务停止后为 0 条，重复登记也不会残留多条。
- 命名管道用真实客户端（Win32 `CreateFileW`+`WriteFile`/`ReadFile`）验证：`status`、`setRegistration(false/true)`、`setAutostart(true/false)`、`bogus`、坏 JSON、空测试文字都按协议返回；`setRegistration(false)` 207 ms 返回（这个命令以前会永久挂起）；注册表 `Run` 值写入与删除往返一致。
- 服务二进制自带控制通道自测：`bluetooth-input-bridge-service.exe --selftest-ctl`（真实命名管道 + 真实蓝牙，依次 status / setRegistration(true) / status / setRegistration(false)，每步打印耗时，任一分支都会收尾并退出）。
- 界面二进制 PE 子系统为 `WINDOWS`（无控制台窗口），平台徽标来自 `get_platform` 命令，Windows 上显示 `Windows 10/11`。

连接真实手机并端到端打字仍待在有蓝牙手机的机器上实测（本机没有配对手机）。`SendInput` 的真实注入往返已由单元测试里的进程内 EDIT 窗口覆盖；跨进程前台场景需要解锁桌面后手动把记事本切到前台验证。

## LÖVE 轻量前端

同一套无界面服务还有一个 LÖVE 11.5 前端，位于 [`../love/`](../love/)：设计稿 900×650、按窗口等比缩放、上限 30 FPS 且只在内容变化时重绘，中文字体只读一次。适合低配置、低分辨率电脑。运行方式、打包脚本与限制见 [`../love/README.md`](../love/README.md)。

Linux 与 Windows 都支持。LÖVE 是 Lua 程序，只会连 Unix 套接字，所以在 Windows 上附带一个小型转接程序 `bluetooth-input-bridge-socket.exe`（本仓库 `src/bin/socket-bridge.rs`）：它在 `%TEMP%\bluetooth-input-bridge-<界面进程号>\control.sock` 监听，把每次连接原样转发到命名管道 `\\.\pipe\bluetooth-input-bridge-control`，不解析内容。LÖVE 侧连不上时会依次拉起转接程序、再拉起服务；界面退出时转接程序随之退出并删掉套接字文件。Tauri 版不需要这个转接程序，因为它直接说命名管道。

Windows 上跑通这条链路踩过两个坑，都在 `love/src/service.lua` 里注释说明，改动前建议先看：

- `WSASocketA` 的协议参数必须传 `0`。传 `IPPROTO_TCP(6)` 会被拒（`WSAEAFNOSUPPORT 10043`），LuaJIT 侧则必须显式声明 `WSASocketA`——`ws2_32` 没有 `socket()`，写成 `ws.socket(...)` 会落到未声明符号的兜底上，返回 0 且不报错，于是 `connect()` 看着“成功”，界面永远停在「正在启动服务」。
- `sockaddr_un` 在 Windows 与 POSIX 两段 `cdef` 里只能声明一次，否则 `ffi.cdef` 整体失败、`native` 为假，界面同样连不上。
