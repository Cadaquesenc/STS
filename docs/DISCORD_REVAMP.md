# Discord revamp — Sol Systems → STS

> **What is actually checkable, added 2026-08-27.** This was written at 03:04, before
> the verdict closed at 06:33. Its plan rests on showing the room "a single checkable
> thing", so here are the three that survive verification — all of them findings about
> the market, none of them a claim about what the software can do.
>
> 1. **A manufactured chart, at address level.** `BwWK17cb…de6s` is 32 bytes off the
>    ed25519 curve — a program address with no private key — and one ordinary wallet
>    signed all **1,704** of its zero-fee transactions. It rescales the quoted SOL
>    reserve a median **55x** against a real move of 1.000x, and it is in **10 of the
>    10 biggest peaks in the corpus**. Two free checks detect it. Confirmed by three
>    independent routes, one of them address-blind.
> 2. **Graduation costs −20.71%, and it is arithmetic rather than a statistic.** Every
>    migration deposits 206,900,000 tokens and 67.4059 SOL into a PumpSwap pool — 50 of
>    the 54 readable pool creations, to the decimal. Median 12-hour giveback −99.64%.
> 3. **Creators are the ones who earn.** +28.8% on 13,130 SOL against −8.1% for
>    everyone else; on the two-thirds of launches where the creator dumps inside a
>    minute, +39.2% on stake at a 60% win rate. Per 100 SOL a non-creator puts in, 6.5
>    goes to the creator and 1.7 to pump.fun. Rebuilt independently to a rounding
>    difference.
>
> Two more, worth posting beside them so it does not read as pure doom: **a real tail
> exists** — 86 coins peaked above 3x with 50+ distinct wallets, 0.70% of 12,205
> launches, and not one is actor-touched; and **a genuine 3x is a 40-second climb, not
> a 3-second spike**, median peak at second 39, with not one of the 86 peaking at or
> before second 3.
>
> **What must not be claimed:** anything of the form "STS said X about this coin"
> through the current build, any live number, any uptime figure written by the process
> being measured, and any millisecond timestamp offered as independently verifiable.
> Nothing in the Rust engine has ever read a real capture. See
> `docs/VERDICT-2026-08-27.md`.

Written 2026-08-27. Working doc for Ethan + the two build accounts.
Sources: the Poke thread (15 Aug positioning brief, 19–20 Aug brand direction, 25 Aug manifesto), and the current server as of 27 Aug.

---

## 1. The problem

It isn't the channel list. Two servers are wearing the same skin.

"Sol Systems" at `/SolanaTrading`, with Giveaway, PnLs, tiktoks and backgrounds, is a 2024-style Solana hangout. Every one of those channels is the exact furniture Ethan told the team on 15 Aug that crypto is "saturated with." Sitting on top of it are #about-sts and #manifesto, selling gatekept forensic intelligence.

That mismatch is what produced the 27 Aug exchange in #Chat — "not a legit bot", "if the nigga is getting hacked 24/7 what makes you think ima trust anything this nigga has ever made."

Nobody in that room has been shown a single checkable thing. Arguing doesn't fix that. Evidence does.

## 2. The decision this rests on

**Is this STS's product server, or the team's trading hangout?**

- Product server: the giveaway/backgrounds culture goes, and some current members drift off. What's left is a room a stranger off TikTok can join and immediately believe.
- Hangout that also mentions STS: everyone stays, and STS permanently reads as a side project some kid is LARPing. That is the read it is already getting.

**Decision: product server.** The launch runs TikTok → link in bio → early access. The people arriving have never met anyone here. The server is the first proof, and right now it proves the wrong thing.

## 3. Structure

Lowercase throughout. Sparse. Matches the terminal look.

**pinned**
- `announcements` — Ethan only. Rare.

**information** — read-only
- `start-here` — what this is, what STS never does, how to get in. Five lines.
- `what-sts-is` — plain description, no adjectives.
- `manifesto` — the 12 slides as text, one message, pinned.
- `changelog` — replaces `logs`. Dated build notes in plain words.

**evidence** — read-only, staff post. This is the new heart of the server.
- `teardowns` — one dead coin per post. Mint address, wallet cluster, millisecond timestamps, funding source, what STS would have said. Every claim copy-pasteable into Solscan by a stranger.
- `results` — backtests and live numbers, **including the losses**. One unflattering number is worth ten teardowns.
- `status` — bot-posted, one line: engine up, launches scanned today, last block seen.

**access**
- `batch-01` — the 50 seats. Seat count and how to get one. Nothing else.
- `requests` — existing `Request`, renamed.

**community**
- `chat`
- `international-chat` — keep, it's genuinely used.
- `pnls` — keep, with one rule: a post needs the mint address and entry/exit, or it's removed. That rule turns the most LARP-prone channel into evidence.

**voice**
- `open-vc`

**staff**
- `staff-chat`
- `content` — the TikTok queue.

**Remove:** `Giveaway` (the loudest "not legit" signal in the server — giveaways are what fake tools do to buy members), `backgrounds`, `tiktoks` (fold clips into `announcements`; a channel of your own TikToks reads as self-promo, one pinned body of work reads as work).

Archive rather than delete where there's history worth keeping — deleting a channel destroys its messages permanently.

## 4. Roles

Four. No more.

- **batch 01** — capped at 50, hard. The cap is the product. Hand out 200 and it isn't batch 01.
- **verified** — anyone who has posted a checkable PnL or a teardown that held up. Free to earn, impossible to fake.
- **build** — Ethan and the two accounts tagged on 15 Aug.
- everyone else — reads all of *information* and *evidence*, talks in `chat`. No locked mystery doors. The gate is on the product, not the room.

## 5. Two things to fix before any of this

**The account is the credibility hole.** The Discord account was disabled 5–13 Aug for suspected compromise, and that is precisely what got thrown back in #Chat. Passkey + 2FA, and a permanent line in `start-here`: STS never DMs first, never asks for a wallet connection, never asks anyone to run a file. The strongest asset here is that STS never touches anyone's wallet — there is nothing to drain. Say it in the first five lines a stranger reads.

**The manifesto promises something the engine doesn't do.** The slides say STS tells you "if a market is real before you put a single penny in" — a shield. But the engine is built to knowingly buy a coin it expects to collapse and exit first; its risk output is time-to-collapse and expected run, never safe/unsafe. The first time a member sees STS buy something it flagged, the shield story dies and takes the rest with it.

Fix the wording now: **STS shows you what's actually happening and what it's worth. It does not tell you what's safe.** That is a stronger claim anyway — nobody else is making it. One word changed in the manifesto copy below (slide 9, "forensic shield" → "reads the raw truth") for exactly this reason.

## 6. Two smaller notes

- The server sits at 14 boosts. Discord level 3 starts at exactly 14, and level 3 is what holds `/SolanaTrading`. One person un-boosting loses the vanity link.
- "Sol Systems", "STS" and `/SolanaTrading` are three names for one thing. Keep the URL — it's free search traffic and it can't be recovered once dropped — but make the server name and the product name the same word.

## 7. Order of work

1. Fill `manifesto` and `start-here`. An empty manifesto channel is worse than no manifesto channel.
2. Post one teardown in `evidence`. One, fully verifiable. That is the entire answer to "not a legit bot."
3. Remove Giveaway and backgrounds, restructure the categories.
4. Roles, and the batch-01 cap.
5. Then rename.

---

# Appendix: channel copy, ready to paste

First person, Discord markdown, no long dashes. `#channel` becomes a clickable mention at build time.
Bot posts `#changelog` and `#status`; Ethan pastes the rest via `~/Code/tools/sts-discord/paste.sh N`.

## `#rules`

```markdown
# rules

i'll keep this short.

### don't shill coins in #chat
if you're in something, put it in #pnls with the mint and your entry so people can actually look at it. a screenshot with the ticker cropped out is just noise.

### nobody here will DM you
not about signals, not about access, not about your wallet. if someone does, they're pretending to be one of us. screenshot it in #chat and i'll ban them.

### i will never DM you first
never ask you to connect a wallet. never ask you to run anything. there is no version of this where i need access to your money.

### argue with me
genuinely. if you think something i posted is wrong, say why and show the data. that's the entire point of this server. what i don't want is people talking shit with nothing behind it.

### no scam links
no lookalike domains, no invite spam.

-# that's it. the rest is judgement.
```

## `#start-here`

*message 1 of 2*

```markdown
# start here

hey. i'm building STS.

every time a coin launches on solana, the whole story is already written on the blockchain. who funded the dev, how many wallets bought in the first second, whether those wallets are actually different people.

almost nobody reads it. reading it properly takes hours, and the coin is dead in eleven minutes.

**STS reads it while it's happening.**

this server is where i put the work as i do it. it's not a hype server. i'm not going to post rocket emojis at you.

### where things are
- #manifesto : why i started this
- #what-sts-is : what it actually does
- #teardowns : coins i've pulled apart, with the addresses, so you can check every word
- #results : what it actually returns, including when that's embarrassing
- #batch-01 : 50 seats, when it's ready
- #changelog : what i built this week
```

*message 2 of 2*

```markdown
# before anything else

> **i will never DM you first.**
> **i will never ask you to connect a wallet.**
> **i will never ask you to run a file, a script or an installer.**

there is nothing in this project that touches your money. no keys, no signing, no connect button. there is nothing for me to steal even if i wanted to.

so if something claiming to be me does any of the above, it isn't me. screenshot it in #chat and i'll deal with it.

-# and i'm never going to tell you a coin is safe. nobody can do that honestly. i can tell you what's actually there.
```

## `#what-sts-is`

*message 1 of 2*

```markdown
# what it actually does

STS watches every coin the second it launches, and answers four things before you've finished reading the ticker.

## 01. who's behind it
where the dev's money came from, which wallets that money touches, and what those wallets launched before this one.
-# if the same person has deployed four coins that all died the same way, that shows up.

## 02. who's already in
how many separate wallets bought in the first few seconds, and whether they're actually separate.
-# one guy with 20 wallets and 20 real people look identical on a chart. they don't look identical in the funding graph. that's the whole trick.

## 03. what's actually pushing it
almost every launch links out to something, and most of the time it isn't a profile, it's one specific tweet. so the coin is really a bet on one piece of news.

STS reads that tweet. who posted it, how old the account is, how many followers it actually has, how old the tweet already was when the coin launched, and how many other coins have already been launched off that exact same tweet.
-# if six coins already used that tweet, it isn't news, it's a farm. and if the accounts pushing it don't hold any of it, they aren't promoting, they're exiting into you.

## 04. what it's worth
this is the part i think everyone else gets wrong. it doesn't say safe or unsafe. it gives you an expected outcome. how long this thing probably has, and how far it runs before it doesn't.
-# sometimes the honest answer is: this is going to collapse, and here's your window.
```

*message 2 of 2*

```markdown
# what it isn't

- **not a speed bot.** i'm not beating block-zero snipers and i'm not going to pretend i am.
- **it never touches your wallet.** ever.
- **not a safety score.** anything selling you a green tick is guessing.
- **not open source**, and i want to be straight about why. the detection rules are the entire product. if i publish them, every dev who rugs people reads them and routes around them by friday.

### where it is right now
a desktop app, written in rust, reading the chain directly instead of trusting someone else's api.

it isn't finished. i'm not going to sell you access to something that isn't finished.

### who's building it
me. the engine stays private for the reason above, but i'm not anonymous about any of this.
https://github.com/Cadaquesenc

-# when the numbers are good enough that i'd want to use it myself, i'll open batch 01.
```
## `#my-why`

*message 1 of 2*

```markdown
# why i'm doing this

i'm not going to pretend i started this because i spotted a gap in the market.

i started it because i kept losing.

not in the funny way people post about. in the quiet way. you check the chart at 3am and the thing you were sure about is down 90%, and you already know what happened, you just can't prove it. so you tell yourself you were greedy, or late, or just bad at this. you close the app. you don't tell anyone.

i did that for months.

and the part that actually got to me wasn't the money. it was thinking i was stupid. that everyone else understood something i didn't, and if i just watched more videos or got into the right group chat i'd find whatever it was.

then i started reading the actual chain data, and it turned out i wasn't stupid. i was playing a game where the other side could see my cards.
```

*message 2 of 2*

```markdown
the same shape, over and over. one person, twenty wallets, half the supply, bought in the millisecond the coin existed. and then me, twenty minutes later, thinking i'd found something.

that isn't a skill issue. it's a rigged table, and nobody tells you, because everyone who knows is making money from you not knowing.

so i started building STS. not because i want to run a company. because i wanted to see the cards.

and here's the part i keep coming back to.

there are thousands of us doing the exact same thing at the exact same time. same 3am, same chart, same feeling, every single one of us convinced we're the only one it's happening to. we were never competing with each other. we're all sat on the same side of the table getting picked off one at a time by the same twenty wallets.

like it or not we're in this together, even if we've never spoken.

that's all this is. not a signals group. not a tool i'm trying to sell you. just me making sure none of us has to sit there at 3am wondering what we did wrong, when we didn't do anything wrong.

-# if you've been there, you've been there. you don't have to say anything. just don't let anyone convince you it happened because you were bad at this.
```
## `#manifesto`

*message 1 of 2*

```markdown
# memecoins don't have value. they have attention.

the second everyone agrees a meme is funny, it becomes a market.

but half the time you're not trading against a community. you're trading against one guy faking volume with 20 wallets.

the crazy part is the blockchain is completely public. people get rugged because nobody actually checks the ledger before buying.

apps like pump.fun make it too easy to skip the research. it felt boring, so you skipped it. i did too.

i hated memecoins. i spent months getting caught in every insider trap, watching wallets get drained, wondering why everyone else seemed to be winning while i was just feeding someone else's bot.
```

*message 2 of 2*

```markdown
most tools and trading bots don't actually protect you. they just help you buy the trap faster. they automate the click and stay completely blind to the manipulation behind it.

i got tired of watching normal people get chewed up by hidden insiders and fake volume, thinking they were just bad at trading, when the game was rigged before they even tapped buy.

so i stopped looking for shortcuts and started building STS. not to trade faster, but to read the raw truth of the blockchain in real time.

it unmasks the insiders. it tracks wallet clusters across the network, so when a deployer splits supply into 30 burner wallets, you see it.

it connects the hype to the ledger. it checks whether the accounts promoting a coin actually hold it, or whether they're being paid to dump on you.

it flips the system. the same tools insiders use to drain retail, pointed the other way.

# you bring the culture. i'll handle the truth.
```

## `#teardowns`

*message 1 of 2*

```markdown
# teardowns

every post in here is one dead coin, pulled apart.

the point isn't to show off the tool. the point is that every claim has an address sitting next to it, so you can open solscan and check whether i'm lying to you.

**i'd genuinely rather you did.**

-# if a teardown in here turns out to be wrong, say so in #chat with the receipt and i'll correct the post and say what i got wrong.
```

*message 2 of 2*

```markdown
### the format

```
TEARDOWN 001 : $TICKER
mint:     <address>
launched: <date> <time UTC>, block <n>

BLOCK ZERO
deployer:   <address>
funded by:  <address>  (<exchange / dispersion wallet>, <n> hops)
first 3s:   <n> wallets, <n>% of supply
of those:   <n> share a funding parent, so one person, not a crowd

DEPLOYER HISTORY
previous launches: <n>
reached raydium:   <n>
collapsed:         <n>

WHAT STS SAID
<the flags, and the exact rule that fired>

WHAT HAPPENED
peak <n>, collapsed <t> after launch
cluster sold <n>% of supply in the first <n> minutes

CHECK IT YOURSELF
<solscan links>
```
```

## `#results`

```markdown
# results

what the thing actually returns.

i'm going to post the bad numbers in here too. not because i'm being noble about it. it's because a tool that only publishes its wins is a marketing account, and you already know exactly what those are worth.

**if a number in here looks unimpressive, it's because it was.**

-# i'd rather show you a real +0.5% than a fake 40x.
```

## `#batch-01`

```markdown
# batch 01

## 50 seats

that's not a marketing number. it's how many people i can actually support while this is still me and two other guys.

### what you get
the terminal when it opens, and the full forensic view instead of just the public flags.

### how you get one
be here and be useful.
- post a teardown that holds up
- find a bug i can reproduce
- put a PnL up in #pnls with the mint attached

there's no form, and there's no waitlist to sign up to for a shot at batch 02.

## seats: 0 / 50

-# and if this isn't for you, that's completely fine. i'm not going to chase anyone or run a countdown timer at you.
```

## `#pnls`

```markdown
# one rule

**mint address. entry. exit.**

a cropped screenshot with the ticker blurred isn't a flex, it's a story. i'm not calling you a liar. i'm saying nobody can tell, and this whole server only works because people can check things.
```

## `#changelog` (bot)

```markdown
# changelog

### 26 aug
subslot ingestion merged. the engine now sees launches inside the block rather than after it.

### 21 aug
moved off electron. the whole thing is rust now.

### 20 aug
deployer history sheet: every launch a wallet cluster made before this one, and how each one ended.

### 10 aug
counting separate wallets in a coin's first 3 seconds sorts outcomes cleanly. 2.6% to 44% chance of a 50% rise at 16+ wallets. after fees, about +0.5% a trade survives.
-# published because it's true, not because it's flattering.
```

## `#status` (bot)

```markdown
# status

-# posted automatically. nothing in here is written by a person.

```
engine: up | launches scanned today: 0 | last block: 0 | 00:00 UTC
```
```
