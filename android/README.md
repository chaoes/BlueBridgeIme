# 蓝牙输入桥（Android）

安卓原生 Compose 应用：搜索经典蓝牙设备、连接 PC RFCOMM 服务、用系统输入法输入文字或语音识别结果。支持 0–3 秒静默自动发送及手动发送；电脑确认收到后清空已发送文字。Windows 客户端接口见 [PROTOCOL.md](PROTOCOL.md)。

GNOME Wayland 的 Rust + Tauri PC 端位于 [desktop](../desktop/README.md)。

## 开发计划与当前实现

1. 建立独立安卓工程，复用本机 Android SDK、Gradle 与 JDK：已完成。
2. 设备发现与连接：已配对列表、附近设备扫描、权限请求、蓝牙开启、RFCOMM 连接/断开：已完成。
3. 输入体验：系统输入法文本框、语音输入法兼容、手动发送、0–3 秒静默自动发送、组字保护、成功回执后清空、键盘弹出时滚动布局：已完成。
4. 传输：UTF-8 JSON 行、Windows 回执、8 秒超时与错误提示：已完成。
5. 连接历史与自动连接：设备列表按本软件最近连接时间倒序排列（最近连接、连接时间、已配对、名称依次排序，并在右侧显示“刚刚/5 分钟前”等相对时间）；打开应用默认进入输入界面，并在后台自动连接上次成功连接的设备，失败自动重试 3 次（每次 5 秒超时）；重试仍失败时留在输入界面，显示失败原因并提供“重新连接”与“设备”入口；用户主动断开后本次运行不再自动连接：已完成。
6. 端到端联调：已用 OPPO Find X8 与 GNOME Wayland 客户端验证文字自动发送、成功回执后清空输入框；Windows 客户端尚未实现。
7. 多语言：界面文案全部资源化，跟随 Android 系统语言，支持中文、英文、俄语、日语、韩语（`values` 为英文默认，另有 `values-zh`、`values-ru`、`values-ja`、`values-ko`）；系统语言不在列表内时显示英文：已完成。

## 构建

```bash
source /home/chao/android-dev/env.sh
cd /home/chao/android-dev/projects/bluetooth-input-bridge/android
./gradlew assembleDebug
```

APK：`app/build/outputs/apk/debug/app-debug.apk`。最低 Android 8.0（API 26），目标 API 36。仅在应用打开期间维持连接；离开并销毁界面后会断开。语音识别取决于用户安装的输入法和其麦克风权限，本应用不直接录音。

## 持续集成

仓库根目录的 [build.yml](../../.github/workflows/build.yml) 会构建本目录：Ubuntu + JDK 21（Temurin）+ Android SDK 36。配置了签名 Secrets 时执行 `assembleRelease` 并校验签名，否则执行 `assembleDebug`；APK 以 `android-apk` 名字上传（保留 30 天），推送 `v*` 标签时附到 Release。签名方法见下一节。

## 发布签名

`release` 变体的签名材料全部从环境变量读取，keystore 不进仓库：

| 变量 | 含义 |
| --- | --- |
| `ANDROID_KEYSTORE_FILE` | keystore 文件路径（CI 中解码到 `$RUNNER_TEMP/release.keystore`） |
| `ANDROID_KEYSTORE_PASSWORD` | keystore 密码 |
| `ANDROID_KEY_ALIAS` | 密钥别名 |
| `ANDROID_KEY_PASSWORD` | 密钥密码 |

生成一个 keystore（只做一次，妥善备份；丢失后无法给已安装用户升级）：

```bash
keytool -genkeypair -v -keystore release.keystore -alias bluetooth-bridge \
  -keyalg RSA -keysize 2048 -validity 10000
base64 -w0 release.keystore          # macOS: base64 -i release.keystore | tr -d '\n'
```

本地签名构建：

```bash
export ANDROID_KEYSTORE_FILE=$PWD/release.keystore
export ANDROID_KEYSTORE_PASSWORD='...'
export ANDROID_KEY_ALIAS=bluetooth-bridge
export ANDROID_KEY_PASSWORD='...'
./gradlew assembleRelease           # app/build/outputs/apk/release/app-release.apk
```

GitHub 仓库 → Settings → Secrets and variables → Actions，添加四个 secret：

- `ANDROID_KEYSTORE_BASE64`：上面 `base64` 输出的单行文本
- `ANDROID_KEYSTORE_PASSWORD`
- `ANDROID_KEY_ALIAS`
- `ANDROID_KEY_PASSWORD`

工作流检测到 `ANDROID_KEYSTORE_BASE64` 后：解码到临时文件 → `assembleRelease` → 用 `apksigner verify --print-certs` 校验并打印证书 → 上传 `bluetooth-input-bridge-<ref>-release.apk`。**没有**该 secret 时（fork PR、本地默认）仍构建 debug APK，文件名带 `-debug`。未配置变量时 `release` 变体自动回退 debug 签名，保证 `assembleRelease` 始终能产出可安装包。

