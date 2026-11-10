# BlueBridgeIme · 蓝牙输入桥

用手机当电脑键盘：手机连着电脑的蓝牙，打字时电脑当前光标处直接出现文字，不用装接收器、不用连同一个 Wi-Fi，也不用切换窗口。

## 功能

- **手机打字，电脑即收**：手机端每段文字经蓝牙送到电脑，电脑自动粘贴进当前焦点，回执显示是否输入成功。
- **开机即用**：打开手机 App 直接进入输入界面，自动连接上次使用的电脑；连不上会自动重试，失败时仍停在输入界面并可重新连接。
- **按使用时间排序**：设备列表按本软件最近连接时间倒序，置顶显示上次那台电脑，并标注「刚刚 / 5 分钟前」。
- **多行与大文本**：支持中文、emoji、多行文本，单条上限 16 KiB，超长会在手机上提示。
- **电脑端开关**：蓝牙服务注册（关闭后手机搜不到本机）、开机自启（登录后无界面常驻），状态持久化。
- **服务与界面分离**：电脑端服务可完全无界面运行，界面只通过本地 socket 读写状态，换前端技术栈不影响服务。
- **五语界面**：中文、英文、俄语、日语、韩语，跟随系统语言，未匹配时回退英文。

## 技术栈

- **Android**：Kotlin、Jetpack Compose、Material 3；经典蓝牙 RFCOMM（`0000e8b6-0000-1000-8000-00805f9b34fb`），JSON 行协议，逐条回执。
- **桌面端**：Rust + tokio 实现蓝牙服务与文字注入；Tauri 2 做薄外壳，前端为原生 HTML/CSS/JS。Linux 用 BlueZ / zbus，Windows 用 Win32 蓝牙 Winsock（`AF_BTH` + `WSASetService`）。另有一个 LÖVE 11.5 轻量前端（[`love/`](love/)），布局相同、等比缩小，适合低配与低分辨率电脑。
- **输入注入**：Linux Wayland 下用 `wl-copy` 写剪贴板 + `/dev/uinput` 虚拟键盘发 Ctrl+V，粘贴后还原原剪贴板；Windows 下用 `SendInput` + `KEYEVENTF_UNICODE` 把字符事件直接送进前台焦点控件，不碰剪贴板。
- **进程间通信**：桌面端界面 ↔ 服务，Linux 走 `$XDG_RUNTIME_DIR` 下的 Unix socket、Windows 走命名管道 `\\.\pipe\bluetooth-input-bridge-control`，同为 JSON 行协议，状态订阅推送。
- **构建与 CI**：Gradle、Cargo、GitHub Actions（APK、Linux 二进制、Linux 的 .deb、Arch 包、LÖVE 三平台包、Windows 安装包，可选发布签名）。

## 已支持的平台

| 平台 | 状态 | 说明 |
| --- | --- | --- |
| Android 8.0+（API 26+） | ✅ 支持 | 手机端，需要经典蓝牙（非 BLE） |
| Linux x86_64（BlueZ + GNOME Wayland） | ✅ 支持 | 桌面端，含无界面服务与开机自启；提供 .deb、Arch 包与免安装 tarball，另有自带服务的 LÖVE 包 |
| Windows 10/11 x64 | ✅ 支持 | 桌面端，Win32 蓝牙 RFCOMM + SendInput 直输（不经过剪贴板），含无界面服务与开机自启，提供 NSIS 安装包 |
| macOS | ❌ 未支持 | 仅提供 LÖVE 界面包（`love/`），无服务后端 |

## 许可证

本项目以 **GNU General Public License v3.0 or later** 发布，详见 [LICENSE](LICENSE)。
