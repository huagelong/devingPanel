const fs = require("node:fs");
const vm = require("node:vm");

const context = {};
vm.runInNewContext(fs.readFileSync("webui/i18n-en.js", "utf8") + "\nthis.messages = I18N_EN;", context);
const dictionary = context.messages;
const strings = new Set();
for (const file of ["src/ui/mod.rs", "src/ui/pages.rs"]) {
  const source = fs.readFileSync(file, "utf8");
  for (const match of source.matchAll(/"((?:\\.|[^"\\])*)"/g)) {
    if (/[\u3400-\u9fff]/u.test(match[1])) strings.add(match[1]);
  }
}
const missing = [...strings].filter((message) => !Object.hasOwn(dictionary, message));
console.log(`Unique desktop strings: ${strings.size}`);
console.log(`Covered by WebUI: ${strings.size - missing.length}`);
console.log(`Missing: ${missing.length}`);
for (const message of missing) console.log(message);
