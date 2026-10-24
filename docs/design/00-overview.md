# devingPanel UI 设计总览与主题规范

> 版本：v0.1
> 日期：2026-10-24
> 依据：docs/requirements.md
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

所有页面共用左侧固定导航 + 顶部栏 + 内容区结构：

```
+-----------+----------------------------------------------+
|           |  顶部栏: 面包屑        [Docker状态] [主题] |
| 侧边栏     +----------------------------------------------+
| (w-56)    |                                              |
|           |                                              |
| - 总览     |              内容区 (滚动)                    |
| - 主机     |   max-w-7xl, px-8 py-6                       |
| - 项目     |                                              |
| - 发布     |                                              |
| - 容器     |                                              |
| - 设置     |                                              |
+-----------+----------------------------------------------+
```

- 侧边栏：`w-56`，背景 `bg-background`，右侧 `border` 分隔；选中项 `bg-accent` + `text-accent-foreground` 圆角高亮。
- 顶部栏：`h-14`，`border-b`；左侧面包屑，右侧固定元素：本地 Docker 状态指示点、主题切换（浅色/深色/跟随系统）。
- 内容区：`max-w-7xl mx-auto px-8 py-6`，与文档布局一致。
- 发布进行中：顶部栏下沿叠加全局进度条（`h-0.5`，info 色），见 docs/design/04-deploy.md。

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

shadcn 默认主题不含 success / warning，按 Tailwind 标准色扩展（深浅色模式同值）：

| Token | 值 | 用途 |
|-------|-----|------|
| success | `#16a34a` (green-600) | 部署成功、容器 running、主机在线 |
| warning | `#d97706` (amber-600) | 健康检查中、磁盘告警 |
| info | `#2563eb` (blue-600) | 进行中（传输/构建）、链接 |
| destructive | 见 Token 表 | 失败、exited、删除操作 |

### 3.4 图表配色

监控图表（CPU / 内存 / 磁盘 / 网络）使用：CPU `#2563eb`、内存 `#9333ea`、磁盘 `#d97706`、网络 `#16a34a`；网格线用 `border` 色，图表背景用 `card` 色。

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
