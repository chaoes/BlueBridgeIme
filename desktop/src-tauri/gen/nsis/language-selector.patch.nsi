; 安装包定制 2/2：语言选择器每次先弹出
;
; tauri 模板只在“没记住过安装语言”时才弹语言选择框（MUI_LANGDLL_DISPLAY 内部判断）。
; 这个安装包只有简体中文和英文两种界面语言，显式声明“总是显示”可以让选择框始终
; 出现在向导之前，用户随时可以切换，而不是被动沿用上次记住的语言。
;
; 补丁位置：模板 .onInit 里调用 MUI_LANGDLL_DISPLAY 的地方。

=== ANCHOR ===
!if "${DISPLAYLANGUAGESELECTOR}" == "true"
    !insertmacro MUI_LANGDLL_DISPLAY
  !endif
=== REPLACE ===
!if "${DISPLAYLANGUAGESELECTOR}" == "true"
    !define MUI_LANGDLL_ALWAYSSHOW
    !define MUI_LANGDLL_ALWAYSSHOWSILENT
    !insertmacro MUI_LANGDLL_DISPLAY
  !endif