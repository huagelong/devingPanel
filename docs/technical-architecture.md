# 技术架构

## 技术选型与边界

正式桌面客户端采用 Go 和 [MyGo](https://github.com/egoist/mygo) 的原生 UI 模式。界面、发布流程和本地数据都在客户端进程内；VPS 只需 SSH、Docker Engine 和 Docker Compose，不部署本产品的服务或 agent，也不在 VPS 构建镜像。`docs/webui` 是独立的 HTML 交互原型，不连接真实 SSH/Docker，也不使用 SQLite；它的 npm 依赖不属于桌面客户端。

按 MyGo 当前文档，原生模板可用以下命令初始化；正式落地前需固定经过验证的 MyGo 版本，并完成目标平台的运行和打包验证：

```sh
go run github.com/egoist/mygo/cmd/mygo@latest init -template native devingPanel
```

MyGo 原生 UI 仅使用 Go，不需要浏览器、WebView、Bun 或前端框架。项目尚无 Go 源码和 `go.mod`；下表是按现有功能文档拟定的直接依赖，版本以初始化后的兼容性验证结果为准。

## 依赖与功能

| 功能 | Go 包或外部程序 | 使用范围 |
| --- | --- | --- |
| 原生 UI、文件/目录对话框、系统通知、系统凭据 | `github.com/egoist/mygo`、`github.com/egoist/mygo/ui` | 客户端基础能力；优先使用 MyGo 自带 API |
| 本地数据 | `github.com/egoist/mygo/plugins/sqlite` | 主机、项目索引、设置、发布任务/历史与待核实状态；原生 Go 代码直接打开数据库 |
| SSH | `golang.org/x/crypto/ssh` | 密码/私钥认证、主机指纹核对、远端命令和输出流 |
| 文件传输 | `github.com/pkg/sftp` | 镜像归档、Compose 文件及 `.env` 上传；连接复用已确认指纹的 SSH 会话 |
| SOCKS5 代理 | `golang.org/x/net/proxy` | 主机选择 SOCKS5 连接方式时使用；HTTP CONNECT 代理可用 Go 标准库实现 |
| SSH 终端 | `github.com/egoist/mygo/plugins/terminal` | 实现主机“连接”入口时渲染终端，接入 SSH PTY 的输入、输出及窗口尺寸变化 |
| 项目配置 | `gopkg.in/yaml.v3` | 读取和写回 `devingpanel.yaml`；项目代码目录可以不是 Git 仓库 |
| 可选 Git 提交标识 | `github.com/go-git/go-git/v5` | 仅当 Tag 规则包含 `{git-sha}` 时，读取项目所属仓库的 `HEAD`；不调用或要求本机 Git CLI |
| S3 配置备份 | `github.com/aws/aws-sdk-go-v2/config`、`github.com/aws/aws-sdk-go-v2/credentials`、`github.com/aws/aws-sdk-go-v2/service/s3` | 实现正式桌面端 S3 备份时引入；HTML 原型中的 S3 只是演示 |

Go 标准库负责本地进程调用、JSON、文件操作、日志、并发及 AES-GCM 加密；备份和锁屏的 PBKDF2 可复用 `golang.org/x/crypto/pbkdf2`。本地 Docker/Buildx/Compose 通过 `os/exec` 调用已安装的 CLI，远端 Docker/Compose 通过 SSH 调用；这些 CLI 是运行环境前提，不是 Go 依赖。镜像构建、导出、上传、加载及回滚不需要额外引入 Docker SDK 或 VPS agent。

`{timestamp}` 是默认 Tag 规则，不要求 Git。选择 `{git-sha}` 时，先在项目目录查找所属仓库并读取当前 `HEAD` 提交；普通仓库和 Git worktree 都需验证。非 Git 目录或尚无提交时明确报错，引导改用 `{timestamp}`，不得静默替换规则。提交哈希不包含未提交的工作区改动，发布前应提示这一含义。WebUI 原型仍使用示例值，不执行真实 Git 读取。

MyGo `Secrets` 用于 SSH、代理、Registry、S3 凭据和敏感环境变量。它的单项上限为 2560 字节；超过上限的敏感材料不能截断或明文落盘，应加密保存为本地文件，只把随机生成的加密密钥存入 `Secrets`，SQLite 仅记录不含秘密的引用。系统凭据存储不可用时阻止保存敏感数据，不回退到明文。首次 SSH 连接须显示 SHA256 主机指纹供独立核对；指纹变化时阻断连接，不能自动信任。

## 代码职责与发布链路

正式客户端按职责分为原生 UI、主机/项目/发布等应用逻辑，以及 SQLite、凭据、SSH/SFTP、本地 Docker CLI 和项目文件的访问代码。UI 不直接执行远端命令；发布任务负责协调预检、构建、传输、启动、健康核对和状态持久化。不需要为每个页面复制一套 SSH 或 Docker 实现。

发布前重新读取并校验项目目录中的 `devingpanel.yaml`，冻结本次配置。先核对本地构建能力、目标主机架构、SSH 信任与凭据、Compose 合并结果和远端 Docker；然后在本机构建对应架构镜像。默认通过 `docker save`、SFTP、远端 `docker load` 分发，或在用户启用 Registry 模式后推送并由目标主机拉取。最后上传 Compose 与权限为 600 的 `.env`，远端执行 Compose 并核对容器健康。多主机共享构建产物，逐主机记录结果；断线、超时或结果不明时持久化为“待核实”，重新连接核对前不自动重试。回滚使用留存的本机归档和远端版本材料，不重新构建。

容器列表、日志、资源监控、Docker 安装引导和精确清理复用上述 SSH/CLI 能力。主机“连接”入口复用 SSH 认证和指纹验证，取得远端 PTY 后连接 MyGo terminal 的 `Conn`，并同步终端尺寸；终端插件包含随应用打包的原生模块。多级跳板连接依次经已信任的 SSH 主机建立到目标的连接，每一跳独立核对指纹；HTTP CONNECT 和 SOCKS5 代理只改变底层连接路径，不跳过目标主机指纹核对。P2 告警可复用 MyGo 系统通知。

## 数据加载与刷新

桌面客户端展示的持久化数据统一从本地 SQLite 加载。页面打开时读取，后台采集先写入 SQLite，此后每 1 分钟重新读取并刷新显示；页面不直接轮询 SSH、Docker 等外部数据源。由当前客户端发起的保存、发布和容器操作，在状态写入 SQLite 后应立即刷新相关视图和进度，不能让用户等待下一次分钟刷新。分钟刷新也不能覆盖未提交的表单输入。

主机数据的后台采集写入周期由“设置 → 本地环境”配置，默认 60 秒，可选 10 / 30 / 60 / 300 秒或自定义 10–3600 秒整数，仅在客户端运行期间执行。采集写入周期与上面的 SQLite 重读周期分别控制；调整采集周期不延迟用户主动操作后的即时刷新。`docs/webui` 只演示该设置，不执行采集或 SQLite 写入。

MyGo SQLite 和 terminal 插件包含随应用打包的原生模块；跨 Windows、macOS、Linux 目标的模块获取、签名、离线启动，以及 SQLite 的事务和迁移需要在选型验证时实际检查。`docs/webui` 不遵守本节的数据源规则。
