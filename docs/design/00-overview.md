# devingPanel UI 设计总览与主题规范

> 版本：v0.1
> 日期：2026-10-24
> 说明：本文档是所有页面设计文档的公共规范。各页面布局以 docs/design/01~06 为准，布局实现必须与文档一致；颜色样式一律使用本文定义的 shadcn 默认主题 Token，不得使用硬编码色值。

---

## 1. 设计原则

1. **shadcn 默认主题**：中性色（neutral/zinc）基调，黑白色为主，仅用状态色表达语义，不引入品牌彩色的渐变与装饰。
2. **信息密度优先**：面向开发者工具，使用紧凑表格与卡片，不做营销式大留白。
3. **一键为主**：核心流程（发布）的操作入口永远可见、一步可达。
4. **状态明确**：异步操作（构建 / 传输 / 部署）必须实时反馈进度与结果。

---

## 2. 信息架构与导航

### 2.1 页面清单

| 编号 | 页面 | 文档 | 优先级 |
|------|------|------|--------|
| P1 | 总览 Dashboard | docs/design/01-dashboard.md | P0 |
| P2 | 主机列表 / 主机详情（含监控） | docs/design/02-hosts.md | P0（监控 P1） |
| P3 | 项目列表 / 项目详情 | docs/design/03-projects.md | P0 |
| P4 | 发布执行 / 发布历史 | docs/design/04-deploy.md | P0 |
| P5 | 容器管理（容器 / 镜像 / 清理） | docs/design/05-containers.md | P1 |
| P6 | 设置 | docs/design/06-settings.md | P0 |

### 2.2 全局布局骨架（App Shell）

所有页面共用横跨整个窗口的顶部栏、其下方的左侧固定导航和内容区：

```
+----------------------------------------------------------+
| Logo / devingPanel     [Docker状态] [锁屏] [主题]          |
+-----------+----------------------------------------------+
| 侧边栏     |                                              |
| (w-56)    |              内容区 (滚动)                    |
| - 总览     |   max-w-7xl, px-8 py-6                       |
| - 主机     |                                              |
| - 项目     |                                              |
| - 发布     |                                              |
| - 容器     |                                              |
| - 设置     |                                              |
+-----------+----------------------------------------------+
```

- 侧边栏：`w-56`，从顶部栏下方开始，背景 `bg-background`，右侧 `border` 分隔；选中项 `bg-accent` + `text-accent-foreground` 圆角高亮。
- 顶部栏：横跨整个窗口，`h-14`、`border-b`；左侧 Logo 与应用名，窄屏时在 Logo 前显示导航菜单按钮；右侧显示本地 Docker 状态、启用锁屏后的手动锁屏按钮和主题快捷切换。页面名称只在内容区页头显示，不在顶部栏重复。顶部按钮在浅色与深色之间切换；“跟随系统”在设置的外观子页选择。
- 本机 Docker 不可用时，内容区显示安装引导、重新检测和原型预览选项；主导航暂不可用。WebUI 的检测状态由预览选项模拟，正式客户端须以本机检测结果决定可用性。
- Windows 桌面客户端将 Docker 状态和主题切换放在窗口标题栏右侧，位于窗口控制按钮左侧；内容区页头显示页面标题。窄窗口标题栏仅显示 Docker 状态点。其他平台及 WebUI 原型仍采用上图布局。
- 内容区：`max-w-7xl mx-auto px-8 py-6`，与文档布局一致。
- 发布进行中：顶部栏下沿叠加全局进度条（`h-0.5`，info 色），见 docs/design/04-deploy.md。
- 窄窗口：侧边栏折叠为顶部菜单，内容区改为 `px-4`，宽表格允许区块内横向滚动；发布配置条换行，按钮始终可见。桌面窗口最小宽度在实现阶段确定。

---

## 3. 颜色 Token（shadcn 默认主题）

以 HSL CSS 变量定义，浅色 / 深色两套。Tailwind 类名直接映射（如 `bg-primary`、`text-muted-foreground`）。

### 3.1 浅色主题（默认）

```css
--background: 0 0% 100%;            /* 页面背景 */
--foreground: 240 10% 3.9%;         /* 主文字 */
--card: 0 0% 100%;                  /* 卡片背景 */
--card-foreground: 240 10% 3.9%;
--popover: 0 0% 100%;
--popover-foreground: 240 10% 3.9%;
--primary: 240 5.9% 10%;            /* 主按钮：近黑 */
--primary-foreground: 0 0% 98%;
--secondary: 240 4.8% 95.9%;        /* 次按钮 */
--secondary-foreground: 240 5.9% 10%;
--muted: 240 4.8% 95.9%;            /* 弱背景 */
--muted-foreground: 240 3.8% 46.1%; /* 次要文字 */
--accent: 240 4.8% 95.9%;           /* 悬停/选中背景 */
--accent-foreground: 240 5.9% 10%;
--destructive: 0 84.2% 60.2%;       /* 危险操作 */
--destructive-foreground: 0 0% 98%;
--border: 240 5.9% 90%;
--input: 240 5.9% 90%;
--ring: 240 5.9% 10%;
--radius: 0.5rem;
```

### 3.2 深色主题

```css
--background: 240 10% 3.9%;
--foreground: 0 0% 98%;
--card: 240 10% 3.9%;
--card-foreground: 0 0% 98%;
--popover: 240 10% 3.9%;
--popover-foreground: 0 0% 98%;
--primary: 0 0% 98%;
--primary-foreground: 240 5.9% 10%;
--secondary: 240 3.7% 15.9%;
--secondary-foreground: 0 0% 98%;
--muted: 240 3.7% 15.9%;
--muted-foreground: 240 5% 64.9%;
--accent: 240 3.7% 15.9%;
--accent-foreground: 0 0% 98%;
--destructive: 0 62.8% 30.6%;
--destructive-foreground: 0 0% 98%;
--border: 240 3.7% 15.9%;
--input: 240 3.7% 15.9%;
--ring: 240 4.9% 83.9%;
```

### 3.3 语义状态色扩展

shadcn 默认主题不含 success / warning，扩展为语义 Token。以下为浅色主题建议值，深色模式单独配置合适的对比度；组件只能使用 Token，不能直接写色值：

| Token | 值 | 用途 |
|-------|-----|------|
| success | green-600 | 部署成功、容器 running、主机在线 |
| warning | amber-600 | 健康检查中、磁盘告警 |
| info | blue-600 | 进行中（传输/构建）、链接 |
| destructive | 见 Token 表 | 失败、exited、删除操作 |

扩展 Token 的参考值（Tailwind 颜色可直接在主题层复用；组件使用 `text-success` 等语义类）：

```css
:root {
  --success: 142.1 76.2% 36.3%;
  --warning: 32.1 94.6% 43.7%;
  --info: 221.2 83.2% 53.3%;
}
.dark {
  --success: 142.1 70.6% 45.3%;
  --warning: 43 96% 56%;
  --info: 217 91% 60%;
}
```

### 3.4 图表配色

监控图表（CPU / 内存 / 磁盘 / 网络）使用 `chart-1` / `chart-2` / `chart-3` / `chart-4` 语义 Token（分别以 blue-600 / violet-600 / amber-600 / green-600 为浅色参考），深色主题调整亮度以满足可读性；网格线用 `border`，背景用 `card`。

```css
:root {
  --chart-1: 221.2 83.2% 53.3%;
  --chart-2: 262.1 83.3% 57.8%;
  --chart-3: 32.1 94.6% 43.7%;
  --chart-4: 142.1 76.2% 36.3%;
}
.dark {
  --chart-1: 217 91% 60%;
  --chart-2: 263 70% 70%;
  --chart-3: 43 96% 56%;
  --chart-4: 142.1 70.6% 45.3%;
}
```

---

## 4. 字体与间距

- 界面字体：系统 UI 栈（`Inter` / `system-ui`）；代码 / 日志 / 镜像名：等宽字体（`JetBrains Mono` / `ui-monospace`）。
- 字号：页面标题 `text-2xl font-semibold tracking-tight`；区块标题 `text-lg font-semibold`；正文 `text-sm`；辅助说明 `text-sm text-muted-foreground`；表格 `text-sm`。
- 间距：页面级区块间 `space-y-6`；卡片内 `p-6`，紧凑卡片 `p-4`；表单行间 `space-y-4`。
- 圆角：统一 `rounded-lg`（radius 0.5rem）；Badge 用 `rounded-full`。

---

## 5. 通用组件约定

| 组件 | 用法约定 |
|------|----------|
| Button | 主操作 `variant=default`；次操作 `variant=outline`；危险操作 `variant=destructive`；图标按钮 `variant=ghost size=icon` |
| Card | 页面区块容器，`CardHeader` 放标题 + 说明，`CardContent` 放内容 |
| Table | 数据列表统一用 Table；行悬停 `hover:bg-muted/50`；操作列右对齐 |
| Dialog | 新建 / 编辑 / 确认类操作；宽度默认 `sm:max-w-lg`，表单复杂的用 `sm:max-w-2xl` |
| Sheet | 容器日志、主机详情等侧滑内容，右侧滑出 `w-[480px]` 或 `sm:max-w-2xl` |
| Tabs | 详情页内的多区块切换 |
| Badge | 状态标签，带彩色圆点；文字 `text-xs` |
| DropdownMenu | 表格行内"更多"操作 |
| AlertDialog | 删除 / 重启等破坏性操作二次确认 |
| Toast | 操作结果反馈，右下角，成功 success 色 / 失败 destructive 色 |
| Progress | 传输进度 |
| Skeleton | 列表 / 卡片加载态 |
| EmptyState | 图标 + 一句话说明 + 主操作按钮，居中于内容区 |

### 5.1 状态 Badge 样式

```
<Badge> = 圆点(8px) + 文字, 背景 = 对应色 10% 透明度, 文字 = 对应色
在线/成功: success   进行中(构建/传输/部署): info   健康检查中/待完善: warning
队列等待/未配置: muted-foreground + border   离线/失败/停止: destructive
```

---

## 6. 异步流程与状态页约定

每个数据页面必须定义四种状态：

1. **加载态**：Skeleton 占位（表格用 5 行骨架，卡片用 3 个骨架卡）。
2. **空态**：EmptyState，主按钮引导到创建动作（如"添加主机"）。
3. **错误态**：Card 内居中错误说明 + "重试" 按钮（`variant=outline`）。
4. **操作反馈**：所有写操作完成后 Toast 反馈；失败 Toast 必须带可读原因。

发布 / 构建等长任务的状态规则见 docs/design/04-deploy.md。

## 7. 多语言（2026-10-25）

- 支持简体中文与英语，默认中文；入口为设置 → 外观 → 语言。
- 导航、标题、表单标签/占位符、弹窗、操作按钮、状态、错误反馈及备份加密流程均按选定语言显示；英文长文案允许自然换行，不能引起整页水平溢出。
- 用户提供的项目/主机/分组名、路径、镜像引用、输入值与程序日志保留原文。业务枚举、路由、表单选项值和备份格式不因显示语言而改变。
- 启用本机锁屏后，顶栏出现手动锁屏图标按钮；锁屏是全屏模态覆盖，复用主题 Token，不让底层设置或发布按钮在锁定状态下可操作。细节见 `docs/design/06-settings.md` 第 6 节。
- 复用集中词典与翻译函数，只翻译界面字面文案；模板插值不自动扫描，不能对整个 DOM 或包含用户数据的整段 HTML 做替换。
