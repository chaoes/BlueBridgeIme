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

- **Android**：Kotlin、Jetpack Compose、Material 3；经典蓝牙 RFCOMM（`e8b6a86a-73ce-4cbb-9ea8-9f9c2fae6d51`），JSON 行协议，逐条回执。
- **桌面端**：Rust（BlueZ / zbus、tokio）实现蓝牙服务与文字注入；Tauri 2 做薄外壳，前端为原生 HTML/CSS/JS。
- **输入注入**：Linux Wayland 下用 `wl-copy` 写剪贴板 + `/dev/uinput` 虚拟键盘发 Ctrl+V，粘贴后还原原剪贴板。
- **进程间通信**：桌面端界面 ↔ 服务走 `$XDG_RUNTIME_DIR` 下的 Unix socket，JSON 行协议，状态订阅推送。
- **构建与 CI**：Gradle、Cargo、GitHub Actions（APK、Linux 二进制、.deb、Arch 包，可选发布签名）。

## 已支持的平台

| 平台 | 状态 | 说明 |
| --- | --- | --- |
| Android 8.0+（API 26+） | ✅ 支持 | 手机端，需要经典蓝牙（非 BLE） |
| Linux x86_64（BlueZ + GNOME Wayland） | ✅ 支持 | 桌面端，含无界面服务与开机自启；提供 .deb、Arch 包与免安装 tarball |
| Windows | 🚧 预留 | 界面与 CI 构建位已留好，蓝牙与注入后端尚未实现 |
| macOS | ❌ 未支持 | — |

## 许可证

本项目以 **GNU General Public License v3.0 or later** 发布，详见 [LICENSE](LICENSE)。
