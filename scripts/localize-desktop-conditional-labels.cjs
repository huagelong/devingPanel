const fs = require("node:fs");

function closingParen(source, open) {
  let depth = 0;
  let quoted = false;
  for (let index = open; index < source.length; index++) {
    const char = source[index];
    if (quoted) {
      if (char === "\\") index++;
      else if (char === '"') quoted = false;
    } else if (char === '"') quoted = true;
    else if (char === "(") depth++;
    else if (char === ")" && --depth === 0) return index;
  }
  throw new Error("Unclosed call");
}

const file = "src/ui/pages.rs";
let source = fs.readFileSync(file, "utf8");
const end = source.search(/\n#\[cfg\(test\)\]\r?\nmod /);
let body = end < 0 ? source : source.slice(0, end);
const suffix = end < 0 ? "" : source.slice(end);
let cursor = 0;
let count = 0;
while (true) {
  const match = /\.(?:label|tooltip|title)\(if\b/g;
  match.lastIndex = cursor;
  const found = match.exec(body);
  if (!found) break;
  const open = body.indexOf("(", found.index);
  const close = closingParen(body, open);
  const expression = body.slice(open + 1, close);
  const localized = expression.replace(/"((?:\\.|[^"\\])*)"/g, (literal, value, offset) => {
    if (!/[\u3400-\u9fff]/u.test(value)) return literal;
    if (/(?:\btr\(|\btfmt!\()\s*$/.test(expression.slice(0, offset))) return literal;
    count++;
    return `tr(${literal})`;
  });
  body = body.slice(0, open + 1) + localized + body.slice(close);
  cursor = open + 1 + localized.length;
}
fs.writeFileSync(file, body + suffix);
console.log(`${count} conditional label options localized`);
