# 项目初始化

本文件是 Codex 的项目环境初始化清单。需要初始化时，在本仓库中告诉 Codex：

> 请读取 `PROJECT_INIT.md` 并按其中步骤初始化项目。

Codex 应从仓库根目录执行本文件中的安装项。仅在用户要求初始化时执行；每项先检查是否已安装，已安装则跳过，失败时报告具体命令和错误。以后需要增加 Skill 或工具时，在对应章节增加独立条目，写清安装条件、命令和验收方法。

## 基础环境

在 PowerShell 中执行：

```powershell
git --version
node --version
npm --version
```

需要 Git 2.41 或更高版本，以及可用的 Node.js、npm 和网络连接。缺少基础工具时先安装，再继续后续步骤。

## 工具

### OCR 命令行工具

`open-code-review-delegate` 依赖 `ocr` 命令。如果 `Get-Command ocr -ErrorAction SilentlyContinue` 没有结果，执行：

```powershell
npm install -g @alibaba-group/open-code-review
```

执行 `ocr --version`，确认命令可用。不需要运行 `ocr config provider` 或 `ocr config model`。

## Skills

### open-code-review-delegate

来源：[`alibaba/open-code-review`](https://github.com/alibaba/open-code-review/tree/main/skills/open-code-review-delegate)。它使用当前 Codex 的模型执行审查，无需为 OCR 单独配置模型凭据。

如果 `.codex/skills/open-code-review-delegate/SKILL.md` 尚不存在，在仓库根目录执行：

```powershell
npx --yes skills add alibaba/open-code-review --skill open-code-review-delegate --agent codex --yes --copy
```

此命令安装项目级 Skill。不要选择同仓库的 `open-code-review`，它使用 OCR 单独配置的模型提供方。若目标目录已经存在，不要覆盖其中内容。

## 初始化验收

```powershell
Test-Path .codex/skills/open-code-review-delegate/SKILL.md
ocr delegate preview --format json
```

`Test-Path` 应返回 `True`，预览命令应成功返回当前仓库的审查文件信息。工作区没有改动时，预览结果可以为空。完成后逐项向用户报告安装和验证结果；新安装的 Skill 通常在新的 Codex 任务中可用。
