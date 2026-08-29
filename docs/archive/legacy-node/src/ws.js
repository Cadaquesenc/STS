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
  }
  start() { this.stopped = false; this.connect(); }
  async stop() {
    this.stopped = true; clearInterval(this.timer); this.timer = null;
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
      if (this.downSince === null) { this.downSince = this.lastMessageAt || Date.now(); this.downReason = reason; }
      this.audit?.emit('socket', 'disconnect', { reason, attempt: this.attempt });
      this.onStatus(`disconnected (${reason}); reconnecting`);
      const wait = Math.min(30_000, 500 * 2 ** this.attempt) * (0.5 + Math.random()); this.attempt++;
      this.reconnectTimer = setTimeout(() => { this.reconnectTimer = null; this.connect(); }, wait);
    };
    ws.addEventListener('open', () => {
      this.lastMessageAt = Date.now(); ws.send(JSON.stringify(this.request));
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
