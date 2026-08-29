// The story behind a coin.
//
// Every launch carries a link to a metadata file — the coin's picture, its
// description, and usually a social link. Three quarters of coins have one, and
// nine in ten of those point at *one specific tweet* rather than a profile. So a
// pump.fun coin is usually not a project with an account; it is a bet on a piece
// of news, with a receipt attached.
//
// That receipt is worth reading, because unlike everything on the chain it is not
// instantly available to everyone. Working out that a tweet is 30 seconds old and
// came from an account with 40,000 followers takes two network round-trips that
// most bots never make.
//
// Nothing here decides anything. It fetches facts and hands them back.

const FX = 'https://api.fxtwitter.com';
const TIMEOUT = 12_000;

// Hosts that are X under another name. Anything else is a different site entirely.
const X_HOSTS = new Set(['x.com', 'www.x.com', 'twitter.com', 'www.twitter.com', 'mobile.twitter.com', 'fxtwitter.com', 'vxtwitter.com']);

// First path segments that are pages, not people.
const NOT_A_HANDLE = new Set(['i', 'home', 'search', 'hashtag', 'explore', 'intent', 'share', 'messages', 'notifications', 'settings', 'compose']);

export class Social {
  /**
   * @param concurrency  how many lookups may be in flight at once — these are
   *                     free public services and there is no reason to hammer them
   */
  constructor({ concurrency = 4 } = {}) {
    this.cache = new Map(); // fxtwitter path -> promise, because coins share tweets
    this.shared = new Map(); // twitter link -> how many coins have used it
    this.inFlight = 0;
    this.queue = [];
    this.concurrency = concurrency;
    this.stats = { metaOk: 0, metaFail: 0, xOk: 0, xFail: 0, cached: 0 };
  }

  /** Everything knowable about a coin's story, or `{ kind: 'none' }`. */
  async lookup(uri, launchMs) {
    const meta = await this.metadata(uri);
    if (!meta) return { kind: 'nometa' };

    const out = {
      kind: 'none',
      telegram: Boolean(meta.telegram),
      website: Boolean(meta.website || meta.external_url),
      words: (meta.description || '').trim().split(/\s+/).filter(Boolean).length,
    };

    const raw = meta.twitter || meta.x;
    if (!raw) return out;

    const link = parseX(String(raw));
    if (!link) return { ...out, kind: 'other' };

    // How many coins have already pointed at this exact link. A story being raced
    // by four coins at once is a different situation from one nobody else spotted.
    const key = link.statusId ? `s:${link.statusId}` : `p:${link.handle.toLowerCase()}`;
    const nth = (this.shared.get(key) || 0) + 1;
    this.shared.set(key, nth);

    const x = await this.x(link);
    if (!x) return { ...out, kind: link.statusId ? 'tweet' : 'profile', handle: link.handle, link, nth, failed: true };

    return {
      ...out,
      kind: link.statusId ? 'tweet' : 'profile',
      handle: x.handle,
      statusId: link.statusId,
      // Handed to the tweet tracker so the launch reading becomes sample zero
      // instead of being fetched a second time.
      link,
      x,
      followers: x.followers,
      accountDays: x.joined ? Math.floor((launchMs - x.joined) / 86_400_000) : null,
      // Negative means the coin existed before the tweet did — which happens, and
      // means the link was chosen after the fact or points at something older.
      tweetAgeSec: x.tweeted ? Math.round((launchMs - x.tweeted) / 1000) : null,
      likes: x.likes ?? null,
      retweets: x.retweets ?? null,
      views: x.views ?? null,
      nth,
    };
  }

  /** The coin's metadata file. Untrusted third-party JSON; read as data only. */
  async metadata(uri) {
    if (!uri || !/^https?:\/\//.test(uri)) return null;
    try {
      const res = await fetch(uri, { signal: AbortSignal.timeout(TIMEOUT), redirect: 'follow' });
      if (!res.ok) throw new Error(String(res.status));
      const j = await res.json();
      this.stats.metaOk++;
      return j && typeof j === 'object' ? j : null;
    } catch {
      this.stats.metaFail++;
      return null;
    }
  }

  /** One X lookup, deduplicated — coins share tweets constantly. */
  x(link) {
    const path = link.statusId ? `/${link.handle}/status/${link.statusId}` : `/${link.handle}`;
    if (this.cache.has(path)) {
      this.stats.cached++;
      return this.cache.get(path);
    }
    const p = this.run(() => this.fetchX(path));
    this.cache.set(path, p);
    if (this.cache.size > 10_000) for (const k of [...this.cache.keys()].slice(0, 2_000)) this.cache.delete(k);
    return p;
  }

  /**
   * Re-read a tweet, deliberately ignoring the cache — the whole point is to
   * catch the numbers changing. Still goes through the same queue, so repeat
   * sampling can never outrun the politeness limit.
   */
  sampleTweet(handle, statusId) {
    return this.run(() => this.fetchX(`/${handle}/status/${statusId}`));
  }

  async fetchX(path) {
    try {
      const res = await fetch(FX + path, { signal: AbortSignal.timeout(TIMEOUT) });
      // A deleted tweet or a handle that never existed redirects rather than 404s.
      if (!res.ok) throw new Error(String(res.status));
      const j = await res.json();
      const t = j.tweet;
      const u = j.user || t?.author;
      if (!u) throw new Error('no user');
      this.stats.xOk++;
      return {
        handle: u.screen_name,
        followers: u.followers ?? null,
        joined: u.joined ? Date.parse(u.joined) : null,
        tweeted: t?.created_at ? Date.parse(t.created_at) : null,
        likes: t?.likes,
        retweets: t?.retweets,
        views: t?.views,
        replies: t?.replies,
        quotes: t?.quotes,
        bookmarks: t?.bookmarks,
      };
    } catch {
      this.stats.xFail++;
      return null;
    }
  }

  /** A queue, so a burst of launches cannot turn into a burst of requests. */
  run(fn) {
    return new Promise((resolve) => {
      this.queue.push({ fn, resolve });
      this.pump();
    });
  }

  pump() {
    while (this.inFlight < this.concurrency && this.queue.length) {
      const { fn, resolve } = this.queue.shift();
      this.inFlight++;
      fn().then((v) => {
        this.inFlight--;
        resolve(v);
        this.pump();
      });
    }
  }
}

/**
 * Pull a handle, and a tweet id if there is one, out of whatever was in the
 * metadata. It might be a full URL, a link to one tweet, or just `@name`.
 */
export function parseX(raw) {
  const s = raw.trim();
  if (!s) return null;

  if (!/^https?:\/\//i.test(s)) {
    const h = s.replace(/^@/, '');
    return /^[A-Za-z0-9_]{1,15}$/.test(h) ? { handle: h, statusId: null } : null;
  }

  let u;
  try {
    u = new URL(s);
  } catch {
    return null;
  }
  if (!X_HOSTS.has(u.hostname.toLowerCase())) return null;

  const parts = u.pathname.split('/').filter(Boolean);
  if (!parts.length) return null;
  const handle = parts[0];
  if (NOT_A_HANDLE.has(handle.toLowerCase())) return null;
  if (!/^[A-Za-z0-9_]{1,15}$/.test(handle)) return null;

  const i = parts.indexOf('status');
  const id = i >= 0 && /^\d+$/.test(parts[i + 1] || '') ? parts[i + 1] : null;
  return { handle, statusId: id };
}

/** The one-line version, for a terminal. */
export function describe(s) {
  if (!s) return 'looking…';
  if (s.kind === 'nometa') return 'no metadata';
  if (s.kind === 'none') return 'no link';
  if (s.kind === 'other') return 'link, not X';
  if (s.failed) return `@${s.handle} — couldn't read`;

  const bits = [`@${s.handle}`];
  if (s.followers != null) bits.push(followers(s.followers));
  if (s.accountDays != null) bits.push(age(s.accountDays));
  if (s.kind === 'tweet' && s.tweetAgeSec != null) bits.push(`tweet ${gap(s.tweetAgeSec)}`);
  if (s.nth > 1) bits.push(`#${s.nth} on it`);
  return bits.join(' · ');
}

const followers = (n) =>
  n >= 1_000_000 ? `${(n / 1e6).toFixed(1)}M` : n >= 1_000 ? `${Math.round(n / 1e3)}k` : String(n);

const age = (d) => (d >= 365 ? `${Math.floor(d / 365)}y` : d >= 1 ? `${d}d` : 'new today');

const gap = (sec) => {
  if (sec < 0) return 'after launch';
  if (sec < 90) return `${sec}s old`;
  if (sec < 5400) return `${Math.round(sec / 60)}m old`;
  if (sec < 172_800) return `${Math.round(sec / 3600)}h old`;
  return `${Math.round(sec / 86_400)}d old`;
};
