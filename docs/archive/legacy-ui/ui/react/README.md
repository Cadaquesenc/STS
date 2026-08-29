# The feed, as a React component

`ui/index.html` is the page the desktop app opens. It is plain HTML, CSS and a
script, because the app loads this folder straight off disk — `git pull` updates
it and nothing has to be rebuilt. That property is worth more than a framework,
so the running page does not use one.

This folder is the same page for a web front end that already has React. It is a
port, not the original: if the two ever disagree, `ui/index.html` is right.

## Using it

```jsx
import LaunchFeed from './ui/react/LaunchFeed';

<LaunchFeed api="http://localhost:4747" onCta={() => …} />
```

- `api` — where the STS server is. Leave it empty when the page is served from
  the same origin.
- `onCta` — what the white button does. It does nothing by default.

Styling is Tailwind plus `launch-feed.css`, which holds the colours and the three
things utility classes cannot say: the grain overlay, the hairline that lights up
along the top of a hovered row, and the keyframe a new row slides in on. No
Tailwind config is needed — every value is written inline.

## What it talks to

Two endpoints, both already in `src/dash.js`:

- `GET /api/feed?limit=40` — the launches that already happened, newest first,
  so the page is never blank when it opens. Refused launches are included.
- `GET /api/live` — the event stream. `launch` fires the instant a mint is
  created; `verdict` follows a few seconds later with what the structural checks
  made of it; `link` reports the state of the watcher's Solana socket.

A launch therefore arrives twice, and that is the point. The row appears saying
ANALYSING because at that moment nothing is known — there are no trades yet to
measure a deployer's share against. When there are, the row answers itself.

## The four answers in the last column

- **ANALYSING** — the read has not landed yet.
- **REJECTED** — refused on structure. The rule that refused it is named beside
  the tag, and the full sentence is on hover.
- **PASSED** — nothing refused it, and nothing recommends it. Most launches.
- **IMMEDIATE_LAUNCH** — refused by nothing and above the candidate bar.

Green and red appear nowhere else on the page. That is deliberate: if the other
columns were coloured too, these two would stop standing out, which is the only
thing the page is for.
