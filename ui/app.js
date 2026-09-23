// ── Tauri IPC Wrapper ──────────────────────────────────────────
async function invoke(cmd, args = {}) {
  if (window.__TAURI__ && window.__TAURI__.core && window.__TAURI__.core.invoke) {
    return await window.__TAURI__.core.invoke(cmd, args);
  }
  if (window.__TAURI_INTERNALS__ && window.__TAURI_INTERNALS__.invoke) {
    return await window.__TAURI_INTERNALS__.invoke(cmd, args);
  }
  console.warn(`[Tauri Mock] Invoke '${cmd}':`, args);
  return null;
}

// ── App State ──────────────────────────────────────────────────
let appState = {
  running: false,
  port: 3456,
  uptime: 0,
  providerId: '',
  providers: [],
  logs: [],
  requestLogs: [],
  requestLogsTotal: 0,
  requestLogsModels: [],
  requestLogsPage: 0,
  requestLogsPageSize: 50,
  logsViewMode: 'table', // 'table' | 'console'
  activeTab: 'overview',
  keyVisible: false,
};

// ── Tab Navigation ─────────────────────────────────────────────
document.querySelectorAll('.tab').forEach(tabBtn => {
  tabBtn.addEventListener('click', () => {
    const tabName = tabBtn.dataset.tab;
    switchTab(tabName);
  });
});

function switchTab(tabName) {
  appState.activeTab = tabName;
  document.querySelectorAll('.tab').forEach(b => {
    b.classList.toggle('active', b.dataset.tab === tabName);
  });
  document.querySelectorAll('.tab-content').forEach(c => {
    c.classList.toggle('active', c.id === `tab-${tabName}`);
  });
  if (tabName === 'logs') {
    if (appState.logsViewMode === 'table') {
      fetchRequestLogs();
    } else {
      fetchLogs();
    }
  } else if (tabName === 'settings') {
    loadSettings();
  }
}

// Expose globally so Tauri backend can navigate tabs and refresh status
window.switchTab = switchTab;
window.refreshStatus = refreshStatus;

if (window.__TAURI__ && window.__TAURI__.event) {
  window.__TAURI__.event.listen('switch-tab', (event) => {
    if (event && event.payload) {
      switchTab(event.payload);
    }
  });
}

// Quick navigation buttons
document.getElementById('btn-goto-logs')?.addEventListener('click', () => switchTab('logs'));
document.getElementById('btn-goto-settings')?.addEventListener('click', () => switchTab('settings'));

// ── Service Controls ───────────────────────────────────────────
const btnToggleService = document.getElementById('btn-toggle-service');
btnToggleService.addEventListener('click', async () => {
  btnToggleService.disabled = true;
  try {
    if (appState.running) {
      await invoke('stop_service');
    } else {
      await invoke('start_service');
    }
    await refreshStatus();
  } catch (err) {
    console.error('Toggle service failed:', err);
    alert('操作失败: ' + err);
  } finally {
    btnToggleService.disabled = false;
  }
});

// ── Status Polling ─────────────────────────────────────────────
async function refreshStatus() {
  try {
    const status = await invoke('get_status');
    if (!status) return;

    appState.running = status.running || status.service_running;
    appState.port = status.port || status.configured_port || 3456;
    appState.providerId = status.provider || '';

    // Update Header
    const dot = document.getElementById('status-dot');
    const text = document.getElementById('status-text');
    dot.className = 'status-dot ' + (appState.running ? 'running' : 'stopped');
    text.textContent = appState.running ? `运行中 · 端口 ${appState.port}` : '已停止';

    btnToggleService.textContent = appState.running ? '停止服务' : '启动服务';
    btnToggleService.className = 'btn btn-small ' + (appState.running ? '' : 'btn-primary');

    // Update Overview Tab
    const cardStatus = document.getElementById('card-status');
    cardStatus.className = 'metric-card ' + (appState.running ? 'running' : 'stopped');
    document.getElementById('metric-status').textContent = appState.running ? '正常运行' : '已停止';
    document.getElementById('metric-port').textContent = appState.port;
    document.getElementById('metric-uptime').textContent = formatUptime(status.uptime_secs || 0);
    document.getElementById('metric-provider').textContent = status.provider || '-';

    document.getElementById('endpoint-messages').textContent = `http://127.0.0.1:${appState.port}/v1/messages`;
    document.getElementById('endpoint-responses').textContent = `http://127.0.0.1:${appState.port}/v1/responses`;
    document.getElementById('endpoint-models').textContent = `http://127.0.0.1:${appState.port}/v1/models`;
    document.getElementById('overview-upstream').textContent = status.upstream_url || '-';
    if (status.version) {
      document.getElementById('app-version').textContent = `v${status.version}`;
    }
    if (status.log_path) {
      document.getElementById('overview-log-path').textContent = status.log_path;
    }
    document.getElementById('overview-lal').textContent = status.launch_at_login ? '已开启' : '未开启';
  } catch (err) {
    console.error('refreshStatus error:', err);
  }
}

function formatUptime(secs) {
  if (secs < 60) return `${secs}秒`;
  const mins = Math.floor(secs / 60);
  if (mins < 60) return `${mins}分 ${secs % 60}秒`;
  const hours = Math.floor(mins / 60);
  return `${hours}时 ${mins % 60}分`;
}

function formatNumber(n) {
  const num = Number(n) || 0;
  if (num < 1000) return String(num);
  if (num < 1000000) return `${(num / 1000).toFixed(num < 10000 ? 1 : 0)}K`;
  return `${(num / 1000000).toFixed(2)}M`;
}

// ── Token Statistics ───────────────────────────────────────────
async function refreshStats() {
  try {
    const s = await invoke('get_stats');
    if (!s) return;
    const set = (id, val) => {
      const el = document.getElementById(id);
      if (el) el.textContent = val;
    };
    set('stat-requests', formatNumber(s.requests_total));
    set('stat-tokens-total', formatNumber(s.tokens_total));
    set('stat-cache-pct', `${s.cache_hit_pct || 0}%`);
    set('stat-requests-failed', formatNumber(s.requests_failed));
    set('stat-tokens-input', formatNumber(s.tokens_input));
    set('stat-tokens-cache-read', formatNumber(s.tokens_cache_read));
    set('stat-tokens-cache-write', formatNumber(s.tokens_cache_write));
    set('stat-tokens-output', formatNumber(s.tokens_output));
  } catch (err) {
    console.error('refreshStats error:', err);
  }
}

// The 缓存命中率 card expands the token detail breakdown below it.
document.getElementById('stat-card-cache')?.addEventListener('click', () => {
  const card = document.getElementById('stat-card-cache');
  const detail = document.getElementById('stat-detail');
  if (!card || !detail) return;
  const open = detail.hasAttribute('hidden');
  if (open) {
    detail.removeAttribute('hidden');
  } else {
    detail.setAttribute('hidden', '');
  }
  card.classList.toggle('active', open);
});

// ── Copy Buttons ───────────────────────────────────────────────
document.querySelectorAll('.btn-copy[data-copy]').forEach(btn => {
  btn.addEventListener('click', async () => {
    const elId = btn.dataset.copy;
    const el = document.getElementById(elId);
    if (el) {
      await navigator.clipboard.writeText(el.textContent.trim());
      const orig = btn.textContent;
      btn.textContent = '已复制!';
      setTimeout(() => { btn.textContent = orig; }, 1500);
    }
  });
});

document.getElementById('btn-open-log-dir')?.addEventListener('click', () => {
  invoke('open_logs_dir');
});

// ── Logs (Real-Time DB Table & Console) ─────────────────────────
const logsContainer = document.getElementById('logs');
const logLevelFilter = document.getElementById('log-level-filter');
const logSearchInput = document.getElementById('log-search-input');
const logAutoscroll = document.getElementById('log-autoscroll');

const tableModelFilter = document.getElementById('table-model-filter');
const tableStatusFilter = document.getElementById('table-status-filter');
const tableStreamedFilter = document.getElementById('table-streamed-filter');
const tableSearchInput = document.getElementById('table-search-input');
const requestLogsTbody = document.getElementById('request-logs-tbody');
const tableLogCount = document.getElementById('table-log-count');
const paginationInfo = document.getElementById('pagination-info');
const btnPrevPage = document.getElementById('btn-prev-page');
const btnNextPage = document.getElementById('btn-next-page');

// View mode toggle
document.getElementById('view-mode-table')?.addEventListener('click', () => {
  setLogsViewMode('table');
});
document.getElementById('view-mode-console')?.addEventListener('click', () => {
  setLogsViewMode('console');
});

function setLogsViewMode(mode) {
  appState.logsViewMode = mode;
  document.getElementById('view-mode-table')?.classList.toggle('active', mode === 'table');
  document.getElementById('view-mode-console')?.classList.toggle('active', mode === 'console');
  
  const tableView = document.getElementById('table-view');
  const consoleView = document.getElementById('console-view');
  const autorefreshLabel = document.getElementById('table-autorefresh-label');
  const btnRefresh = document.getElementById('btn-refresh-table');
  const btnClearDb = document.getElementById('btn-clear-db-logs');

  if (mode === 'table') {
    if (tableView) tableView.style.display = 'flex';
    if (consoleView) consoleView.style.display = 'none';
    if (autorefreshLabel) autorefreshLabel.style.display = 'flex';
    if (btnRefresh) btnRefresh.style.display = 'inline-block';
    if (btnClearDb) btnClearDb.style.display = 'inline-block';
    fetchRequestLogs();
  } else {
    if (tableView) tableView.style.display = 'none';
    if (consoleView) consoleView.style.display = 'flex';
    if (autorefreshLabel) autorefreshLabel.style.display = 'none';
    if (btnRefresh) btnRefresh.style.display = 'none';
    if (btnClearDb) btnClearDb.style.display = 'none';
    fetchLogs();
  }
}

// Table filter listeners
let searchDebounceTimer = null;
tableSearchInput?.addEventListener('input', () => {
  clearTimeout(searchDebounceTimer);
  searchDebounceTimer = setTimeout(() => {
    fetchRequestLogs(true);
  }, 250);
});

tableModelFilter?.addEventListener('change', () => fetchRequestLogs(true));
tableStatusFilter?.addEventListener('change', () => fetchRequestLogs(true));
tableStreamedFilter?.addEventListener('change', () => fetchRequestLogs(true));

document.getElementById('btn-refresh-table')?.addEventListener('click', () => {
  fetchRequestLogs(false);
});

document.getElementById('btn-clear-db-logs')?.addEventListener('click', async () => {
  if (confirm('确定要清空数据库中的所有请求记录吗？此操作不可恢复。')) {
    try {
      await invoke('clear_request_logs');
      fetchRequestLogs(true);
    } catch (e) {
      alert('清空失败: ' + e);
    }
  }
});

btnPrevPage?.addEventListener('click', () => {
  if (appState.requestLogsPage > 0) {
    appState.requestLogsPage--;
    fetchRequestLogs(false);
  }
});

btnNextPage?.addEventListener('click', () => {
  const maxPage = Math.ceil(appState.requestLogsTotal / appState.requestLogsPageSize) - 1;
  if (appState.requestLogsPage < maxPage) {
    appState.requestLogsPage++;
    fetchRequestLogs(false);
  }
});

// Fetch Request Logs from SQLite DB
async function fetchRequestLogs(resetPage = false) {
  if (resetPage) {
    appState.requestLogsPage = 0;
  }

  const modelVal = tableModelFilter?.value || 'all';
  const statusVal = tableStatusFilter?.value || 'all';
  const streamVal = tableStreamedFilter?.value || 'all';
  const searchVal = tableSearchInput?.value.trim() || '';

  const filter = {
    limit: appState.requestLogsPageSize,
    offset: appState.requestLogsPage * appState.requestLogsPageSize,
  };

  if (modelVal && modelVal !== 'all') {
    filter.model = modelVal;
  }
  if (statusVal && statusVal !== 'all') {
    filter.status_group = statusVal;
  }
  if (streamVal === 'stream') {
    filter.streamed = true;
  } else if (streamVal === 'sync') {
    filter.streamed = false;
  }
  if (searchVal) {
    filter.search = searchVal;
  }

  try {
    const res = await invoke('get_request_logs', { filter });
    if (res) {
      appState.requestLogs = res.items || [];
      appState.requestLogsTotal = res.total || 0;
      appState.requestLogsModels = res.models || [];
      updateModelDropdown();
      renderRequestLogs();
    }
  } catch (err) {
    console.error('fetchRequestLogs error:', err);
  }
}

function updateModelDropdown() {
  if (!tableModelFilter) return;
  const currentVal = tableModelFilter.value;
  const models = appState.requestLogsModels || [];

  let html = `<option value="all">全部模型</option>`;
  models.forEach(m => {
    html += `<option value="${escapeHtml(m)}">${escapeHtml(m)}</option>`;
  });
  tableModelFilter.innerHTML = html;

  if (currentVal && Array.from(tableModelFilter.options).some(o => o.value === currentVal)) {
    tableModelFilter.value = currentVal;
  } else {
    tableModelFilter.value = 'all';
  }
}

function renderRequestLogs() {
  if (!requestLogsTbody) return;
  const items = appState.requestLogs || [];

  if (items.length === 0) {
    requestLogsTbody.innerHTML = `<tr><td colspan="8" class="table-empty">未匹配到任何请求记录</td></tr>`;
  } else {
    requestLogsTbody.innerHTML = items.map(item => {
      const isErr = (item.status >= 400 || item.error);
      const rowClass = isErr ? 'row-error' : '';

      let statusBadge = '';
      if (item.status >= 200 && item.status < 300) {
        statusBadge = `<span class="badge-pill status-pill status-2xx">${item.status} OK</span>`;
      } else if (item.status >= 400 && item.status < 500) {
        statusBadge = `<span class="badge-pill status-pill status-4xx">${item.status}</span>`;
      } else {
        statusBadge = `<span class="badge-pill status-pill status-5xx">${item.status || 'ERR'}</span>`;
      }

      const streamBadge = item.streamed
        ? `<span class="badge-pill stream-pill stream-true">流式</span>`
        : `<span class="badge-pill stream-pill stream-false">非流式</span>`;

      const inputFormatted = item.input_tokens ? item.input_tokens.toLocaleString() : '0';
      const outputFormatted = item.output_tokens ? item.output_tokens.toLocaleString() : '0';
      const cacheHint = (item.cache_read_tokens > 0)
        ? `<span class="cell-cache-hint">cache: ${item.cache_read_tokens.toLocaleString()}</span>`
        : '';

      const durationFormatted = formatDuration(item.duration_ms);

      return `<tr class="${rowClass}">
        <td class="cell-time">${escapeHtml(item.created_at)}</td>
        <td>
          <div class="cell-model-box">
            <span class="cell-model" title="${escapeHtml(item.model)}">${escapeHtml(item.model)}</span>
            <span class="cell-route">${escapeHtml(item.route)}</span>
          </div>
        </td>
        <td class="cell-num">
          ${inputFormatted}
          ${cacheHint}
        </td>
        <td class="cell-num">${outputFormatted}</td>
        <td class="cell-duration">${durationFormatted}</td>
        <td style="text-align: center;">${streamBadge}</td>
        <td style="text-align: center;">${statusBadge}</td>
        <td style="text-align: center;">
          <button type="button" class="btn-inspect" data-req-id="${item.id}">详情</button>
        </td>
      </tr>`;
    }).join('');
  }

  // Update pagination
  const total = appState.requestLogsTotal;
  const pageSize = appState.requestLogsPageSize;
  const totalPages = Math.max(1, Math.ceil(total / pageSize));
  const currentPage = appState.requestLogsPage + 1;

  if (tableLogCount) {
    tableLogCount.textContent = `${total} 条记录`;
  }
  if (paginationInfo) {
    paginationInfo.textContent = `第 ${currentPage} / ${totalPages} 页 · 共 ${total} 条`;
  }

  if (btnPrevPage) btnPrevPage.disabled = (currentPage <= 1);
  if (btnNextPage) btnNextPage.disabled = (currentPage >= totalPages);

  // Bind inspect buttons
  document.querySelectorAll('.btn-inspect').forEach(btn => {
    btn.addEventListener('click', (e) => {
      const id = parseInt(e.currentTarget.dataset.reqId, 10);
      showRequestModal(id);
    });
  });
}

function formatDuration(ms) {
  if (!ms || ms <= 0) return '0ms';
  if (ms < 1000) return `${ms}ms`;
  return `${(ms / 1000).toFixed(2)}s`;
}

// Request detail modal
function showRequestModal(id) {
  const item = (appState.requestLogs || []).find(r => r.id === id);
  if (!item) return;

  const modalBody = document.getElementById('modal-req-body');
  const modalOverlay = document.getElementById('request-modal-overlay');
  if (!modalBody || !modalOverlay) return;

  const totalTokens = (item.input_tokens || 0) + (item.output_tokens || 0);

  let statusBadge = '';
  if (item.status >= 200 && item.status < 300) {
    statusBadge = `<span class="badge-pill status-pill status-2xx">${item.status} OK</span>`;
  } else if (item.status >= 400 && item.status < 500) {
    statusBadge = `<span class="badge-pill status-pill status-4xx">${item.status}</span>`;
  } else {
    statusBadge = `<span class="badge-pill status-pill status-5xx">${item.status || 'ERR'}</span>`;
  }

  const streamText = item.streamed ? '是 (Stream)' : '否 (Non-stream)';

  let errorHtml = '';
  if (item.error) {
    errorHtml = `<tr>
      <td class="detail-key">错误信息</td>
      <td class="detail-val"><div class="error-block">${escapeHtml(item.error)}</div></td>
    </tr>`;
  }

  modalBody.innerHTML = `
    <table class="detail-table">
      <tr><td class="detail-key">请求 ID</td><td class="detail-val">#${item.id}</td></tr>
      <tr><td class="detail-key">时间</td><td class="detail-val">${escapeHtml(item.created_at)}</td></tr>
      <tr><td class="detail-key">模型 ID</td><td class="detail-val"><strong>${escapeHtml(item.model)}</strong></td></tr>
      <tr><td class="detail-key">请求路径</td><td class="detail-val">${escapeHtml(item.route)}</td></tr>
      <tr><td class="detail-key">状态</td><td class="detail-val">${statusBadge}</td></tr>
      <tr><td class="detail-key">传输模式</td><td class="detail-val">${streamText}</td></tr>
      <tr><td class="detail-key">响应耗时</td><td class="detail-val">${formatDuration(item.duration_ms)} (${item.duration_ms} ms)</td></tr>
      <tr><td class="detail-key">输入 Tokens</td><td class="detail-val">${item.input_tokens.toLocaleString()}</td></tr>
      <tr><td class="detail-key">缓存读取</td><td class="detail-val">${item.cache_read_tokens.toLocaleString()}</td></tr>
      <tr><td class="detail-key">缓存写入</td><td class="detail-val">${item.cache_write_tokens.toLocaleString()}</td></tr>
      <tr><td class="detail-key">输出 Tokens</td><td class="detail-val">${item.output_tokens.toLocaleString()}</td></tr>
      <tr><td class="detail-key">总计消耗</td><td class="detail-val"><strong>${totalTokens.toLocaleString()}</strong></td></tr>
      ${errorHtml}
    </table>
  `;

  modalOverlay.classList.add('active');
}

function closeRequestModal() {
  document.getElementById('request-modal-overlay')?.classList.remove('active');
}

document.getElementById('btn-close-modal')?.addEventListener('click', closeRequestModal);
document.getElementById('btn-dismiss-modal')?.addEventListener('click', closeRequestModal);
document.getElementById('request-modal-overlay')?.addEventListener('click', (e) => {
  if (e.target.id === 'request-modal-overlay') {
    closeRequestModal();
  }
});

// Console Fallback Logs
logLevelFilter?.addEventListener('change', renderLogs);
logSearchInput?.addEventListener('input', renderLogs);

document.getElementById('btn-clear-logs')?.addEventListener('click', async () => {
  await invoke('clear_logs');
  appState.logs = [];
  renderLogs();
});

document.getElementById('btn-reveal-logs')?.addEventListener('click', () => {
  invoke('open_logs_dir');
});

async function fetchLogs() {
  try {
    const res = await invoke('get_logs');
    if (res && res.entries) {
      appState.logs = res.entries;
      renderLogs();
    }
  } catch (err) {
    console.error('fetchLogs error:', err);
  }
}

function renderLogs() {
  if (!logsContainer || !logLevelFilter || !logSearchInput) return;
  const level = logLevelFilter.value;
  const q = logSearchInput.value.trim().toLowerCase();

  const filtered = appState.logs.filter(entry => {
    if (level !== 'ALL' && entry.level !== level) return false;
    if (q && !entry.message.toLowerCase().includes(q)) return false;
    return true;
  });

  const countEl = document.getElementById('log-count');
  if (countEl) {
    countEl.textContent = `${filtered.length} 条日志`;
  }

  logsContainer.innerHTML = filtered.map(l => {
    return `<div class="log-line ${l.level}">
      <span class="ts">${escapeHtml(l.ts)}</span>
      <span class="lvl">[${escapeHtml(l.level)}]</span>
      <span class="msg">${escapeHtml(l.message)}</span>
    </div>`;
  }).join('');

  if (logAutoscroll && logAutoscroll.checked) {
    logsContainer.scrollTop = logsContainer.scrollHeight;
  }
}

function escapeHtml(s) {
  return String(s)
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;');
}

// ── Settings ───────────────────────────────────────────────────
let isSettingsLoaded = false;

async function loadProviders() {
  try {
    const res = await invoke('get_providers');
    if (res && res.providers) {
      appState.providers = res.providers;
      const select = document.getElementById('setting-provider');
      select.innerHTML = res.providers.map(p => {
        return `<option value="${escapeHtml(p.id)}">${escapeHtml(p.name)}</option>`;
      }).join('') + '<option value="custom">自定义服务商 (Custom)</option>';
    }
  } catch (err) {
    console.error('loadProviders error:', err);
  }
}

async function loadSettings() {
  await loadProviders();
  try {
    const s = await invoke('get_settings');
    if (!s) return;

    document.getElementById('setting-provider').value = s.provider_id || 'workbuddy-cn';
    document.getElementById('setting-custom-url').value = s.custom_url || '';
    document.getElementById('setting-api-key').value = s.api_key || '';
    document.getElementById('setting-port').value = s.port || 3456;
    document.getElementById('setting-bind').value = s.bind || '127.0.0.1';
    document.getElementById('setting-reasoning-model').value = s.reasoning_model || '';
    document.getElementById('setting-completion-model').value = s.completion_model || '';
    document.getElementById('setting-model-map').value = s.model_map || '';
    document.getElementById('setting-sanitize-terms').value = s.sanitize_terms || '';
    document.getElementById('setting-launch-at-login').checked = Boolean(s.launch_at_login);

    // A null switch means "follow the provider preset": show the effective
    // value, and say so, so an unset switch is not mistaken for off.
    const presetStream = Boolean(s.force_stream_preset);
    const forceStream = s.force_stream === null || s.force_stream === undefined
      ? presetStream
      : Boolean(s.force_stream);
    document.getElementById('setting-force-stream').checked = forceStream;
    const streamHint = document.getElementById('force-stream-hint');
    streamHint.textContent = (s.force_stream === null || s.force_stream === undefined)
      ? `跟随服务商预设：${presetStream ? '开启' : '关闭'}`
      : '已手动覆盖服务商预设值';

    // Load Claude config
    loadClaudeConfig();
    isSettingsLoaded = true;
  } catch (err) {
    console.error('loadSettings error:', err);
  }
}

async function loadClaudeConfig() {
  try {
    const res = await invoke('get_claude_config');
    if (res) {
      const env = res.env || {};
      // Prioritize ANTHROPIC_MODEL; only fallback to top-level model if it's not the generic 'sonnet' alias
      const modelVal = env.ANTHROPIC_MODEL || (res.model && res.model !== 'sonnet' ? res.model : '');
      document.getElementById('setting-claude-model').value = modelVal;
      document.getElementById('setting-claude-sonnet').value = env.ANTHROPIC_DEFAULT_SONNET_MODEL || '';
      document.getElementById('setting-claude-opus').value = env.ANTHROPIC_DEFAULT_OPUS_MODEL || '';
      document.getElementById('setting-claude-haiku').value = env.ANTHROPIC_DEFAULT_HAIKU_MODEL || '';
    }
  } catch (e) {
    console.warn('loadClaudeConfig error:', e);
  }
}

document.getElementById('btn-toggle-key-visibility')?.addEventListener('click', () => {
  const input = document.getElementById('setting-api-key');
  const btn = document.getElementById('btn-toggle-key-visibility');
  appState.keyVisible = !appState.keyVisible;
  input.type = appState.keyVisible ? 'text' : 'password';
  btn.textContent = appState.keyVisible ? '隐藏' : '显示';
});

document.getElementById('btn-save-settings')?.addEventListener('click', async () => {
  const statusEl = document.getElementById('save-status');
  statusEl.className = 'save-status';
  statusEl.textContent = '保存中...';

  const payload = {
    provider_id: document.getElementById('setting-provider').value,
    custom_url: document.getElementById('setting-custom-url').value,
    api_key: document.getElementById('setting-api-key').value,
    port: parseInt(document.getElementById('setting-port').value, 10) || 3456,
    bind: document.getElementById('setting-bind').value || '127.0.0.1',
    reasoning_model: document.getElementById('setting-reasoning-model').value,
    completion_model: document.getElementById('setting-completion-model').value,
    model_map: document.getElementById('setting-model-map').value,
    launch_at_login: document.getElementById('setting-launch-at-login').checked,
    sanitize_terms: document.getElementById('setting-sanitize-terms').value,
    force_stream: document.getElementById('setting-force-stream').checked,
  };

  try {
    const res = await invoke('save_settings', { body: payload });
    statusEl.className = 'save-status ok';
    statusEl.textContent = '✓ 配置保存成功';
    await refreshStatus();
    setTimeout(() => { statusEl.textContent = ''; }, 3000);
  } catch (err) {
    statusEl.className = 'save-status err';
    statusEl.textContent = '✗ 保存失败: ' + err;
  }
});

// Fetch Models
document.getElementById('btn-fetch-models')?.addEventListener('click', async () => {
  const container = document.getElementById('models-container');
  container.innerHTML = '<span class="hint">正在拉取模型列表中...</span>';
  try {
    const res = await invoke('fetch_models');
    if (res && res.models && res.models.length > 0) {
      container.innerHTML = res.models.map(m => {
        return `<span class="model-chip" title="点击填入主模型" data-model="${escapeHtml(m.id)}">${escapeHtml(m.id)}${m.name ? ` <small>(${escapeHtml(m.name)})</small>` : ''}</span>`;
      }).join('');

      container.querySelectorAll('.model-chip').forEach(chip => {
        chip.addEventListener('click', () => {
          const modelId = chip.dataset.model;
          document.getElementById('setting-claude-model').value = modelId;
          document.getElementById('setting-claude-sonnet').value = modelId;
        });
      });
    } else {
      container.innerHTML = '<span class="hint">未找到可用模型或当前提供商不支持模型列表查询</span>';
    }
  } catch (err) {
    container.innerHTML = `<span class="hint" style="color: var(--error);">拉取失败: ${escapeHtml(err)}</span>`;
  }
});

// Apply Claude Code Config
document.getElementById('btn-apply-claude-config')?.addEventListener('click', async () => {
  const status = document.getElementById('claude-config-status');
  status.textContent = '正在写入...';
  try {
    const body = {
      model: document.getElementById('setting-claude-model').value.trim() || undefined,
      sonnet: document.getElementById('setting-claude-sonnet').value.trim() || undefined,
      opus: document.getElementById('setting-claude-opus').value.trim() || undefined,
      haiku: document.getElementById('setting-claude-haiku').value.trim() || undefined,
    };
    await invoke('apply_claude_config', { body });
    status.style.color = 'var(--success)';
    status.textContent = '✓ 写入 ~/.claude/settings.json 成功';
    setTimeout(() => { status.textContent = ''; }, 3000);
    await loadClaudeConfig();
  } catch (err) {
    status.style.color = 'var(--error)';
    status.textContent = '✗ 写入失败: ' + err;
  }
});

// Apply Codex Model Catalog
async function loadCodexConfig() {
  const status = document.getElementById('codex-config-status');
  if (!status) return;
  try {
    const cfg = await invoke('get_codex_config');
    if (!cfg.supported) {
      status.textContent = cfg.reason || '未找到 Codex 配置目录';
      return;
    }
    status.textContent = cfg.catalog_exists
      ? '当前目录: ' + cfg.catalog_path
      : '尚未写入目录文件: ' + cfg.config_path;
  } catch (err) {
    status.textContent = '';
  }
}

document.getElementById('btn-apply-codex-config')?.addEventListener('click', async () => {
  const status = document.getElementById('codex-config-status');
  status.style.color = '';
  status.textContent = '正在拉取模型并写入...';
  try {
    const res = await invoke('apply_codex_config');
    status.style.color = 'var(--success)';
    status.textContent = `✓ 已写入 ${res.models} 个模型到 ${res.catalog_path}，重启 Codex 后生效`;
  } catch (err) {
    status.style.color = 'var(--error)';
    status.textContent = '✗ ' + err;
  }
});

loadCodexConfig();

// Test upstream
async function handleTestUpstream(resultContainer) {
  resultContainer.textContent = '正在向上游发起测试请求...';
  resultContainer.className = 'test-result';
  try {
    const res = await invoke('test_upstream', { body: {} });
    if (res && res.ok) {
      resultContainer.className = 'test-result ok';
      resultContainer.textContent = `✓ 连通性测试通过 (${res.detail || '上游流式响应正常'})`;
    } else {
      resultContainer.className = 'test-result err';
      resultContainer.textContent = `✗ 测试异常: ${res ? res.detail : '未收到有效内容'}`;
    }
  } catch (err) {
    resultContainer.className = 'test-result err';
    resultContainer.textContent = `✗ 连接失败: ${err}`;
  }
}

document.getElementById('btn-test-upstream-quick')?.addEventListener('click', () => {
  handleTestUpstream(document.getElementById('quick-test-result'));
});
document.getElementById('btn-test-upstream-settings')?.addEventListener('click', () => {
  handleTestUpstream(document.getElementById('quick-test-result'));
  switchTab('overview');
});

// ── Command Palette (⌘K) ───────────────────────────────────────
const paletteOverlay = document.getElementById('palette-overlay');
const paletteInput = document.getElementById('palette-input');
const paletteList = document.getElementById('palette-list');

const paletteActions = [
  { label: '打开概览 (Overview)', kbd: '1', action: () => switchTab('overview') },
  { label: '查看日志 (Logs)', kbd: '2', action: () => switchTab('logs') },
  { label: '服务设置 (Settings)', kbd: '3', action: () => switchTab('settings') },
  { label: '启动/暂停代理服务', kbd: 'S', action: () => btnToggleService.click() },
  { label: '测试上游连通性', kbd: 'T', action: () => handleTestUpstream(document.getElementById('quick-test-result')) },
  { label: '清空日志记录', kbd: 'C', action: () => document.getElementById('btn-clear-logs')?.click() },
  { label: '打开系统日志目录', kbd: 'O', action: () => invoke('open_logs_dir') },
];

let selectedPaletteIndex = 0;

function openPalette() {
  paletteOverlay.classList.add('open');
  paletteInput.value = '';
  selectedPaletteIndex = 0;
  renderPalette();
  paletteInput.focus();
}

function closePalette() {
  paletteOverlay.classList.remove('open');
}

function renderPalette() {
  const q = paletteInput.value.trim().toLowerCase();
  const matches = paletteActions.filter(a => !q || a.label.toLowerCase().includes(q));

  if (matches.length === 0) {
    paletteList.innerHTML = '<div class="palette-item" style="color: var(--muted);">未找到匹配项</div>';
    return;
  }

  if (selectedPaletteIndex >= matches.length) {
    selectedPaletteIndex = 0;
  }

  paletteList.innerHTML = matches.map((item, idx) => {
    const sel = idx === selectedPaletteIndex ? 'selected' : '';
    return `<div class="palette-item ${sel}" data-index="${idx}">
      <span>${escapeHtml(item.label)}</span>
      <kbd>${escapeHtml(item.kbd)}</kbd>
    </div>`;
  }).join('');

  paletteList.querySelectorAll('.palette-item').forEach((el, idx) => {
    el.addEventListener('click', () => {
      matches[idx].action();
      closePalette();
    });
  });
}

document.getElementById('btn-open-palette')?.addEventListener('click', openPalette);

paletteOverlay.addEventListener('click', (e) => {
  if (e.target === paletteOverlay) closePalette();
});

paletteInput.addEventListener('input', () => {
  selectedPaletteIndex = 0;
  renderPalette();
});

paletteInput.addEventListener('keydown', (e) => {
  const q = paletteInput.value.trim().toLowerCase();
  const matches = paletteActions.filter(a => !q || a.label.toLowerCase().includes(q));

  if (e.key === 'ArrowDown') {
    e.preventDefault();
    if (matches.length > 0) {
      selectedPaletteIndex = (selectedPaletteIndex + 1) % matches.length;
      renderPalette();
    }
  } else if (e.key === 'ArrowUp') {
    e.preventDefault();
    if (matches.length > 0) {
      selectedPaletteIndex = (selectedPaletteIndex - 1 + matches.length) % matches.length;
      renderPalette();
    }
  } else if (e.key === 'Enter') {
    e.preventDefault();
    if (matches[selectedPaletteIndex]) {
      matches[selectedPaletteIndex].action();
      closePalette();
    }
  } else if (e.key === 'Escape') {
    closePalette();
  }
});

// Global Shortcuts (⌘K / Ctrl+K)
window.addEventListener('keydown', (e) => {
  if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === 'k') {
    e.preventDefault();
    if (paletteOverlay.classList.contains('open')) {
      closePalette();
    } else {
      openPalette();
    }
  }
});

// ── Init ───────────────────────────────────────────────────────
refreshStatus();
refreshStats();
loadSettings();
setInterval(refreshStatus, 2000);
setInterval(refreshStats, 2000);
setInterval(() => {
  if (appState.activeTab === 'logs') {
    if (appState.logsViewMode === 'table') {
      const autoRefreshEl = document.getElementById('table-autorefresh');
      if (autoRefreshEl && autoRefreshEl.checked) {
        fetchRequestLogs(false);
      }
    } else {
      fetchLogs();
    }
  }
}, 2000);
