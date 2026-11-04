const fs = require("node:fs");

const literal = '"(?:\\\\.|[^"\\\\])*"';
const methods = ["child", "label", "placeholder", "title", "accessibility_label", "tooltip", "page_head"];
const notifications = ["success", "warning", "error", "info"];
for (const file of ["src/ui/mod.rs", "src/ui/pages.rs"]) {
  const source = fs.readFileSync(file, "utf8");
  const end = source.search(/\n#\[cfg\(test\)\]\r?\nmod /);
  let body = end < 0 ? source : source.slice(0, end);
  const suffix = end < 0 ? "" : source.slice(end);
  let replacements = 0;
  for (const method of methods) {
    const pattern = new RegExp(`(\\.${method}\\(\\s*)(${literal})(\\s*[,\\)])`, "g");
    body = body.replace(pattern, (match, before, value, after) => {
      if (!/[\u3400-\u9fff]/u.test(value)) return match;
      replacements++;
      return `${before}tr(${value})${after}`;
    });
  }
  for (const method of notifications) {
    const pattern = new RegExp(`(Notification::${method}\\(\\s*)(${literal})(\\s*[,\\)])`, "g");
    body = body.replace(pattern, (match, before, value, after) => {
      if (!/[\u3400-\u9fff]/u.test(value)) return match;
      replacements++;
      return `${before}tr(${value})${after}`;
    });
  }
  fs.writeFileSync(file, body + suffix);
  console.log(`${file}: ${replacements} display strings localized`);
}
