# 项目初始化

本文件是 Codex 的项目环境初始化清单。需要初始化时，在本仓库中告诉 Codex：

> 请读取 `PROJECT_INIT.md` 并按其中步骤初始化项目。

Codex 应从仓库根目录执行本文件中的安装项。仅在用户要求初始化时执行；每项先检查是否已安装，完整安装则跳过，部分安装先核对缺失项，失败时报告具体命令和错误。以后需要增加 Skill 或工具时，在对应章节增加独立条目，写清安装条件、命令和验收方法。

## 基础环境

在 PowerShell 中执行：

```powershell
git --version
node --version
npm --version
```

需要 Git 2.41 或更高版本、Node.js 18.19 或更高版本，以及可用的 npm 和网络连接。缺少基础工具时先安装，再继续后续步骤。

## 工具

### OCR 命令行工具

`open-code-review-delegate` 依赖 `ocr` 命令。如果 `Get-Command ocr -ErrorAction SilentlyContinue` 没有结果，执行：

```powershell
npm install -g @alibaba-group/open-code-review
```

执行 `ocr --version`，确认命令可用。不需要运行 `ocr config provider` 或 `ocr config model`。

## Skills

安装前检查项目级和用户级目录，记录已有 Skill 的路径和版本，避免重复安装：

| 范围 | 检查目录 |
| --- | --- |
| 项目级 | `.codex/skills`、`.agents/skills` |
| 用户级 | `$CODEX_HOME/skills`（未设置时为 `$HOME/.codex/skills`）、`$HOME/.agents/skills` |

`open-code-review-delegate` 和 Superpowers 可复用已有的用户级安装；缺失时按下文安装到项目。本项目的 Impeccable 按 `AGENTS.md` 使用用户级安装。若只发现项目级 Impeccable，先核对版本和本地改动，再统一安装范围。已有文件不直接覆盖，初始化也不自动升级 Skill 或启用 hooks。

### open-code-review-delegate

来源：[`alibaba/open-code-review`](https://github.com/alibaba/open-code-review/tree/main/skills/open-code-review-delegate)。它使用当前 Codex 的模型执行审查，无需为 OCR 单独配置模型凭据。

如果上述目录中均未找到 `open-code-review-delegate/SKILL.md`，在仓库根目录执行：

```powershell
npx --yes skills add alibaba/open-code-review --skill open-code-review-delegate --agent codex --yes --copy
```

此命令安装项目级 Skill。不要选择同仓库的 `open-code-review`，它使用 OCR 单独配置的模型提供方。若目标目录已经存在，不要覆盖其中内容。

### Superpowers

来源：[`obra/superpowers`](https://github.com/obra/superpowers)。该仓库提供一组开发流程 Skills；本项目的初始化清单包含其全部 Skills。

检查上述目录中的 `using-superpowers/SKILL.md` 和其他 Superpowers Skills。尚未安装时，在仓库根目录执行：

```powershell
npx --yes skills add obra/superpowers --skill '*' --agent codex --yes --copy
```

此命令安装项目级 Skills。若已有部分 Superpowers Skills，先核对完整清单、缺失项和版本，再补齐，不能仅凭 `using-superpowers` 入口存在就判定安装完整，也不要直接重跑命令覆盖现有文件。使用时仍须遵守本项目 `AGENTS.md` 的首次需求只分析、分支管理和审查要求。

### Impeccable

来源：[`pbakaus/impeccable`](https://github.com/pbakaus/impeccable)。它提供界面设计与检查能力，使用官方安装器安装 Codex 的用户级 Skill。

如果用户级目录中没有 `impeccable/SKILL.md`，且已核对项目级目录，在仓库根目录执行：

```powershell
npx --yes impeccable install --providers=codex --scope=global --no-hooks
```

安装后记录实际目录，Windows 下通过该目录的 `scripts/impeccable.cmd` 执行工具。使用范围和设计规范以 `AGENTS.md` 为准。安装不运行 `$impeccable init` 或 `$impeccable document`；只有用户明确要求建立或整理相应文档时，才分别生成 `PRODUCT.md` 或 `DESIGN.md`。

## 初始化验收

在 PowerShell 中检查安装路径：

```powershell
$taskCodexHome = if ($env:CODEX_HOME) { $env:CODEX_HOME } else { Join-Path $HOME '.codex' }
$taskUserSkillRoots = @(
    (Join-Path $taskCodexHome 'skills')
    (Join-Path $HOME '.agents/skills')
)
$taskSkillRoots = @('.codex/skills', '.agents/skills') + $taskUserSkillRoots

foreach ($taskSkill in @('open-code-review-delegate', 'using-superpowers', 'impeccable')) {
    $taskRootsToCheck = if ($taskSkill -eq 'impeccable') { $taskUserSkillRoots } else { $taskSkillRoots }
    $taskSkillPaths = @($taskRootsToCheck | ForEach-Object {
        $taskSkillPath = Join-Path $_ "$taskSkill/SKILL.md"
        if (Test-Path -LiteralPath $taskSkillPath -PathType Leaf) { $taskSkillPath }
    })
    [PSCustomObject]@{
        Skill = $taskSkill
        Installed = $taskSkillPaths.Count -gt 0
        Paths = $taskSkillPaths -join '; '
    }
}

ocr --version
ocr delegate preview --format json
```

三项 `Installed` 都应为 `True`，并显示实际路径；其中 `using-superpowers` 只验证入口，仍需核对完整 Skill 清单。出现多个同名安装时，核对版本和本地改动，确认使用哪一份。

OCR 版本和预览命令应成功，预览结果应返回当前工作区的可审查文件和排除文件信息；没有可审查的变更时，可审查列表可以为空。通过 Impeccable 实际安装目录下的启动器执行 `--version` 和 `--help`，确认命令可用。新安装的 Skills 需要在新的 Codex 会话中确认能被发现和读取。

完成后逐项报告安装、跳过和验证结果，包括 Skill 路径、版本及尚未通过的检查。仅修改本清单不代表已执行初始化。
