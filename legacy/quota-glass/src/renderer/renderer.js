/* Quota Glass renderer: no credentials, network access, or Node.js required. */
(() => {
  'use strict';

  const api = window.quota;
  const $ = (id) => document.getElementById(id);
  const initialState = {
    status: 'setup', message: '', lastRefresh: null, buckets: [],
    settings: { refreshSeconds: 60, alertThreshold: 20, alertsEnabled: false },
    binaryLabel: '自動尋找', loginPending: false,
  };
  let state = initialState;
  let busyAction = null;
  const pendingActions = new Set();
  let settingsDirty = false;
  let unsubscribe = null;
  let historySignature = '';
  const historyDrafts = new Map();
  const countdowns = [];
  const rateDisplays = [];
  const statusLabels = {
    setup: '尚未連線', connecting: '連線中', live: '已連線',
    offline: '離線', 'auth-required': '需要登入', demo: '示範數據',
  };

  function element(tag, className, content) {
    const node = document.createElement(tag);
    if (className) node.className = className;
    if (content !== undefined) node.textContent = String(content);
    return node;
  }

  function icon(type) {
    const svg = document.createElementNS('http://www.w3.org/2000/svg', 'svg');
    svg.setAttribute('viewBox', '0 0 20 20');
    svg.setAttribute('fill', 'none');
    svg.setAttribute('aria-hidden', 'true');
    const path = document.createElementNS('http://www.w3.org/2000/svg', 'path');
    path.setAttribute('d', type === 'credit'
      ? 'M10 2.5 16.5 6v8L10 17.5 3.5 14V6L10 2.5Zm0 0V10m0 0 6.5-4M10 10l-6.5-4M10 10v7.5'
      : 'm4 10 4 4 8-8');
    path.setAttribute('stroke', 'currentColor');
    path.setAttribute('stroke-width', '1.5');
    path.setAttribute('stroke-linecap', 'round');
    path.setAttribute('stroke-linejoin', 'round');
    svg.append(path);
    return svg;
  }

  function finite(value) {
    return typeof value === 'number' && Number.isFinite(value);
  }

  function remainingValue(value) {
    return finite(value) ? Math.max(0, Math.min(100, value)) : null;
  }

  function level(value) {
    if (value === null) return 'unknown';
    if (value <= 5) return 'critical';
    return value < state.settings.alertThreshold ? 'low' : 'healthy';
  }

  function formatPercent(value) {
    if (value === null) return '—';
    return String(value);
  }

  function durationLabel(window) {
    const minutes = window.durationMins;
    if (finite(minutes) && minutes > 0) {
      if (minutes % 10080 === 0) return `${minutes / 10080} 週時段`;
      if (minutes % 1440 === 0) return `${minutes / 1440} 天時段`;
      if (minutes % 60 === 0) return `${minutes / 60} 小時時段`;
      return `${minutes} 分鐘時段`;
    }
    return window.kind === 'secondary' ? '次要時段' : '主要時段';
  }

  function countdown(timestamp) {
    if (!finite(timestamp)) return '未回報重設時間';
    // Codex resetsAt is Unix seconds; lastRefresh and history timestamps are milliseconds.
    const seconds = Math.ceil((timestamp * 1000 - Date.now()) / 1000);
    if (seconds <= 0) return '已到重設時間 · 等待更新';
    const days = Math.floor(seconds / 86400);
    const hours = Math.floor((seconds % 86400) / 3600);
    const minutes = Math.floor((seconds % 3600) / 60);
    if (days > 0) return `${days} 天 ${hours} 小時後重設`;
    if (hours > 0) return `${hours} 小時 ${minutes} 分鐘後重設`;
    if (minutes > 0) return `${minutes} 分 ${seconds % 60} 秒後重設`;
    return `${seconds} 秒後重設`;
  }

  function etaLabel(minutes) {
    if (!finite(minutes) || minutes < 0) return null;
    if (minutes < 1) return '不足 1 分鐘';
    if (minutes < 60) return `約 ${Math.round(minutes)} 分鐘`;
    if (minutes < 1440) return `約 ${Math.floor(minutes / 60)} 小時 ${Math.round(minutes % 60)} 分鐘`;
    return `約 ${Math.floor(minutes / 1440)} 天 ${Math.round((minutes % 1440) / 60)} 小時`;
  }

  function createRate(rate, isCredits = false) {
    const box = element('div', isCredits ? 'pace credits-pace' : 'pace');
    const staleMessage = element('p', 'pace-unavailable', '資料已過期，暫停速率與耗盡時間估計');
    staleMessage.hidden = true;
    box.append(staleMessage);
    rateDisplays.push({ box, staleMessage });
    if (!rate || !finite(rate.perMinute) || rate.perMinute < 0) {
      box.append(element('p', 'pace-unavailable', '速度待觀察 · 至少需要 1 分鐘'));
      return box;
    }
    const row = element('p', 'pace-row');
    row.append(element('span', '', '消耗速度'));
    const amount = rate.perMinute === 0 ? '0' : rate.perMinute < 0.01 ? '<0.01' : new Intl.NumberFormat('zh-Hant', { maximumFractionDigits: 2 }).format(rate.perMinute);
    row.append(element('strong', '', `${amount} ${isCredits ? '點' : '百分點'}/分鐘`));
    box.append(row);
    const eta = rate.perMinute > 0 ? etaLabel(rate.etaMinutes) : null;
    if (eta) {
      const estimate = element('p', 'pace-eta');
      estimate.append(element('span', '', '按此速度，'), element('strong', '', eta), element('span', '', '後用盡'));
      box.append(estimate);
    } else {
      box.append(element('p', 'pace-eta', rate.perMinute === 0 ? '尚未觀察到淨消耗' : '暫無用盡時間估算'));
    }
    if (finite(rate.observedMinutes)) box.title = `最近 ${new Intl.NumberFormat('zh-Hant', { maximumFractionDigits: 1 }).format(rate.observedMinutes)} 分鐘的帳戶整體平均速度（最多 15 分鐘）`;
    return box;
  }

  function createWindow(window, bucketName) {
    const value = remainingValue(window.remaining);
    const wrapper = element('div', 'quota-window');
    wrapper.dataset.level = level(value);
    const label = durationLabel(window);
    const heading = element('h3', 'window-label', label);
    const amount = element('p', 'remaining-value', formatPercent(value));
    if (value !== null) amount.append(element('small', '', '%'));
    amount.setAttribute('aria-label', value === null ? '剩餘額度不明' : `剩餘 ${formatPercent(value)}%`);
    const progress = element('progress');
    progress.max = 100;
    progress.value = value === null ? 0 : value;
    progress.setAttribute('aria-label', `${bucketName}，${label}，剩餘額度`);
    progress.setAttribute('aria-valuetext', value === null ? '未提供' : `剩餘 ${formatPercent(value)}%`);
    const reset = element('p', 'reset-time', countdown(window.resetsAt));
    if (finite(window.resetsAt)) reset.title = `重設時間：${new Date(window.resetsAt * 1000).toLocaleString()}`;
    countdowns.push({ node: reset, timestamp: window.resetsAt });
    wrapper.append(heading, amount, progress, reset, createRate(window.rate));
    return wrapper;
  }

  function createBucket(bucket, index) {
    const name = typeof bucket.name === 'string' && bucket.name.trim() ? bucket.name : `額度分組 ${index + 1}`;
    const windows = Array.isArray(bucket.windows) ? bucket.windows.filter((item) => item && typeof item === 'object') : [];
    const card = element('article', 'bucket-card');
    const header = element('div', 'bucket-header');
    const titleGroup = element('div');
    const title = element('h3', 'bucket-name', name);
    title.id = `bucket-title-${index}`;
    card.setAttribute('aria-labelledby', title.id);
    titleGroup.append(title, element('span', 'bucket-type', '伺服器回報的額度分組'));
    const indicator = element('span', 'bucket-indicator');
    const reported = windows.map((window) => remainingValue(window.remaining)).filter((value) => value !== null);
    const lowest = reported.length ? Math.min(...reported) : null;
    indicator.dataset.level = level(lowest);
    indicator.title = lowest === null ? '額度未提供' : lowest <= 5 ? '額度即將用盡' : lowest < state.settings.alertThreshold ? '額度偏低' : '仍有可用額度';
    if (lowest !== null && lowest >= state.settings.alertThreshold) indicator.append(icon('check'));
    else indicator.textContent = lowest === null ? '—' : '!';
    header.append(titleGroup, indicator);
    card.append(header);

    if (windows.length) {
      const grid = element('div', 'windows');
      grid.dataset.count = String(windows.length);
      for (const window of windows) grid.append(createWindow(window, name));
      card.append(grid);
    } else {
      card.append(element('p', 'window-unknown', '尚未回報額度時段。'));
    }

    const creditRow = element('div', 'credits-row');
    creditRow.append(icon('credit'), element('span', '', '點數'));
    const credits = bucket.credits;
    let creditText = '未回報';
    let isUnknown = true;
    if (credits && typeof credits === 'object') {
      if (credits.unlimited) { creditText = '無上限'; isUnknown = false; }
      else if (credits.balance !== null && credits.balance !== undefined && String(credits.balance).trim()) {
        creditText = String(credits.balance); isUnknown = false;
      } else if (credits.hasCredits) { creditText = '可使用 · 餘額未回報'; }
      else { creditText = '沒有可用點數'; }
    }
    const creditValue = element('span', `credits-value${isUnknown ? ' is-muted' : ''}`, creditText);
    creditRow.append(creditValue);
    card.append(creditRow);
    if (credits && !credits.unlimited && credits.hasCredits) card.append(createRate(credits.rate, true));
    return card;
  }

  function renderHistory() {
    const history = state.history && typeof state.history === 'object' ? state.history : {};
    const events = Array.isArray(history.events) ? history.events : [];
    const recent = events.filter((item) => item && typeof item === 'object').slice(-8).reverse();
    $('history-panel').hidden = !['live', 'offline', 'demo'].includes(state.status) && recent.length === 0;
    $('history-count').textContent = events.length ? `${events.length} 次` : '';
    $('history-empty').hidden = recent.length > 0;
    $('history-error').hidden = !state.historyError;
    $('history-error').textContent = typeof state.historyError === 'string' ? state.historyError : '';
    $('export-history').disabled = state.status === 'demo' || busyAction !== null;
    $('clear-history').disabled = state.status === 'demo' || busyAction !== null;
    $('history-description-text').textContent = state.status === 'demo'
      ? '示範模式不會記錄或修改真實帳戶的歷史。'
      : '記錄額度上升的時間，可自行標記原因，方便比較優惠券與全域重設。';
    const signature = JSON.stringify(recent);
    if (signature === historySignature) return;
    historySignature = signature;
    const fragment = document.createDocumentFragment();
    for (const event of recent) {
      const item = element('article', 'history-event');
      const heading = element('div', 'history-event-heading');
      const windowName = event.window === 'credits' ? '點數' : event.window === 'secondary' ? '次要時段' : '主要時段';
      heading.append(element('strong', '', `${event.bucketName || event.bucket || '額度分組'} · ${windowName}`));
      const stamp = finite(event.timestamp) ? new Date(event.timestamp) : null;
      heading.append(element('time', '', stamp ? stamp.toLocaleString('zh-Hant', { month: 'short', day: 'numeric', hour: '2-digit', minute: '2-digit' }) : '時間未提供'));
      item.append(heading);
      const before = event.before || {};
      const after = event.after || {};
      const from = event.window === 'credits' ? before.balance : before.remaining;
      const to = event.window === 'credits' ? after.balance : after.remaining;
      item.append(element('p', 'history-change', `${finite(from) ? from : '—'}${event.window === 'credits' ? ' 點' : '%'} → ${finite(to) ? to : '—'}${event.window === 'credits' ? ' 點' : '%'}${event.window === 'credits' ? '' : ' 剩餘'}`));
      if (event.gap || event.clockRollback) item.append(element('p', 'history-gap', event.clockRollback ? '偵測到系統時間變動，無法可靠比較間隔。' : '觀察期間有資料缺口，實際重設時間未能確定。'));
      const form = element('form', 'history-tag-form');
      const draft = historyDrafts.get(String(event.id)) || { source: event.source || 'unknown', note: event.note || '' };
      const source = element('select', 'history-source');
      source.setAttribute('aria-label', `${event.bucketName || '額度分組'} 重設原因`);
      for (const [value, label] of [['unknown', '原因未確定'], ['coupon', '優惠券'], ['global', '全域重設'], ['scheduled', '排程重設']]) {
        const option = element('option', '', label);
        option.value = value;
        source.append(option);
      }
      source.value = ['unknown', 'coupon', 'global', 'scheduled'].includes(draft.source) ? draft.source : 'unknown';
      const note = element('input', 'history-note');
      note.type = 'text';
      note.maxLength = 180;
      note.placeholder = '模型／工作量備註（選填）';
      note.setAttribute('aria-label', '模型或工作量備註');
      note.value = draft.note;
      const save = element('button', 'button button-small', '儲存');
      save.type = 'submit';
      save.disabled = state.status === 'demo';
      source.disabled = note.disabled = state.status === 'demo';
      const feedback = element('span', 'history-feedback');
      feedback.setAttribute('role', 'status');
      const remember = () => { historyDrafts.set(String(event.id), { source: source.value, note: note.value }); feedback.textContent = ''; };
      source.addEventListener('change', remember);
      note.addEventListener('input', remember);
      form.addEventListener('submit', async (submission) => {
        submission.preventDefault();
        save.disabled = true;
        const saved = await run('tagReset', event.id, { source: source.value, note: note.value });
        if (saved) { historyDrafts.delete(String(event.id)); feedback.textContent = '已儲存'; }
        save.disabled = false;
      });
      form.append(source, note, save, feedback);
      item.append(form);
      fragment.append(item);
    }
    $('history-events').replaceChildren(fragment);
  }

  function applySettingsFields() {
    if (settingsDirty) return;
    for (const [id, value] of [['refresh-seconds', state.settings.refreshSeconds], ['alert-threshold', state.settings.alertThreshold]]) {
      const select = $(id);
      if (![...select.options].some((option) => option.value === String(value))) {
        const option = element('option', '', id === 'refresh-seconds' ? `${value} 秒` : `剩餘 ${value}%`);
        option.value = String(value);
        select.append(option);
      }
      select.value = String(value);
    }
    $('alerts-enabled').checked = state.settings.alertsEnabled;
  }

  function updateClock() {
    for (const entry of countdowns) entry.node.textContent = countdown(entry.timestamp);
    const updated = $('updated-label');
    const dot = $('updated-dot');
    const hasTime = finite(state.lastRefresh);
    const age = hasTime ? Math.max(0, Date.now() - state.lastRefresh) : null;
    const stale = state.status === 'offline' || (hasTime && age > Math.max(state.settings.refreshSeconds * 2500, 120000));
    for (const { box, staleMessage } of rateDisplays) {
      for (const child of box.children) child.hidden = child === staleMessage ? !stale : stale;
    }
    dot.dataset.fresh = String(hasTime && !stale && state.status === 'live');
    dot.dataset.stale = String(stale);
    if (state.status === 'demo') updated.textContent = '示範預覽 · 並非帳戶數據';
    else if (state.status === 'connecting' || busyAction === 'refresh') updated.textContent = '正在檢查額度…';
    else if (!hasTime) updated.textContent = state.status === 'offline' ? '離線 · 無已儲存額度' : '等待連線';
    else {
      const seconds = Math.floor(age / 1000);
      const relative = seconds < 10 ? '剛剛' : seconds < 60 ? `${seconds} 秒前` : seconds < 3600 ? `${Math.floor(seconds / 60)} 分鐘前` : seconds < 86400 ? `${Math.floor(seconds / 3600)} 小時前` : `${Math.floor(seconds / 86400)} 天前`;
      updated.textContent = `${stale ? '資料已過時 · ' : ''}${relative}更新`;
    }
    updated.title = hasTime ? `上次成功更新：${new Date(state.lastRefresh).toLocaleString()}` : '';
  }

  function updateControls() {
    const locked = Boolean(busyAction) || state.status === 'connecting';
    const loginPending = Boolean(state.loginPending);
    for (const id of ['connect-button', 'login-button', 'choose-binary', 'choose-binary-settings', 'demo-toggle']) $(id).disabled = locked || loginPending;
    $('refresh-button').disabled = locked || loginPending || state.status === 'setup' || state.status === 'auth-required';
    $('refresh-button').classList.toggle('is-busy', busyAction === 'refresh' || state.status === 'connecting');
    $('save-settings').disabled = Boolean(busyAction);
    $('export-history').disabled = Boolean(busyAction) || state.status === 'demo';
    $('clear-history').disabled = Boolean(busyAction) || state.status === 'demo';
    $('cancel-login').hidden = !loginPending;
    $('cancel-login').disabled = busyAction === 'cancelLogin';
    $('connect-label').textContent = state.status === 'connecting' ? '正在連線…' : state.status === 'offline' ? '重新連接 Codex' : '連接已登入的 Codex';
    $('login-button').textContent = loginPending ? '正在登入…' : '登入 Codex';
    if (!api) {
      for (const id of ['connect-button', 'login-button', 'choose-binary', 'choose-binary-settings', 'demo-toggle', 'refresh-button', 'save-settings', 'hide-button']) $(id).disabled = true;
    }
  }

  function render(nextState) {
    if (!nextState || typeof nextState !== 'object' || !Object.hasOwn(statusLabels, nextState.status)) return;
    state = {
      ...initialState, ...nextState,
      settings: { ...initialState.settings, ...nextState.settings },
      buckets: Array.isArray(nextState.buckets) ? nextState.buckets.filter((item) => item && typeof item === 'object') : [],
    };
    $('connection-status').dataset.status = state.status;
    $('status-label').textContent = statusLabels[state.status];
    const hasBuckets = state.buckets.length > 0;
    const showSetup = state.status === 'setup' || state.status === 'auth-required' || state.loginPending || (state.status === 'connecting' && !hasBuckets) || (state.status === 'offline' && !hasBuckets);
    $('setup-panel').hidden = !showSetup;
    $('quota-panel').hidden = !hasBuckets && showSetup;
    $('empty-state').hidden = hasBuckets || showSetup;
    $('setup-heading').textContent = state.loginPending ? '請在瀏覽器完成登入。' : state.status === 'auth-required' ? '請重新登入 Codex。' : state.status === 'offline' ? '重新連上 Codex。' : '連接你的 Codex。';
    $('setup-description').textContent = state.loginPending ? '請在瀏覽器完成 Codex 登入，亦可隨時取消再試。' : state.status === 'auth-required' ? '登入 Codex 後，再連接以讀取帳戶的可用額度。' : state.status === 'offline' ? '請檢查網路與 Codex 執行檔，然後再試。' : '讀取這部電腦上已登入的 Codex 額度，無需提供 API 金鑰。';
    $('binary-label').textContent = state.binaryLabel || '自動尋找';
    $('binary-label').title = state.binaryLabel || '自動尋找';
    $('demo-toggle').checked = state.status === 'demo';

    let message = typeof state.message === 'string' ? state.message : '';
    let tone = '';
    if (state.status === 'demo') { message = '目前是示範預覽。下方數字只是例子，並非你的帳戶用量。'; tone = 'demo'; }
    else if (state.status === 'offline') { message = message || '未能連接 Codex。畫面保留上次成功更新的額度，資料可能已過時。'; tone = 'warning'; }
    else if (state.status === 'auth-required') { message = message || '請先登入 Codex 帳戶，才能讀取額度。'; tone = 'warning'; }
    $('notice').hidden = !message;
    $('notice').dataset.tone = tone;
    $('notice-text').textContent = message;

    countdowns.length = 0;
    rateDisplays.length = 0;
    const fragment = document.createDocumentFragment();
    state.buckets.forEach((bucket, index) => fragment.append(createBucket(bucket, index)));
    $('buckets').replaceChildren(fragment);
    renderHistory();
    applySettingsFields();
    updateControls();
    updateClock();
  }

  async function run(action, ...arguments_) {
    if (!api || typeof api[action] !== 'function') {
      showError('請透過 Quota Glass 桌面應用程式連接 Codex。');
      return false;
    }
    if (busyAction && action !== 'cancelLogin') return false;
    const pending = { action };
    pendingActions.add(pending);
    busyAction = action;
    $('error-notice').hidden = true;
    updateControls();
    updateClock();
    try {
      const result = await api[action](...arguments_);
      if (result && typeof result.error === 'string' && result.error) {
        showError(result.error);
        return false;
      }
      if (result && result.saved === false) return false;
      if (result && typeof result === 'object' && Object.hasOwn(statusLabels, result.status)) render(result);
      else if (action !== 'hide') render(await api.getState());
      return true;
    } catch (error) {
      showError(error && typeof error.message === 'string' ? error.message : '發生錯誤，請再試一次。');
      return false;
    } finally {
      pendingActions.delete(pending);
      busyAction = [...pendingActions].at(-1)?.action || null;
      updateControls();
      updateClock();
    }
  }

  function showError(message) {
    $('error-notice').textContent = message;
    $('error-notice').hidden = false;
  }

  function showSettings(open) {
    $('settings-panel').hidden = !open;
    $('settings-button').setAttribute('aria-expanded', String(open));
    $('settings-button').setAttribute('aria-label', open ? '關閉設定' : '開啟設定');
    if (open) $('refresh-seconds').focus();
    else $('settings-button').focus();
  }

  function setTheme(theme) {
    if (theme === 'system') document.documentElement.removeAttribute('data-theme');
    else document.documentElement.dataset.theme = theme;
    $('theme-button').title = `外觀：${{ system: '跟隨系統', dark: '深色', light: '淺色' }[theme]}。按一下切換。`;
    $('theme-button').setAttribute('aria-label', `外觀：${{ system: '跟隨系統', dark: '深色', light: '淺色' }[theme]}。按一下切換。`);
    try { localStorage.setItem('quota-glass-theme', theme); } catch (_) { /* Storage may be disabled. */ }
  }

  let theme = 'system';
  try {
    const saved = localStorage.getItem('quota-glass-theme');
    if (['dark', 'light', 'system'].includes(saved)) theme = saved;
  } catch (_) { /* The system appearance remains available without storage. */ }
  setTheme(theme);
  $('theme-button').addEventListener('click', () => {
    theme = { system: 'dark', dark: 'light', light: 'system' }[theme];
    setTheme(theme);
  });
  $('settings-button').addEventListener('click', () => showSettings($('settings-panel').hidden));
  $('settings-close').addEventListener('click', () => showSettings(false));
  document.addEventListener('keydown', (event) => {
    if (event.key === 'Escape' && !$('settings-panel').hidden) { event.preventDefault(); showSettings(false); }
  });
  $('connect-button').addEventListener('click', () => run('connect'));
  $('login-button').addEventListener('click', () => run('login'));
  $('cancel-login').addEventListener('click', () => run('cancelLogin'));
  $('choose-binary').addEventListener('click', () => run('chooseBinary'));
  $('choose-binary-settings').addEventListener('click', () => run('chooseBinary'));
  $('refresh-button').addEventListener('click', () => run('refresh'));
  $('hide-button').addEventListener('click', () => run('hide'));
  $('export-history').addEventListener('click', () => run('exportHistory'));
  $('clear-history').addEventListener('click', async () => {
    if (await run('clearHistory')) historyDrafts.clear();
  });
  $('demo-toggle').addEventListener('change', async (event) => {
    await run('setDemo', event.target.checked);
    $('demo-toggle').checked = state.status === 'demo';
  });
  $('settings-form').addEventListener('input', () => { settingsDirty = true; $('settings-feedback').textContent = '尚未儲存'; });
  $('settings-form').addEventListener('submit', async (event) => {
    event.preventDefault();
    const settings = {
      refreshSeconds: Number($('refresh-seconds').value),
      alertThreshold: Number($('alert-threshold').value),
      alertsEnabled: $('alerts-enabled').checked,
    };
    if (await run('setSettings', settings)) {
      settingsDirty = false;
      applySettingsFields();
      $('settings-feedback').textContent = '已儲存';
    }
  });

  render(initialState);
  if (api) {
    if (typeof api.onState === 'function') unsubscribe = api.onState(render);
    api.getState().then(render).catch((error) => showError(error.message || '無法載入應用程式狀態。'));
  } else {
    showError('無法使用桌面連線，請透過 Quota Glass 桌面應用程式開啟。');
  }
  const clock = window.setInterval(updateClock, 1000);
  window.addEventListener('beforeunload', () => {
    window.clearInterval(clock);
    if (typeof unsubscribe === 'function') unsubscribe();
  });
})();
