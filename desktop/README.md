# 蓝牙输入桥 · GNOME PC 端

桌面端分为两部分：一个**无界面的 Rust 服务**负责蓝牙 RFCOMM、消息解析和文字注入；一个**可替换的前端外壳**（当前为 Tauri 2 + 原生 HTML/CSS/JS）只通过本地控制 socket 读写状态。界面技术栈可以随时更换，服务不受影响，也可以完全脱离界面运行（例如由 systemd 用户服务开机启动）。通信格式和 UUID 与 [协议文档](../android/PROTOCOL.md) 一致。

## 结构

```
src-tauri/
  src/bin/bridge-service.rs   # headless 服务入口（无 Tauri、无窗口）
  src/service/linux.rs        # BlueZ Profile、控制 socket、wl-copy/uinput 注入、systemd
  src/service/client.rs       # 前端侧 Unix socket 客户端
  src/lib.rs                  # Tauri 薄壳：命令 + 状态转发
  src/protocol.rs             # 共享 JSON 行协议与会话去重
web/                          # 当前前端（原生 HTML/CSS/JS）
docs/service-api.md           # 控制 socket 协议
```

## 运行

需要 Rust、GTK3/WebKit2GTK 4.1 开发库、BlueZ、`wl-copy` / `wl-paste`、可访问的 `/dev/uinput`。

```bash
cd /home/chao/android-dev/projects/bluetooth-input-bridge/desktop/src-tauri
cargo build --release

# 只跑服务（无界面，日志在 stderr）
./target/release/bluetooth-input-bridge-service

# 跑界面；若服务未运行，界面会自动拉起同目录的服务
./target/release/bluetooth-input-desktop
```

两者可以独立启动、独立停止。关闭界面不会中断已建立的蓝牙会话；服务停止后界面会显示离线状态并定期重试拉起。

## 持续集成

仓库根目录的 [build.yml](../../.github/workflows/build.yml) 的 `desktop` 作业会构建本目录：Ubuntu 24.04 + 稳定版 Rust，安装 WebKitGTK 4.1 等依赖，执行 `cargo build --locked --release --bins` 并跑 `cargo test --locked --lib`；产物 `desktop-linux` 含两个可执行文件，推送 `v*` 标签时打包成 `.tar.gz` 发 Release。

`windows` 作业已经预留好，默认只在手动触发并勾选 `build_windows` 时运行；Windows 后端尚未实现，见 [docs/windows.md](docs/windows.md)。

## 界面

- 状态卡显示服务状态、当前设备的**设备名与 MAC**（设备名经 BlueZ `Device1.Alias`/`Name` 解析，失败时回退为 MAC）。
- **服务注册**开关：即时 `RegisterProfile` / `UnregisterProfile`，控制手机能否发现本机；选择持久化在 `~/.config/bluetooth-input-bridge/service.json`。
- **开机自启**开关：写入并启用/停用 `~/.config/systemd/user/bluetooth-input-bridge.service`（`WantedBy=graphical-session.target`，`ExecStart` 指向当前服务二进制）。重新构建或移动二进制后，重新切换一次开关即可刷新路径。
- **01 软件指引与介绍**：三步说明与「允许配对 3 分钟」。
- **02 测试输入框**：一个本地纯文本输入框，用来试打字、确认键盘与输入法正常；内容只留在窗口内，不发送、不改动剪贴板。
- **界面语言**：跟随操作系统区域设置（Linux 依次读取 `LANGUAGE`/`LC_ALL`/`LC_MESSAGES`/`LANG`，其他平台由 `sys-locale` 提供），支持中文、英文、俄语、日语、韩语；未匹配到这些语言时回退英文。前端字典在 `web/main.js`，服务层返回的错误文本目前仍是中文，会原样显示。
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

服务不持久保存输入文本，仅在当前会话中保留最多 256 条消息 ID 及其回执用于去重。Windows 后端目前是接口占位，未提供 Windows 可执行程序。

## 验证

已在 OPPO Find X8 与本机 GNOME 51 Wayland 上完成实机验证：安卓连接成功、自动发送文本、PC 端临时 GTK 输入框收到相同文字、安卓收到成功回执。本轮重构后另行验证了 headless 服务独立运行、状态查询、服务注册开关、开机自启开关（systemd `is-enabled` 往返）以及测试输入框的注入（gedit 中收到 `bridge test ✓ 中文行`），并用真实窗口截图确认 900×650 下无滚动。

```bash
cd /home/chao/android-dev/projects/bluetooth-input-bridge/desktop/src-tauri
cargo check
cargo test --lib
```
