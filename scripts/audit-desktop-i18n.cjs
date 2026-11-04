const fs = require("node:fs");

for (const file of ["src/ui/mod.rs", "src/ui/pages.rs"]) {
  const source = fs.readFileSync(file, "utf8");
  const end = source.search(/\n#\[cfg\(test\)\]\r?\nmod /);
  const body = end < 0 ? source : source.slice(0, end);
  const lines = body.split(/\r?\n/);
  let count = 0;
  for (let index = 0; index < lines.length; index++) {
    const line = lines[index];
    for (const match of line.matchAll(/"((?:\\.|[^"\\])*)"/g)) {
      if (!/[\u3400-\u9fff]/u.test(match[1])) continue;
      const before = line.slice(0, match.index);
      if (/(?:\btr\(|\btfmt!\()\s*$/.test(before)) continue;
      count++;
      console.log(`${file}:${index + 1}: ${line.trim()}`);
    }
  }
  console.error(`${file}: ${count} literal occurrences require review`);
}
