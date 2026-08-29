// A static server for `ui/`.
//
// The window is loaded over HTTP rather than off `file://` for one reason:
// `app.js` is a module, and Chrome refuses a module script from a file origin.
// Tauri serves the same directory over its own protocol, so HTTP is the closer
// of the two to how the window actually runs.

import { createServer } from "node:http";
import { readFile } from "node:fs/promises";
import { extname, join, normalize } from "node:path";

const TYPES = {
  ".html": "text/html; charset=utf-8",
  ".css": "text/css; charset=utf-8",
  ".js": "text/javascript; charset=utf-8",
  ".mjs": "text/javascript; charset=utf-8",
  ".json": "application/json; charset=utf-8",
};

export async function serve(root) {
  const server = createServer(async (request, response) => {
    const url = new URL(request.url, "http://127.0.0.1");
    const path = url.pathname === "/" ? "/index.html" : url.pathname;

    // Normalised and re-rooted before it touches the disk. A test server that
    // will serve `../../../etc/passwd` is still a server.
    const resolved = join(root, normalize(path).replace(/^(\.\.[/\\])+/, ""));
    if (!resolved.startsWith(root)) {
      response.writeHead(403).end("no");
      return;
    }

    try {
      const body = await readFile(resolved);
      response.writeHead(200, {
        "content-type": TYPES[extname(resolved)] ?? "application/octet-stream",
        "cache-control": "no-store",
      });
      response.end(body);
    } catch {
      response.writeHead(404).end("not found");
    }
  });

  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  const { port } = server.address();
  return {
    origin: `http://127.0.0.1:${port}`,
    close: () => new Promise((resolve) => server.close(resolve)),
  };
}
