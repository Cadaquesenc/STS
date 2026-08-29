// Watching a tweet move.
//
// A single snapshot of a tweet tells you how big it is. It cannot tell you
// whether that size is real. Genuine attention arrives as a curve — fast at
// first, then slowing. Bought attention arrives as a step: nothing, nothing,
// then thousands of views with no replies behind them.
//
// So each tweet is checked several times over the ten minutes after we first
// see it, and the whole series is written down. Nothing here judges the shape.
// It records it, so the judging can happen later against coins whose outcomes
// we already know.
//
// Tweets are tracked once each, not once per coin. Several coins routinely
// launch on the same tweet, and polling it five times over would be both wrong
// and rude to a free service.
import { Records } from './record.js';
import { SCHEMA } from './session.js';

// Seconds after first sighting. Dense early because that is where the shape is:
// a real tweet's first minute looks nothing like its tenth.
export const SCHEDULE = [30, 120, 300, 600];

export class TweetTracker {
  constructor({ social, schedule = SCHEDULE, save = true, dir = null, session = null }) {
    this.social = social;
    this.schedule = schedule;
    // One file per session, like the coins. A tweet is sampled for ten minutes,
    // so under the dated naming a run that crossed midnight split those too.
    this.records = save ? new Records({ name: 'tweets', session, ...(dir ? { dir } : {}) }) : null;
    this.watching = new Map(); // statusId -> series
    this.stats = { tracked: 0, samples: 0, failed: 0 };
  }

  /**
   * Start following a tweet, or note that another coin referenced one we are
   * already following.
   *
   * @param link  {handle, statusId} from social.js
   * @param mint  the coin that pointed at it
   * @param known what the first lookup already told us, so the launch moment
   *              becomes sample zero rather than being thrown away
   */
  track(link, mint, known) {
    if (!link?.statusId) return;

    const existing = this.watching.get(link.statusId);
    if (existing) {
      existing.coins.push(mint);
      return;
    }

    const t0 = Date.now();
    const series = {
      t0,
      statusId: link.statusId,
      handle: link.handle,
      tweetedAt: known?.tweeted ?? null,
      // How old the tweet was when the first coin appeared on it. The thing we
      // most want to explain.
      ageAtFirstCoinSec: known?.tweeted ? Math.round((t0 - known.tweeted) / 1000) : null,
      joined: known?.joined ?? null,
      coins: [mint],
      samples: [],
      timers: [],
    };
    // Sample zero is free — we already fetched it to describe the coin.
    if (known) series.samples.push(sample(0, known));

    this.watching.set(link.statusId, series);
    this.stats.tracked++;

    // Timers reach ten minutes out. Left referenced, a tweet booked seconds
    // before shutdown would hold the whole process open waiting for a sample
    // nobody is going to read.
    for (const at of this.schedule) {
      series.timers.push(setTimeout(() => this.poll(series, at), at * 1000).unref());
    }
    series.timers.push(setTimeout(() => this.finish(series), (this.schedule.at(-1) + 5) * 1000).unref());
  }

  async poll(series, at) {
    const x = await this.social.sampleTweet(series.handle, series.statusId);
    if (!x) {
      this.stats.failed++;
      // A tweet that stops answering is itself worth knowing about — deleted
      // tweets are a real pattern, not a gap in our data.
      series.samples.push({ at, gone: true });
      return;
    }
    this.stats.samples++;
    series.samples.push(sample(at, x));
  }

  finish(series) {
    if (!this.watching.delete(series.statusId)) return;
    for (const t of series.timers) clearTimeout(t);
    if (!this.records) return;
    const { timers, ...rec } = series;
    // Same reason as the coin and track rows: `tweets-<session>.jsonl` has no
    // header of its own, so the shape has to travel on the row.
    this.records.write({ v: SCHEMA, ...rec });
  }

  /** Write out everything still in flight. A short series is a fact; a missing one is a hole. */
  async close() {
    for (const series of [...this.watching.values()]) this.finish(series);
    await this.records?.close();
  }

  get written() {
    return this.records?.written ?? 0;
  }
}

const sample = (at, x) => ({
  at,
  views: x.views ?? null,
  likes: x.likes ?? null,
  retweets: x.retweets ?? null,
  replies: x.replies ?? null,
  quotes: x.quotes ?? null,
  bookmarks: x.bookmarks ?? null,
  // The author's following can move too, and a spike in it is its own signal.
  followers: x.followers ?? null,
});
