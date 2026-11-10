; 英文界面文本（tauri 打包 NSIS 时用的 LangString 表）
; 来源：tauri 源码 crates/tauri-bundler/src/bundle/windows/nsis/languages/English.nsh，
; 在 tauri.conf.json 的 bundle.windows.nsis.customLanguageFiles 里引用。
; 这个文件不是模板，不做占位符替换，所以原文里的 product_name 占位符已换成产品名。
LangString addOrReinstall ${LANG_ENGLISH} "Add/Reinstall components"
LangString alreadyInstalled ${LANG_ENGLISH} "Already Installed"
LangString alreadyInstalledLong ${LANG_ENGLISH} "${PRODUCTNAME} ${VERSION} is already installed. Select the operation you want to perform and click Next to continue."
LangString appRunning ${LANG_ENGLISH} "Bluetooth Input Bridge is running! Please close it first then try again."
LangString appRunningOkKill ${LANG_ENGLISH} "Bluetooth Input Bridge is running!$\nClick OK to kill it"
LangString chooseMaintenanceOption ${LANG_ENGLISH} "Choose the maintenance option to perform."
LangString chooseHowToInstall ${LANG_ENGLISH} "Choose how you want to install ${PRODUCTNAME}."
LangString createDesktop ${LANG_ENGLISH} "Create desktop shortcut"
LangString dontUninstall ${LANG_ENGLISH} "Do not uninstall"
LangString dontUninstallDowngrade ${LANG_ENGLISH} "Do not uninstall (Downgrading without uninstall is disabled for this installer)"
LangString failedToKillApp ${LANG_ENGLISH} "Failed to kill Bluetooth Input Bridge. Please close it first then try again"
LangString installingWebview2 ${LANG_ENGLISH} "Installing WebView2..."
LangString newerVersionInstalled ${LANG_ENGLISH} "A newer version of ${PRODUCTNAME} is already installed! It is not recommended that you install an older version. If you really want to install this older version, it's better to uninstall the current version first. Select the operation you want to perform and click Next to continue."
LangString older ${LANG_ENGLISH} "older"
LangString olderOrUnknownVersionInstalled ${LANG_ENGLISH} "An $R4 version of ${PRODUCTNAME} is installed on your system. It's recommended that you uninstall the current version before installing. Select the operation you want to perform and click Next to continue."
LangString silentDowngrades ${LANG_ENGLISH} "Downgrades are disabled for this installer, can't proceed with the silent installer, please use the graphical interface installer instead.$\n"
LangString unableToUninstall ${LANG_ENGLISH} "Unable to uninstall!"
LangString uninstallApp ${LANG_ENGLISH} "Uninstall ${PRODUCTNAME}"
LangString uninstallBeforeInstalling ${LANG_ENGLISH} "Uninstall before installing"
LangString unknown ${LANG_ENGLISH} "unknown"
LangString webview2AbortError ${LANG_ENGLISH} "Failed to install WebView2! The app can't run without it. Try restarting the installer."
LangString webview2DownloadError ${LANG_ENGLISH} "Error: Downloading WebView2 Failed - $0"
LangString webview2DownloadSuccess ${LANG_ENGLISH} "WebView2 bootstrapper downloaded successfully"
LangString webview2Downloading ${LANG_ENGLISH} "Downloading WebView2 bootstrapper..."
LangString webview2InstallError ${LANG_ENGLISH} "Error: Installing WebView2 failed with exit code $1"
LangString webview2InstallSuccess ${LANG_ENGLISH} "WebView2 installed successfully"
LangString deleteAppData ${LANG_ENGLISH} "Delete the application data"
; tauri 模板把 $(choowHowToInstall) 当旧拼写的回退键读取（installer.nsi 的 PageReinstall），
; 不定义会留下 makensis warning 6040；这里补一份同样的文案。
LangString choowHowToInstall ${LANG_ENGLISH} "Choose how you want to install ${PRODUCTNAME}."
