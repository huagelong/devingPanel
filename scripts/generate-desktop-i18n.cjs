const fs = require("node:fs");
const vm = require("node:vm");

const context = {};
vm.runInNewContext(fs.readFileSync("webui/i18n-en.js", "utf8") + "\nthis.messages = I18N_EN;", context);
const web = context.messages;
const output = "src/ui/i18n_en.json";
const saved = fs.existsSync(output) ? JSON.parse(fs.readFileSync(output, "utf8")) : {};
const strings = new Set();
function rustFiles(dir) {
  return fs.readdirSync(dir, { withFileTypes: true }).flatMap((entry) => {
    const path = `${dir}/${entry.name}`;
    return entry.isDirectory() ? rustFiles(path) : entry.name.endsWith(".rs") ? [path] : [];
  });
}
for (const file of rustFiles("src")) {
  const source = fs.readFileSync(file, "utf8").split(/\n#\[cfg\(test\)\]\r?\nmod /)[0];
  for (const match of source.matchAll(/"((?:\\.|[^"\\])*)"/g)) {
    if (/[\u3400-\u9fff]/u.test(match[1])) strings.add(match[1]);
  }
}

const messages = {};
for (const key of strings) {
  if (Object.hasOwn(saved, key)) messages[key] = saved[key];
  else if (Object.hasOwn(web, key)) messages[key] = web[key];
}

async function translate(key) {
  const placeholders = [];
  const masked = key.replace(/\{[^{}]*\}/g, (value) => {
    const marker = `ZXQPLACEHOLDER${placeholders.length}ZXQ`;
    placeholders.push(value);
    return marker;
  });
  const url = new URL("https://translate.googleapis.com/translate_a/single");
  for (const [name, value] of Object.entries({ client: "gtx", sl: "zh-CN", tl: "en", dt: "t", q: masked })) {
    url.searchParams.set(name, value);
  }
  for (let attempt = 0; attempt < 3; attempt++) {
    try {
      const response = await fetch(url, { signal: AbortSignal.timeout(15000) });
      if (!response.ok) throw new Error(`HTTP ${response.status}`);
      const data = await response.json();
      let english = data[0].map((part) => part[0]).join("");
      english = english.replace(/ZXQPLACEHOLDER(\d+)ZXQ/gi, (_, number) => placeholders[Number(number)] ?? "");
      if (/[\u3400-\u9fff]/u.test(english) || !english.trim()) throw new Error("incomplete translation");
      return english;
    } catch (error) {
      if (attempt === 2) throw new Error(`${key}: ${error.message}`);
      await new Promise((resolve) => setTimeout(resolve, 500 * (attempt + 1)));
    }
  }
}

async function main() {
  const missing = [...strings].filter((key) => !Object.hasOwn(messages, key));
  if (process.argv.includes("--count")) {
    console.log(`${strings.size} unique strings, ${missing.length} need translations`);
    return;
  }
  let next = 0;
  let completed = 0;
  const errors = [];
  async function worker() {
    while (next < missing.length) {
      const key = missing[next++];
      try {
        messages[key] = await translate(key);
      } catch (error) {
        errors.push(error.message);
      }
      completed++;
      if (completed % 25 === 0) {
        fs.writeFileSync(output, JSON.stringify(messages, null, 2) + "\n");
        console.log(`${completed}/${missing.length}`);
      }
    }
  }
  await Promise.all(Array.from({ length: 8 }, worker));
  for (const [key, value] of Object.entries(messages)) {
    let revised = value;
    if (key.includes("镜像") && !key.includes("镜像源") && !key.includes("镜像安装")) {
      revised = revised.replace(/\bmirrors\b/gi, "images").replace(/\bmirror\b/gi, "image")
        .replace(/\bmirroring\b/gi, "image");
    }
    if (key.includes("发布")) revised = revised.replace(/\bPosting\b/g, "Deployment");
    if (key.includes("仓库")) revised = revised.replace(/\bwarehouse\b/gi, "repository");
    messages[key] = revised;
  }
  const overrides = {
    "本机构建": "Local build",
    "镜像传输": "Image transfer",
    "镜像加载": "Image loading",
    "准备 Compose": "Prepare Compose",
    "健康检查": "Health check",
    "镜像传输 ": "Image transfer ",
    " 字节": " bytes",
    "传输镜像": "Transfer images",
    "发布中…": "Deploying…",
    "暂无镜像": "No images",
    "暂无容器": "No containers",
    "暂无主机": "No hosts",
    "已选 {} 台": "{} hosts selected",
    "预览可清理版本": "Preview versions to clean",
    "确认清理以上版本": "Confirm cleanup",
    "按每项目 / 每个 Docker daemon 的确认版本链计算保留数。": "Retention is based on the confirmed version chain for each project and Docker daemon.",
    "保留数须为 1～100。": "Keep count must be between 1 and 100.",
    "主机已保存；请从主机列表测试 SSH 和 Docker": "Host saved. Test SSH and Docker from the host list.",
    "构建镜像：": "Build image: ",
    "远端加载镜像：": "Load remote image: ",
    "清理中，请保持客户端运行…": "Cleaning up. Keep the client running…",
  };
  Object.assign(messages, overrides);
  fs.writeFileSync(output, JSON.stringify(messages, null, 2) + "\n");
  console.log(`Translated ${Object.keys(messages).length}/${strings.size} strings`);
  if (errors.length) {
    console.error(errors.join("\n"));
    process.exitCode = 1;
  }
}

main().catch((error) => { console.error(error); process.exitCode = 1; });
