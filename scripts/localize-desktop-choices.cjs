const fs = require("node:fs");

for (const file of ["src/ui/mod.rs", "src/ui/pages.rs"]) {
  const source = fs.readFileSync(file, "utf8");
  const end = source.search(/\n#\[cfg\(test\)\]\r?\nmod /);
  let body = end < 0 ? source : source.slice(0, end);
  const suffix = end < 0 ? "" : source.slice(end);
  let count = 0;
  body = body.replace(/(\.(?:label|title|child)\()if ([^{\r\n]+)\{ ("[^"\r\n]*[\u3400-\u9fff][^"\r\n]*") \} else \{ ("[^"\r\n]*[\u3400-\u9fff][^"\r\n]*") \}(\))/gu,
    (_, start, condition, yes, no, close) => {
      count++;
      return `${start}if ${condition}{ tr(${yes}) } else { tr(${no}) }${close}`;
    });
  body = body.replace(/\(("[^"\r\n]*[\u3400-\u9fff][^"\r\n]*"), (self\.palette\(\)\.[a-z_]+)\)/gu,
    (_, label, color) => {
      count++;
      return `(tr(${label}), ${color})`;
    });
  fs.writeFileSync(file, body + suffix);
  console.log(`${file}: ${count} binary labels localized`);
}
