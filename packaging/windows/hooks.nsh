; ============================================================================
; OpenPPTView 安装程序的定制部分
; ============================================================================
;
; 由 `tauri.conf.json` 的 `nsis.installerHooks` 指过来：Tauri 会把这个文件
; **原样复制**到打包工作目录，再 `!include` 到模板最前面。放这里的理由有三：
;
;   1. 中文文本只有放在「原样复制」的文件里才不会被中间环节重新编码弄坏
;      （模板本身会被 handlebars 渲染一遍，非 ASCII 字符容易走样）；
;   2. 模板保持纯 ASCII，将来跟上游对新版本时 diff 干净；
;   3. 这是 Tauri 官方留的扩展点，不靠私有约定。
;
; 语言 ID 这里直接写数字（2052 简体中文 / 1033 英文）：本文件在
; `MUI_LANGUAGE` 之前就被 include 了，用 `${LANG_SIMPCHINESE}` 取不到值。
;
; ---------------------------------------------------------------------------
; 界面朝向
; ---------------------------------------------------------------------------
;
; 这个安装程序**不用 NSIS 原版向导的样子**（欢迎页 / 许可协议 / 目录 / 下一步），
; 而是仿 WPS 那种单窗口定制安装器：一页装完，一个大按钮，字大、话少。
; 目标用户是不太会用电脑的老师，所以每个决定都按「他会不会懵」来判断：
;
;   · 一页搞定：安装位置 + 两个选择 + 一个大按钮，没有「下一步」要猜；
;   · 按钮就一个，而且做得又大又粗（14pt 加粗），一眼知道点哪儿；
;   · 说明文字压暗一档放小，正文用 10~11pt，比 Windows 默认大一档；
;   · 向导自带的「上一步 / 下一步 / 取消」全部藏起来，避免误点；
;   · 措辞按口语写：「装到这台电脑上」「装错了也没关系，随时可以卸载」。
;
; 实现要点：自绘页面（nsDialogs）里那个大按钮**不是**向导的按钮，点它时
; 去 `BM_CLICK` 那个被藏起来的「下一步」，这样安装流程仍走模板原有的分段，
; 我们只换皮不动骨。

!define OPPVREGKEY "Software\OpenPPTView"

!ifndef BS_DEFPUSHBUTTON
  !define BS_DEFPUSHBUTTON 0x00000001
!endif
!ifndef BM_SETSTYLE
  !define BM_SETSTYLE 0x0400
!endif

; ---------------------------------------------------------------------------
; 文案
; ---------------------------------------------------------------------------
;
; 语气按「跟长辈说话」来写：短句、口头语、不用术语。

; 第 1 页：安装
LangString oppvMainTitle   2052 "装到这台电脑上"
LangString oppvMainTitle   1033 "Install on this computer"

LangString oppvMainSub     2052 "选好位置，点一下底下的大按钮就开始装。"
LangString oppvMainSub     1033 "Pick a folder, then press the big button."

LangString oppvPathLabel   2052 "装到哪个文件夹："
LangString oppvPathLabel   1033 "Install to:"

LangString oppvBrowse      2052 "换一个…"
LangString oppvBrowse      1033 "Change..."

LangString oppvModeLabel   2052 "平时您怎么操作这台电脑？"
LangString oppvModeLabel   1033 "How do you usually use this computer?"

LangString oppvModeAuto    2052 "说不准（推荐）"
LangString oppvModeAuto    1033 "Not sure (recommended)"

LangString oppvModeTouch   2052 "用手指点屏幕"
LangString oppvModeTouch   1033 "Touch screen"

LangString oppvModeMouse   2052 "用鼠标键盘"
LangString oppvModeMouse   1033 "Mouse and keyboard"

LangString oppvDesktop     2052 "在桌面上放一个快捷方式（方便找）"
LangString oppvDesktop     1033 "Put a shortcut on the desktop"

LangString oppvHint        2052 "选「用手指点屏幕」的话，应用里的按钮会更大、还带中文名字。$\r$\n装错了也不要紧，随时都能卸掉。"
LangString oppvHint        1033 "Choosing the touch screen option makes the app's buttons bigger and adds labels.$\r$\nInstalling is safe: you can uninstall at any time."

LangString oppvInstallNow  2052 "开 始 安 装"
LangString oppvInstallNow  1033 "I N S T A L L"

LangString oppvPathEmpty   2052 "还没有选装到哪个文件夹，请先点「换一个…」或者留着默认的。"
LangString oppvPathEmpty   1033 "Please choose an install folder first."

LangString oppvPickFolder  2052 "选一个文件夹"
LangString oppvPickFolder  1033 "Choose a folder"

; 第 2 页：进度
LangString oppvProgressTitle 2052 "正在安装"
LangString oppvProgressTitle 1033 "Installing"

LangString oppvProgressSub   2052 "几秒钟就好，请不要关掉这个窗口。"
LangString oppvProgressSub   1033 "This takes a few seconds. Please keep this window open."

; 第 3 页：完成
LangString oppvDoneTitle   2052 "装好了"
LangString oppvDoneTitle   1033 "All done"

LangString oppvDoneSub     2052 "以后双击课件就能打开。"
LangString oppvDoneSub     1033 "Double-click any slide from now on."

LangString oppvDoneBody    2052 "OpenPPTView 已经装到这台电脑上了。"
LangString oppvDoneBody    1033 "OpenPPTView is now installed."

LangString oppvDoneHow     2052 "以后双击 .pptx 文件就能直接打开。$\r$\n放映的时候，按 F5 开始，按 Esc 退出。"
LangString oppvDoneHow     1033 "Double-click any .pptx file to open it.$\r$\nWhile presenting: F5 starts, Esc exits."

LangString oppvDoneNote    2052 "要是双击课件没有用 OpenPPTView 打开，$\r$\n在应用首页点「默认打开方式」，注册一下就好了。"
LangString oppvDoneNote    1033 "If slides open in another app, use 'Default app' on the home page to register OpenPPTView."

LangString oppvDoneOpen    2052 "装好以后马上就打开一个试试"
LangString oppvDoneOpen    1033 "Open OpenPPTView right now"

LangString oppvDoneButton  2052 "完 成"
LangString oppvDoneButton  1033 "D O N E"

; ---------------------------------------------------------------------------
; 控件句柄与选项
; ---------------------------------------------------------------------------

Var OppvPathCtl
Var OppvBrowseCtl
Var OppvModeAutoCtl
Var OppvModeTouchCtl
Var OppvModeMouseCtl
Var OppvDesktopCtl
Var OppvHintCtl
Var OppvMainBtn
Var OppvDoneOpenCtl
Var OppvDoneBtn

Var OppvDesktop
Var OppvMode
; 读命令行用的一次性变量（不占 $0~$9，免得踩到模板寄存器的值）
Var OppvCmdProbe

; ---------------------------------------------------------------------------
; 小工具
; ---------------------------------------------------------------------------

/**
 * 把向导自带的按钮藏起来。
 *
 * 原版向导右下角是「上一步 / 下一步 / 取消」，对不熟电脑的人是干扰 ——
 * 我们的页面上自己有按钮。`$HWNDPARENT` 的子控件 1/2/3 就是这三个。
 *
 * 传 1 表示连「下一步」一起藏：安装页与完成页都是这样，
 * 那两页的推进由页面上的大按钮发 `WM_COMMAND`（控件 ID 1）来完成。
 * 传 0 表示留着「取消」—— 装 WebView2 时可能要等几分钟，得给人留个退路。
 */
!macro OppvHideWizardButtons HIDE_NEXT
  Push $0
  GetDlgItem $0 $HWNDPARENT 2      ; 取消 / 关闭
  ${If} ${HIDE_NEXT} == 1
    ShowWindow $0 ${SW_HIDE}
  ${EndIf}
  GetDlgItem $0 $HWNDPARENT 3      ; 上一步
  ShowWindow $0 ${SW_HIDE}
  ${If} ${HIDE_NEXT} == 1
    GetDlgItem $0 $HWNDPARENT 1    ; 下一步 / 安装
    ShowWindow $0 ${SW_HIDE}
  ${EndIf}
  Pop $0
!macroend

/** 给一个控件换成指定字号 / 字重的字体。 */
!macro OppvSetFont CTL SIZE WEIGHT
  Push $R0
  CreateFont $R0 "$(^Font)" "${SIZE}" "${WEIGHT}"
  SendMessage ${CTL} ${WM_SETFONT} $R0 1
  Pop $R0
!macroend

/**
 * 定时再藏一次按钮。
 *
 * 自绘页面的创建函数跑完之后，向导还会刷新一次按钮状态；万一那一下把
 * 「下一步」又显示出来，页面上就多出一个我们不想让人点的按钮。这个
 * 250ms 的定时器只是把同一件事再说一遍（幂等），页面关掉时随之消失。
 */
!macro OppvKeepButtonsHidden
  ${NSD_CreateTimer} OppvHideButtonsTick 250
!macroend

Function OppvHideButtonsTick
  !insertmacro OppvHideWizardButtons 1
FunctionEnd

; ---------------------------------------------------------------------------
; 第 1 页：装到哪儿 + 怎么用 + 开始安装
; ---------------------------------------------------------------------------

/**
 * 主页面。
 *
 * 一页放下所有决定：装到哪、桌面要不要快捷方式、平时怎么操作电脑。
 * 底下一个大按钮就是全部 —— 没有「下一步」要猜。
 */
Function OppvMainPage
  ; 静默 / 被动安装（`/P`）时这一页不该出现。
  ;
  ; 这里自己去命令行里找 `/P`，不读模板的 `$PassiveMode`：
  ; 本文件被 include 到模板最前面，那时 `Var PassiveMode` 还没声明
  ; （上一版因此报过 warning 6000，条件永远是假）。
  ${GetOptions} $CMDLINE "/P" $OppvCmdProbe
  ${IfNot} ${Errors}
    Abort
  ${EndIf}

  ; 每次进这一页都把默认值摆好（老师来回翻页时不会被上一次的残留状态影响）
  StrCpy $OppvDesktop ${BST_CHECKED}
  StrCpy $OppvMode "auto"

  !insertmacro MUI_HEADER_TEXT "$(oppvMainTitle)" "$(oppvMainSub)"

  nsDialogs::Create 1018
  Pop $0
  ${If} $0 == error
    Abort
  ${EndIf}

  ; 向导自带的按钮全藏掉：这一页只有一个主角（底下那个大按钮）
  !insertmacro OppvHideWizardButtons 1
  !insertmacro OppvKeepButtonsHidden

  ; 安装位置
  ${NSD_CreateLabel} 0 0 100% 14u "$(oppvPathLabel)"
  Pop $0
  SetCtlColors $0 "1D1D1F" transparent
  !insertmacro OppvSetFont $0 11 700

  ${NSD_CreateText} 0 18u -64u 15u "$INSTDIR"
  Pop $OppvPathCtl
  SetCtlColors $OppvPathCtl "1D1D1F" "FFFFFF"
  !insertmacro OppvSetFont $OppvPathCtl 10 400

  ${NSD_CreateButton} -60u 18u 60u 15u "$(oppvBrowse)"
  Pop $OppvBrowseCtl
  !insertmacro OppvSetFont $OppvBrowseCtl 10 400
  ${NSD_OnClick} $OppvBrowseCtl OppvBrowseClicked

  ; 平时怎么操作电脑：三选一，一行放完
  ${NSD_CreateLabel} 0 42u 100% 14u "$(oppvModeLabel)"
  Pop $0
  SetCtlColors $0 "1D1D1F" transparent
  !insertmacro OppvSetFont $0 11 700

  ${NSD_CreateRadioButton} 0 60u 33% 12u "$(oppvModeAuto)"
  Pop $OppvModeAutoCtl
  ${NSD_CreateRadioButton} 33% 60u 33% 12u "$(oppvModeTouch)"
  Pop $OppvModeTouchCtl
  ${NSD_CreateRadioButton} 66% 60u -1u 12u "$(oppvModeMouse)"
  Pop $OppvModeMouseCtl
  ${NSD_Check} $OppvModeAutoCtl
  !insertmacro OppvSetFont $OppvModeAutoCtl 10 400
  !insertmacro OppvSetFont $OppvModeTouchCtl 10 400
  !insertmacro OppvSetFont $OppvModeMouseCtl 10 400

  ; 一条浅灰细线：把「装到哪」和「怎么用」分成两段
  ${NSD_CreateLabel} 0 78u 100% 1u ""
  Pop $0
  SetCtlColors $0 "" "E3E4EA"

  ${NSD_CreateCheckbox} 0 86u 100% 14u "$(oppvDesktop)"
  Pop $OppvDesktopCtl
  ${NSD_Check} $OppvDesktopCtl
  !insertmacro OppvSetFont $OppvDesktopCtl 10 400

  ; 说明文字压暗一档：它是注解，不该和选项抢注意力
  ${NSD_CreateLabel} 0 106u 100% 22u "$(oppvHint)"
  Pop $OppvHintCtl
  SetCtlColors $OppvHintCtl "6E6E73" transparent
  !insertmacro OppvSetFont $OppvHintCtl 9 400

  ; 主角：开始安装。占满整行、34u 高、14pt 加粗
  ${NSD_CreateButton} 0 132u 100% 34u "$(oppvInstallNow)"
  Pop $OppvMainBtn
  !insertmacro OppvSetFont $OppvMainBtn 14 700
  SendMessage $OppvMainBtn ${BM_SETSTYLE} ${BS_DEFPUSHBUTTON} 1
  ${NSD_SetFocus} $OppvMainBtn
  ${NSD_OnClick} $OppvMainBtn OppvInstallClicked

  nsDialogs::Show
FunctionEnd

/** 「换一个…」：挑一个文件夹填进输入框。 */
Function OppvBrowseClicked
  ${NSD_GetText} $OppvPathCtl $0
  nsDialogs::SelectFolderDialog "$(oppvPickFolder)" "$0"
  Pop $0
  ${If} $0 != error
    ${NSD_SetText} $OppvPathCtl "$0"
  ${EndIf}
FunctionEnd

/**
 * 「开始安装」：先把这一页的选择收好，再去点那个藏起来的「下一步」。
 *
 * 不直接 `SendMessage $HWNDPARENT` 走向导逻辑，是因为模板里的分段
 * （写注册表、建快捷方式、注册文件关联）都挂在向导的页序上 ——
 * 走原路最省心，我们只负责换个样子。
 */
Function OppvInstallClicked
  ${NSD_GetText} $OppvPathCtl $0
  ${If} $0 == ""
    MessageBox MB_ICONEXCLAMATION|MB_OK "$(oppvPathEmpty)"
    Return
  ${EndIf}
  StrCpy $INSTDIR $0

  Call OppvCollectOptions

  ; 直接给向导发「下一步」的单击通知（控件 ID 1），而不是 BM_CLICK ——
  ; 按钮被藏起来了，BM_CLICK 那种模拟鼠标的做法不靠谱；WM_COMMAND 是
  ; 按钮被真点下去时一样会发到父窗口的消息，走的是同一条路。
  SendMessage $HWNDPARENT ${WM_COMMAND} 1 0
FunctionEnd

/** 离开这一页时把选项收进变量（走的是「下一步」之外的路径时也要收）。 */
Function OppvMainPageLeave
  Call OppvCollectOptions
FunctionEnd

/** 把三个单选项与勾选框收进 `$OppvMode` / `$OppvDesktop`。 */
Function OppvCollectOptions
  ${If} $OppvPathCtl <> 0
    ${NSD_GetText} $OppvPathCtl $0
    ${If} $0 != ""
      StrCpy $INSTDIR $0
    ${EndIf}
  ${EndIf}

  ; 静默安装时控件从没被创建过（句柄为 0），此时保留默认值
  ${If} $OppvDesktopCtl <> 0
    ${NSD_GetState} $OppvDesktopCtl $OppvDesktop
  ${Else}
    StrCpy $OppvDesktop ${BST_CHECKED}
  ${EndIf}

  ${If} $OppvModeTouchCtl == 0
    StrCpy $OppvMode "auto"
    Return
  ${EndIf}
  ${NSD_GetState} $OppvModeTouchCtl $0
  ${If} $0 = ${BST_CHECKED}
    StrCpy $OppvMode "touch"
  ${Else}
    ${NSD_GetState} $OppvModeMouseCtl $0
    ${If} $0 = ${BST_CHECKED}
      StrCpy $OppvMode "mouse"
    ${Else}
      StrCpy $OppvMode "auto"
    ${EndIf}
  ${EndIf}
FunctionEnd

; ---------------------------------------------------------------------------
; 第 2 页：进度
; ---------------------------------------------------------------------------

/**
 * 进度页：把「上一步 / 下一步」藏掉，页头换成一句人话。
 *
 * 进度条本身是系统画的，够用；这里只保证老师看到的是「正在安装 / 几秒钟就好」，
 * 而不是原版那句干巴巴的提示。
 */
Function OppvProgressShow
  !insertmacro OppvHideWizardButtons 0
  !insertmacro MUI_HEADER_TEXT "$(oppvProgressTitle)" "$(oppvProgressSub)"
FunctionEnd

; ---------------------------------------------------------------------------
; 第 3 页：装好了
; ---------------------------------------------------------------------------

/** 完成页：告诉他装好了、以后怎么用，然后一个大按钮收尾。 */
Function OppvDonePage
  ; 静默 / 被动安装（`/P`）时这一页不该出现（原因同主页面的注释）
  ${GetOptions} $CMDLINE "/P" $OppvCmdProbe
  ${IfNot} ${Errors}
    Abort
  ${EndIf}

  !insertmacro MUI_HEADER_TEXT "$(oppvDoneTitle)" "$(oppvDoneSub)"

  nsDialogs::Create 1018
  Pop $0
  ${If} $0 == error
    Abort
  ${EndIf}

  ${NSD_CreateLabel} 0 0 100% 18u "$(oppvDoneBody)"
  Pop $0
  SetCtlColors $0 "1D1D1F" transparent
  !insertmacro OppvSetFont $0 13 700

  ${NSD_CreateLabel} 0 26u 100% 32u "$(oppvDoneHow)"
  Pop $0
  SetCtlColors $0 "1D1D1F" transparent
  !insertmacro OppvSetFont $0 11 400

  ${NSD_CreateLabel} 0 64u 100% 30u "$(oppvDoneNote)"
  Pop $0
  SetCtlColors $0 "6E6E73" transparent
  !insertmacro OppvSetFont $0 9 400

  ${NSD_CreateCheckbox} 0 100u 100% 14u "$(oppvDoneOpen)"
  Pop $OppvDoneOpenCtl
  ${NSD_Check} $OppvDoneOpenCtl
  !insertmacro OppvSetFont $OppvDoneOpenCtl 11 400

  ${NSD_CreateButton} 0 132u 100% 34u "$(oppvDoneButton)"
  Pop $OppvDoneBtn
  !insertmacro OppvSetFont $OppvDoneBtn 14 700
  SendMessage $OppvDoneBtn ${BM_SETSTYLE} ${BS_DEFPUSHBUTTON} 1
  ${NSD_SetFocus} $OppvDoneBtn
  ${NSD_OnClick} $OppvDoneBtn OppvDoneClicked

  !insertmacro OppvHideWizardButtons 1
  !insertmacro OppvKeepButtonsHidden

  nsDialogs::Show
FunctionEnd

/**
 * 「完成」：走向导自己的「下一步」把安装程序收掉。
 *
 * 不去 `WM_CLOSE` 是因为那会被当成「中途退出」，可能弹确认框；
 * 点「下一步」在最后一页就等于正常收尾，完成页的 leave 也照常执行
 * （要不要顺手把应用打开，在 leave 里按勾选决定）。
 */
Function OppvDoneClicked
  SendMessage $HWNDPARENT ${WM_COMMAND} 1 0
FunctionEnd

/** 完成页离开时（不管是点大按钮还是关窗口）按勾选决定要不要打开。 */
Function OppvDonePageLeave
  ${If} $OppvDoneOpenCtl <> 0
    ${NSD_GetState} $OppvDoneOpenCtl $0
    ${If} $0 = ${BST_CHECKED}
      Call RunMainBinary
    ${EndIf}
  ${EndIf}
FunctionEnd

; ---------------------------------------------------------------------------
; 安装前后
; ---------------------------------------------------------------------------

!macro NSIS_HOOK_PREINSTALL
  ; 兜底：静默安装 / 上一页没走过时，也要有合理的默认值写进注册表
  ${If} $OppvMode == ""
    StrCpy $OppvMode "auto"
  ${EndIf}
  ${If} $OppvDesktop == ""
    StrCpy $OppvDesktop ${BST_CHECKED}
  ${EndIf}
!macroend

!macro NSIS_HOOK_POSTINSTALL
  ; 记住老师的选择：应用首启按它走，不必再自己猜设备
  WriteRegStr HKLM "${OPPVREGKEY}" "InputMode" "$OppvMode"
  WriteRegDWORD HKLM "${OPPVREGKEY}" "DesktopShortcut" "$OppvDesktop"

  ; 桌面快捷方式在这里建，而不是放在「完成」页的复选框里 ——
  ; 装的时候就按他勾的办，省得回头找不到程序
  ${If} $OppvDesktop = ${BST_CHECKED}
    Call CreateOrUpdateDesktopShortcut
  ${EndIf}
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  ; 卸载时把这一项留着的选择清掉，免得重装后读到上一次的旧值
  DeleteRegKey HKLM "${OPPVREGKEY}"
!macroend
