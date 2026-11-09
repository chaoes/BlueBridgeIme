# Windows 版：预留位置与实现清单

当前仓库只完成了 Linux 后端。Windows 端的**位置**已经留好：CI 里有 `windows` 作业，代码里有 `src-tauri/src/windows.rs` 这个边界文件，前端与协议完全复用。本文记录要补什么、补在哪里，避免以后重新摸索结构。

## 现在能运行的部分

- `cargo build --release --bins` 在 Windows 上能编译通过（`windows.rs` 返回占位错误，服务二进制打印「仅支持 Linux」后退出）。
- Tauri 窗口能打开，界面文案与状态卡片正常显示，但所有蓝牙相关操作都会得到「尚未实现」类错误。
- 手动触发 CI 并勾选 `build_windows` 会产出两个 `.exe`（产物名 `desktop-windows-placeholder`，保留 7 天）。

## 待实现

| 层 | Linux 现状 | Windows 需要 |
| --- | --- | --- |
| 蓝牙服务注册 | BlueZ `RegisterProfile`（zbus） | Winsock 蓝牙 API：`WSASetService` / `BluetoothSetServiceState` 注册同一 UUID `e8b6a86a-73ce-4cbb-9ea8-9f9c2fae6d51` 的 RFCOMM 服务 |
| 监听与收包 | `Profile1.NewConnection` + Unix socket fd | `SOCK_STREAM` + `AF_BLUETOOTH`/`BTHPROTO_RFCOMM`，`bind`/`listen`/`accept`，得到的 socket 包装成 `Read + Write` 交给 `protocol::Session` |
| 文本注入 | wl-copy + `/dev/uinput` 发 Ctrl+V | `SendInput` + `KEYEVENTF_UNICODE` 逐字符发送（对中文/emoji 也适用）；若目标窗口是管理员权限进程，普通权限注入会被 UIPI 拦截，需要在文档/界面里说明 |
| 进程间控制通道 | Unix socket `$XDG_RUNTIME_DIR/.../control.sock` | 命名管道 `\\.\pipe\bluetooth-input-bridge-control`（或回环 TCP），JSON 行协议与事件格式保持不变；`tauri` 外壳里的 `service/client.rs` 需要按平台抽象出 transport |
| 服务注册开关 | 动态 `RegisterProfile`/`UnregisterProfile` | 动态调用 `WSASetService(..., RNRSERVICE_DELETE)` 或停掉监听 socket；重启监听即可恢复 |
| 开机自启 | systemd 用户单元 `graphical-session.target` | 注册表 `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`，或计划任务（需要管理员时用后者） |
| 设备名 | BlueZ `Device1.Alias`/`Name` | `BluetoothGetDeviceInfo` / `BluetoothFindFirstDevice` 读取友好名称；失败时回退 MAC |

## 代码落点

```
desktop/src-tauri/src/
  windows.rs               当前占位入口（run()），实现后在这里启动监听与注入
  lib.rs                   `#[cfg(target_os = "windows")]` 分支已经接好：setup 时 spawn windows::run()
  service/
    client.rs              Linux 专用（Unix socket）；Windows 版建议新增 client_windows.rs + `#[cfg]` 分派
    mod.rs                 `pub mod linux` 已按 cfg 收在 lib.rs 里，Windows 模块可平行加进来
  bin/bridge-service.rs    非 Linux 分支现在是「仅支持 Linux」提示，改成调用 Windows 的 run_headless()
  web/                     不用改：状态字段、命令名、i18n 全部复用
```

约定不变的部分：

- 线路协议：`PROTOCOL.md` 里的 UUID、JSON 行、16 KiB 文本上限、8 秒回执超时。
- 控制协议：`docs/service-api.md` 里的 `status` / `subscribe` / `setRegistration` / `setAutostart` / `enablePairing` / `sendTest` 命令与 camelCase 状态字段。
- 服务与界面分离：Windows 后端同样应做成可无界面运行的二进制，UI 只当客户端。

## CI 收尾

实现完成、Windows 作业稳定通过后，在 `.github/workflows/build.yml` 里去掉 `windows` 作业的 `if: ${{ inputs.build_windows }}`，让 push/PR 也构建；需要的话把 `.exe` 加进 `release` 作业的打包列表。