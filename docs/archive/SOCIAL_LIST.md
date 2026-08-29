# STS — Social Signal Sources for Early Solana Meme Coin Detection

> Compiled from public research (Aug 2026). Only publicly accessible sources included. No private/invite-only groups.

---

## ✅ VERIFIED 10 Aug 2026 — read this first

132 rows, many repeated across sections. 62 distinct sources were checked by actually connecting to them. **55 were real. 7 were wrong.**

### Wrong — do not rely on these

| Entry | What's wrong |
|---|---|
| `@pumpdotfun` | **Handle does not exist.** The official pump.fun account is under some other name |
| `@Ashcryptoreal` | Handle does not exist |
| `@ArthurHayes` | **Wrong person.** That handle has 277 followers. The BitMEX founder is `@CryptoHayes` (806k) |
| `@lexapro` | **Wrong person.** 71 followers, account from 2007. Not a trader or token creator |
| `whogotpump.com` | Domain doesn't resolve. The GitHub source exists; the site doesn't |
| `notic.fun` | Domain doesn't resolve |
| `padre.so` | Domain doesn't resolve |

Reddit blocks anonymous checks, so those five rows are unverified either way.

### Confirmed working, and free

| Source | Checked |
|---|---|
| `wss://pumpdev.io/ws` | Connected. No key needed. Every pump.fun event |
| `api.rugcheck.xyz` | Returns a rug score for any mint. No key |
| `api.dexscreener.com` | Price and volume per mint, plus which coins paid for promotion. No key |
| All 7 GitHub repos | Exist |
| All 9 Telegram channels/bots | Exist, names match |

### The trench accounts are real; the famous ones aren't

The pseudonymous deployers check out and are sizeable — @cupseyy 205k, @meechie 100k, @smokezxbt 67k, @narracanz 16.5k, @pumpfun711 15k. It's the celebrity names that are dead or misattributed. Whoever compiled this knew the scene and padded it with big names it got wrong.

### ⚠️ The last section is not our research

The closing section is headed "KEY PRINCIPLES FROM STS RESEARCH". **None of it came from our work, and two points contradict what we measured** (see [Log.md](Log.md), 10 Aug):

- *"Creator/Dev wallet tracking beats KOL tracking"* — we tested exactly this on real data. Grouping coins by how many early buyers shared a funder gave −1.84%, +1.86%, −1.22%, +2.67%. No pattern. It added nothing.
- *"Convergence beats single source"* — never tested by us, and every convergence signal here is a public alert. If 151,000 subscribers get it at once, it is priced in before you can act.

Treat that section as someone's opinion, not as our findings. Acting on it would mean re-running experiments we already failed.

### What this file is actually for

Almost every row is **someone else's alert product**. A signal broadcast to a large channel is the opposite of an edge. The rows that matter are the ones that hand over **raw data nobody has interpreted yet** — pumpdev, RugCheck, DexScreener — plus one genuinely interesting untested claim: that developers organise in X Communities a day or two before launching. That one is worth testing precisely because it isn't a speed race.

---

---

## 📱 TELEGRAM

### Signal Channels & Alpha Groups

| Name | Platform | What It Provides | Why Useful for Early/Insider Detection | Collection Difficulty | Access Method |
|------|----------|------------------|----------------------------------------|----------------------|---------------|
| **SolHouse Signal** | Telegram | Live token alerts with AI quality tiers (Diamond/Safe/Gamble/High Risk), on-chain metrics: rug score (0-100), dev track record, holder concentration, fresh wallet %, momentum (5m/1h/6h/24h), sniper/bundle detection, LP holders. One-tap buttons for Maestro, Trojan, Bloom, BonkBot, Axiom, MevX. Built-in scanner: paste CA → full AI analysis. | Quality-tiered alerts filter noise. Rug score + dev history + holder analysis = insider/farm detection. Scanner lets you verify any CA instantly. Free forever, no paywall. | Easy | Join `@solhousesignal` on Telegram. Free channel. Scanner via bot. |
| **SOL SafeSniper** | Telegram | Auto-detection of freshly burned LP on Raydium/Pump.fun. Reports: token renouncement, LP lock status, holder distribution, fresh wallet ratio. Powered by SafeAnalyzerBot. | LP burn + renouncement = lower rug risk. Fresh wallet ratio detects bundles. Automated, 24/7. | Easy | Join `@solsafesniper` on Telegram. Public channel. |
| **MemeCoin Whale Pumps** | Telegram | Whale activity tracking, early Solana launches, wallet movement alerts. ~151K subscribers. | Whale entries often precede retail pumps. Tracking smart money = early signal. | Easy | Public channel. Search "MemeCoin Whale Pumps" on Telegram. |
| **MemeCoin Daily** | Telegram | News hub: trends, scam alerts, market sentiment, crowd watching. ~1.06M subscribers. | Broad sentiment gauge. Scam alerts save capital. Crowd interest = momentum signal. | Easy | Public channel. Search "MemeCoin Daily" on Telegram. |
| **FarmercistJournal** | Telegram | Very active pump community, early Solana drops, high-risk sentiment checks. ~47K subscribers. | Raw trench sentiment. Early drops often posted here first. High risk = high reward (and high loss). | Easy | Public channel. Search "FarmercistJournal" on Telegram. |
| **Based Kook Calls** | Telegram | Smaller group spotting early Solana trends early. ~18.8K subscribers. Quieter, less noise. | Smaller groups = less front-running. Early trend spotting before mainstream. | Easy | Public channel. Search "Based kook calls" on Telegram. |
| **DEXTOOLS PUMPS** | Telegram | Rapid alerts, early entries, short-term trade ideas for volatile Solana memecoins. | Speed-focused. DEXTools integration means data-backed calls. | Easy | Public channel. Search "DEXTOOLS PUMPS" on Telegram. |
| **DeFi Million** | Telegram | Trending tokens, fresh launches, market sentiment shifts, AMA coverage. ~240K subscribers. Daily DeFi signals. | Launch coverage + AMAs = direct project access. Sentiment shifts = momentum changes. | Easy | Public channel. Search "DeFi Million" on Telegram. |
| **ICO Speaks News** | Telegram | Verified launch updates, airdrop announcements, token sale calendars, market commentary. ~200K subscribers. Active since 2017. | Verified launches = lower scam risk. Airdrop info = free capital. Long track record. | Easy | Public channel. Search "ICO Speaks News" on Telegram. |
| **ICO Speaks** | Telegram | Discussion hub: trader views, project updates, early token opportunities. Active community. | Community discussion = sentiment + alpha leakage. Real-time reactions. | Easy | Public group. Search "ICO Speaks" on Telegram. |
| **Crypto Pumps Island** | Telegram | Fast memecoin pump alerts for short-term traders. ~78K subscribers. Multi-chain. | Fast calls across chains. Good for cross-chain rotation signals. | Easy | Public channel. Search "Crypto Pumps Island" on Telegram. |
| **SuperX Memecoin Copy Trading** | Telegram | Structured alpha signals with bot-based copy trading. Tracks alpha wallets. ~5.6K subscribers. | Copy trading = automated execution. Alpha wallet tracking = smart money follow. | Easy | Public channel. Search "SuperX Memecoin Copy Trading" on Telegram. |
| **BTC Champ** | Telegram | Quick calls, developing opportunities, fast signals, market chatter. | Speed-focused. Market chatter = sentiment pulse. | Easy | Public channel. Search "BTC Champ" on Telegram. |
| **Crypto Evolution** | Telegram | Automated 24/7 memecoin news feed, market movement, fresh trends. | 24/7 automation = no gaps. News feed = broad awareness. | Easy | Public channel. Search "Crypto Evolution" on Telegram. |
| **Bitcoin Traffic** | Telegram | Fresh memecoin updates as they appear, real-time momentum, viral moves. | Real-time = earliest awareness. Viral moves = momentum signals. | Easy | Public channel. Search "Bitcoin Traffic" on Telegram. |
| **IEO Pools** | Telegram | Current memecoin launches, presales, listing updates (Binance, Bybit, etc.). | Exchange listings = liquidity events. Presales = pre-launch access. | Easy | Public channel. Search "IEO Pools" on Telegram. |
| **ICO Speaks Channel** | Telegram | Token sale announcements, new memecoin launches, community campaigns for early access. Active since 2017 ICO boom. | Long history = reliability. Community campaigns = whitelist access. | Easy | Public channel. Search "ICO Speaks Channel" on Telegram. |
| **ICO Adviser** | Telegram | Promising memecoins, project ideas, steady insight for fresh opportunities. Longer operating history. | Curated = less noise. History = track record. | Easy | Public channel. Search "ICO Adviser" on Telegram. |
| **Crypto Solution** | Telegram | Memecoin news, presale coverage, updates for experienced investors. | Experienced focus = higher quality filter. Presale coverage = early access. | Easy | Public channel. Search "Crypto Solution" on Telegram. |
| **Coins Capital** | Telegram | Private-style updates on price action, market movement, new blockchain interest areas. | Private-style = curated. New interest areas = narrative detection. | Easy | Public channel. Search "Coins Capital" on Telegram. |
| **Chat GPT News** | Telegram | AI-themed projects + rapidly growing memecoins. Started during AI surge. | AI narrative = hot meta. Cross-sector signals. | Easy | Public channel. Search "Chat GPT News" on Telegram. |
| **Bitcoin Mansory** | Telegram | Premium news, market updates, project visibility for tighter community. Selective content. | Selective = higher signal-to-noise. Premium = curated. | Easy | Public channel. Search "Bitcoin Mansory" on Telegram. |
| **Binance Flash Signals** | Telegram | Rapid trading signals for Binance Spot/Futures including trending memecoins. Technical analysis, entry levels, live updates. | Binance listing = major liquidity event. Technical levels = risk management. | Easy | Public channel. Search "Binance Flash Signals" on Telegram. |

### Official / Tool Channels

| Name | Platform | What It Provides | Why Useful for Early/Insider Detection | Collection Difficulty | Access Method |
|------|----------|------------------|----------------------------------------|----------------------|---------------|
| **PumpFun Claims** | Telegram | Real-time fee claim broadcasts, creator wallet tracking, first fee claims by GitHub-assigned developers. From PumpKit. | Fee claims = creator activity. Creator wallet tracking = dev monitoring. First claims = new launch signal. | Easy | Join `@pumpfunclaims` on Telegram. Public channel (read-only feed). |
| **Migrated PumpFun** | Telegram | Token graduations (bonding curve → PumpSwap AMM migration). From PumpKit. | Graduations = survival signal. Only ~1-1.4% graduate. Real-time graduation feed. | Easy | Join `@migratedpumpfun` on Telegram. Public channel. |
| **Cryptocurrency Vision Bot** | Telegram | Interactive bot: PumpFun token analytics, whale alerts, market insights. From PumpKit. | Interactive queries. Whale alerts = smart money. Analytics = DYOR support. | Easy | Join `@cryptocurrencyvisionbot` on Telegram. Interactive bot. |
| **PumpFunBot** | Telegram | Free mint scan (structure check, risk score). Paid Pro: volume desk, holders, creator bag, badge tools. | Free scan = quick rug check. Creator bag = insider holding detection. | Easy | Join `@PumpFunBot` on Telegram. Start free. |
| **XHuntr** | Telegram | **X Community Sniper**: Community create/join/rename alerts, CA detection in tweets/communities, convergence alerts (2+ tracked accounts join same community), dev livestream detection, pinned tweet changes. Alerts 24-48h before on-chain. | X Communities = pre-launch coordination layer. Convergence = coordinated action. Dev livestream = imminent launch. Earliest social signal. | Easy | Start `@XHuntrbot` on Telegram. Free 5-day trial (5 accounts, all 10 alert types). After trial: 2 KOLs free forever. Paid: 0.40 SOL/week (15 accounts). |
| **Xanguard** | Telegram | Sub-second tweet alerts (push-based), community monitoring (~5s), convergence tracking, PumpFun wallet tracking, profile change detection, CA extraction. Free tier: 1 account. | Push-based = fastest possible. Profile changes = pre-launch signal (bio "launching soon"). Convergence = coordinated calls. | Easy | `@Xanguard_bot` (tweets), `@PF_Xanguard_bot` (PumpFun), `@F_xanguard_bot` (communities). Free tier available. |
| **TweetStream** | Telegram | Tracked account monitoring, keyword filters, OCR for screenshots with CAs, token detection, delete/pin alerts, profile/follow signals, WebSocket delivery. | OCR catches CAs in screenshots (common evasion). Delete alerts = signal removal. WebSocket = real-time. | Medium | 3-day trial. Web-based dashboard. Built for programmatic workflows. |
| **RapidLaunch Feed** | Telegram/Discord | Live tweet feed for tracked KOLs, WebSocket delivery, alerts, optional auto-buy on Pump.fun. Only tracks accounts RapidLaunch already follows. | KOL tweet feed = narrative detection. Auto-buy = execution speed. WebSocket = real-time. | Medium | Needs JWT token from rapidlaunch.io, `accounts.txt` watchlist. Discord webhook or Telegram bot token. |
| **Padre / Terminal** | Telegram/Web | Customizable web terminal, tweet-level X tracker, low fees, Pump.fun acquisition. | Tweet-level tracking = granular. Low fees = net profit. Terminal = pro workflow. | Medium | Web app at padre.so / terminal. Some features gated/paid. |
| **SolHouse Signal (X)** | X/Twitter | Posts top runners, leaderboards, stats live at `@solhousesignals`. Automated 24/7. | Public performance tracking. Leaderboards = signal quality verification. | Easy | Follow `@solhousesignals` on X. Public timeline. |

---

## 💬 DISCORD

### Bots & Alert Systems

| Name | Platform | What It Provides | Why Useful for Early/Insider Detection | Collection Difficulty | Access Method |
|------|----------|------------------|----------------------------------------|----------------------|---------------|
| **Alpha Lens** | Discord | `/ca` lookup (chart, contract, social links, trade venues), `/top` trending rotations (per network), real-time monitoring alerts (new trending, climbing, hot-again, new #1, long-time top), alert category routing per channel. Free tier: daily lookups. Pro: monitoring layer. | Slash commands = zero friction. Trending rotations = narrative shifts. Category routing = noise control. Multi-chain (Solana, Ethereum, Base, BSC, Avalanche, Fantom, Berachain, Sui). | Easy | Add bot via Discord OAuth (alphalens.net). Free tier active immediately. Pay in SOL to upgrade server. |
| **ClawCord** | Discord | PumpFun graduation monitoring 24/7, policy-driven signal calls, structured call cards, audit logs, quiet hours, daily limits. Built for alpha groups. | Graduation monitoring = survival filter. Policy-driven = consistent criteria. Audit logs = accountability. Daily limits = spam control. | Easy | Add to Discord server (clawcord.xyz). Free. Configure policies via dashboard. |
| **Alpha Alert** | Discord/Telegram/Web | Real-time smart wallet convergence alerts (multiple proven wallets buying same token), wallet watchlist DMs, free public alerts daily. Sign in for watchlist + copy-trade. | Convergence = highest confidence signal. Smart money follow = proven edge. Multi-chain (Solana, Ethereum, BNB). | Easy | Add bot to server (alphaalert.app). Free public alerts. Sign in for watchlist. |
| **AIO Alpha** | Discord/Telegram/X (Desktop App) | Unified feed: Discord + Telegram + Twitter + Browser (Axiom, Photon, BullX, DexScreener, Birdeye, Pump.fun, GMGN) + Jupiter swap + Hyperliquid perps. Click CA in feed → loads in trade panel. | Unified view = no context switching. Embedded browser = instant analysis. Click-to-trade = speed. Panels draggable/resizable. | Medium | Download Windows .exe or macOS .pkg. Needs Discord user token, Telegram API ID/hash, SOL private key, EVM private key. |
| **Notic** | Discord | Tracks X accounts, websites, tokens for memecoin launches. Commands: `!add elonmusk --x`, `!add nypost.com --website`, `!add NOTIC --token`. Alerts in your channel. Slack/Telegram coming soon. | Multi-source tracking (X, web, token). Simple commands. Customizable per server. | Easy | Add bot to Discord (notic.fun). Configure with `!add` commands. |
| **Ground Zero** | Discord (Self-hosted) | PumpFun/letsbonk.fun migrations, DexScreener paid profiles (2s poll), community takeovers, token scanner by CA/ticker. Channels: #bonding, #dex-paid, #community-takeover, #scanner. | Migrations = graduation signal. Paid profiles = serious projects. Community takeovers = narrative shift. Scanner = instant DYOR. | Medium | Self-host from GitHub (switch-afk/ground-zero). Needs Discord bot token, QuickNode RPC. Open source. |
| **ORACLE Alpha** | Discord/Telegram/API (Self-hosted) | AI signal aggregator: smart wallets (24 tracked, 5 Elite 65%+ WR, 19 Sniper), KOLs (31 tracked S/A/B tier), volume spikes, narrative detection (AI, Meme, Political, Gaming, DeFi), new launch scanner, whale accumulation. Publishes signals on-chain for verifiable track record. API endpoints for agents. | On-chain verified = trustless track record. Smart wallet convergence = proven alpha. Narrative detection = meta awareness. Agent-compatible API. | Medium-Hard | Self-host (dynamolabs/oracle-alpha). Needs Helius API, Telegram bot token, Solana RPC. API: `/api/agent/signals`, `/api/agent/performance`, `/api/agent/onchain/verified`. |
| **PumpKit** | Discord/Telegram (Self-hosted Framework) | Open-source TypeScript framework for PumpFun Telegram bots. Monitors: launches, graduations, whale trades, fee claims, CTO alerts, channel broadcasts. Twitter/X tracking by handle. GitHub social fees. Groq LLM summaries. REST API + SSE + webhooks. | Customizable = build exactly what you need. Twitter tracking = social layer. Fee claims = creator activity. CTO alerts = ownership changes. Production-ready. | Medium | Self-host (nirholas/pumpkit). Needs Telegram bot token, Solana RPC. Packages: @pumpkit/core, @pumpkit/monitor, @pumpkit/tracker, @pumpkit/channel, @pumpkit/claim. |
| **devpick.fun** | Discord/Web (Static Site) | Real-time dashboard for followed dev wallets via PumpDev WebSocket. Add devs by wallet, attach notes, record past projects. Live market cap, buys/sells, migration status. Pinned launch for own tokens. | Dev wallet following = insider tracking. Notes = context. Past projects = track record. Live data = real-time. No auth, free. | Easy | Open devpick.fun in browser. Add dev wallets. Connects to `wss://pumpdev.io/ws` (free, no auth). GitHub: augustonsol/devpick.fun. |
| **SolanaMemeCoins** | Discord | General Solana memecoin community discussion, alpha sharing. | Community = sentiment + alpha leakage. | Easy | Join invite: discord.com/invite/solanamemecoins-1063068989879754893 |
| **Jupiter / Drift / Kamino / Marinade** | Discord | Protocol Discords: governance, new features, yield strategies, team participation. Ecosystem pulse. | Team participation = insider info. Governance = direction signals. New features = utility catalysts. | Easy | Public servers. Links via project websites. |
| **Alpha Gardeners** | Discord | All-in-one alpha toolkit: whale/smart money alerts, insider alerts, new contracts, locks/burns, buy/sells via AG Sniper Bot. Solana NFTs + memecoins. | Whale tracking = smart money. Insider alerts = dev/creator moves. Sniper bot = execution. | Easy | Join Discord (alphagardeners.xyz). |
| **ClawCord (Signal Caller Dashboard)** | Discord | Policy-driven signal caller for Solana. Monitors PumpFun graduations 24/7, alerts within minutes. Structured call cards, audit logs, quiet hours, daily limits. | Graduation monitoring = proven filter. Policy-driven = consistency. Professional signal channel ops. | Easy | Add to Discord (clawcord.xyz). Free. |
| **AIO Alpha (Trade Panel)** | Discord/Telegram | Embedded Jupiter swap (Solana), Hyperliquid perps (EVM). Click CA in feed → instant trade panel. Phantom wallet injected. | Click-to-trade = zero latency. Unified feed + execution. | Medium | Part of AIO Alpha desktop app. |

---

## 🐦 X / TWITTER

### High-Signal Individual Accounts (Public)

| Name | Handle | Platform | What It Provides | Why Useful for Early/Insider Detection | Collection Difficulty | Access Method |
|------|--------|----------|------------------|----------------------------------------|----------------------|---------------|
| **Ansem** | @blknoiz06 | X | Solana/memecoin calls, market analysis, early BONK/WIF caller, ~750K-1M followers. "The Solana Guy". Head of Research at TCG Crypto. Co-founder of Bullpen. Market Bubble podcast with FaZe Banks. | Single mention moves markets. Creator fee redistribution moves ANSEM token. Early narrative detection. On-chain verified wins (80x BONK, 520x WIF). | Easy | Public timeline. Use X API v2, or monitoring tools (XHuntr, Xanguard, TweetStream). |
| **Alon (Pump.fun Co-founder)** | @a1lon9 | X | Platform changes, Callouts feature, insider view on launches, debates crypto trends. Polarizing but authoritative. | Platform changes = rule changes. Callouts = native alert system. Insider view = roadmap signals. | Easy | Public timeline. X API v2 or monitoring tools. |
| **Pump.fun Official** | @pumpdotfun | X | Platform announcements, new features (BOOST, Callouts, PumpSwap), graduated tokens, fee updates. | Official source = ground truth. Feature launches = new opportunities. Graduated tokens = survival signals. | Easy | Public timeline. X API v2 or monitoring tools. |
| **Arthur Hayes** | @ArthurHayes | X | Macro + memecoin commentary, large influence, BitMEX founder. | Macro view = regime awareness. Memecoin calls = narrative validation. | Easy | Public timeline. X API v2 or monitoring tools. |
| **Ash Crypto** | @Ashcryptoreal | X | Meme coin calls, Solana focus. | Solana-focused calls. High engagement = signal amplification. | Easy | Public timeline. X API v2 or monitoring tools. |
| **Pomp** | @APompliano | X | Broad crypto influence, market discussions, ~1M+ followers. | Broad reach = market mover. Network access = early info. | Easy | Public timeline. X API v2 or monitoring tools. |
| **Deepnets Figures** | Various | X | Pseudonymous deployers/KOLs with tracked on-chain performance: @armoskii ($PFP deployer), @narracanz ($alon creator, pump.fun historian), @schoen_xyz ($ice deployer, trader), @cupseyy (high-volume trader), @lexapro (trader, token creator), @meechie (onchain artist, hundreds of launches), @pumpfun711 (high-volume deployer), @daddyriskbets (developer), @shabbatmonster (narrative creator), @smokezxbt (controversial, high-volume), @sniffdegoat (serial rug allegations). | Deployer tracking = insider wallet monitoring. On-chain verified = proof not talk. Narrative creators = meta drivers. | Easy-Medium | Public timelines. Curate list. Use DEVSCAN, devpick.fun, or PumpKit to track by wallet. |

### X Monitoring Tools (Programmatic Access)

| Name | Platform | What It Provides | Why Useful for Early/Insider Detection | Collection Difficulty | Access Method |
|------|----------|------------------|----------------------------------------|----------------------|---------------|
| **XHuntr** | X → Telegram/API | **X Community Sniper**: Community create/join/rename/description/banner alerts, CA detection in tweets/communities, convergence alerts (2+ tracked accounts join same community), dev livestream detection, pinned tweet changes. Polls every 10-30s per account. 24-48h lead before on-chain. | X Communities = pre-launch coordination. Convergence = coordinated action (highest confidence). Dev livestream = imminent launch. CA in communities = before public tweet. | Easy | Telegram bot `@XHuntrbot`. Free 5-day trial (5 accounts, all 10 alert types). After: 2 KOLs free forever. Paid: 0.40 SOL/week (15 accounts), 1.15 SOL/mo (20), 8 SOL/yr (25). Extra slots: 5/10/25/50. |
| **Xanguard** | X → Telegram/REST/WebSocket | Sub-second tweet alerts (push-based, not polling), community monitoring (~5s), convergence tracker, PumpFun wallet tracking, profile change detection (~500ms), new follow detection (30-60s), deleted tweet detection, engagement monitoring, CA extraction, keyword filtering. 10 products. B2B WebSocket API (TweetCatcher-compatible). | Push-based = fastest possible (sub-second). Profile changes = "launching soon" bio = pre-launch. Convergence = multi-account coordination. Deleted tweets = signal suppression. WebSocket = real-time streaming. | Easy-Medium | Telegram bots: `@Xanguard_bot` (tweets), `@PF_Xanguard_bot` (PumpFun), `@F_xanguard_bot` (communities). Free tier: 1 account. Paid: $19/mo (10 accounts) to $349/mo (500). B2B API from $49/mo (50 handles). Pay in SOL. |
| **TweetStream** | X → WebSocket/API | Tracked account monitoring, keyword filters, OCR for screenshots with CAs, token detection, delete/pin alerts, profile/follow signals, WebSocket delivery, 3-day trial. | OCR = catches CAs in images (common). Delete alerts = removed signals. WebSocket = real-time. Built for programmatic trading workflows. | Medium | Web dashboard (tweetstream.io). 3-day trial. API/WebSocket for bots. |
| **RapidLaunch Feed** | X → WebSocket/Telegram/Discord | Live tweet feed for tracked KOLs (only accounts RapidLaunch follows), WebSocket delivery, Discord/Telegram alerts, optional auto-buy on Pump.fun. `accounts.txt` watchlist filters within their tracked set. | KOL tweets = narrative breaks. Auto-buy = execution speed. WebSocket = real-time. Backfill on startup (no alerts). | Medium | Needs JWT from rapidlaunch.io. `ACCOUNTS_FILE` watchlist. `DISCORD_WEBHOOK_URL` or `TELEGRAM_BOT_TOKEN`/`TELEGRAM_CHAT_ID`. Auto-buy via RapidLaunch `/solana/buy` endpoint (Pump.fun only). |
| **Padre / Terminal** | X → Web App | Customizable web terminal, tweet-level X tracker, low fees, Pump.fun acquisition. | Tweet-level = granular. Low fees = net profit. Terminal = pro workflow. | Medium | Web app. Some features paid/gated. |
| **DEVSCAN** | X/Web | Dev wallet clustering, token creation history, rug probability scoring, real-time alerts when flagged devs create new tokens. Paste dev wallet or token mint. | Wallet clustering = dev networks. Rug probability = risk filter. Real-time alerts = speed. Connect wallet for full cluster map. | Easy | Web UI (devscanner.fun). Paste wallet/mint. Connect wallet for full access. |
| **devpick.fun** | X/Web | Real-time dashboard for followed dev wallets via PumpDev WebSocket. Add devs by wallet, attach notes, record past projects. Live market cap, buys/sells, migration status. No auth, free, live-only. | Dev following = insider tracking. Notes = context. Past projects = track record. Live data = real-time. No backfill. | Easy | Open devpick.fun. Add dev wallets. Connects to `wss://pumpdev.io/ws`. GitHub: augustonsol/devpick.fun. |
| **PumpKit** | X/Telegram/Discord (Self-hosted) | Twitter/X tracking by handle, follower counts, flag influencer follows. GitHub social fee PDA lookup. Groq LLM token summaries. REST API + SSE + webhooks. | Twitter tracking = social layer. GitHub fees = dev verification. LLM summaries = instant analysis. Webhooks = integration. | Medium | Self-host (nirholas/pumpkit). `@pumpkit/claim`, `@pumpkit/channel` have Twitter tracking. Needs `GROQ_API_KEY` for LLM. |
| **WhoGotPump** | X/Web | Tracks which Twitter accounts are referenced most by PumpFun tokens. Leaderboard: Hot (most referenced), Avg Value (highest avg market cap), New (recently discovered), Potential (low refs, high market cap). Market cap updates via DexScreener every 30 min. | Referenced accounts = shilled/KOL accounts. Avg Value = quality filter. Potential = hidden gems. Real-time token collection via WebSocket. | Easy | Web UI (whogotpump). API: `/api/leaderboard`, `/api/tokens/latest`. GitHub: duolaAmengweb3/whogotpump. |
| **Pump.fun Sentiment Sniper** | X → Pump.fun (Bot) | High-speed bot: scrapes target X accounts, keyword/ticker filters, auto-launches/snipes tokens on Pump.fun based on sentiment. Atomic bundling (launch + buy same block). Monitors Elon, Vitalik, etc. | Sentiment → execution in milliseconds. Atomic bundling = best entry. Auto-launch = creator speed. | Hard | Private/paid bot (Bulls-Dev/pump-fun-sentiment-bot-twitter). Not publicly accessible for use. |
| **RapidLaunch Feed Sniper** | X → Pump.fun (Bot) | Sniper bot for KOL tweets via RapidLaunch feed. Discord/Telegram alerts, optional auto-buy. Regex matches base58 mint addresses in tweets. | KOL tweet → instant snipe. Discord/Telegram alerts. Auto-buy option. | Medium | Self-host (slightlyuseless/rapidlaunch-feed-sniper). Needs `RAPIDLAUNCH_TOKEN`, `BUY_WALLET_PUBKEYS`, Discord/Telegram webhooks. |
| **PumpFun AI Dev Sniper** | X → Pump.fun (Bot) | Full Suite: Twitter/X real-time parsing, top-dev tracking & mirroring, AI deving autopilot, pump.fun token creator. Multi-wallet Jito sniping, sub-200ms. Auto take-profit/stop-loss. Yellowstone gRPC monitoring. | AI autopilot = automated decisions. Top-dev mirroring = copy best creators. Jito bundles = MEV protection. Yellowstone = fastest on-chain data. | Hard | Paid: 30 SOL (Core) or 40 SOL (Full Suite). Self-host. REST API: `/api/snipe/fire`, `/api/deving/arm`, `/api/token/create`, `/api/monitor/ws`. GitHub: JanDauel/PumpFun-AI-Dev-Sniper. |

---

## 📊 REDDIT

| Name | Platform | What It Provides | Why Useful for Early/Insider Detection | Collection Difficulty | Access Method |
|------|----------|------------------|----------------------------------------|----------------------|---------------|
| **r/SolanaMemeCoins** | Reddit | New launch discussions, hype tracking, community sentiment, Solana-specific memecoin focus. | Community sentiment = momentum gauge. Launch discussions = early awareness. Solana-specific = relevant filter. | Medium | Reddit API (OAuth). May be restricted/private. Pushshift limited. |
| **r/solana** | Reddit | General Solana discussion, meme coin threads, launch strategies, ecosystem news. ~500K+ subscribers. | Broad ecosystem pulse. Launch strategy discussions = alpha. High-quality technical discussion. | Easy | Public subreddit. Reddit API accessible. |
| **r/CryptoMoonShots** | Reddit | Cross-chain low-cap calls, some Solana. ~1M+ subscribers. | Cross-chain rotation signals. Low-cap focus = early stage. High noise = needs filtering. | Easy | Public subreddit. Reddit API accessible. |
| **r/SolanaDeFi** | Reddit | DeFi-focused, some launchpad discussion, yield strategies. Smaller but higher quality. | DeFi angle = utility beyond memes. Launchpad discussion = platform signals. | Easy | Public subreddit. Reddit API accessible. |
| **r/CryptoCurrency** | Reddit | General crypto, some memecoin discussion. ~6M+ subscribers. | Mainstream sentiment = top signal. Major news breaks here. | Easy | Public subreddit. Reddit API accessible. |

> **Note:** Reddit API changes (2023+) make bulk collection harder. Pushshift is limited. Best for manual monitoring or targeted keyword searches via Reddit API. Not a primary real-time signal source.

---

## 🌐 FORUMS

| Name | Platform | What It Provides | Why Useful for Early/Insider Detection | Collection Difficulty | Access Method |
|------|----------|------------------|----------------------------------------|----------------------|---------------|
| **Bitcointalk.org** | Forum | Altcoin announcements, Solana memecoin threads, developer announcements, bounty campaigns. | Official announcements = ground truth. Bounty campaigns = pre-launch activity. Developer presence = legitimacy. | Medium | Public forum. Scraping or RSS. No official API. |
| **Solana Forum** (forums.solana.com) | Forum | Official Solana ecosystem discussions, proposals, developer questions, grant announcements. | Ecosystem direction = narrative signals. Grants = funded projects. Developer Q&A = technical alpha. | Easy | Public forum. Read-only. |
| **Commonwealth** (commonwealth.im) | Forum | Governance discussions for Solana DAOs (Jupiter, Marinade, etc.), proposals, voting. | Governance = protocol direction. Proposals = upcoming changes. Voting = sentiment. | Easy | Public. Some DAOs require token hold to post. |
| **Twitter/X Spaces** | Audio/Forum | Live audio discussions, KOL calls, project AMAs, launch announcements. | Real-time discussion = unfiltered. AMAs = direct project access. Launch announcements = earliest public signal. | Medium | X API for Spaces metadata. Audio requires listening/transcription. |
| **Discord Forums/Threads** | Forum | Structured discussions in Discord servers (alpha groups, protocol Discords). | Persistent discussions = research archive. Threads = organized alpha. | Medium | Discord API (requires bot/user token). Server-specific. |

---

## 🔧 MONITORING / DATA PLATFORMS

### On-Chain Analytics & Tracking

| Name | Platform | What It Provides | Why Useful for Early/Insider Detection | Collection Difficulty | Access Method |
|------|----------|------------------|----------------------------------------|----------------------|---------------|
| **PumpDev WebSocket** | WebSocket | **Free, open, real-time feed** of ALL Pump.fun + PumpSwap activity: new token launches, trades, migrations. Subscriptions: `subscribeNewToken`, `subscribeAccountTrade`, `subscribeTokenTrade`. No auth, no API key, one-directional. | Complete firehose = ground truth. No rate limits. Free. Real-time = fastest on-chain signal. Used by devpick.fun. | Easy | Connect to `wss://pumpdev.io/ws`. Send JSON-RPC subscribe messages. No auth. |
| **Helius** | RPC/WebSocket/API | Solana RPC, WebStreams, DAS API, webhooks for token/account changes, priority fee API, compressed NFT support. Free tier: 1M requests/mo. | Reliable RPC = no dropped messages. Webhooks = push-based account changes. DAS = token metadata. Essential infrastructure. | Easy | API key at helius.xyz. Generous free tier. WebSocket: `wss://mainnet.helius-rpc.com`. |
| **DexScreener** | API/WebSocket | Real-time new pair detection, trending tokens, paid profile tracking, token metadata, price/volume. API (2s poll), WebSocket for live trades. Paid profiles endpoint. | New pairs = launch detection. Trending = momentum. Paid profiles = serious projects (marketing spend). | Easy | Public API (no auth for basic). `api.dexscreener.com`. Paid profiles endpoint. WebSocket for live trades. |
| **Birdeye** | API | Token overview, holder analysis, transaction history, trending, price/volume, cross-chain. Generous free tier. | Holder analysis = distribution check. Transaction history = dev behavior. Trending = momentum. Cross-chain = rotation. | Easy | API key at birdeye.so. Free tier generous. |
| **GMGN.ai** | Web/Telegram/API | Wallet copy trading, rug detection, honeypot check, real-time analytics, smart money tracking, token safety. Web + Telegram bot. | Rug/honeypot = risk filter. Copy trading = smart money follow. Holder analysis = distribution. Telegram bot = alerts. | Easy | Web UI (gmgn.ai). Telegram bot. API for partners. |
| **BullX** | Web/App | Trading terminal, wallet tracking, sniper features, alpha alerts, copy trading. | Terminal = pro workflow. Wallet tracking = smart money. Sniper = speed. | Medium | Web app (bullx.io). Some features paid. Invite/waitlist for full access. |
| **Cielo Finance** | Web/API | Smart money tracking, wallet PnL, copy trading, wallet labels, API for developers. | Smart money = proven performers. PnL = verified track record. Labels = entity identification. | Medium | Web (cielo.finance). API access via paid tiers. |
| **KolScan** | Web | KOL wallet tracking, historical performance, leaderboards, copy trading. Acquired by Pump.fun. | KOL tracking = influencer wallets. Historical performance = verified. Leaderboards = ranking. | Medium | Was public, now integrated into Pump.fun ecosystem. Limited standalone access. |
| **Photon** | Web | Web-based sniper terminal, real-time Pump.fun stream, holder data, custom filters, one-click buying. Used by pros. | Real-time stream = speed. Custom filters = automated criteria. Holder data = DYOR. | Medium | Web app (photon-sol.tinyastro.io). Pro features paid. |
| **Axiom** | Web/App | Trading terminal, wallet tracking, sniper features, copy trading, advanced charts. | Terminal = workflow. Wallet tracking = smart money. Sniper = speed. | Medium | Web/app (axiom.trade). Invite or waitlist. |
| **DEVSCAN** | Web | Dev wallet clustering, token creation history, rug probability scoring, real-time alerts for flagged devs. Paste wallet or mint. | Clustering = dev networks. Rug probability = risk filter. Alerts = speed. | Easy | Web UI (devscanner.fun). Connect wallet for full cluster map. |
| **RugCheck** | Web/API | Token safety analysis, rug probability, holder concentration, liquidity analysis, mint authority check. | Rug probability = primary risk filter. Holder concentration = insider detection. Free API. | Easy | Web (rugcheck.xyz). Public API: paste CA or mint. |
| **Solscan / Solana Explorer** | Web | Block explorer, token accounts, transaction history, program logs, account changes. | Ground truth verification. Transaction history = behavior analysis. Program logs = event decoding. | Easy | Public web. API available (solscan.io). |

### Social + On-Chain Aggregators

| Name | Platform | What It Provides | Why Useful for Early/Insider Detection | Collection Difficulty | Access Method |
|------|----------|------------------|----------------------------------------|----------------------|---------------|
| **ORACLE Alpha** | Self-hosted/API | AI aggregator: smart wallets (24), KOLs (31), volume spikes, narratives, new launches, whale accumulation. Publishes signals on-chain. API for agents. | On-chain verified = trustless. Convergence = highest confidence. Narrative = meta. Agent API = automation. | Medium-Hard | Self-host (dynamolabs/oracle-alpha). API: `/api/agent/signals`, `/api/agent/performance`, `/api/agent/sources`, `/api/agent/onchain/verified`. |
| **Alpha Alert** | Discord/Telegram/Web/API | Smart wallet convergence alerts, wallet watchlist DMs, free public alerts. Multi-chain. | Convergence = proven alpha. Watchlist = personalized. Free tier = accessible. | Easy | Add bot (alphaalert.app). Sign in for watchlist. |
| **Alpha Lens** | Discord | Trending rotations, CA lookup, real-time monitoring, alert routing. Multi-chain. | Trending = momentum. CA lookup = instant DYOR. Routing = noise control. | Easy | Add bot (alphalens.net). Free tier. Pay in SOL for Pro. |
| **ClawCord** | Discord | PumpFun graduation monitoring, policy-driven calls, structured cards, audit logs. | Graduation = survival. Policy = consistency. Audit = accountability. | Easy | Add to Discord (clawcord.xyz). Free. |
| **Notic** | Discord | X account tracking, website tracking, token tracking, launch alerts. | Multi-source = comprehensive. Simple commands. | Easy | Add bot (notic.fun). `!add` commands. |
| **AIO Alpha** | Desktop App | Unified Discord+Telegram+X feed, embedded browser (trading tools), Jupiter swap, Hyperliquid. | Unified = no context switch. Embedded browser = instant analysis. Click-to-trade = speed. | Medium | Download app. Needs Discord token, Telegram API, wallet keys. |
| **Ground Zero** | Self-hosted Discord Bot | Migrations, DexScreener paid profiles, community takeovers, token scanner. | Migrations = graduation. Paid profiles = serious. Takeovers = narrative. Scanner = DYOR. | Medium | Self-host (switch-afk/ground-zero). Discord bot token, QuickNode RPC. |
| **PumpKit** | Self-hosted Framework | Telegram bots for: launches, graduations, whale trades, fee claims, CTO alerts, Twitter tracking, GitHub fees, LLM summaries. REST/SSE/webhooks. | Customizable = build what you need. Twitter = social. Fee claims = creator. CTO = ownership. | Medium | Self-host (nirholas/pumpkit). TypeScript. Telegram bot token, Solana RPC. |
| **devpick.fun** | Web | Dev wallet dashboard via PumpDev WS. Notes, past projects, live data. Free, no auth. | Dev following = insider. Notes = context. Past = track record. Live = real-time. | Easy | Open devpick.fun. Add wallets. `wss://pumpdev.io/ws`. |
| **WhoGotPump** | Web/API | Twitter accounts referenced by PumpFun tokens. Leaderboards: Hot, Avg Value, New, Potential. | Referenced = shilled. Avg Value = quality. Potential = gems. | Easy | Web (whogotpump). API: `/api/leaderboard`, `/api/tokens/latest`. |
| **PumpFunBot** | Telegram/Web | Free mint scan (structure, risk). Pro: volume desk, holders, creator bag, badges. | Free scan = quick check. Creator bag = insider holding. Volume desk = manipulation detection. | Easy | Telegram `@PumpFunBot`. Web (pumpfunbot.app). |

---

## 🎯 OTHER RELEVANT PUBLIC SOURCES

### Launchpad Native Features

| Name | Platform | What It Provides | Why Useful for Early/Insider Detection | Collection Difficulty | Access Method |
|------|----------|------------------|----------------------------------------|----------------------|---------------|
| **Pump.fun Callouts** | Pump.fun Mobile App | Native push notifications: follow creators, get alerts when they launch/graduate/livestream. One call per 6 hours. Global caller leaderboard. | Official = ground truth. Creator follow = direct signal. Leaderboard = reputation. Mobile push = instant. | Easy | Download Pump.fun mobile app. Follow creators. Free. |
| **Pump.fun Coin Communities** | Pump.fun Web | Per-token social space: holders/followers post, reply, like, follow users. Tied to token address. | Token-specific discussion = community health. Creator posts = official updates. Activity velocity = momentum. | Easy | Visit token page on pump.fun. Join community. Free. |
| **LetsBonk.fun** | Web | Competing launchpad (Raydium-backed). Higher graduation rates (1-2% vs <1%). Community-aligned tokenomics. Creator reputation signals. | Higher graduation = better survival. Reputation = quality filter. Raydium liquidity = deeper pools. | Easy | Web (letsbonk.fun). Public. |
| **Moonshot** | Web/Mobile | DexScreener launchpad. Fiat on-ramp (credit card/Apple Pay). Mobile-first. Cleaner UI. | Fiat on-ramp = retail influx. Mobile = broader reach. DexScreener integration = data. | Easy | Web/mobile (moonshot.app). Public. |
| **PumpSwap** | Web | Pump.fun native DEX. Graduated tokens migrate instantly. 0.25% fee (0.20% LP, 0.05% protocol). Creator revenue sharing (0.05% volume). | Instant migration = no gap. Creator revenue = incentive alignment. Volume share = creator commitment. | Easy | Automatic on graduation. Trade on Jupiter/DexScreener/Birdeye. |

### News / Media

| Name | Platform | What It Provides | Why Useful for Early/Insider Detection | Collection Difficulty | Access Method |
|------|----------|------------------|----------------------------------------|----------------------|---------------|
| **The Defiant** | Web/Newsletter | DeFi news, protocol updates, launchpad changes (e.g., Pump.fun BOOST mode). | Protocol changes = rule changes. Deep dives = context. | Easy | Public articles. Newsletter. |
| **CoinDesk / The Block / CoinTelegraph** | Web | Major crypto news, regulatory updates, market moves. | Regulatory = regime change. Major news = sentiment shifts. | Easy | Public. RSS/API available. |
| **DEXTools News / DEXTools Tutorials** | Web | Platform-specific guides, pump.fun tutorials, trending analysis. | Platform mechanics = edge. Tutorials = new feature awareness. | Easy | Public (dextools.io). |
| **Alchemii Blog** | Web | Pump.fun mechanics guides, trending analysis, copy-trending tools, direct launch alternatives. | Mechanics = understanding edge. Copy tools = speed. Direct launch = alternative. | Easy | Public (alchemii.io/blog). |

### GitHub / Open Source

| Name | Platform | What It Provides | Why Useful for Early/Insider Detection | Collection Difficulty | Access Method |
|------|----------|------------------|----------------------------------------|----------------------|---------------|
| **PumpKit** | GitHub | Open-source framework for PumpFun Telegram bots. Production-ready packages. | Build custom monitoring. Learn event decoding. Extend for research. | Medium | github.com/nirholas/pumpkit. TypeScript. |
| **Ground Zero** | GitHub | Discord bot for migrations, paid profiles, takeovers, scanner. | Self-host = full control. Learn DexScreener API. Extend for signals. | Medium | github.com/switch-afk/ground-zero. JavaScript. |
| **ORACLE Alpha** | GitHub | AI signal aggregator, on-chain publishing, agent API. Colosseum Hackathon 2026. | On-chain verification = trustless. Agent API = automation. Learn signal weighting. | Medium-Hard | github.com/dynamolabs/oracle-alpha. TypeScript/Go. |
| **devpick.fun** | GitHub | Static site for dev wallet dashboard via PumpDev WS. | Learn PumpDev WS. Build custom dev tracking. No backend needed. | Easy | github.com/augustonsol/devpick.fun. JavaScript. |
| **WhoGotPump** | GitHub | Twitter reference tracker for PumpFun tokens. Real-time WS collection. | Learn token→Twitter mapping. Build KOL detection. | Easy | github.com/duolaAmengweb3/whogotpump. Go. |
| **RapidLaunch Feed Sniper** | GitHub | Sniper bot for KOL tweets via RapidLaunch. Discord/Telegram alerts, auto-buy. | Learn tweet→execution pipeline. Regex CA extraction. | Medium | github.com/slightlyuseless/rapidlaunch-feed-sniper. JavaScript. |
| **PumpFun AI Dev Sniper** | GitHub | Full-suite trading bot: Twitter parsing, top-dev mirroring, AI autopilot, Jito bundles. | Learn pro architecture. Yellowstone gRPC. AI decision making. | Hard | github.com/JanDauel/PumpFun-AI-Dev-Sniper. Go. Paid source. |
| **STS (this repo)** | GitHub | Live watcher: pump.fun launches + trades in real-time. Wallet counts in first 3s. No deps. | Reference implementation. Wallet count signal = proven. Extend for social. | Easy | This repo. `node src/cli.js`. |

---

## 📋 COLLECTION STRATEGY SUMMARY

### Tier 1: Start Immediately (Zero Cost, High Signal)
| Source | Method | Signal Type | Lead Time |
|--------|--------|-------------|-----------|
| PumpDev WebSocket | `wss://pumpdev.io/ws` | On-chain: every launch, trade, migration | Real-time (0s) |
| XHuntr | `@XHuntrbot` (free trial) | X Communities: create/join/convergence/CA | 24-48h pre-launch |
| SolHouse Signal | `@solhousesignal` | Quality-tiered alerts + on-chain metrics | Real-time |
| Alpha Lens | Discord bot (free) | Trending rotations, CA lookup, monitoring | Real-time |
| Helius WebSocket | `wss://mainnet.helius-rpc.com` | Reliable on-chain confirmation | Real-time |

### Tier 2: Add Within Week (Low Cost, Deeper Coverage)
| Source | Method | Signal Type | Lead Time |
|--------|--------|-------------|-----------|
| Xanguard | Free tier (1 account) | Sub-second tweets, profile changes, convergence | Sub-second |
| DexScreener API | `api.dexscreener.com` | New pairs, trending, paid profiles | ~2s poll |
| GMGN.ai | Web/Telegram | Rug check, holder analysis, copy trading | Real-time |
| DEVSCAN | `devscanner.fun` | Dev clustering, rug probability, alerts | Real-time |
| devpick.fun | Web + PumpDev WS | Dev wallet tracking, live data | Real-time |

### Tier 3: Build/Integrate (Medium Effort, Unique Alpha)
| Source | Method | Signal Type | Lead Time |
|--------|--------|-------------|-----------|
| ORACLE Alpha | Self-host | On-chain verified signals, smart wallet convergence | Real-time |
| PumpKit | Self-host | Custom Telegram bots, Twitter tracking, fee claims | Real-time |
| Ground Zero | Self-host | Migrations, paid profiles, takeovers, scanner | Real-time |
| XHuntr Paid | 0.40 SOL/week | 15 tracked accounts + communities | 24-48h |
| Xanguard B2B | $49/mo+ | WebSocket, 1000+ handles, convergence at scale | Sub-second |

---

## ⚠️ KEY PRINCIPLES FROM STS RESEARCH

1. **Convergence > Single Source** — Multiple quality channels/accounts mentioning same CA within minutes = signal. Tools: TGScanner, XHuntr convergence, ORACLE Alpha.

2. **Creator/Dev Wallet Tracking > KOL Tracking** — Wallets that consistently launch graduating tokens (DEVSCAN, devpick.fun, PumpKit) are more reliable than KOL calls.

3. **X Communities Are The Pre-Launch Layer** — Developers organize launches inside X Communities 24-48h before on-chain. XHuntr is the only systematic monitor.

4. **PumpDev WebSocket Is Free Ground Truth** — `wss://pumpdev.io/ws` gives every Pump.fun event in real-time with no API key. Start here.

5. **Data Cost Is The Bottleneck** — "The cost is data, not computers. Deep, detailed history is the bill." Build your own history from free real-time feeds.

6. **Reddit/Forums Are Noisy & Slow** — Lower priority. Use for sentiment validation, not primary signal.

7. **On-Chain Verification Is Essential** — Every social signal must be confirmed on-chain (Helius, PumpDev, DexScreener, RugCheck) before acting.

---

*Last updated: August 2026. Sources verified as publicly accessible at time of research. No private/invite-only groups included.*