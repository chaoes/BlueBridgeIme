# 打包说明

桌面端产物有两种发行包，CI 在推送 `v*` 标签或手动触发工作流时自动构建，并和 APK、tarball 一起挂到 Release：

- `bluetooth-input-bridge_<version>_amd64.deb` —— Debian / Ubuntu（`desktop-deb` 作业）
- `bluetooth-input-bridge-<version>-1-x86_64.pkg.tar.zst` —— Arch Linux（`desktop-arch` 作业，在 archlinux 容器里跑 `makepkg`）

两个包都会安装：

| 路径 | 内容 |
| --- | --- |
| `/usr/bin/bluetooth-input-desktop` | Tauri 界面（薄壳） |
| `/usr/bin/bluetooth-input-bridge-service` | 无界面蓝牙服务 |
| `/usr/share/applications/bluetooth-input-bridge.desktop` | 桌面菜单项 |
| `/usr/share/icons/hicolor/scalable/apps/bluetooth-input-bridge.svg` | 图标 |

## 本地构建

先构建 release 二进制（在 `desktop/` 下）：

```bash
cargo build --locked --release --bins --manifest-path src-tauri/Cargo.toml
```

### .deb（需要 `dpkg-deb`，建议在 Ubuntu/Debian 容器里跑）

```bash
packaging/deb/build.sh 0.1.0 amd64 dist
dpkg-deb -I dist/*.deb
```

> 二进制与系统库（glibc）绑定：在哪个发行版构建，就应给同级或更新的发行版用。CI 在 Ubuntu 24.04 上构建 deb。

### Arch 包（需要 `base-devel`）

```bash
cd packaging/arch
makepkg -f            # 生成 bluetooth-input-bridge-<pkgver>-1-x86_64.pkg.tar.zst
```

`PKGBUILD` 里的 `build()` 直接调用仓库里的 Cargo 工程，`pkgver` 由 CI 按标签改写；本地构建前可自行改 `pkgver`。安装：`sudo pacman -U *.pkg.tar.zst`。

## 运行时依赖

- Debian/Ubuntu：`libwebkit2gtk-4.1-0 libgtk-3-0 libsoup-3.0-0 librsvg2-2 libevdev2`（`libssl3`/`libssl3t64` 二选一）
- Arch：`webkit2gtk-4.1 gtk3 libsoup3 librsvg libevdev openssl`

界面还需要可用的 Wayland 会话（`wl-copy`、`/dev/uinput` 权限由系统提供）、BlueZ。
