# UI DESIGN

> 根据当前仓库反向整理。2026-11-27 布局更新。本文描述已落地的 WebUI 原型及其设计文档，不代表尚未实现的正式客户端已经满足这些规则。

## 1. 目的、范围与维护

本文供维护 `docs/webui` 和后续客户端界面时判断复用、变体与例外。当前项目的页面规范以对应的 `docs/design` 为准，运行代码用于核实落地情况，`DESIGN.md` 提供跨端默认基线；发现差异时先记录并核对，不默默改写事实。新增页面或例外完成后，核对原型、`docs/design` 对应页和本文；正式客户端界面还须与原型逐页对照。

**审计范围**：检查了 `docs/webui/index.html`、`app.js`、`styles.css`、`i18n*.js`、`backup-ui.js`、`screen-lock.js`、`package.json`、`README.md`、主要 Playwright 测试，以及 `docs/design/00-overview.md` 至 `06-settings.md` 和 `DESIGN.md`。覆盖总览、主机列表/详情与编辑、项目列表/详情与配置、发布执行/历史、容器/日志、设置及加载、空、错误、Docker 不可用、锁屏状态。未发现正式客户端源码、独立图片/字体资源、Tailwind 配置或组件库源码；本次没有逐一验证所有备份/加密分支的视觉和真实设备辅助技术表现。

## 2. 产品界面特征与设计优先级

- [已确认] 当前项目是本地构建和远端运行的管理工具原型。主要任务按“看状态 → 找对象 → 配置/发布 → 核对结果”组织；总览直接进入发布，列表与详情保留状态、操作和历史入口。新增页面优先保证对象身份、状态、原因和下一步可见，再处理装饰。证据：`docs/webui/README.md`“页面”“原型边界”，`docs/webui/app.js` 的 `dashboard`、`deployPage`，`docs/design/00-overview.md` 第 1–2 节。
- [已确认] 原型数据、日志和远端操作须与真实结果区分；现有原型在侧栏和相关页面标明演示边界。新增演示状态也需标明来源，不能将动画或模拟数值写成执行成功。证据：`docs/webui/app.js` 的 `render`、`dockerSetup`，`docs/webui/README.md`“原型边界”，`docs/design/04-deploy.md` 第 3、5 节。

## 3. 已确认原则、非目标与禁止项

- [已确认] 原型以语义颜色变量表达背景、前景、状态和图表；新增原型样式先查变量及已有类，不能为单页重复定义同义颜色。证据：`docs/webui/styles.css` 的 `:root`、`:root.dark`、`.badge`、`.progress`，`docs/design/00-overview.md` 第 3 节。
- [已确认] 页面按操作任务组织：页头给标题和主操作，列表/详情按信息组呈现，长任务显示逐目标状态与可展开日志。不要把发布结果简化成单一进度数字。证据：`docs/webui/app.js` 的 `head`、`hostsPage`、`projectDetail`、`deployPage`，`docs/design/04-deploy.md` 第 2–4 节。
- [已确认] 当前项目没有营销首页；首页是可操作的总览。不要把 `DESIGN.md` 的跨端默认组件清单当作当前 WebUI 已安装的组件库。证据：`docs/webui/app.js` 的 `dashboard`，`docs/webui/package.json`，`DESIGN.md`“组件规范”。

## 4. 页面骨架、容器与信息层级

```text
宽屏： [56px 顶栏：标识 | Docker 状态、锁屏、主题]
        [224px 侧栏] [页头/操作 → 筛选或 Tabs → 内容/表格/详情]
窄屏： [顶栏：菜单 | 状态点、主题]
        [页头/操作换行 → 单列内容；表格行显示字段名与操作]
```

- [已确认] `index.html` 只有一套 App Shell：`#topbar`、`#sidebar`、`main#content`、原生 `dialog`/`sheet` 与 `#toasts`。顶栏高 56px，侧栏宽 224px；主区最大宽 1440px，宽屏内边距为上下 24px、左右 32px，直接子区块间 24px。新原型页用 `head` 生成页头。证据：`docs/webui/index.html`，`docs/webui/styles.css` 的 `#topbar`、`#sidebar`、`.workspace`、`main`，`docs/webui/app.js` 的 `head`。
- [合法变体] `settingsPage` 在主区内另用 184px 分类导航和单个分类内容；低于 1024px 时用“设置分类”Select 替代。它是设置页内部导航，不取代全局侧栏。证据：`docs/webui/app.js` 的 `settingsPage`，`docs/webui/styles.css` 的 `.settings-layout`、`.settings-mobile` 和 1023px 媒体查询。
- [已确认] 总览使用活动主列与 288px 主机辅助列，1279px 及以下顺序排列；指标使用共用读数带。项目配置使用字段区与 224px 摘要，1279px 及以下隐藏辅助摘要。资源表格容器不足 1080px，或窗口不超过 1023px 时，复用同一 DOM 排成带字段名的记录。证据：`docs/webui/app.js` 的 `dashboard`、`projectDetail`、`refreshLayout`；`styles.css` 的 `.overview-layout`、`.project-workspace`、`.metric-strip`、`.table-wrap.records`。

## 5. Token 与视觉尺寸

- [已确认] 原型颜色源为 `styles.css` 的 HSL 变量。浅色核心值：`--background` 0 0% 100%，`--foreground` 240 10% 3.9%，`--primary` 240 5.9% 10%，`--border` 240 5.9% 90%、`--input` 240 4% 55%；深色值由 `:root.dark` 对应替换。`--card`/`--popover` 与各自 `-foreground` 成对使用；状态取 `--success`、`--warning`、`--info`、`--destructive-text`（危险按钮背景用 `--destructive`），图表用 `--chart-1` 至 `--chart-4`。证据：`docs/webui/styles.css` 的 `:root`、`:root.dark`。
- [已确认] 原型字体是 `14px/1.5 Inter, system-ui, sans-serif`，代码/日志用 `JetBrains Mono, ui-monospace, SFMono-Regular, Consolas, monospace`。页标题 24px/600，区块标题默认 18px/600，工作区的 section 标题为 16px/600，三级标题 14px/600，辅助字 12px；未加载独立字体文件，实际显示依赖本机字体。证据：`docs/webui/styles.css` 的 `body`、`h1`–`h3`、`small`、`.mono`，`docs/webui/index.html`。
- [已确认] 主要间距按语义使用：页面区块 24px、普通堆叠/栅格 16px、卡片头和内容 24px、紧凑卡片 16px、表格单元格上下 12px/左右 16px。半径变量 `--radius: 0.5rem` 用于卡片/导航/弹窗；按钮和输入实际为 6px；边框通常 1px。弹出菜单和 Toast 使用阴影，普通对象用边框分层，连续配置区以间距和分隔线分组。Tabs 用下划线表示选中。证据：`docs/webui/styles.css` 的 `main`、`.stack`、`.grid`、`.card`、`.compact`、`th,td`、`.menu-list`、`.toast`。
- [合法变体] 交互尺寸随用途变化：普通按钮/输入/Select 最小高 36px，图标按钮固定宽 36px；Tabs 项 40px；主机编辑弹窗内字段压到 28px，是局部紧凑表单。复制该紧凑尺寸到其他页面前需单独确认可用性。证据：`docs/webui/styles.css` 的 `button`、`.icon-btn`、`.tabs button`、`.host-dialog input`。
- [合法变体] 状态徽标用 12px 字、8px 圆点、背景为对应状态色 10% 透明度；`muted` 用边框而非状态填充。进度条普通高 6px，顶栏全局发布进度高 2px。证据：`docs/webui/styles.css` 的 `.badge`、`.dot`、`.progress`、`.global-progress`，`docs/webui/app.js` 的 `badge`、`progress`。
- [已确认] 当前层级的显式值是侧栏 20、顶栏 30、菜单打开时 5/30、Toast 100；模态由原生 `<dialog>.showModal()` 进入浏览器顶层，不以这些数值推断所有浮层排序。证据：`docs/webui/styles.css` 的 `#sidebar`、`#topbar`、`.menu[open]`、`#toasts`，`docs/webui/app.js` 的 `modal`。

## 6. 公共组件、状态与复用边界

公共 UI 目前是 `app.js` 中的 HTML 渲染函数加 `styles.css` 类，不是 React/Vue 组件包。修改基础表现时检查全部调用页。

| 组件与证据 | 何时使用、合法变体 | 关键状态与避免的替代写法 |
| --- | --- | --- |
| [已确认] `button`/`ib`/`go`；`docs/webui/app.js`，`docs/webui/styles.css` 的 `button`、`.primary`、`.danger`、`.ghost` | 主命令用 `primary`，危险确认用 `danger`，次要操作用默认/`ghost`，链接导航用 `go`；图标命令用 `ib` | 保留 hover、focus-visible、disabled；图标按钮需名称与 title。不要为同一命令另写按钮 HTML 或无名图标。 |
| [已确认] `field`/`input`/`select`；`docs/webui/app.js`，`docs/webui/styles.css` 的 `.field`、`input`、`select` | 表单字段复用标签、控件、帮助文字；筛选区 Select 可用 `.inline-select`；多行用 `textarea` | 保留可见标签、焦点和禁用说明；不要以 placeholder 替代标签。项目配置提供持续错误汇总及 `aria-invalid`/`aria-describedby` 字段关联；其他表单沿用已有校验。 |
| [已确认] `card`/`head`；`docs/webui/app.js`，`docs/webui/styles.css` 的 `.card`、`.page-head` | `head` 放页名和操作；`card` 放有独立标题/边界的信息组，`editorSection` 用于无外层边框的配置区，资源表格不强制套边框 | 卡片有边框、8px 半径；不要复制一个局部页头/卡片样式作为新公共能力。 |
| [已确认] `table`/`row`；`docs/webui/app.js`，`docs/webui/styles.css` 的 `.table-wrap`、`.action-col` | 多列比较与历史记录；紧凑历史列集由 `historyTable(compact)` 选择 | 桌面行 hover、操作列靠右；窄窗口字段分组、操作占整行；空态在表格所属区块呈现。不要把宽表强压成不可读窄列。 |
| [已确认] `tabs`；`docs/webui/app.js`，`docs/webui/styles.css` 的 `.tabs` | 同一对象的详情分区、发布执行/历史等互斥视图；选中由 `aria-selected`、字重和底部下划线表示 | 可横滚，当前项自动可见，溢出时提供滚动提示；支持方向键、Home/End 与焦点恢复。 |
| [已确认] `modal`/`confirmAction`；`docs/webui/app.js`，`docs/webui/index.html` 的 `#dialog`/`#sheet` | 创建、编辑、确认用原生 Dialog；日志/补充内容可用 Sheet；危险操作通过确认函数点明对象与结果 | 标题、关闭、提交/取消与滚动；避免新增自制遮罩和另一个弹窗管理器。 |
| [已确认] `toast`/`empty`/`badge`/`progress`；`docs/webui/app.js`，`docs/webui/styles.css` 对应类 | 短时操作结果、空/错误说明、状态、进度；错误或未知结果仍需留在页面可见位置 | Toast 5 秒消失，项目编辑时位于保存区上方且不截获指针；状态配文字和色彩；不能仅凭颜色或 Toast 表达需要人工处理的失败。 |
| [合法变体] `menu`、设置 Select、主机连接分段项；`docs/webui/app.js` 的 `menu`、`settingsPage`、`hostForm`，`docs/webui/styles.css` 的 `.menu`、`.host-segments` | 行级更多命令用 `details.menu`，设置窄屏分类用 Select，连接方式用 Radio 分段项 | 菜单支持外部点击/Escape 关闭，分段项有 checked/disabled/focus-visible；不要将三种控件混为同一个业务选择 API。 |

## 7. 表单、表格、导航、弹窗、反馈与危险操作

- [已确认] 全局导航由 `navItems` 与 hash 路由生成，当前项同时有 `.active` 和 `aria-current="page"`；设置子路由有独立地址。新增页面应接入已有路由与导航生成方式。证据：`docs/webui/app.js` 的 `navItems`、`parseRoute`、`render`、`settingsPage`。
- [已确认] 发布/回滚、容器删除与清理要展示对象、影响和资格；不可用时禁用并给原因，结果未知时保留“待核实”而非成功。证据：`docs/webui/app.js` 的 `deployPage`、`confirmAction`、`containersPage`，`docs/design/04-deploy.md` 第 3–4 节。
- [合法变体] 默认及主机表单 Dialog 宽度上限为 512px；多字段任务可用 `.wide` 672px，主机表单内边距 16px；日志 Sheet 靠右且上限 672px，终端弹窗另有 960×680px 上限。选择依据是内容任务和滚动长度，不按页面名称复制。证据：`docs/webui/styles.css` 的 `dialog`、`.wide`、`.host-dialog`、`.sheet`、`.terminal-dialog`，`docs/design/02-hosts.md` 第 3.2 节。
- [已确认] 写操作结果使用 `toast`；需要继续处理的原因也在页面内容、提示或确认层可见。表格操作区采用紧凑按钮/菜单，危险操作通过确认路径执行。证据：`docs/webui/app.js` 的 `toast`、`historyTable`、`containersPage`、`confirmAction`，`docs/webui/styles.css` 的 `.toast`、`.action-col`。

## 8. 加载、空、错误、成功、禁用与无权限

- [已确认] 原型预览可切换 normal/loading/empty/error/denied；加载为 3 组卡片骨架加 5 行骨架，主要空态由 `empty` 给说明和可用下一步，错误态给具体原因及“重试”。新数据页至少检查这些可找到的状态。证据：`docs/webui/app.js` 的 `render`、`empty`、`dashboard`，`docs/webui/README.md`“原型预览选项”。
- [合法变体] 总览“最近发布”无记录时只显示 muted 文案；发布历史页无记录时显示 `empty` 和“去发布”，项目历史页显示 `empty` 和“填充演示记录”。备份 S3 空记录在同一页面已有创建流程时保留说明型 `.backup-empty`。按页面任务选择，不将总览的简短提示扩展到完整历史页。证据：`docs/design/01-dashboard.md` 第 5 节、`docs/design/00-overview.md` 第 6 节，`docs/webui/app.js` 的 `historyTable`、`projectDetail`，`docs/webui/backup-ui.js` 的 `renderBackup`。
- [合法变体] Docker 不可用时显示安装引导和重新检测，导航为不可用项；锁屏时显示全屏原生 Dialog 并隐藏底层界面。它们是环境/安全阻断，不等同于普通空数据。证据：`docs/webui/app.js` 的 `dockerSetup`、`render`，`docs/webui/styles.css` 的 `[data-screen-locked]`、`#screen-lock`，`docs/webui/screen-lock.js`。
- [已确认] `disabled` 的按钮半透明且不能触发；发布资格、保留策略、目标缺失等在调用处给出理由。成功/失败 Badge 和 Toast 各有语义色。证据：`docs/webui/styles.css` 的 `button:disabled`、`.badge`、`.toast`，`docs/webui/app.js` 的 `deployPage`、`badge`、`toast`。
- [已确认] 无权限按受限范围处理：整页不可访问时使用 `permissionDenied`，复用 `empty` 展示原因和“返回总览”；仅操作受限时保留可读页面，在操作附近用 `notice` 说明原因并禁用。WebUI 的 denied 仅为原型预览，不执行真实权限校验。证据：`docs/webui/app.js` 的 `permissionDenied`、`render`，`docs/design/00-overview.md` 第 6 节。

## 9. 响应式策略

- [已确认] 布局媒体查询为 1279、1023、767、700、599px：1279 调整网格列数，1023 切设置分类导航并重排表格行，767 收起全局侧栏并改主区内边距/表单列数，700 调整项目目标绑定表单，599 将 Compose 路径与按钮分行。另有减少动态效果与强制颜色媒体查询；没有容器查询或移动端独立组件目录。证据：`docs/webui/styles.css` 的布局、`prefers-reduced-motion` 与 `forced-colors` 媒体查询，`docs/webui/app.js` 的 `render`。
- [合法变体] 767px 以下通过顶栏菜单展开全局侧栏，页面操作可换行，发布配置按钮占整行；表格按公共断点改为字段分组。窄窗口的设置导航另以 Select 取代二级导航。证据：`docs/webui/styles.css` 的 767/1023px 媒体查询、`.table-wrap`，`docs/webui/app.js` 的 `settingsPage`。
- [已确认] 正式客户端响应式策略已定：从 WebUI 页面布局出发，按客户端可用宽度、输入方式与系统字号逐页测得折叠点，不直接复制媒体查询数值。当前原型在 1440/1024/768/700/390px 的总览、主机、项目构建、发布和外观页检查中未见整页横向溢出。证据：`docs/design/00-overview.md` 第 2 节，`docs/webui/styles.css` 的媒体查询，`docs/webui/tests/review-fixes.spec.cjs` 的视口检查。
- [待确认] 正式客户端尚无实现，最小窗口宽度、触摸目标尺寸和系统字体放大后的实际折叠点仍需在客户端上测量；这些数值不得从原型或 `DESIGN.md` 默认基线推断。

## 10. 主题

- [已确认] 原型提供 light/dark/system，初始为 system；`theme()` 根据 `prefers-color-scheme` 给根元素切换 `.dark`，并在 localStorage 持久化，顶栏按钮在浅/深之间快捷切换，设置外观页可选跟随系统。新增语义色需同时核对两套值。证据：`docs/webui/styles.css` 的 `:root.dark`，`docs/webui/app.js` 的 `theme`、`render`、`settingsPage`。
- [已确认] 继续使用浅色、深色、跟随系统三档；强制颜色模式下用系统 `Highlight` 保留导航和 Tabs 的选中标识，并检查文字、按钮边界与焦点，不新增第四套主题 Token。证据：`docs/webui/styles.css` 的 `@media (forced-colors: active)`，`docs/design/00-overview.md` 第 6 节。
- [待确认] 尚无正式客户端主题实现和完整辅助技术/对比度实测；正式客户端选定技术栈后须核对平台强制颜色与高对比度表现，再决定是否需要额外映射。

## 11. 内容、图标、图片、动效、无障碍

- [已确认] 当前图标和标识由 `app.js` 内联 SVG `paths` 与 `icon()` 统一输出，普通图标 `aria-hidden`，图标按钮由 `ib()` 加 `aria-label`/`title`。仓库未安装图标库或独立图片资产；新增图标先查 `paths`，有明确用途再补。证据：`docs/webui/app.js` 的 `paths`、`icon`、`ib`、`render`，`docs/webui/package.json`。
- [已确认] 中文/英文界面文案由 `i18n.js`/`i18n-en.js` 与 `I18n.t` 处理；用户输入、主机/项目名、路径和日志保留原文。新增可见字串及无障碍名称要走同一翻译路径，并检查英文长词换行。证据：`docs/webui/i18n.js`、`i18n-en.js`，`docs/webui/app.js` 的 `render`、`field`，`docs/design/00-overview.md` 第 7 节。
- [已确认] 骨架呼吸与顶栏进度动画为 1.6s；系统偏好减少动态效果时关闭动画。页面交互主要依赖即时状态变化，不能用连续动画替代文字结果。证据：`docs/webui/styles.css` 的 `.skeleton`、`.global-progress`、`@keyframes`、`prefers-reduced-motion`。
- [已确认] 全局 `focus-visible` 为 2px `--ring` 轮廓、3px offset；原生 Dialog、导航 `aria-current`、进度 `role=progressbar`、Toast `aria-live` 已有局部语义。新增交互保留可见焦点和可读名称。证据：`docs/webui/styles.css` 的 `:focus-visible`，`docs/webui/index.html`，`docs/webui/app.js` 的 `tabs`、`progress`、`modal`、`render`。
- [已确认] 原型 Tabs 使用 roving `tabindex`：当前项为 0，其他项为 -1；左右方向键循环切换，Home/End 到首末项，路由或备份内容重绘后焦点回到新当前项。正式客户端选型后按同一键盘行为核对组件，不直接复制原型实现。证据：`docs/webui/app.js` 的 `tabs`、`pendingTabFocus`、`keydown`，`docs/design/00-overview.md` 第 5 节。

## 12. 技术实现约束

- [已确认] 现有 WebUI 是静态 HTML、原生 JavaScript、单份 CSS 和 hash 路由；`package.json` 的运行时依赖为 `js-yaml`，Playwright 是开发测试依赖。没有 React、Vue、Tailwind、CSS-in-JS、现成 UI 或图标组件库。维护原型先复用 `app.js` 渲染函数、`styles.css` 变量及 `docs/design` 页面约定；不要只因 `DESIGN.md` 提到 shadcn 就添加一套重复能力。证据：`docs/webui/index.html`、`package.json`、`app.js`、`styles.css`，`docs/webui/README.md`“代码结构”。
- [合法变体] 备份和锁屏的业务交互拆在 `backup-ui.js`、`screen-lock.js`，页面基础样式与 Shell 仍共享 `styles.css`；新功能可按职责拆脚本，但公共 Button/Dialog/Toast 的语义继续复用。证据：`docs/webui/index.html`、`docs/webui/backup-ui.js`、`docs/webui/screen-lock.js`。

## 13. 新页面流程与验收清单

1. 搜索 `app.js` 的 `head`、`card`、`table`、`tabs`、`field`、`button`、`modal`、`empty` 和 `styles.css` 变量；确定最接近的页面路由与 `docs/design` 约定。
2. 用页面任务列出入口、主操作、对象身份、结果、权限和阻断状态；新增例外先写清适用条件与证据。
3. 代码搜索确认新样式没有重定义同义颜色/间距/圆角，新增按钮与状态复用现有函数；检查中文、英文、浅色、深色。
4. 固定视口截图核对 1440px、1024px、768px、700px、390px：侧栏/设置分类、表单换列、按钮换行、表格字段分组、Dialog 与 Sheet 不遮挡。已有测试入口见 `docs/webui/tests/ui.spec.cjs`。
5. 在 normal/loading/empty/error/denied 预览以及 Docker 不可用、禁用、提交中、成功/失败状态检查对象、原因和下一步；涉及危险操作走确认与结果反馈。
6. 用 Tab、方向键、Home、End、Enter、Space、Escape 检查导航、Tabs、菜单、弹窗关闭与焦点可见；检查图标按钮名称、Tab 选择和减少动态效果。未验证的辅助技术表现记录为待确认。
7. 实现与原型、`docs/design` 对应页逐页核对；更新有依据的本文条目，再运行相关页面测试。

## 14. 历史冲突、待确认与迁移建议

裁决顺序：公共组件或语义 Token → 多个核心页覆盖 → 最近维护路径 → 交互/响应式/无障碍完整度 → 管理任务密度 → 排除第三方默认或局部补丁。证据不足时保留候选，不取平均值。

本轮按 `docs/design` 对应页面规范核对后，未保留未解决的历史冲突。原先四条分别处理如下：

- [合法变体] `DESIGN.md` 给尚未实现的新产品提供跨端默认值，当前 WebUI 的 14px 正文和 1279/1023/767/700px 媒体查询按 `docs/design/00-overview.md` 的原型页面及现有布局执行。正式客户端尺寸仍属第 9 节待确认项。证据：`DESIGN.md` 开头及“布局规范”，`docs/design/00-overview.md` 第 2、4 节，`docs/webui/styles.css` 的 `body`、`@media`。
- [已确认] 主机 Dialog 已从 448px 调整到 512px 上限，符合 `sm:max-w-lg`；原生 HTML/CSS 是 WebUI 的实现形式，不代表页面设计文档要求安装 shadcn/Tailwind。证据：`docs/design/02-hosts.md` 第 3.2 节，`docs/design/00-overview.md` 第 5 节，`docs/webui/styles.css` 的 `.host-dialog`，`docs/webui/package.json`。
- [合法变体] 总览最近发布保留页面文档指定的 muted 文案；完整历史空态已改为居中说明和行动按钮；S3 记录空态保留与同页创建流程相邻的说明。证据：`docs/design/01-dashboard.md` 第 5 节，`docs/design/00-overview.md` 第 6 节，`docs/webui/app.js` 的 `historyTable`，`docs/webui/backup-ui.js` 的 `renderBackup`。
- [合法变体] `DESIGN.md` 的默认字距 0 不覆盖 `docs/design/00-overview.md` 单独规定的页标题 `tracking-tight`；原型品牌与统计值为局部强调。新增文字角色不直接照抄这些局部字距。证据：`DESIGN.md`“字体”，`docs/design/00-overview.md` 第 4 节，`docs/webui/styles.css` 的 `.brand`、`h1`、`.stat`。

建议迁移顺序：确定正式客户端的技术映射与可用视口后，核对字段、空态和键盘状态。局部样式只在截图与键盘检查证明有问题时调整。

## 15. 审计证据索引

| 来源 | 本次用途 |
| --- | --- |
| `docs/webui/README.md`、`package.json`、`index.html` | 原型边界、依赖、入口、Shell |
| `docs/webui/app.js` | 路由、六组页面、公共渲染函数、主题和预览状态 |
| `docs/webui/styles.css` | 语义变量、排版、组件状态、断点与层级 |
| `docs/webui/i18n.js`、`i18n-en.js`、`backup-ui.js`、`screen-lock.js` | 多语言、备份/锁屏的复用边界 |
| `docs/webui/tests/ui.spec.cjs`、`settings-pages.spec.cjs` | 现有视口和页面回归路径，非无障碍合格证明 |
| `docs/design/00-overview.md` 至 `06-settings.md`、`DESIGN.md` | 已写的页面意图、跨端基线及与原型的差异 |

## 后续 AI 编程助手执行约定

开始 UI 任务前先读 `UI DESIGN.md`。新建组件前搜索公共组件、Token 与相似页面；优先复用 `[已确认]` 规则和现有能力。遇到 `[历史冲突]` 或 `[待确认]` 时停止猜测，报告对应证据、2–3 个候选方案及影响。不得为单个页面新增语义重复的颜色、间距、圆角或组件变体。完成后按上述视口、状态、键盘路径和一致性清单自检；真实需求需要例外时，说明原因与影响范围，并同步更新本文。


## 17. 页面构图更新（2026-11-27）

- 总览优先展示离线主机及各项目最新发布的异常结果，再显示当前任务、最近发布与主机概览。提醒最多直接显示 4 项；较早失败不覆盖较新成功。
- 项目配置页使用目录与状态摘要、下划线页签、字段区、可选配置摘要和共用底部保存区；保存范围、草稿与目录授权规则不变。字段区上限 800px，名称和 Tag 上限 360px。
- 发布页的轻底色工具条承载配置，逐主机结果与可展开日志直接排列。日志高 32vh，桌面最低 240px、最高 480px，窄窗口最低 200px。
- 主机列表与项目列表的工具条显示记录数量，容器页将主机切换和容器/镜像页签放在同一工具条。完整字段和危险操作确认仍保留。
- 设置使用无外层边框的限宽分组，内容上限 800px，一般输入上限 640px、选择框上限 320px。导航和辅助摘要以轻微中性色底区分。

来源为 `docs/webui/app.js`、`styles.css` 及 `docs/design/00~06` 的当前实现与页面说明。正式客户端尚未实现，本次不代表真实远端操作已验收。
