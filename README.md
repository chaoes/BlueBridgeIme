# BlueBridgeIme · 蓝牙输入桥

用手机当电脑键盘：手机连着电脑的蓝牙，打字时电脑当前光标处直接出现文字，不用装接收器、不用连同一个 Wi-Fi，也不用切换窗口。

## 功能

- **手机打字，电脑即收**：手机端每段文字经蓝牙送到电脑，直接出现在当前光标所在的输入框里，回执显示是否输入成功。
- **不经过剪贴板**：Linux 用虚拟键盘发 `Ctrl+V`（粘贴后还原原剪贴板）；Windows 用 `SendInput` 把字符事件直接送进前台焦点控件，不占剪贴板，也不会把剪贴板内容误带出来。
- **按权限如实报错**：本程序和目标窗口权限不一致（例如本程序以管理员身份运行）时不会假装成功，而是回执「本程序以管理员身份运行，输入到不了普通权限的窗口」。程序从不自动提权。
- **开机即用**：打开手机 App 直接进入输入界面，自动连接上次使用的电脑；连不上会自动重试，失败时仍停在输入界面并可重新连接。
- **按使用时间排序**：设备列表按本软件最近连接时间倒序，置顶显示上次那台电脑，并标注「刚刚 / 5 分钟前」。
- **多行与大文本**：支持中文、emoji、多行文本，单条上限 16 KiB，超长会在手机上提示。
- **电脑端开关**：蓝牙服务注册（关闭后手机搜不到本机）、开机自启（登录后无界面常驻），状态持久化。
- **服务与界面分离**：电脑端服务可完全无界面运行，界面只通过本地 socket 读写状态，换前端技术栈不影响服务。
- **五语界面**：中文、英文、俄语、日语、韩语，跟随系统语言，未匹配时回退英文。

## 技术栈

- **Android**：Kotlin、Jetpack Compose、Material 3；经典蓝牙 RFCOMM（`0000e8b6-0000-1000-8000-00805f9b34fb`，Windows 侧的 SDP 也按 16 位短 UUID `0xe8b6` 注册，两者等价），JSON 行协议，逐条回执。
- **桌面端**：Rust + tokio 实现蓝牙服务与文字注入；Tauri 2 做薄外壳，前端为原生 HTML/CSS/JS。Linux 用 BlueZ / zbus，Windows 用 Win32 蓝牙 Winsock（`AF_BTH` + `WSASetService`）。另有一个 LÖVE 11.5 轻量前端（[`love/`](love/)），布局相同、等比缩小，适合低配与低分辨率电脑；Linux 与 Windows 都能用，Windows 上由一个小型 Rust 转接程序把命名管道接成 Unix socket。
- **输入注入**：Linux Wayland 下用 `wl-copy` 写剪贴板 + `/dev/uinput` 虚拟键盘发 Ctrl+V，粘贴后还原原剪贴板；Windows 下用 `SendInput` + `KEYEVENTF_UNICODE` 把字符事件直接送进前台焦点控件，不碰剪贴板。
- **进程间通信**：桌面端界面 ↔ 服务，Linux 走 `$XDG_RUNTIME_DIR` 下的 Unix socket、Windows 走命名管道 `\\.\pipe\bluetooth-input-bridge-control`，同为 JSON 行协议，状态订阅推送。LÖVE 前端（Lua）只会连 Unix socket，因此在 Windows 上附带 `bluetooth-input-bridge-socket.exe` 做管道 ↔ socket 的双向转发。
- **构建与 CI**：Gradle、Cargo、GitHub Actions（APK、Linux 二进制、Linux 的 .deb、Arch 包、LÖVE 三平台包、Windows 安装包与免安装 exe，可选发布签名）。Windows 侧的全部产物都在 Ubuntu runner 上交叉编译：安装包用 `x86_64-pc-windows-msvc` + `rust-lld`，免安装 exe 用 `x86_64-pc-windows-gnu` + mingw-w64。
- **版本**：当前为 `0.2.1`，Windows 后端与双平台 LÖVE 前端正式合并进 `main` 的版本（`0.2.0` 是首个同时支持 Linux 与 Windows 的 LÖVE 前端版本）。

## 快速开始（Windows）

1. 下载发布页里的 `bluetooth-input-bridge-love-0.2.1-windows-x86_64.zip`，解压到任意目录（不需要管理员权限）。
2. 双击 `BlueBridgeIme.exe`。界面连不上服务时会自己在后台拉起 `bluetooth-input-bridge-service.exe` 与 `bluetooth-input-bridge-socket.exe`，退出时一起关掉。
3. 手机与电脑先在「Windows 设置 → 蓝牙和设备」里配对一次；打开手机 App，选中这台电脑即可开始打字。
4. 想让它常驻，可在界面里打开「服务注册」「开机自启」两个开关。

> 发布页里 Windows 一共有三种形态，任选其一：
> - `bluetooth-input-bridge-love-0.2.1-windows-x86_64.zip`：LÖVE 轻量前端，自带服务与 socket 转接，免安装，**推荐**。
> - `bluetooth-input-bridge-0.2.1-windows-x64-setup.exe`：NSIS 安装包，装到 `Program Files` 并写注册表，可卸载。
> - `bluetooth-input-bridge-0.2.1-windows-x64-portable.exe`：界面 + 服务二合一的免安装 exe，配合任意目录里的 `bluetooth-input-bridge-socket.exe` 也能用；不带 LÖVE 运行时。
>
> 三者都在 CI 里由 Ubuntu runner 交叉打包，未做代码签名，Windows 第一次运行可能提示「已保护你的电脑」，选「更多信息 → 仍要运行」即可。

## 已支持的平台

| 平台 | 状态 | 说明 |
| --- | --- | --- |
| Android 8.0+（API 26+） | ✅ 支持 | 手机端，需要经典蓝牙（非 BLE） |
| Linux x86_64（BlueZ + GNOME Wayland） | ✅ 支持 | 桌面端，含无界面服务与开机自启；提供 .deb、Arch 包与免安装 tarball，另有自带服务的 LÖVE 包 |
| Windows 10/11 x64 | ✅ 支持 | 桌面端，Win32 蓝牙 RFCOMM + SendInput 直输（不经过剪贴板），含无界面服务与开机自启；发布页提供 LÖVE 包（推荐，免安装）、NSIS 安装包与免安装 exe，均由 Linux 交叉打包 |
| macOS | ❌ 未支持 | 仅提供 LÖVE 界面包（`love/`），无服务后端 |

## 目录

- [`android/`](android/)：手机端 App。
- [`desktop/`](desktop/)：Tauri 桌面端、无界面服务、Windows 的 socket 转接器；实现细节见 [`desktop/docs/windows.md`](desktop/docs/windows.md)。
- [`love/`](love/)：LÖVE 轻量前端（Linux / Windows / macOS 界面）。
- [`android/PROTOCOL.md`](android/PROTOCOL.md)：蓝牙链路协议说明。
- [`desktop/docs/service-api.md`](desktop/docs/service-api.md)：界面 ↔ 服务的本地 IPC 命令与状态字段。

## 许可证

本项目以 **GNU General Public License v3.0 or later** 发布，详见 [LICENSE](LICENSE)。
