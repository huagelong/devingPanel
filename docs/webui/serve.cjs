const http = require("node:http");
const fs = require("node:fs");
const path = require("node:path");
const files = {
  "/": "index.html",
  "/index.html": "index.html",
  "/styles.css": "styles.css",
  "/app.js": "app.js",
  "/i18n.js": "i18n.js",
  "/i18n-en.js": "i18n-en.js",
  "/backup.js": "backup.js",
  "/backup-ui.js": "backup-ui.js",
  "/screen-lock.js": "screen-lock.js",
  "/js-yaml.min.js": "js-yaml.min.js",
  "/manifest.js": "manifest.js",
};
const types = {
  ".html": "text/html; charset=utf-8",
  ".css": "text/css; charset=utf-8",
  ".js": "text/javascript; charset=utf-8",
};
http
  .createServer((req, res) => {
    const file = files[new URL(req.url, "http://localhost").pathname];
    if (!file) {
      res.writeHead(404);
      res.end("Not found");
      return;
    }
    res.writeHead(200, {
      "Content-Type": types[path.extname(file)],
      "Cache-Control": "no-store",
    });
    fs.createReadStream(path.join(__dirname, file)).pipe(res);
  })
  .listen(Number(process.env.PORT) || 4173, "127.0.0.1", function () {
    console.log(`devingPanel prototype: http://127.0.0.1:${this.address().port}`);
  });
