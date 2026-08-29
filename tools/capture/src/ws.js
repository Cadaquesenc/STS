// Reconnecting websocket with explicit gap accounting and optional audit events.
export class Socket {
  // `label` is what gets written down instead of `url`. Endpoints carry API keys
  // and the audit log is a file on disk and a row in a database, so the raw URL
  // must never be what lands in either.
  constructor({ url, request, onMessage, onGap = () => {}, onStatus = () => {}, idleMs = 90_000, audit = null, label = null }) {
    this.label = label ?? '(endpoint)';
    this.url = url; this.request = request; this.onMessage = onMessage;
    this.onGap = onGap; this.onStatus = onStatus; this.idleMs = idleMs; this.audit = audit;
    this.ws = null; this.timer = null; this.reconnectTimer = null; this.stopped = false;
    this.attempt = 0; this.downSince = null; this.downReason = null; this.lastMessageAt = 0;
    // When this connection came up, or null while it is down. A launch seen
    // 200 ms after a reconnect is a different observation from one seen
    // mid-stream, and until now nothing written down could tell them apart.
    this.connectedAt = null;
  }
  start() { this.stopped = false; this.connect(); }
  /** Seconds this connection has been continuously up, or null while down. */
  connectedForSec(now = Date.now()) {
    return this.connectedAt === null ? null : Math.max(0, Math.floor((now - this.connectedAt) / 1000));
  }
  /**
   * Downtime the run has accumulated, including an outage still in progress.
   *
   * `onGap` only fires once service resumes, so a coin whose window ended while
   * the socket was still down would otherwise be written as complete. Asking
   * here rather than waiting is the difference between a labelled hole and an
   * invisible one.
   */
  openDownMs(now = Date.now()) {
    return this.downSince === null ? 0 : Math.max(0, now - this.downSince);
  }
  async stop() {
    this.stopped = true; this.connectedAt = null; clearInterval(this.timer); this.timer = null;
    clearTimeout(this.reconnectTimer); this.reconnectTimer = null;
    if (this.ws) { try { this.ws.close(); } catch {} this.ws = null; }
    this.audit?.emit('socket', 'stop');
  }
  connect() {
    if (this.stopped) return;
    let settled = false;
    const ws = new WebSocket(this.url); this.ws = ws;
    const down = (reason) => {
      if (settled) return; settled = true; clearInterval(this.timer); this.timer = null;
      this.connectedAt = null;
      if (this.downSince === null) { this.downSince = this.lastMessageAt || Date.now(); this.downReason = reason; }
      this.audit?.emit('socket', 'disconnect', { reason, attempt: this.attempt });
      this.onStatus(`disconnected (${reason}); reconnecting`);
      const wait = Math.min(30_000, 500 * 2 ** this.attempt) * (0.5 + Math.random()); this.attempt++;
      this.reconnectTimer = setTimeout(() => { this.reconnectTimer = null; this.connect(); }, wait);
    };
    ws.addEventListener('open', () => {
      this.lastMessageAt = Date.now(); this.connectedAt = this.lastMessageAt; ws.send(JSON.stringify(this.request));
      this.audit?.emit('socket', 'connect', { url: this.label }); this.onStatus('connected; subscribing');
      this.timer = setInterval(() => {
        if (Date.now() - this.lastMessageAt > this.idleMs) { try { ws.close(); } catch {} down('idle'); }
      }, 5_000);
    });
    ws.addEventListener('message', (ev) => {
      this.lastMessageAt = Date.now(); this.attempt = 0;
      let msg;
      try { msg = JSON.parse(typeof ev.data === 'string' ? ev.data : ev.data.toString()); }
      catch { this.audit?.emit('decode', 'json_error', {}, { level: 'warn' }); return; }
      if (this.downSince !== null && msg.id === this.request.id) {
        const to = Date.now(); this.onGap({ from: this.downSince, to, ms: to - this.downSince, reason: this.downReason });
        this.audit?.emit('socket', 'gap', { from: this.downSince, to, ms: to - this.downSince, reason: this.downReason });
        this.downSince = null; this.downReason = null;
      }
      this.onMessage(msg);
    });
    ws.addEventListener('error', () => down('error')); ws.addEventListener('close', () => down('close'));
  }
}
