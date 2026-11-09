# 蓝牙输入桥 · Bluetooth Input Bridge

把手机当电脑键盘用：手机端（Android）通过经典蓝牙 RFCOMM 把输入的文字发给电脑端（Linux），电脑端把文字注入当前焦点。一个仓库同时容纳两个平台端。

## 目录结构

```
android/                     Android 客户端（Jetpack Compose，minSdk 26 / targetSdk 36）
  app/                       UI、蓝牙连接、自动重连、连接历史、五语资源
  PROTOCOL.md                手机 ↔ 电脑的线路协议（UUID、JSON 行格式）
desktop/                     Linux 桌面端
  src-tauri/src/service/     无界面的 Rust 服务：BlueZ RFCOMM、收包、注入（可脱离 UI 运行）
  src-tauri/src/bin/         独立的 bluetooth-input-bridge-service 入口
  src-tauri/src/lib.rs       Tauri 薄壳：只通过本地 socket 与服务工作
  web/                       前端（原生 HTML/CSS/JS，随时可换技术栈）
  docs/service-api.md        控制 socket 的命令与状态字段
  docs/windows.md            Windows 版预留说明与待实现清单
.github/workflows/build.yml  CI：Android APK、Linux 桌面端、Windows 占位（手动）
```

## 快速开始

### Android

```bash
cd android
./gradlew assembleDebug          # 产物 app/build/outputs/apk/debug/app-debug.apk
adb install -r app/build/outputs/apk/debug/app-debug.apk
```

本机开发环境（非必需）可先 `source /home/chao/android-dev/env.sh` 设置 Android SDK、JDK 与代理。

### Linux 桌面端

```bash
cd desktop/src-tauri
cargo build --release            # 生成 bluetooth-input-desktop 与 bluetooth-input-bridge-service
./target/release/bluetooth-input-bridge-service   # 可单独无界面运行；界面也会自动拉起
./target/release/bluetooth-input-desktop
```

依赖（Ubuntu/Debian）：

```bash
sudo apt install libwebkit2gtk-4.1-dev libgtk-3-dev libayatana-appindicator3-dev \
                 librsvg2-dev libsoup-3.0-dev libssl-dev pkg-config
```

在 BlueZ + GNOME Wayland 下验证过；界面语言跟随系统（中/英/俄/日/韩，未匹配回退英文）。

## 持续集成

`.github/workflows/build.yml` 在 push（`main`/`master`）、Pull Request 和手动触发时运行：

| 作业 | 平台 | 内容 |
| --- | --- | --- |
| `android` | ubuntu-latest | JDK 21 + Android SDK 36；配了签名 Secrets 时 `assembleRelease` 并校验签名，否则 `assembleDebug`，上传 `android-apk` |
| `desktop` | ubuntu-24.04 | WebKitGTK 依赖 + stable Rust，`cargo build --locked --release --bins`，跑协议单元测试，上传 `desktop-linux` |
| `windows` | windows-latest | **占位**：只在手动触发且勾选 `build_windows` 时运行，构建出的是未接通后端的 exe |
| `release` | ubuntu-24.04 | 推送 `v*` 标签时，把 APK 和 Linux `.tar.gz` 发到 Release |

## 平台支持

- **Linux**：完整功能。BlueZ profile 注册、RFCOMM 收发、wl-copy + uinput 注入、systemd 用户服务开机自启。
- **Windows**：仅预留位置（构建作业、`windows.rs` 边界、[docs/windows.md](desktop/docs/windows.md) 的实现清单）。当前能构建出的桌面端不包含蓝牙与注入后端。
- **macOS**：未适配。

## 发布

```bash
git tag v0.2.0
git push origin main --tags
```

APK 默认使用 debug 签名，可直接安装测试。要发正式签名的包，在仓库 Secrets 里配置 `ANDROID_KEYSTORE_BASE64`、`ANDROID_KEYSTORE_PASSWORD`、`ANDROID_KEY_ALIAS`、`ANDROID_KEY_PASSWORD` 四项，工作流会自动改跑 `assembleRelease`、校验证书并把签名 APK 附到 Release；生成 keystore 与配置步骤见 [android/README.md](android/README.md#发布签名)。