# P6 设置 设计文档

> 需求映射：D-6/D-7 保留与清理策略、SEC-1/SEC-3 凭据加密与审计日志、T-2 镜像仓库（可选）、国际化、主题切换

## 1. 页面目的

集中管理全局策略：发布保留策略、外观、数据与安全选项。

## 2. 入口

- 侧边栏"设置"。

## 3. 布局

纵向 Card 分组，`space-y-6`，每组 `CardHeader`(标题+说明) + `CardContent`(表单)：

```
内容区
|
|-- 页头: "设置"
|
|-- Card: 发布保留策略
|   历史镜像保留数量   [Select: 3 / 5 / 10 / 自定义 Input number]
|   清理时机           [RadioGroup: 每次发布完成后自动清理 (默认) / 仅手动]
|   [立即清理所有主机历史镜像] (variant=outline, AlertDialog 确认)
|
|-- Card: 外观
|   主题    [RadioGroup: 浅色 / 深色 / 跟随系统]
|   语言    [Select: 简体中文(默认) / English] (预留 i18n)
|
|-- Card: 镜像仓库 (可选)
|   说明 (muted): "未配置时使用 SSH 直连传输镜像；配置后发布时改为 push + 远端 pull"
|   Registry 地址   [Input (mono), 如 ghcr.io/username]
|   认证           [用户名 Input + Password Input] (凭据同样加密存储)
|   [测试登录] (variant=outline)
|
|-- Card: 告警 (P2 预留，当前版本隐藏)
|   阈值告警: CPU / 内存 / 磁盘 阈值 Input number + 持续时长 Select
|
|-- Card: 本地环境
|   Docker 路径检测结果 + [重新检测]
|   数据存储位置: 显示 SQLite 文件路径 (mono, 只读) + [打开所在文件夹]
|
|-- Card: 安全
|   凭据存储方式: "操作系统凭据管理器（加密）" (只读说明, muted)
|   审计日志: [打开日志文件夹]
```

## 4. 交互与状态

- 所有设置项即改即存，无需整页保存按钮；保存成功 Toast。
- 主题设置与顶部栏主题切换按钮联动（同一份数据）。
- [测试登录] 成功显示 success 文案"登录成功"；失败显示 destructive 原因。
- [立即清理] 执行中按钮 loading，完成后 Toast 汇总"已从 3 台主机清理 7 个镜像，释放 1.8 GB"。
