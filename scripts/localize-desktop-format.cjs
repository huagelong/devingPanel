const fs = require("node:fs");

const targets = [
  /\.(?:child|label|title|placeholder|accessibility_label|page_head)\(\s*format!\(\s*"[^"\r\n]*[\u3400-\u9fff][^"\r\n]*"/gu,
  /Notification::(?:success|warning|error|info)\(\s*format!\(\s*"[^"\r\n]*[\u3400-\u9fff][^"\r\n]*"/gu,
];
for (const file of ["src/ui/mod.rs", "src/ui/pages.rs"]) {
  const source = fs.readFileSync(file, "utf8");
  const end = source.search(/\n#\[cfg\(test\)\]\r?\nmod /);
  let body = end < 0 ? source : source.slice(0, end);
  const suffix = end < 0 ? "" : source.slice(end);
  let replacements = 0;
  for (const pattern of targets) {
    body = body.replace(pattern, (match) => {
      replacements++;
      return match.replace("format!", "tfmt!");
    });
  }
  fs.writeFileSync(file, body + suffix);
  console.log(`${file}: ${replacements} formatted display strings localized`);
}
