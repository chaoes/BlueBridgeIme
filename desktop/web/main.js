const tauri = window.__TAURI__;
const byId = id => document.getElementById(id);

// 后端（src/lib.rs）的 get_platform 决定：平台标签、服务注册到哪个蓝牙守护进程、
// 配对失败时去哪个系统设置里手动开。前端不能靠 User-Agent 猜——各平台 webview 都是
// 同一个 Chromium 内核，猜不准。
let platform = { os: 'unsupported', label: '', registrationNoteKey: 'registrationNoteOnLinux', pairingHintKey: 'pairingHintLinux' };

const I18N = {
  zh: {
    appTitle: '蓝牙输入桥 · PC',
    brand: '蓝牙输入桥',
    tagline: '让手机输入法为这台电脑输入文字',
    stateStarting: '正在启动服务…',
    stateServiceDown: '服务未运行',
    stateConnected: '手机已连接',
    stateWaiting: '等待手机连接',
    stateRegistrationOff: '服务注册已关闭',
    received: '已接收',
    deviceLabel: '当前设备',
    notConnected: '未连接',
    hintConnecting: '正在连接后台服务。',
    hintServiceDown: '后台服务尚未启动，稍后会重试；也可以手动启动无界面服务。',
    hintConnected: '在电脑上聚焦目标输入框，然后使用手机输入法。',
    hintWaiting: '服务已发布；在安卓端搜索并连接本机。',
    hintRegistrationOff: '打开「服务注册」后，手机才能发现本机。',
    registrationTitle: '服务注册',
    registrationNoteOn: '向 BlueZ 发布 RFCOMM 服务',
    registrationNoteOnLinux: '向 BlueZ 发布 RFCOMM 服务',
    registrationNoteOnWindows: '登记到 Windows 蓝牙服务的 SDP 记录',
    registrationNoteOff: '服务已关闭，不再接受新连接',
    autostartTitle: '开机自启',
    autostartNote: '登录后自动运行无界面服务',
    guideTitle: '软件指引与介绍',
    guideIntro: '手机与这台电脑配对后，「蓝牙输入桥」会把手机输入的内容经蓝牙送到电脑，再输入到当前焦点。',
    guideIntroWindows: '手机与这台电脑配对后，「蓝牙输入桥」会把手机输入的内容经蓝牙送到电脑，再用模拟按键输入到当前焦点；不走剪贴板，覆盖当前选中内容。',
    guideStep1: '在手机上打开应用，连接本机。',
    guideStep2: '把电脑的光标放进要输入的位置。',
    guideStep3: '在手机上打字，电脑端自动输入。',
    pairingButton: '允许配对 3 分钟',
    pairingHint: '首次配对时使用；若系统拒绝，可在 GNOME 蓝牙设置中开启。',
    pairingHintLinux: '首次配对时使用；若系统拒绝，可在 GNOME 蓝牙设置中开启。',
    pairingHintWindows: '首次配对时使用；若系统拒绝，可在 Windows「设置 → 蓝牙和其他设备」中添加设备。',
    pairingOk: '已开启，3 分钟后自动关闭。请立即在安卓端搜索本机。',
    pairingFailed: '开启失败：',
    testTitle: '测试输入框',
    testIntro: '用来试打字，确认键盘和输入法正常。内容只留在本窗口，不会发送，也不会输入到别处。',
    testPlaceholder: '在这里试打字…',
    footerMethod: '输入方式',
    footerPrivacy: '不保存收到的文本 · 服务与界面分离',
    toggleFailed: '切换失败：',
    openInApp: '请在 Tauri 桌面应用中打开此界面。',
  },
  en: {
    appTitle: 'Bluetooth Input Bridge · PC',
    brand: 'Bluetooth Input Bridge',
    tagline: 'Type on your phone, type on this PC',
    stateStarting: 'Starting the service…',
    stateServiceDown: 'Service not running',
    stateConnected: 'Phone connected',
    stateWaiting: 'Waiting for phone',
    stateRegistrationOff: 'Service registration is off',
    received: 'Received',
    deviceLabel: 'Current device',
    notConnected: 'Not connected',
    hintConnecting: 'Connecting to the background service.',
    hintServiceDown: 'The background service is not running. It will retry shortly; you can also start the headless service manually.',
    hintConnected: 'Focus the target input on the PC, then use your phone keyboard.',
    hintWaiting: 'Service published. Search for and connect to this PC on Android.',
    hintRegistrationOff: 'Turn on Service registration so the phone can discover this PC.',
    registrationTitle: 'Service registration',
    registrationNoteOn: 'RFCOMM service published to BlueZ',
    registrationNoteOnLinux: 'RFCOMM service published to BlueZ',
    registrationNoteOnWindows: 'SDP record published to the Windows Bluetooth service',
    registrationNoteOff: 'Service off; new connections are refused',
    autostartTitle: 'Start at login',
    autostartNote: 'Runs the headless service after you log in',
    guideTitle: 'Guide and overview',
    guideIntro: 'After pairing your phone with this PC, Bluetooth Input Bridge sends what you type over Bluetooth and types it into the focused input.',
    guideIntroWindows: 'After pairing your phone with this PC, Bluetooth Input Bridge sends what you type over Bluetooth and types it into the focused input with simulated keystrokes; it never uses the clipboard and replaces the current selection.',
    guideStep1: 'Open the app on your phone and connect to this PC.',
    guideStep2: 'Put the PC cursor where you want to type.',
    guideStep3: 'Type on the phone; the PC types it automatically.',
    pairingButton: 'Allow pairing for 3 minutes',
    pairingHint: 'Use this for the first pairing. If the system refuses, enable it in GNOME Bluetooth settings.',
    pairingHintLinux: 'Use this for the first pairing. If the system refuses, enable it in GNOME Bluetooth settings.',
    pairingHintWindows: 'Use this for the first pairing. If the system refuses, add the device in Windows Settings → Bluetooth & devices.',
    pairingOk: 'Enabled; it turns off after 3 minutes. Search for this PC on Android now.',
    pairingFailed: 'Could not enable: ',
    testTitle: 'Test input',
    testIntro: 'Try typing here to check the keyboard and IME. Text stays in this window; it is not sent or typed anywhere.',
    testPlaceholder: 'Try typing here…',
    footerMethod: 'Input method',
    footerPrivacy: 'Received text is not stored · Service and UI are separate',
    toggleFailed: 'Could not change the setting: ',
    openInApp: 'Open this page inside the Tauri desktop app.',
  },
  ru: {
    appTitle: 'Мост ввода Bluetooth · ПК',
    brand: 'Мост ввода Bluetooth',
    tagline: 'Вводите с телефона — печатайте на этом ПК',
    stateStarting: 'Запуск сервиса…',
    stateServiceDown: 'Сервис не запущен',
    stateConnected: 'Телефон подключён',
    stateWaiting: 'Ожидание телефона',
    stateRegistrationOff: 'Регистрация сервиса выключена',
    received: 'Получено',
    deviceLabel: 'Текущее устройство',
    notConnected: 'Не подключено',
    hintConnecting: 'Подключение к фоновому сервису.',
    hintServiceDown: 'Фоновый сервис не запущен. Скоро будет повторная попытка; можно запустить службу вручную.',
    hintConnected: 'Поставьте курсор в нужное поле на ПК и используйте клавиатуру телефона.',
    hintWaiting: 'Сервис опубликован. Найдите и подключитесь к этому ПК на Android.',
    hintRegistrationOff: 'Включите «Регистрация сервиса», чтобы телефон видел этот ПК.',
    registrationTitle: 'Регистрация сервиса',
    registrationNoteOn: 'Сервис RFCOMM опубликован в BlueZ',
    registrationNoteOnLinux: 'Сервис RFCOMM опубликован в BlueZ',
    registrationNoteOnWindows: 'Запись SDP зарегистрирована в службе Bluetooth Windows',
    registrationNoteOff: 'Сервис выключен; новые подключения отклоняются',
    autostartTitle: 'Автозапуск',
    autostartNote: 'Запускать фоновый сервис после входа',
    guideTitle: 'Руководство и описание',
    guideIntro: 'После сопряжения телефона с этим ПК «Мост ввода Bluetooth» передаёт введённое по Bluetooth и вводит в активное поле.',
    guideIntroWindows: 'После сопряжения телефона с этим ПК «Мост ввода Bluetooth» передаёт введённое по Bluetooth и вводит в активное поле имитацией клавиш; буфер обмена не используется, текущее выделение заменяется.',
    guideStep1: 'Откройте приложение на телефоне и подключитесь к этому ПК.',
    guideStep2: 'Поставьте курсор ПК туда, где нужно печатать.',
    guideStep3: 'Печатайте на телефоне — ПК введёт текст автоматически.',
    pairingButton: 'Разрешить сопряжение на 3 минуты',
    pairingHint: 'Для первого сопряжения. Если система откажет, включите в настройках Bluetooth GNOME.',
    pairingHintLinux: 'Для первого сопряжения. Если система откажет, включите в настройках Bluetooth GNOME.',
    pairingHintWindows: 'Для первого сопряжения. Если система откажет, добавьте устройство в «Параметры → Bluetooth и устройства» Windows.',
    pairingOk: 'Включено; выключится через 3 минуты. Найдите этот ПК на Android.',
    pairingFailed: 'Не удалось включить: ',
    testTitle: 'Тестовое поле',
    testIntro: 'Печатайте здесь, чтобы проверить клавиатуру и ввод. Текст остаётся в этом окне и никуда не отправляется.',
    testPlaceholder: 'Печатайте здесь…',
    footerMethod: 'Способ ввода',
    footerPrivacy: 'Полученный текст не сохраняется · Сервис и интерфейс разделены',
    toggleFailed: 'Не удалось изменить настройку: ',
    openInApp: 'Откройте эту страницу в приложении Tauri.',
  },
  ja: {
    appTitle: 'Bluetooth入力ブリッジ · PC',
    brand: 'Bluetooth入力ブリッジ',
    tagline: 'スマホの入力でこのPCに文字を入力',
    stateStarting: 'サービスを起動中…',
    stateServiceDown: 'サービスが動作していません',
    stateConnected: 'スマホ接続済み',
    stateWaiting: 'スマホの接続を待っています',
    stateRegistrationOff: 'サービス登録がオフです',
    received: '受信済み',
    deviceLabel: '現在のデバイス',
    notConnected: '未接続',
    hintConnecting: 'バックグラウンドサービスに接続しています。',
    hintServiceDown: 'バックグラウンドサービスが起動していません。まもなく再試行します。手動で起動もできます。',
    hintConnected: 'PC側の入力欄にカーソルを置き、スマホのキーボードで入力してください。',
    hintWaiting: 'サービスを公開しました。Androidから本機を検索して接続してください。',
    hintRegistrationOff: '「サービス登録」をオンにすると、スマホから本機を検出できます。',
    registrationTitle: 'サービス登録',
    registrationNoteOn: 'RFCOMMサービスをBlueZに公開済み',
    registrationNoteOnLinux: 'RFCOMMサービスをBlueZに公開済み',
    registrationNoteOnWindows: 'Windows BluetoothサービスのSDPレコードに登録済み',
    registrationNoteOff: 'サービス停止中。新しい接続は拒否されます',
    autostartTitle: 'ログイン時に起動',
    autostartNote: 'ログイン後にヘッドレスサービスを自動起動',
    guideTitle: '使い方と概要',
    guideIntro: 'スマホとこのPCをペアリングすると、入力内容がBluetoothでPCに送られ、フォーカス中の欄に入力されます。',
    guideIntroWindows: 'スマホとこのPCをペアリングすると、入力内容がBluetoothでPCに送られ、フォーカス中の欄にキー入力をエミュレートして入力されます。クリップボードは使わず、選択中の範囲は置き換わります。',
    guideStep1: 'スマホでアプリを開き、本機に接続します。',
    guideStep2: 'PCのカーソルを入力したい位置に置きます。',
    guideStep3: 'スマホで入力すると、PC側に自動で入力されます。',
    pairingButton: '3分間ペアリングを許可',
    pairingHint: '初回のペアリング用です。システムに拒否された場合はGNOMEのBluetooth設定で有効にしてください。',
    pairingHintLinux: '初回のペアリング用です。システムに拒否された場合はGNOMEのBluetooth設定で有効にしてください。',
    pairingHintWindows: '初回のペアリング用です。システムに拒否された場合はWindowsの「設定 → Bluetooth とデバイス」から追加してください。',
    pairingOk: '有効にしました。3分後に自動で解除されます。今すぐAndroidから本機を検索してください。',
    pairingFailed: '有効にできませんでした：',
    testTitle: 'テスト入力欄',
    testIntro: 'ここで入力してキーボードとIMEを確認できます。内容はこのウィンドウ内に留まり、送信も貼り付けもされません。',
    testPlaceholder: 'ここで入力してみてください…',
    footerMethod: '入力方式',
    footerPrivacy: '受信テキストは保存しません · サービスと画面は分離',
    toggleFailed: '設定を変更できませんでした：',
    openInApp: 'この画面はTauriデスクトップアプリで開いてください。',
  },
  ko: {
    appTitle: '블루투스 입력 브리지 · PC',
    brand: '블루투스 입력 브리지',
    tagline: '휴대폰 입력으로 이 PC에 입력',
    stateStarting: '서비스 시작 중…',
    stateServiceDown: '서비스가 실행되지 않음',
    stateConnected: '휴대폰 연결됨',
    stateWaiting: '휴대폰 연결 대기 중',
    stateRegistrationOff: '서비스 등록이 꺼져 있음',
    received: '수신',
    deviceLabel: '현재 기기',
    notConnected: '연결 안 됨',
    hintConnecting: '백그라운드 서비스에 연결하는 중입니다.',
    hintServiceDown: '백그라운드 서비스가 실행되지 않았습니다. 곧 다시 시도하며, 직접 시작할 수도 있습니다.',
    hintConnected: 'PC에서 입력할 곳에 커서를 두고 휴대폰 키보드로 입력하세요.',
    hintWaiting: '서비스를 게시했습니다. Android에서 이 PC를 검색해 연결하세요.',
    hintRegistrationOff: '「서비스 등록」을 켜야 휴대폰이 이 PC를 찾을 수 있습니다.',
    registrationTitle: '서비스 등록',
    registrationNoteOn: 'RFCOMM 서비스를 BlueZ에 게시함',
    registrationNoteOnLinux: 'RFCOMM 서비스를 BlueZ에 게시함',
    registrationNoteOnWindows: 'Windows Bluetooth 서비스의 SDP 레코드에 등록됨',
    registrationNoteOff: '서비스 꺼짐. 새 연결이 거부됩니다',
    autostartTitle: '로그인 시 시작',
    autostartNote: '로그인 후 헤드리스 서비스를 자동 실행',
    guideTitle: '사용 안내',
    guideIntro: '휴대폰과 이 PC를 페어링하면 입력한 내용이 블루투스로 전송되어 현재 커서 위치에 입력됩니다.',
    guideIntroWindows: '휴대폰과 이 PC를 페어링하면 입력한 내용이 블루투스로 전송되어 현재 커서 위치에 키 입력을 모방해 입력됩니다. 클립보드는 쓰지 않으며 선택된 범위는 대체됩니다.',
    guideStep1: '휴대폰에서 앱을 열고 이 PC에 연결하세요.',
    guideStep2: 'PC 커서를 입력할 위치에 두세요.',
    guideStep3: '휴대폰으로 입력하면 PC에 자동으로 입력됩니다.',
    pairingButton: '3분 동안 페어링 허용',
    pairingHint: '처음 페어링할 때 사용하세요. 시스템이 거부하면 GNOME 블루투스 설정에서 켜세요.',
    pairingHintLinux: '처음 페어링할 때 사용하세요. 시스템이 거부하면 GNOME 블루투스 설정에서 켜세요.',
    pairingHintWindows: '처음 페어링할 때 사용하세요. 시스템이 거부하면 Windows「설정 → Bluetooth 및 장치」에서 추가하세요.',
    pairingOk: '켜졌습니다. 3분 후 자동으로 꺼집니다. 지금 Android에서 이 PC를 검색하세요.',
    pairingFailed: '켜지 못했습니다: ',
    testTitle: '테스트 입력란',
    testIntro: '여기에 입력해 키보드와 입력기를 확인하세요. 내용은 이 창에만 남고 전송되거나 붙여넣어지지 않습니다.',
    testPlaceholder: '여기에 입력해 보세요…',
    footerMethod: '입력 방식',
    footerPrivacy: '받은 텍스트는 저장하지 않음 · 서비스와 화면 분리',
    toggleFailed: '설정을 변경하지 못했습니다: ',
    openInApp: '이 화면은 Tauri 데스크톱 앱에서 열어야 합니다.',
  },
};

let t = I18N.en;
let lang = 'en';
let lastStatus = null;

function applyStatic() {
  document.documentElement.lang = lang === 'zh' ? 'zh-CN' : lang;
  document.title = t.appTitle;
  document.querySelectorAll('[data-i18n]').forEach(node => {
    const text = t[node.dataset.i18n];
    if (text) node.textContent = text;
  });
  document.querySelectorAll('[data-i18n-placeholder]').forEach(node => {
    const text = t[node.dataset.i18nPlaceholder];
    if (text) node.placeholder = text;
  });
  applyPlatform();
}

// 平台相关的文案：标题栏徽标、服务注册去向、配对失败时的提示，以及 Windows 上
// “不是粘贴而是模拟按键”这类差异。标签与文案键都来自后端，这里只做查表。
function applyPlatform() {
  const badge = byId('platform');
  if (badge) {
    badge.textContent = platform.label || '';
    badge.hidden = !platform.label;
    badge.title = platform.os === 'unsupported' ? '当前平台尚未实现蓝牙后端' : '';
  }
  const note = byId('registration-note');
  if (note) {
    const key = platform.registrationNoteKey;
    note.dataset.i18n = key;
    if (t[key]) note.textContent = t[key];
  }
  const hint = byId('pairing-result');
  if (hint) hint.dataset.i18n = platform.pairingHintKey;
  const intro = document.querySelector('[data-i18n="guideIntro"]');
  if (intro) {
    // Windows 上后端用 SendInput 模拟按键而不是剪贴板，指引里要讲清楚。
    const key = platform.os === 'windows' ? 'guideIntroWindows' : 'guideIntro';
    intro.dataset.i18n = key;
    if (t[key]) intro.textContent = t[key];
  }
}

function render(status) {
  lastStatus = status;
  const connected = Boolean(status.connectedDevice);
  const listening = Boolean(status.listening);
  const running = Boolean(status.serviceRunning);

  const dot = byId('dot');
  dot.className = 'dot ' + (connected ? 'live' : status.lastError ? 'error' : listening ? 'live' : '');

  byId('state').textContent = !running
    ? t.stateServiceDown
    : connected
      ? t.stateConnected
      : listening
        ? t.stateWaiting
        : t.stateRegistrationOff;

  byId('hint').textContent = !running
    ? t.hintServiceDown
    : connected
      ? t.hintConnected
      : listening
        ? t.hintWaiting
        : t.hintRegistrationOff;

  byId('count').textContent = String(status.receivedCount ?? 0);
  byId('device').textContent = status.connectedDevice || t.notConnected;
  const address = byId('address');
  address.textContent = status.connectedAddress || '';
  address.hidden = !status.connectedAddress;
  byId('method').textContent = status.injectionMethod;

  const registration = byId('registration');
  registration.checked = Boolean(status.registrationEnabled);
  registration.disabled = !running;
  byId('registration-note').textContent = listening
    ? t.registrationNoteOn
    : t.registrationNoteOff;

  const autostart = byId('autostart');
  autostart.checked = Boolean(status.autostartEnabled);
  autostart.disabled = !running;

  const error = byId('error');
  if (status.lastError) {
    error.hidden = false;
    error.textContent = status.lastError;
  } else {
    error.hidden = true;
    error.textContent = '';
  }
}

function showError(message) {
  const error = byId('error');
  error.hidden = false;
  error.textContent = message;
}

async function invoke(command, args) {
  return tauri.core.invoke(command, args);
}

function attachToggle(input, command) {
  input.addEventListener('change', async event => {
    const enabled = event.target.checked;
    event.target.disabled = true;
    try {
      render(await invoke(command, { enabled }));
    } catch (error) {
      showError(t.toggleFailed + String(error));
      render(lastStatus);
    }
  });
}

async function boot() {
  if (!tauri?.core?.invoke) {
    applyStatic();
    showError(t.openInApp);
    return;
  }
  try {
    const [detectedLocale, detectedPlatform] = await Promise.all([
      invoke('get_locale'),
      invoke('get_platform'),
    ]);
    if (I18N[detectedLocale]) {
      lang = detectedLocale;
      t = I18N[detectedLocale];
    }
    if (detectedPlatform && detectedPlatform.os) platform = detectedPlatform;
  } catch (error) {
    console.warn('locale/platform detection failed', error);
  }
  applyStatic();

  invoke('get_status').then(render).catch(error => showError(String(error)));
  tauri.event.listen('bridge-status', event => render(event.payload));
  attachToggle(byId('registration'), 'set_registration');
  attachToggle(byId('autostart'), 'set_autostart');

  byId('pairing').addEventListener('click', async () => {
    const button = byId('pairing');
    button.disabled = true;
    try {
      await invoke('enable_pairing');
      byId('pairing-result').textContent = t.pairingOk;
    } catch (error) {
      byId('pairing-result').textContent = t.pairingFailed + String(error);
    } finally {
      button.disabled = false;
    }
  });
}

boot();
