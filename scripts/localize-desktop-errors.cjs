const fs = require("node:fs");

for (const file of ["src/ui/mod.rs", "src/ui/pages.rs"]) {
  const source = fs.readFileSync(file, "utf8");
  const end = source.search(/\n#\[cfg\(test\)\]\r?\nmod /);
  let body = end < 0 ? source : source.slice(0, end);
  const suffix = end < 0 ? "" : source.slice(end);
  const before = body;
  body = body.replace(/Notification::(error|warning)\(error\)/g, "Notification::$1(tr_system(&error))");
  body = body.replace(/\.child\(error\.clone\(\)\)/g, ".child(tr_system(error))");
  body = body.replace(/\bsystem\((?=&error|error)/g, "tr_system(");
  fs.writeFileSync(file, body + suffix);
  console.log(`${file}: ${before === body ? "unchanged" : "error displays localized"}`);
}
