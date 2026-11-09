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

工作副本里已经生成了一套本地签名材料，**不进 git**：

```
android/keystore/release.keystore        # PKCS12 keystore，别名 bluetooth-bridge
android/keystore/keystore.properties     # storeFile / 密码 / 别名
android/keystore/keystore-base64.txt     # 给 GitHub Secrets 用的单行 base64
```

`android/.gitignore` 已忽略 `/keystore/`。**请单独备份这个目录**：keystore 丢失后，已安装用户无法覆盖升级（签名不一致）。

gradle 的取用顺序（`app/build.gradle.kts`）：

1. 环境变量 `ANDROID_KEYSTORE_FILE` + 密码/别名（CI 用）；
2. `android/keystore/keystore.properties`（本地，自动读取）；
3. 都没有 → release 回退 debug 签名。

本地直接构建即可，不需要导出环境变量：

```bash
./gradlew assembleRelease     # app/build/outputs/apk/release/app-release.apk
```

### 配到 GitHub Actions

仓库 → Settings → Secrets and variables → Actions，添加：

| Secret | 值 |
| --- | --- |
| `ANDROID_KEYSTORE_BASE64` | `android/keystore/keystore-base64.txt` 的全部内容（单行） |
| `ANDROID_KEYSTORE_PASSWORD` | `keystore.properties` 里的 `storePassword` |
| `ANDROID_KEY_ALIAS` | `bluetooth-bridge` |
| `ANDROID_KEY_PASSWORD` | `keystore.properties` 里的 `keyPassword` |

工作流检测到 `ANDROID_KEYSTORE_BASE64` 后：解码到临时文件 → `assembleRelease` → 用 `apksigner verify --print-certs` 校验并打印证书 → 上传 `bluetooth-input-bridge-<ref>-release.apk`。**没有**该 secret（fork PR）时仍构建 debug APK，文件名带 `-debug`。

### 换一套签名材料（可选）

```bash
keytool -genkeypair -v -keystore android/keystore/release.keystore -storetype PKCS12 \
  -alias bluetooth-bridge -keyalg RSA -keysize 2048 -validity 10000
base64 -w0 android/keystore/release.keystore > android/keystore/keystore-base64.txt
```

再同步 `keystore.properties` 里的密码即可。
