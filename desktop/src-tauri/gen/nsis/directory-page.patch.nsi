; 安装包定制 1/2：目录选择页文案
;
; 由 build.rs 合并进 tauri-bundler 的 installer.nsi（tauri.conf.json 的
; bundle.windows.nsis.template 指向 build.rs 生成的 generated-nsis/installer.nsi）。
; 补丁规则：把 ANCHOR 与 REPLACE 两个标记之间那一整段“锚点行”替换成 REPLACE 之后的内容。
;
; 作用：安装向导的“选择目录”页原本用 MUI 默认的标题/说明（路径：Choose the folder in
; which to install ...），中文本地化后仍然含糊。这里换成本项目的说法，说明这个页面
; 就是选安装位置的地方，并且程序按普通目录安装、装好之后可以整体搬走。MUI 的这几个宏
; 必须在 !insertmacro MUI_PAGE_DIRECTORY 之前定义，所以补丁贴在该页面定义处。
;
; tauri 升级后如果构建日志报“找不到锚点”，把新模板里 !insertmacro MUI_PAGE_DIRECTORY
; 前面那两行抄到 ANCHOR 区即可（不要改动其中任何两花括号占位符）。

=== ANCHOR ===
!define MUI_PAGE_CUSTOMFUNCTION_PRE SkipIfPassive
!insertmacro MUI_PAGE_DIRECTORY
=== REPLACE ===
!define MUI_DIRECTORYPAGE_TEXT_TOP "请选择 ${PRODUCTNAME} 的安装位置。$\r$\n$\r$\n本程序按普通目录安装，装好之后也可以把整个目录搬到别处。选择“所有用户”时建议保留默认的 Program Files 目录；选择“当前用户”时无需管理员权限，会装在你的用户目录下。"
!define MUI_DIRECTORYPAGE_TEXT_DESTINATION "安装位置："
!define MUI_DIRECTORYPAGE_VERIFYONLEAVE
!define MUI_PAGE_CUSTOMFUNCTION_PRE SkipIfPassive
!insertmacro MUI_PAGE_DIRECTORY