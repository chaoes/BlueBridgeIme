; 蓝牙输入桥 · Windows 安装器钩子
; （在 tauri.conf.json 的 bundle > windows > nsis > installerHooks 里引用）
;
; 安装包由 tauri 的 NSIS 模板生成，向导顺序：
;   语言选择 → 欢迎 → 安装模式（仅当前用户 / 所有用户）→ 目录选择页（可自定义安装位置）
;   → 开始菜单文件夹 → 安装 → 完成页（可选：创建桌面快捷方式、安装后启动）
;
; 这个文件只做 tauri 模板没提供、但本项目需要的事：
;
;   1. 目录页文案：明确告诉用户这里能选安装位置，而且是绿色可搬动的目录。
;   2. PREINSTALL：清掉旧版本留下的、可能指向别的目录的自启值。服务按 exe 完整路径校验，
;      路径过期的 Run 值等于失效的自启项，留着只会让人困惑。
;   3. POSTINSTALL：记录安装位置（界面的“开机自启”开关会读它来定位服务），
;      并清理一次历史锁文件。默认不写开机自启——是否常驻由用户自己在界面里决定。
;   4. PREUNINSTALL：先结束可能还在跑的服务/界面进程，否则蓝牙 SDP 记录可能被占用。
;   5. POSTUNINSTALL：摘掉自启值和安装位置；日志与配置（%APPDATA%\bluebridge-ime）
;      保留，重装可沿用；想彻底清掉删那个目录即可。
;
; 语法提示：本文件会被 NSIS 原样插进 installer.nsi（include 位置在 tauri 模板第 36 行，
; 早于所有页面定义），所以这里写裸语句，不要再套一层 !macro。这里 !define 的宏只有
; 在插入点之后才可见，跨段请用运行时命令（WriteRegStr / DeleteRegValue / ReadRegStr）。
; 文件开头 include 的 LogicLib 只在插入点之后可见，PREUNINSTALL 宏体
; 要等模板里 !insertmacro 时才展开，那段代码属于后续 Section，所以能用。

!include LogicLib.nsh

; ---- NSIS_PREINSTALL ----------------------------------------------------
!macro NSIS_HOOK_PREINSTALL
  ; 历史版本可能把自启值写在两个名字下，且指向旧安装目录。
  DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "BluetoothInputBridge"
!macroend

; ---- NSIS_POSTINSTALL ---------------------------------------------------
!macro NSIS_HOOK_POSTINSTALL
  WriteRegStr HKCU "Software\bluebridge-ime" "InstallDir" "$INSTDIR"
  Delete "$APPDATA\bluebridge-ime\service.lock"
  Delete "$LOCALAPPDATA\BluetoothInputBridge\instance.mutex"
!macroend

; ---- NSIS_PREUNINSTALL --------------------------------------------------
; 先结束可能还在跑的服务/界面进程，否则蓝牙 SDP 记录和管道会一直被占用到重启。
;
; 注意：这里【不】用 taskkill 写日志。进程本来就没在跑时 taskkill 会打印
;   「错误: 没有找到进程 “xxx.exe”。」并以 128 退出，
; 而 NSIS 把 *:: 插件的任何非零返回值当错误显示在日志框和“详细信息”里，
; 卸载明明成功的用户会以为出了问题（之前就这么踩过）。
;
; 也不能用 nsProcess：tauri 自带的 NSIS 里没有这个插件（不联网下载插件，CI 与本地行为一致）。
; 所以只用内置插件 nsExec + 一个很小的 System::Call 检测：
;   0 = ERROR_SUCCESS = 进程存在，否则 87 / 5 = 找不到进程（顺带覆盖 UIPI 拒绝的情况）。
; 全程静默，只有“真的杀掉了”才写一行日志。
!macro NSIS_HOOK_PREUNINSTALL
  !insertmacro BIB_KillIfRunning "bluetooth-input-bridge-service.exe" "服务进程"
  !insertmacro BIB_KillIfRunning "bluetooth-input-desktop.exe" "界面进程"

  ; tauri 模板在 PREUNINSTALL 之后还会跑一次 RestartManager 版的
  ; CheckIfAppIsRunning（按文件占用检测界面程序），那个归模板管，这里不重复。
!macroend

!macro BIB_KillIfRunning EXE_NAME LABEL
  StrCpy $R8 0
  System::Call 'kernel32::CreateMutex(p 0, i 0, t "${EXE_NAME}") p .r7 ?e'
  System::Call 'kernel32::CloseHandle(p r7)'
  ${If} $R8 == 0
    nsExec::Exec 'taskkill /F /IM ${EXE_NAME}'
    Pop $0
    ${If} $0 == "0"
      DetailPrint "已结束正在运行的${LABEL}"
    ${EndIf}
  ${EndIf}
!macroend
; ---- NSIS_POSTUNINSTALL -------------------------------------------------
!macro NSIS_HOOK_POSTUNINSTALL
  DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "BluetoothInputBridge"
  DeleteRegKey HKCU "Software\bluebridge-ime"
!macroend
