/**
 * Ryns Control - Frontend Application
 * Manages Server lifecycle, 55 MCP Tools policies, and Safety Vetoes.
 */

(function () {
  'use strict';

  // --- Tauri IPC Abstraction ---
  const hasTauri = typeof window !== 'undefined' &&
    (window.__TAURI__ || window.__TAURI_INTERNALS__);

  function redactSensitive(val) {
    if (!val || typeof val !== 'object') return val;
    const copy = Array.isArray(val) ? [...val] : { ...val };
    for (const k of Object.keys(copy)) {
      if (/token|auth|secret|password/i.test(k) && typeof copy[k] === 'string') {
        copy[k] = '[REDACTED]';
      } else if (typeof copy[k] === 'object') {
        copy[k] = redactSensitive(copy[k]);
      }
    }
    return copy;
  }

  const tauriInvoke = async (cmd, args = {}) => {
    if (window.__TAURI__?.core?.invoke) {
      return window.__TAURI__.core.invoke(cmd, args);
    }
    if (window.__TAURI__?.invoke) {
      return window.__TAURI__.invoke(cmd, args);
    }
    if (window.__TAURI_INTERNALS__?.invoke) {
      return window.__TAURI_INTERNALS__.invoke(cmd, args);
    }
    // Fallback Mock for non-Tauri browser testing
    console.warn(`[Mock IPC] Invoked: ${cmd}`, redactSensitive(args));
    return mockInvoke(cmd, args);
  };

  const tauriListen = async (eventName, handler) => {
    if (window.__TAURI__?.event?.listen) {
      return window.__TAURI__.event.listen(eventName, handler);
    }
    console.warn(`[Mock IPC] Listening for event: ${eventName}`);
    return () => {};
  };

  // --- In-Browser Mock Store ---
  let mockServerRunning = false;
  let mockServerPort = 8799;
  let mockSettings = {
    server_port: 8799,
    server_url: 'http://127.0.0.1:8799',
    auth_token: '',
    safety_mode: 'enforce',
    autostart: false
  };
  let mockTools = [
    { name: 'execute_command', description: 'Executes a terminal command with pre-execution safety gates', access: 'ask', safety: true, safety_capable: true },
    { name: 'read_file', description: 'Read file contents from the workspace filesystem', access: 'enabled', safety: true, safety_capable: true },
    { name: 'write_file', description: 'Write full content to a file at given path', access: 'enabled', safety: true, safety_capable: true },
    { name: 'list_directory', description: 'Lists entries inside a directory', access: 'enabled', safety: false, safety_capable: false },
    { name: 'system_stats', description: 'Collects CPU, memory, and OS runtime metrics', access: 'enabled', safety: false, safety_capable: false }
  ];
  let mockPending = [];
  let mockHistory = [];

  async function mockInvoke(cmd, args) {
    switch (cmd) {
      case 'get_settings': return { ...mockSettings };
      case 'save_settings': mockSettings = { ...mockSettings, ...args.settings }; return null;
      case 'get_server_status':
        return {
          running: mockServerRunning,
          pid: mockServerRunning ? 42100 : null,
          is_external: false,
          port: mockServerPort,
          message: mockServerRunning ? 'Server running' : 'Server stopped'
        };
      case 'start_server': mockServerRunning = true; return { running: true, pid: 42100, is_external: false, port: mockServerPort, message: 'Server started' };
      case 'stop_server': mockServerRunning = false; return null;
      case 'restart_server': mockServerRunning = true; return { running: true, pid: 42101, is_external: false, port: mockServerPort, message: 'Server restarted' };
      case 'fetch_server_config':
        if (!mockServerRunning) throw new Error('Server unreachable');
        return {
          version: '1.0.0',
          port: mockServerPort,
          mode: mockSettings.safety_mode,
          auth: false,
          tool_counts: { enabled: 4, disabled: 0, ask: 1 },
          workspace: '/Users/mock/RynsWorkspace',
          server_pid: 42100
        };
      case 'fetch_tools_list':
        if (!mockServerRunning) throw new Error('Server unreachable');
        return mockTools;
      case 'update_tool_policy': {
        const t = mockTools.find(x => x.name === args.payload.tool);
        if (t) {
          t.access = args.payload.access;
          t.safety = args.payload.safety;
        }
        return { ok: true };
      }
      case 'get_autostart': return mockSettings.autostart;
      case 'set_autostart': mockSettings.autostart = args.enabled; return null;
      case 'list_pending': return [...mockPending];
      case 'decide': {
        const idx = mockPending.findIndex(p => p.id === args.id);
        const req = idx !== -1 ? mockPending.splice(idx, 1)[0] : null;
        mockHistory.unshift({
          id: args.id,
          tool: req?.tool || 'execute_command',
          command: req?.command || '',
          cwd: req?.cwd || '',
          reason: req?.reason || '',
          ruleId: req?.ruleId || req?.rule_id || '',
          decision: args.decision,
          decided_at: String(Math.floor(Date.now() / 1000))
        });
        return null;
      }
      case 'get_audit_history': return [...mockHistory];
      case 'clear_audit_history': mockHistory = []; return null;
      case 'test_connection':
        return { ok: mockServerRunning, message: mockServerRunning ? 'Connected' : 'Connection refused', pending_count: mockPending.length };
      default:
        throw new Error(`Unknown mock command: ${cmd}`);
    }
  }

  // --- Application State ---
  const state = {
    activeView: 'server', // 'server' | 'tools' | 'safety'
    serverStatus: {
      running: false,
      pid: null,
      is_external: false,
      port: 8799,
      message: 'Checking...'
    },
    serverConfig: null,
    tools: [],
    selectedToolName: null,
    toolsFilterText: '',
    toolsFilterAccess: 'all', // 'all' | 'enabled' | 'ask' | 'disabled'
    pending: [],
    history: [],
    selectedSafetyId: null,
    safetyFilterQuery: '',
    safetyActiveTab: 'pending', // 'pending' | 'history'
    settings: {
      server_port: 8799,
      server_binary_path: null,
      server_url: 'http://127.0.0.1:8799',
      auth_token: '',
      safety_mode: 'enforce',
      autostart: false
    }
  };

  // --- DOM Elements ---
  const el = {
    // Topbar
    tabNavServer: document.getElementById('tabNavServer'),
    tabNavTools: document.getElementById('tabNavTools'),
    tabNavSafety: document.getElementById('tabNavSafety'),
    toolsTotalBadge: document.getElementById('toolsTotalBadge'),
    pendingCount: document.getElementById('pendingCount'),
    serverStatus: document.getElementById('serverStatus'),
    statusDot: document.getElementById('statusDot'),
    statusLabel: document.getElementById('statusLabel'),
    btnOpenSettings: document.getElementById('btnOpenSettings'),

    // Views
    viewServer: document.getElementById('viewServer'),
    viewTools: document.getElementById('viewTools'),
    viewSafety: document.getElementById('viewSafety'),

    // Server View
    serverStateBadge: document.getElementById('serverStateBadge'),
    btnStartServer: document.getElementById('btnStartServer'),
    btnRestartServer: document.getElementById('btnRestartServer'),
    btnStopServer: document.getElementById('btnStopServer'),
    serverStatPort: document.getElementById('serverStatPort'),
    serverStatPid: document.getElementById('serverStatPid'),
    serverStatMode: document.getElementById('serverStatMode'),
    serverStatAuth: document.getElementById('serverStatAuth'),
    serverStatWorkspace: document.getElementById('serverStatWorkspace'),
    serverStatVersion: document.getElementById('serverStatVersion'),
    toggleAutostart: document.getElementById('toggleAutostart'),
    summaryEnabledCount: document.getElementById('summaryEnabledCount'),
    summaryAskCount: document.getElementById('summaryAskCount'),
    summaryDisabledCount: document.getElementById('summaryDisabledCount'),

    // Tools View
    toolsFilterInput: document.getElementById('toolsFilterInput'),
    countAll: document.getElementById('countAll'),
    countEnabled: document.getElementById('countEnabled'),
    countAsk: document.getElementById('countAsk'),
    countDisabled: document.getElementById('countDisabled'),
    toolsList: document.getElementById('toolsList'),
    toolsEmptyState: document.getElementById('toolsEmptyState'),
    noToolSelected: document.getElementById('noToolSelected'),
    toolSheet: document.getElementById('toolSheet'),
    sheetToolName: document.getElementById('sheetToolName'),
    sheetToolTitle: document.getElementById('sheetToolTitle'),
    segAccessEnabled: document.getElementById('segAccessEnabled'),
    segAccessAsk: document.getElementById('segAccessAsk'),
    segAccessDisabled: document.getElementById('segAccessDisabled'),
    accessHelpText: document.getElementById('accessHelpText'),
    switchSafety: document.getElementById('switchSafety'),
    safetySwitchWrapper: document.getElementById('safetySwitchWrapper'),
    safetyIncapableNotice: document.getElementById('safetyIncapableNotice'),
    sheetToolDesc: document.getElementById('sheetToolDesc'),

    // Safety View
    tabPending: document.getElementById('tabPending'),
    tabHistory: document.getElementById('tabHistory'),
    safetyTabBadge: document.getElementById('safetyTabBadge'),
    historyCount: document.getElementById('historyCount'),
    pendingSection: document.getElementById('pendingSection'),
    historySection: document.getElementById('historySection'),
    queueFilter: document.getElementById('queueFilter'),
    btnRefresh: document.getElementById('btnRefresh'),
    queueList: document.getElementById('queueList'),
    emptyQueue: document.getElementById('emptyQueue'),
    historyList: document.getElementById('historyList'),
    emptyHistory: document.getElementById('emptyHistory'),
    btnClearHistory: document.getElementById('btnClearHistory'),
    noSelectionView: document.getElementById('noSelectionView'),
    detailView: document.getElementById('detailView'),
    detailTool: document.getElementById('detailTool'),
    detailReqId: document.getElementById('detailReqId'),
    timeoutCountdown: document.getElementById('timeoutCountdown'),
    timeoutBarFill: document.getElementById('timeoutBarFill'),
    detailCwd: document.getElementById('detailCwd'),
    btnCopyCwd: document.getElementById('btnCopyCwd'),
    detailCommand: document.querySelector('#detailCommand code'),
    btnCopyCmd: document.getElementById('btnCopyCmd'),
    detailRuleId: document.getElementById('detailRuleId'),
    detailReason: document.getElementById('detailReason'),
    btnAllow: document.getElementById('btnAllow'),
    btnAllowSession: document.getElementById('btnAllowSession'),
    btnDeny: document.getElementById('btnDeny'),

    // Settings Modal
    settingsModal: document.getElementById('settingsModal'),
    btnCloseSettings: document.getElementById('btnCloseSettings'),
    btnCancelSettings: document.getElementById('btnCancelSettings'),
    inputServerPort: document.getElementById('inputServerPort'),
    inputServerBinary: document.getElementById('inputServerBinary'),
    selectSafetyMode: document.getElementById('selectSafetyMode'),
    inputAuthToken: document.getElementById('inputAuthToken'),
    btnToggleToken: document.getElementById('btnToggleToken'),
    btnTestConn: document.getElementById('btnTestConn'),
    testResultIndicator: document.getElementById('testResultIndicator'),
    settingsForm: document.getElementById('settingsForm'),
    btnSaveSettings: document.getElementById('btnSaveSettings'),

    // Toast
    toastContainer: document.getElementById('toastContainer')
  };

  // --- Toast Helper ---
  function showToast(message, isError = false, duration = 3000) {
    const toast = document.createElement('div');
    toast.className = 'toast' + (isError ? ' toast-error' : '');
    toast.textContent = message;
    el.toastContainer.appendChild(toast);
    setTimeout(() => {
      toast.style.opacity = '0';
      toast.style.transition = 'opacity 0.2s';
      setTimeout(() => toast.remove(), 200);
    }, duration);
  }

  function escapeHtml(str) {
    if (!str) return '';
    return String(str)
      .replace(/&/g, '&amp;')
      .replace(/</g, '&lt;')
      .replace(/>/g, '&gt;')
      .replace(/"/g, '&quot;')
      .replace(/'/g, '&#039;');
  }

  // --- VIEW SWITCHING ---
  function switchView(viewName) {
    state.activeView = viewName;

    el.tabNavServer.classList.toggle('active', viewName === 'server');
    el.tabNavServer.setAttribute('aria-selected', viewName === 'server');
    el.tabNavTools.classList.toggle('active', viewName === 'tools');
    el.tabNavTools.setAttribute('aria-selected', viewName === 'tools');
    el.tabNavSafety.classList.toggle('active', viewName === 'safety');
    el.tabNavSafety.setAttribute('aria-selected', viewName === 'safety');

    el.viewServer.classList.toggle('hidden', viewName !== 'server');
    el.viewTools.classList.toggle('hidden', viewName !== 'tools');
    el.viewSafety.classList.toggle('hidden', viewName !== 'safety');

    if (viewName === 'tools' && state.tools.length === 0 && state.serverStatus.running) {
      loadTools();
    }
  }

  el.tabNavServer.addEventListener('click', () => switchView('server'));
  el.tabNavTools.addEventListener('click', () => switchView('tools'));
  el.tabNavSafety.addEventListener('click', () => switchView('safety'));

  // --- 1. SERVER CARD & CONFIG POLLING ---
  async function pollServer() {
    try {
      // 1. Get process status
      const procStatus = await tauriInvoke('get_server_status');
      state.serverStatus = procStatus || { running: false, pid: null, is_external: false, port: 8799 };

      // 2. Poll server config if running
      if (state.serverStatus.running) {
        try {
          const cfg = await tauriInvoke('fetch_server_config');
          state.serverConfig = cfg;
        } catch {
          // If config fetch fails, treat server as stopped/unreachable
          state.serverStatus.running = false;
          state.serverConfig = null;
        }
      } else {
        state.serverConfig = null;
      }
    } catch {
      state.serverStatus = { running: false, pid: null, is_external: false, port: 8799 };
      state.serverConfig = null;
    }

    renderServerStatus();
  }

  function renderServerStatus() {
    const isRunning = state.serverStatus.running;
    const isExternal = state.serverStatus.is_external;
    const port = state.serverStatus.port || state.settings.server_port || 8799;

    // Topbar Pill
    if (isRunning) {
      el.statusDot.className = 'status-dot running';
      el.statusLabel.textContent = `Running :${port}` + (isExternal ? ' (ext)' : '');
      el.serverStatus.title = `MCP Server active on port ${port}`;
    } else {
      el.statusDot.className = 'status-dot stopped';
      el.statusLabel.textContent = 'Stopped';
      el.serverStatus.title = 'MCP Server is currently not running';
    }

    // Server Card State
    const card = document.querySelector('.server-card.main-card');
    if (isRunning) {
      card?.classList.add('running-state');
      card?.classList.remove('stopped-state');
      el.serverStateBadge.textContent = isExternal ? 'EXTERNAL INSTANCE' : 'RUNNING';
      el.serverStateBadge.className = 'card-badge running';
      el.btnStartServer.disabled = true;
      el.btnStopServer.disabled = isExternal; // Cannot stop external PID
      el.btnRestartServer.disabled = isExternal;
    } else {
      card?.classList.remove('running-state');
      card?.classList.add('stopped-state');
      el.serverStateBadge.textContent = 'STOPPED';
      el.serverStateBadge.className = 'card-badge stopped';
      el.btnStartServer.disabled = false;
      el.btnStopServer.disabled = true;
      el.btnRestartServer.disabled = true;
    }

    // Server Card Stats
    el.serverStatPort.textContent = String(port);
    el.serverStatPid.textContent = isRunning && state.serverStatus.pid ? String(state.serverStatus.pid) : '-';
    
    if (state.serverConfig) {
      const mode = state.serverConfig.mode || state.serverConfig.safety_mode || state.settings.safety_mode;
      const auth = typeof state.serverConfig.auth === 'boolean'
        ? state.serverConfig.auth
        : (typeof state.serverConfig.auth_enabled === 'boolean' ? state.serverConfig.auth_enabled : false);
      const workspace = state.serverConfig.workspace || state.serverConfig.workspace_root || '~/RynsWorkspace';
      const version = state.serverConfig.version || '-';

      el.serverStatMode.textContent = mode;
      el.serverStatAuth.textContent = auth ? 'Enabled (Bearer)' : 'Disabled (Localhost)';
      el.serverStatWorkspace.textContent = workspace;
      el.serverStatVersion.textContent = version;

      const counts = state.serverConfig.tool_counts || { enabled: 0, disabled: 0, ask: 0 };
      el.summaryEnabledCount.textContent = counts.enabled;
      el.summaryAskCount.textContent = counts.ask;
      el.summaryDisabledCount.textContent = counts.disabled;

      el.countEnabled.textContent = counts.enabled;
      el.countAsk.textContent = counts.ask;
      el.countDisabled.textContent = counts.disabled;
    } else {
      el.serverStatMode.textContent = isRunning ? state.settings.safety_mode : '-';
      el.serverStatAuth.textContent = '-';
      el.serverStatWorkspace.textContent = '-';
      el.serverStatVersion.textContent = '-';
      el.summaryEnabledCount.textContent = '0';
      el.summaryAskCount.textContent = '0';
      el.summaryDisabledCount.textContent = '0';
    }
  }

  // Server Control Button Handlers
  el.btnStartServer.addEventListener('click', async () => {
    el.btnStartServer.disabled = true;
    showToast('Starting MCP server child process...');
    try {
      const res = await tauriInvoke('start_server');
      state.serverStatus = res;
      showToast('Server started');
      pollServer();
      if (state.activeView === 'tools') loadTools();
    } catch (err) {
      showToast(`Failed to start server: ${err}`, true, 5000);
      pollServer();
    }
  });

  el.btnStopServer.addEventListener('click', async () => {
    el.btnStopServer.disabled = true;
    showToast('Stopping MCP server...');
    try {
      await tauriInvoke('stop_server');
      showToast('Server stopped');
      pollServer();
    } catch (err) {
      showToast(`Failed to stop server: ${err}`, true, 5000);
      pollServer();
    }
  });

  el.btnRestartServer.addEventListener('click', async () => {
    el.btnRestartServer.disabled = true;
    showToast('Restarting MCP server...');
    try {
      const res = await tauriInvoke('restart_server');
      state.serverStatus = res;
      showToast('Server restarted');
      pollServer();
      if (state.activeView === 'tools') loadTools();
    } catch (err) {
      showToast(`Failed to restart server: ${err}`, true, 5000);
      pollServer();
    }
  });

  // Autostart Toggle
  el.toggleAutostart.addEventListener('change', async (e) => {
    const enabled = e.target.checked;
    try {
      await tauriInvoke('set_autostart', { enabled });
      state.settings.autostart = enabled;
      showToast(`Launch at login ${enabled ? 'enabled' : 'disabled'}`);
    } catch (err) {
      e.target.checked = !enabled;
      showToast(`Failed to update launch at login: ${err}`, true);
    }
  });

  // --- 2. TOOLS MANAGEMENT (The Core) ---
  async function loadTools() {
    try {
      const list = await tauriInvoke('fetch_tools_list');
      if (Array.isArray(list)) {
        state.tools = list;
        el.toolsTotalBadge.textContent = list.length;
        el.countAll.textContent = list.length;

        // If no tool selected yet, select first
        if (!state.selectedToolName && list.length > 0) {
          state.selectedToolName = list[0].name;
        }
        renderToolsList();
        renderToolSheet();
      }
    } catch (e) {
      console.warn('Tools fetch failed (server may be offline):', e);
      if (state.tools.length === 0) {
        el.toolsEmptyState.classList.remove('hidden');
      }
    }
  }

  function renderToolsList() {
    const q = state.toolsFilterText.trim().toLowerCase();
    const accessFilter = state.toolsFilterAccess;

    const filtered = state.tools.filter(t => {
      if (accessFilter !== 'all' && t.access !== accessFilter) return false;
      if (!q) return true;
      return (
        t.name.toLowerCase().includes(q) ||
        (t.description && t.description.toLowerCase().includes(q))
      );
    });

    el.toolsList.innerHTML = '';
    if (filtered.length === 0) {
      el.toolsEmptyState.classList.remove('hidden');
    } else {
      el.toolsEmptyState.classList.add('hidden');
      filtered.forEach(t => {
        const li = document.createElement('li');
        li.className = 'tool-item' + (t.name === state.selectedToolName ? ' selected' : '');
        li.dataset.name = t.name;

        const accessClass = t.access === 'enabled' ? 'enabled'
          : t.access === 'ask' ? 'ask' : 'disabled';

        li.innerHTML = `
          <div class="tool-item-header">
            <span class="tool-item-name">${escapeHtml(t.name)}</span>
            <div class="tool-badges-group">
              <span class="access-badge ${accessClass}">${escapeHtml(t.access)}</span>
              ${t.safety_capable ? `<span class="safety-mini-badge ${t.safety ? 'active' : ''}">safety</span>` : ''}
            </div>
          </div>
          <div class="tool-item-desc">${escapeHtml(t.description || '')}</div>
        `;

        li.addEventListener('click', () => {
          state.selectedToolName = t.name;
          renderToolsList();
          renderToolSheet();
        });

        el.toolsList.appendChild(li);
      });
    }
  }

  function renderToolSheet() {
    const tool = state.tools.find(t => t.name === state.selectedToolName);
    if (!tool) {
      el.noToolSelected.classList.remove('hidden');
      el.toolSheet.classList.add('hidden');
      return;
    }

    el.noToolSelected.classList.add('hidden');
    el.toolSheet.classList.remove('hidden');

    el.sheetToolName.textContent = tool.name;
    el.sheetToolTitle.textContent = tool.safety_capable ? 'Gated Tool' : 'Direct Tool';
    el.sheetToolDesc.textContent = tool.description || '(No description provided)';

    // Update segmented control
    [el.segAccessEnabled, el.segAccessAsk, el.segAccessDisabled].forEach(btn => {
      const accessVal = btn.dataset.access;
      btn.classList.toggle('selected', tool.access === accessVal);
    });

    if (tool.access === 'enabled') {
      el.accessHelpText.textContent = 'Tool is exposed and executes normally under safety policies.';
    } else if (tool.access === 'ask') {
      el.accessHelpText.textContent = 'Mandatory approval required on every invocation before tool can run.';
    } else {
      el.accessHelpText.textContent = 'Tool is completely disabled and hidden from the assistant.';
    }

    // Safety switch
    if (!tool.safety_capable) {
      el.switchSafety.checked = false;
      el.switchSafety.disabled = true;
      el.safetySwitchWrapper.title = 'No pre-execution safety checks available for this tool';
      el.safetyIncapableNotice.classList.remove('hidden');
    } else {
      el.switchSafety.disabled = false;
      el.switchSafety.checked = !!tool.safety;
      el.safetySwitchWrapper.title = 'Toggle cc-safety-net / secret path checks';
      el.safetyIncapableNotice.classList.add('hidden');
    }
  }

  async function updateToolPolicy(newAccess, newSafety) {
    const tool = state.tools.find(t => t.name === state.selectedToolName);
    if (!tool) return;

    const oldAccess = tool.access;
    const oldSafety = tool.safety;

    // Optimistic update
    tool.access = newAccess;
    tool.safety = newSafety;
    renderToolsList();
    renderToolSheet();

    try {
      await tauriInvoke('update_tool_policy', {
        payload: {
          tool: tool.name,
          access: newAccess,
          safety: newSafety
        }
      });
      showToast(`Updated policy for ${tool.name}`);
      pollServer(); // Refresh counts from /api/config
    } catch (err) {
      // Revert optimistic update on error
      tool.access = oldAccess;
      tool.safety = oldSafety;
      renderToolsList();
      renderToolSheet();
      showToast(`Failed to update tool policy: ${err}`, true, 5000);
    }
  }

  // Segmented control click handlers
  [el.segAccessEnabled, el.segAccessAsk, el.segAccessDisabled].forEach(btn => {
    btn.addEventListener('click', () => {
      const tool = state.tools.find(t => t.name === state.selectedToolName);
      if (!tool) return;
      const targetAccess = btn.dataset.access;
      updateToolPolicy(targetAccess, tool.safety);
    });
  });

  // Safety switch change
  el.switchSafety.addEventListener('change', (e) => {
    const tool = state.tools.find(t => t.name === state.selectedToolName);
    if (!tool || !tool.safety_capable) return;
    updateToolPolicy(tool.access, e.target.checked);
  });

  // Filter handlers
  el.toolsFilterInput.addEventListener('input', (e) => {
    state.toolsFilterText = e.target.value;
    renderToolsList();
  });

  document.querySelectorAll('.filter-segmented .seg-btn').forEach(btn => {
    btn.addEventListener('click', () => {
      document.querySelectorAll('.filter-segmented .seg-btn').forEach(b => b.classList.remove('active'));
      btn.classList.add('active');
      state.toolsFilterAccess = btn.dataset.filter;
      renderToolsList();
    });
  });

  // --- 3. SAFETY APPROVALS CONSOLE ---
  function parseTimestampMs(raw) {
    if (typeof raw === 'number') {
      return raw > 1e11 ? raw : raw * 1000;
    }
    if (typeof raw === 'string') {
      const trimmed = raw.trim();
      const num = Number(trimmed);
      if (!isNaN(num)) {
        return num > 1e11 ? num : num * 1000;
      }
      const parsed = Date.parse(trimmed);
      if (!isNaN(parsed)) return parsed;
      const normalized = trimmed.replace(/(\.\d{3})\d+/, '$1');
      const normParsed = Date.parse(normalized);
      if (!isNaN(normParsed)) return normParsed;
    }
    return 0;
  }

  function getRemainingSeconds(req) {
    let expiresAtMs = parseTimestampMs(req.expires_at);
    if (!expiresAtMs && req.created_at) {
      const createdMs = parseTimestampMs(req.created_at);
      if (createdMs) expiresAtMs = createdMs + 120000;
    }
    if (!expiresAtMs) return 120;
    const diff = Math.floor((expiresAtMs - Date.now()) / 1000);
    return Math.max(0, diff);
  }

  function renderSafetyQueue() {
    const query = state.safetyFilterQuery.trim().toLowerCase();
    const filtered = state.pending.filter(item => {
      if (!query) return true;
      return (
        (item.command && item.command.toLowerCase().includes(query)) ||
        (item.tool && item.tool.toLowerCase().includes(query)) ||
        (item.id && item.id.toLowerCase().includes(query)) ||
        (item.ruleId && item.ruleId.toLowerCase().includes(query)) ||
        (item.rule_id && item.rule_id.toLowerCase().includes(query))
      );
    });

    el.pendingCount.textContent = state.pending.length;
    el.safetyTabBadge.textContent = state.pending.length;
    if (state.pending.length > 0) {
      el.pendingCount.classList.add('has-items');
      el.safetyTabBadge.classList.add('has-items');
    } else {
      el.pendingCount.classList.remove('has-items');
      el.safetyTabBadge.classList.remove('has-items');
    }

    el.queueList.innerHTML = '';
    if (filtered.length === 0) {
      el.emptyQueue.classList.remove('hidden');
    } else {
      el.emptyQueue.classList.add('hidden');
      filtered.forEach(req => {
        const li = document.createElement('li');
        li.className = 'queue-item' + (req.id === state.selectedSafetyId ? ' selected' : '');
        li.dataset.id = req.id;

        const rem = getRemainingSeconds(req);
        const rule = req.ruleId || req.rule_id || 'rule';

        li.innerHTML = `
          <div class="item-top">
            <span class="tool-tag-sm">${escapeHtml(req.tool || 'COMMAND')}</span>
            <span class="timeout-badge ${rem < 20 ? 'urgent' : ''}" data-timeout-id="${escapeHtml(req.id)}">${rem}s</span>
          </div>
          <div class="item-command-snippet">${escapeHtml(req.command || '')}</div>
          <div class="item-reason">
            <span class="item-rule">${escapeHtml(rule)}</span>
            <span>&bull;</span>
            <span>${escapeHtml(req.reason || 'Veto')}</span>
          </div>
        `;

        li.addEventListener('click', () => {
          state.selectedSafetyId = req.id;
          renderSafetyQueue();
        });

        el.queueList.appendChild(li);
      });
    }

    renderSafetyDetail();
  }

  function renderSafetyHistory() {
    el.historyCount.textContent = state.history.length;
    el.historyList.innerHTML = '';

    if (state.history.length === 0) {
      el.emptyHistory.classList.remove('hidden');
    } else {
      el.emptyHistory.classList.add('hidden');
      state.history.forEach(rec => {
        const li = document.createElement('li');
        li.className = 'history-item';
        const decClass = rec.decision === 'allow' ? 'allow'
          : rec.decision === 'allow_session' ? 'allow_session' : 'deny';

        const dateStr = rec.decided_at
          ? new Date(Number(rec.decided_at) * 1000).toLocaleTimeString()
          : '';

        li.innerHTML = `
          <div class="history-top">
            <span class="tool-tag-sm">${escapeHtml(rec.tool || 'EXEC')}</span>
            <span class="decision-badge ${decClass}">${escapeHtml(rec.decision)}</span>
          </div>
          <div class="item-command-snippet">${escapeHtml(rec.command || '')}</div>
          <div class="item-reason">
            <span class="item-rule">${escapeHtml(rec.ruleId || rec.rule_id || '-')}</span>
            <span>&bull;</span>
            <span>${dateStr}</span>
          </div>
        `;
        el.historyList.appendChild(li);
      });
    }
  }

  function renderSafetyDetail() {
    const selected = state.pending.find(p => p.id === state.selectedSafetyId);
    if (!selected) {
      el.noSelectionView.classList.remove('hidden');
      el.detailView.classList.add('hidden');
      return;
    }

    el.noSelectionView.classList.add('hidden');
    el.detailView.classList.remove('hidden');

    el.detailTool.textContent = selected.tool || 'COMMAND';
    el.detailReqId.textContent = `ID: ${selected.id}`;
    el.detailCwd.textContent = selected.cwd || '/';
    el.detailCommand.textContent = selected.command || '';
    el.detailRuleId.textContent = selected.ruleId || selected.rule_id || 'unspecified';
    el.detailReason.textContent = selected.reason || 'Safety veto triggered';

    updateCountdownDisplay(selected);
  }

  function updateCountdownDisplay(req) {
    if (!req) return;
    const rem = getRemainingSeconds(req);
    el.timeoutCountdown.textContent = `${rem}s`;
    const pct = Math.min(100, Math.max(0, (rem / 120) * 100));
    el.timeoutBarFill.style.width = `${pct}%`;

    if (rem < 20) {
      el.timeoutCountdown.classList.add('urgent');
      el.timeoutBarFill.classList.add('urgent');
    } else {
      el.timeoutCountdown.classList.remove('urgent');
      el.timeoutBarFill.classList.remove('urgent');
    }
  }

  async function performDecision(decision) {
    if (!state.selectedSafetyId) return;
    const id = state.selectedSafetyId;
    const req = state.pending.find(p => p.id === id);
    if (!req) return;

    const currentIndex = state.pending.findIndex(p => p.id === id);

    try {
      await tauriInvoke('decide', { id, decision });
      showToast(`Decision executed: ${decision.toUpperCase()}`);

      state.pending = state.pending.filter(p => p.id !== id);
      state.history.unshift({
        id,
        tool: req.tool,
        command: req.command,
        cwd: req.cwd,
        reason: req.reason,
        ruleId: req.ruleId || req.rule_id || '',
        decision,
        decided_at: String(Math.floor(Date.now() / 1000))
      });

      if (state.pending.length > 0) {
        const nextIdx = Math.min(currentIndex, state.pending.length - 1);
        state.selectedSafetyId = state.pending[nextIdx].id;
      } else {
        state.selectedSafetyId = null;
      }

      renderSafetyQueue();
      renderSafetyHistory();
    } catch (err) {
      showToast(`Decision failed: ${err}`, true, 5000);
    }
  }

  // Safety Action Buttons
  el.btnAllow.addEventListener('click', () => performDecision('allow'));
  el.btnAllowSession.addEventListener('click', () => performDecision('allow_session'));
  el.btnDeny.addEventListener('click', () => performDecision('deny'));

  el.btnCopyCwd.addEventListener('click', () => {
    const text = el.detailCwd.textContent;
    if (text && text !== '-') {
      navigator.clipboard?.writeText(text);
      showToast('Copied CWD to clipboard');
    }
  });

  el.btnCopyCmd.addEventListener('click', () => {
    const text = el.detailCommand.textContent;
    if (text) {
      navigator.clipboard?.writeText(text);
      showToast('Copied Command to clipboard');
    }
  });

  el.tabPending.addEventListener('click', () => {
    el.tabPending.classList.add('active');
    el.tabHistory.classList.remove('active');
    el.pendingSection.classList.remove('hidden');
    el.historySection.classList.add('hidden');
  });

  el.tabHistory.addEventListener('click', () => {
    el.tabHistory.classList.add('active');
    el.tabPending.classList.remove('active');
    el.historySection.classList.remove('hidden');
    el.pendingSection.classList.add('hidden');
  });

  el.btnRefresh.addEventListener('click', async () => {
    try {
      const p = await tauriInvoke('list_pending');
      if (Array.isArray(p)) state.pending = p;
      renderSafetyQueue();
      showToast('Queue refreshed');
    } catch (e) {
      showToast(`Refresh error: ${e}`, true);
    }
  });

  el.btnClearHistory.addEventListener('click', async () => {
    try {
      await tauriInvoke('clear_audit_history');
      state.history = [];
      renderSafetyHistory();
      showToast('History cleared');
    } catch (err) {
      console.error(err);
    }
  });

  el.queueFilter.addEventListener('input', (e) => {
    state.safetyFilterQuery = e.target.value;
    renderSafetyQueue();
  });

  // --- SSE & TAURI LISTENERS ---
  async function setupTauriListeners() {
    await tauriListen('safety-event', (event) => {
      const payload = event.payload;
      if (!payload) return;

      if (payload.type === 'pending' && payload.request) {
        const req = payload.request;
        const existingIdx = state.pending.findIndex(p => p.id === req.id);
        if (existingIdx !== -1) {
          state.pending[existingIdx] = req;
        } else {
          state.pending.push(req);
          if (!state.selectedSafetyId) {
            state.selectedSafetyId = req.id;
          }
          showToast(`New approval veto: ${req.tool}`);
        }
        renderSafetyQueue();
      } else if (payload.type === 'resolved') {
        const resolvedId = payload.id;
        state.pending = state.pending.filter(p => p.id !== resolvedId);
        if (state.selectedSafetyId === resolvedId) {
          state.selectedSafetyId = state.pending.length > 0 ? state.pending[0].id : null;
        }
        renderSafetyQueue();
        tauriInvoke('get_audit_history').then(hist => {
          if (Array.isArray(hist)) {
            state.history = hist;
            renderSafetyHistory();
          }
        }).catch(() => {});
      }
    });

    await tauriListen('connection-status', () => {
      // Background worker reports status
    });
  }

  // --- 4. SETTINGS MODAL ---
  let isSavingSettings = false;

  function openSettings() {
    el.inputServerPort.value = state.settings.server_port || 8799;
    el.inputServerBinary.value = state.settings.server_binary_path || '';
    el.selectSafetyMode.value = state.settings.safety_mode || 'enforce';
    el.inputAuthToken.value = state.settings.auth_token || '';
    el.inputAuthToken.type = 'password';
    el.testResultIndicator.textContent = '';
    el.testResultIndicator.className = 'test-result-indicator';
    el.settingsModal.showModal();
  }

  el.btnOpenSettings.addEventListener('click', openSettings);
  el.btnCloseSettings.addEventListener('click', () => el.settingsModal.close());
  el.btnCancelSettings.addEventListener('click', () => el.settingsModal.close());

  el.btnToggleToken.addEventListener('click', () => {
    const isPass = el.inputAuthToken.type === 'password';
    el.inputAuthToken.type = isPass ? 'text' : 'password';
  });

  el.btnTestConn.addEventListener('click', async () => {
    const port = Number(el.inputServerPort.value) || 8799;
    const url = `http://127.0.0.1:${port}`;
    const token = el.inputAuthToken.value.trim();
    el.testResultIndicator.textContent = 'Probing...';
    el.testResultIndicator.className = 'test-result-indicator';

    try {
      const res = await tauriInvoke('test_connection', { url, token });
      if (res && res.ok) {
        el.testResultIndicator.textContent = `OK: ${res.message}`;
        el.testResultIndicator.className = 'test-result-indicator success';
      } else {
        el.testResultIndicator.textContent = `Refused: ${res?.message || 'Server stopped'}`;
        el.testResultIndicator.className = 'test-result-indicator error';
      }
    } catch (err) {
      el.testResultIndicator.textContent = `Error: ${err}`;
      el.testResultIndicator.className = 'test-result-indicator error';
    }
  });

  el.settingsForm.addEventListener('submit', async (e) => {
    e.preventDefault();
    if (isSavingSettings) return;
    isSavingSettings = true;
    if (el.btnSaveSettings) el.btnSaveSettings.disabled = true;

    try {
      const port = Number(el.inputServerPort.value) || 8799;
      const binary = el.inputServerBinary.value.trim();
      const mode = el.selectSafetyMode.value;
      const token = el.inputAuthToken.value.trim();

      const oldToken = (state.settings?.auth_token || '').trim();
      const tokenChanged = oldToken !== token;

      // Fresh status check: query current status at submit time, falling back to cached state
      let serverStatus = state.serverStatus;
      try {
        const freshStatus = await tauriInvoke('get_server_status');
        if (freshStatus) {
          serverStatus = freshStatus;
          state.serverStatus = freshStatus;
        }
      } catch (_statusErr) {
        // Fall back to cached value only if the invoke throws
      }

      const isRunning = Boolean(serverStatus?.running);
      const isExternal = Boolean(serverStatus?.is_external);

      const newSettings = {
        ...state.settings,
        server_port: port,
        server_url: `http://127.0.0.1:${port}`,
        server_binary_path: binary,
        safety_mode: mode,
        auth_token: token
      };

      await tauriInvoke('save_settings', { settings: newSettings });
      state.settings = newSettings;
      el.settingsModal.close();

      if (tokenChanged && isRunning && !isExternal) {
        showToast('Restarting server with new token…');
        try {
          const res = await tauriInvoke('restart_server');
          if (res) state.serverStatus = res;
          await pollServer();
          if (res?.is_external) {
            showToast('Settings saved, but server restart failed — use Restart', true, 5000);
          } else {
            if (state.activeView === 'tools') loadTools();
            showToast('Auth updated — server restarted');
          }
        } catch (restartErr) {
          showToast(`Settings saved, but server restart failed: ${restartErr}`, true, 5000);
          pollServer();
        }
      } else if (tokenChanged && isRunning && isExternal) {
        showToast(
          'Auth token saved — but an external server owns the port; restart it manually (panel stays disconnected until then)',
          true,
          5000
        );
        pollServer();
      } else if (tokenChanged && !isRunning) {
        showToast('Auth token saved — applies on next start');
        pollServer();
      } else {
        showToast('Settings saved');
        pollServer();
      }
    } catch (err) {
      showToast(`Save failed: ${err}`, true, 5000);
    } finally {
      isSavingSettings = false;
      if (el.btnSaveSettings) el.btnSaveSettings.disabled = false;
    }
  });

  // --- KEYBOARD SHORTCUTS ---
  window.addEventListener('keydown', (e) => {
    if (['INPUT', 'TEXTAREA', 'SELECT'].includes(e.target.tagName) || el.settingsModal.open) {
      if (e.key === 'Escape' && el.settingsModal.open) el.settingsModal.close();
      return;
    }

    if (e.key === '1') switchView('server');
    else if (e.key === '2') switchView('tools');
    else if (e.key === '3') switchView('safety');
    else if (e.key === ',') openSettings();
    else if (state.activeView === 'safety') {
      if (e.key === 'a' || e.key === 'A') { e.preventDefault(); performDecision('allow'); }
      else if (e.key === 's' || e.key === 'S') { e.preventDefault(); performDecision('allow_session'); }
      else if (e.key === 'd' || e.key === 'D') { e.preventDefault(); performDecision('deny'); }
      else if (e.key === 'j' || e.key === 'ArrowDown') { e.preventDefault(); navQueue(1); }
      else if (e.key === 'k' || e.key === 'ArrowUp') { e.preventDefault(); navQueue(-1); }
      else if (e.key === '/') { e.preventDefault(); el.queueFilter.focus(); }
    } else if (state.activeView === 'tools') {
      if (e.key === '/') { e.preventDefault(); el.toolsFilterInput.focus(); }
    }
  });

  function navQueue(delta) {
    if (state.pending.length === 0) return;
    const currentIdx = state.pending.findIndex(p => p.id === state.selectedSafetyId);
    let nextIdx = currentIdx + delta;
    if (nextIdx < 0) nextIdx = 0;
    if (nextIdx >= state.pending.length) nextIdx = state.pending.length - 1;
    state.selectedSafetyId = state.pending[nextIdx].id;
    renderSafetyQueue();
  }

  // Timer loop
  setInterval(() => {
    if (state.activeView === 'safety' && state.pending.length > 0) {
      document.querySelectorAll('[data-timeout-id]').forEach(badge => {
        const id = badge.dataset.timeoutId;
        const req = state.pending.find(p => p.id === id);
        if (req) {
          const rem = getRemainingSeconds(req);
          badge.textContent = `${rem}s`;
          if (rem < 20) badge.classList.add('urgent');
          else badge.classList.remove('urgent');
        }
      });
      const selected = state.pending.find(p => p.id === state.selectedSafetyId);
      if (selected) updateCountdownDisplay(selected);
    }
  }, 1000);

  // Poll server every 2 seconds
  setInterval(pollServer, 2000);

  // --- BOOTSTRAP ---
  async function init() {
    try {
      const s = await tauriInvoke('get_settings');
      if (s) state.settings = s;
    } catch {}

    try {
      const autostart = await tauriInvoke('get_autostart');
      el.toggleAutostart.checked = !!autostart;
    } catch {}

    pollServer();
    loadTools();

    try {
      const p = await tauriInvoke('list_pending');
      if (Array.isArray(p)) {
        state.pending = p;
        if (p.length > 0) state.selectedSafetyId = p[0].id;
        renderSafetyQueue();
      }
    } catch {}

    try {
      const h = await tauriInvoke('get_audit_history');
      if (Array.isArray(h)) {
        state.history = h;
        renderSafetyHistory();
      }
    } catch {}

    setupTauriListeners();
  }

  init();

})();
