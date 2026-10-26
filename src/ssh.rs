use crate::host_store::{self, ConnectionMaterial, Host};
use russh::client;
use russh::keys::{HashAlg, PrivateKey, PrivateKeyWithHashAlg, PublicKeyOrCertificate};
use russh::{ChannelMsg, Disconnect};
use russh_sftp::client::RawSftpSession;
use russh_sftp::client::SftpSession;
use russh_sftp::protocol::{FileAttributes, OpenFlags};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
    mpsc::SyncSender,
};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub enum TestResult {
    NeedsTrust {
        address: String,
        port: u16,
        previous: Option<String>,
        fingerprint: String,
    },
    Connected {
        docker_version: String,
        compose_version: String,
        system_kernel: String,
        architecture: Option<String>,
        disk_available_kib: u64,
        sftp_v3_handshake: bool,
    },
}

#[derive(Clone)]
pub struct ContainerInfo {
    pub id: String,
    pub name: String,
    pub image: String,
    pub status: String,
    pub state: String,
    pub ports: String,
    pub cpu: String,
    pub memory: String,
}

#[derive(Clone)]
pub struct ImageInfo {
    pub reference: String,
    pub size: String,
    pub created: String,
    pub dangling: bool,
    pub container_refs: Option<u32>,
}

pub struct Inventory {
    pub containers: Vec<ContainerInfo>,
    pub images: Vec<ImageInfo>,
}

pub struct MonitorSnapshot {
    pub cpu_percent: f64,
    pub memory_used_kib: u64,
    pub memory_total_kib: u64,
    pub disk_percent: f64,
    pub network_bytes_per_sec: Option<(f64, f64)>,
    pub containers: Option<Vec<ContainerInfo>>,
}

#[allow(dead_code)] // The transfer phase will consume this after release execution is wired up.
pub struct UploadedArchive {
    pub directory: String,
    pub path: String,
    pub bytes: u64,
    pub sha256: String,
}

fn validated_upload_directory(raw: &str) -> Result<String, String> {
    let name = raw
        .strip_prefix("/tmp/devingpanel.")
        .ok_or("远端临时目录路径无效。")?;
    if name.len() != 32 || !name.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("远端临时目录路径无效。".into());
    }
    Ok(format!("/tmp/devingpanel.{name}"))
}

fn verified_sha256(text: &str) -> Result<String, String> {
    let digest = text.split_whitespace().next().ok_or("远端校验值缺失。")?;
    if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("远端校验值无效。".into());
    }
    Ok(digest.to_ascii_lowercase())
}

#[derive(Debug)]
pub enum MonitorError {
    Configuration(String),
    Connection(String),
    Sampling(String),
}

impl MonitorError {
    pub fn message(self) -> String {
        match self {
            Self::Configuration(message) | Self::Connection(message) | Self::Sampling(message) => {
                message
            }
        }
    }
}

impl From<String> for MonitorError {
    fn from(message: String) -> Self {
        Self::Sampling(message)
    }
}

#[derive(Clone)]
pub struct DiskUsageRow {
    pub kind: String,
    pub total: String,
    pub reclaimable: String,
}

pub struct DiskPreview {
    pub rows: Vec<DiskUsageRow>,
    pub unused_networks: usize,
}

pub enum LogEvent {
    Line(String),
    Error(String),
    Ended,
}

struct LogLineBuffer {
    pending: Vec<u8>,
    truncated: bool,
}

impl LogLineBuffer {
    fn new() -> Self {
        Self {
            pending: Vec::new(),
            truncated: false,
        }
    }

    fn push(&mut self, data: &[u8], sender: &SyncSender<LogEvent>) -> bool {
        for byte in data {
            if *byte == b'\n' {
                if !self.flush(sender) {
                    return false;
                }
            } else if self.pending.len() < 16 * 1024 {
                self.pending.push(*byte);
            } else {
                self.truncated = true;
            }
        }
        true
    }

    fn flush(&mut self, sender: &SyncSender<LogEvent>) -> bool {
        let text = String::from_utf8_lossy(&self.pending);
        let mut line: String = text
            .chars()
            .filter(|ch| *ch == '\t' || !ch.is_control())
            .collect();
        if self.truncated {
            line.push_str(" …[单行截断]");
        }
        self.pending.clear();
        self.truncated = false;
        sender.send(LogEvent::Line(line)).is_ok()
    }
}

#[derive(Clone, Copy)]
pub enum ContainerAction {
    Start,
    Stop,
    Restart,
    Delete,
}

impl ContainerAction {
    pub fn label(self) -> &'static str {
        match self {
            Self::Start => "启动",
            Self::Stop => "停止",
            Self::Restart => "重启",
            Self::Delete => "删除",
        }
    }
}

fn container_command(endpoint: &str, id: &str, action: ContainerAction) -> Result<String, String> {
    validate_container_id(id)?;
    let verb = match action {
        ContainerAction::Start => "start",
        ContainerAction::Stop => "stop --time 10",
        ContainerAction::Restart => "restart --time 10",
        ContainerAction::Delete => "rm",
    };
    Ok(docker_command(endpoint, &format!("container {verb} {id}")))
}

fn validate_container_id(id: &str) -> Result<(), String> {
    if id.is_empty() || id.len() > 64 || !id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("容器 ID 无效。".into());
    }
    Ok(())
}

fn same_connection_target(current: &Host, expected: &Host) -> bool {
    current.id == expected.id
        && current.address == expected.address
        && current.port == expected.port
        && current.username == expected.username
        && current.auth == expected.auth
        && current.trusted_fingerprint == expected.trusted_fingerprint
}

fn material_for(expected: &Host) -> Result<ConnectionMaterial, String> {
    let material = host_store::connection_material(expected.id)?;
    if !same_connection_target(&material.host, expected) {
        return Err("主机连接配置已变化，请重新核对指纹。".into());
    }
    Ok(material)
}

async fn trusted_handle(
    material: ConnectionMaterial,
) -> Result<client::Handle<HostKeyChecker>, String> {
    if material.trusted_fingerprint.is_none() {
        return Err("请先在主机页确认 SSH 指纹。".into());
    }
    match connect_authenticated(material).await? {
        ConnectionOutcome::NeedsTrust(_) => Err("主机指纹已变化，操作已阻断。".into()),
        ConnectionOutcome::Authenticated(handle) => Ok(handle),
    }
}

fn image_size_bytes(text: &str) -> Result<f64, String> {
    let unit_at = text
        .find(|ch: char| !ch.is_ascii_digit() && ch != '.')
        .ok_or("Docker 镜像大小无效。")?;
    let value: f64 = text[..unit_at]
        .parse()
        .map_err(|_| "Docker 镜像大小无效。")?;
    let multiplier = match &text[unit_at..] {
        "B" => 1.,
        "kB" => 1e3,
        "MB" => 1e6,
        "GB" => 1e9,
        "TB" => 1e12,
        "PB" => 1e15,
        _ => return Err("Docker 镜像大小单位无效。".into()),
    };
    if !value.is_finite() || value < 0. {
        return Err("Docker 镜像大小无效。".into());
    }
    Ok(value * multiplier)
}

fn parse_disk_preview(df: &str, networks: &str, dangling: &str) -> Result<DiskPreview, String> {
    let images = parse_rows(dangling)?;
    let mut image_bytes = 0.;
    for image in &images {
        image_bytes += image_size_bytes(&safe_field(image, "Size")?)?;
    }
    let disk_rows = parse_rows(df)?;
    if disk_rows.is_empty() {
        return Err("Docker 磁盘预检为空。".into());
    }
    let mut rows = Vec::new();
    for row in disk_rows {
        let kind = safe_field(&row, "Type")?;
        if !matches!(
            kind.as_str(),
            "Images" | "Containers" | "Build Cache" | "Local Volumes"
        ) {
            return Err("Docker 磁盘预检类型无效。".into());
        }
        if kind == "Local Volumes" || kind == "Images" {
            continue;
        }
        rows.push(DiskUsageRow {
            kind,
            total: safe_field(&row, "TotalCount")?,
            reclaimable: safe_field(&row, "Reclaimable")?,
        });
    }
    rows.insert(
        0,
        DiskUsageRow {
            kind: "Images".into(),
            total: images.len().to_string(),
            reclaimable: format!("约 {:.1} MB（镜像大小合计）", image_bytes / 1e6),
        },
    );
    let mut unused_networks = 0;
    for id in networks.lines() {
        if unused_networks >= 2000
            || id.is_empty()
            || id.len() > 64
            || !id.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err("Docker 网络预检结果无效。".into());
        }
        unused_networks += 1;
    }
    Ok(DiskPreview {
        rows,
        unused_networks,
    })
}

async fn fetch_disk_preview(material: ConnectionMaterial) -> Result<DiskPreview, String> {
    let handle = trusted_handle(material).await?;
    let endpoint = local_docker_endpoint(&handle).await?;
    let df = command_limited(
        &handle,
        &docker_command(&endpoint, "system df --format '{{json .}}'"),
        64 * 1024,
    )
    .await?;
    let networks = command_limited(
        &handle,
        &docker_command(
            &endpoint,
            "network ls --filter dangling=true --format '{{.ID}}'",
        ),
        64 * 1024,
    )
    .await?;
    let dangling = command_limited(
        &handle,
        &docker_command(
            &endpoint,
            "image ls --filter dangling=true --format '{{json .}}'",
        ),
        256 * 1024,
    )
    .await?;
    let _ = handle.disconnect(Disconnect::ByApplication, "", "en").await;
    parse_disk_preview(&df, &networks, &dangling)
}

pub fn disk_preview(expected_host: Host) -> Result<DiskPreview, String> {
    let material = material_for(&expected_host)?;
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| "无法初始化 SSH 运行环境。")?
        .block_on(async {
            tokio::time::timeout(Duration::from_secs(30), fetch_disk_preview(material))
                .await
                .map_err(|_| "Docker 磁盘预检超时，请核对主机连接。")?
        })
}

async fn execute_disk_prune(material: ConnectionMaterial) -> Result<Option<String>, String> {
    let handle = trusted_handle(material).await?;
    let endpoint = local_docker_endpoint(&handle).await?;
    let mut channel = handle
        .channel_open_session()
        .await
        .map_err(|_| "无法打开远端 SSH 会话，清理未开始。")?;
    channel
        .exec(true, docker_command(&endpoint, "system prune --force"))
        .await
        .map_err(|_| "清理命令已发送但结果待核实；请到主机核对。")?;
    let mut output = Vec::new();
    let mut exit = None;
    while let Some(message) = channel.wait().await {
        match message {
            ChannelMsg::Data { data } => {
                if output.len().saturating_add(data.len()) > 64 * 1024 {
                    return Err("清理输出过大，结果待核实；请到主机核对。".into());
                }
                output.extend_from_slice(&data);
            }
            ChannelMsg::ExitStatus { exit_status } => exit = Some(exit_status),
            _ => {}
        }
    }
    let _ = handle.disconnect(Disconnect::ByApplication, "", "en").await;
    match exit {
        Some(0) => {
            let output = String::from_utf8(output).ok();
            Ok(output.and_then(|text| {
                text.lines().rev().find_map(|line| {
                    let value = line.strip_prefix("Total reclaimed space: ")?.trim();
                    (value.len() <= 64 && image_size_bytes(value).is_ok()).then(|| value.to_owned())
                })
            }))
        }
        Some(_) => Err("远端 Docker 报告清理失败；请到主机核对详情。".into()),
        None => Err("SSH 连接中断，清理结果待核实；请到主机核对，不要盲目重试。".into()),
    }
}

pub fn disk_prune(expected_host: Host) -> Result<Option<String>, String> {
    let material = material_for(&expected_host)?;
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| "无法初始化 SSH 运行环境。")?
        .block_on(async {
            tokio::time::timeout(Duration::from_secs(120), execute_disk_prune(material))
                .await
                .map_err(|_| "清理超时，远端结果待核实；请到主机核对，不要盲目重试。")?
        })
}

async fn execute_container_command(
    handle: &client::Handle<HostKeyChecker>,
    text: &str,
    action: ContainerAction,
) -> Result<(), String> {
    let mut channel = handle
        .channel_open_session()
        .await
        .map_err(|_| "无法打开远端 SSH 会话，操作未开始。")?;
    channel
        .exec(true, text)
        .await
        .map_err(|_| "容器命令已发送但结果待核实；请刷新状态后再决定是否重试。")?;
    let mut exit = None;
    let mut size = 0usize;
    while let Some(message) = channel.wait().await {
        match message {
            ChannelMsg::Data { data } => {
                size = size.saturating_add(data.len());
                if size > 4096 {
                    return Err("容器命令输出过大，结果待核实；请刷新状态。".into());
                }
            }
            ChannelMsg::ExitStatus { exit_status } => exit = Some(exit_status),
            _ => {}
        }
    }
    match exit {
        Some(0) => Ok(()),
        Some(_) => Err(format!(
            "远端 Docker 报告{}失败；请核对容器状态与主机权限。",
            action.label()
        )),
        None => Err("SSH 连接中断，容器操作结果待核实；请刷新状态后再决定是否重试。".into()),
    }
}

struct HostKeyChecker {
    trusted: Option<String>,
    observed: Arc<Mutex<Option<(String, bool)>>>,
}

impl client::Handler for HostKeyChecker {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        key: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        let fingerprint = key.public_key().fingerprint(HashAlg::Sha256).to_string();
        let accepted = self.trusted.as_deref() == Some(&fingerprint);
        *self
            .observed
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some((fingerprint, !accepted));
        Ok(accepted)
    }
}

async fn command_limited(
    handle: &client::Handle<HostKeyChecker>,
    command: &str,
    limit: usize,
) -> Result<String, String> {
    let mut channel = handle
        .channel_open_session()
        .await
        .map_err(|_| "无法打开远端 SSH 会话。")?;
    channel
        .exec(true, command)
        .await
        .map_err(|_| "无法执行远端能力检查。")?;
    let mut bytes = Vec::new();
    let mut exit = None;
    while let Some(message) = channel.wait().await {
        match message {
            ChannelMsg::Data { data } => {
                if bytes.len().saturating_add(data.len()) > limit {
                    return Err("远端检查输出过大。".into());
                }
                bytes.extend_from_slice(&data);
            }
            ChannelMsg::ExitStatus { exit_status } => exit = Some(exit_status),
            _ => {}
        }
    }
    if exit != Some(0) {
        return Err("远端 Docker、Compose 或系统工具不可用，或当前用户没有权限。".into());
    }
    String::from_utf8(bytes).map_err(|_| "远端检查输出编码无效。".into())
}

async fn command(handle: &client::Handle<HostKeyChecker>, command: &str) -> Result<String, String> {
    command_limited(handle, command, 4096).await
}

fn validate_local_endpoint(endpoint: &str) -> Result<String, String> {
    let path = endpoint
        .strip_prefix("unix:///")
        .ok_or("远端 Docker 端点不是本机 Unix socket。")?;
    if path.is_empty()
        || endpoint.len() > 512
        || !path.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'.' | b'_' | b'-' | b':' | b'@')
        })
    {
        return Err("远端 Docker socket 路径无效，操作已阻断。".into());
    }
    Ok(endpoint.to_owned())
}

fn docker_command(endpoint: &str, tail: &str) -> String {
    format!("docker --host {endpoint} {tail}")
}

async fn local_docker_endpoint(handle: &client::Handle<HostKeyChecker>) -> Result<String, String> {
    let environment = command(
        handle,
        "sh -c 'printf \"%s\\n%s\\n\" \"$DOCKER_CONTEXT\" \"$DOCKER_HOST\"'",
    )
    .await
    .map_err(|_| "无法读取远端 Docker 连接环境。")?;
    let mut lines = environment.lines();
    let context = lines.next().ok_or("远端 Docker 连接环境无效。")?;
    let host = lines.next().ok_or("远端 Docker 连接环境无效。")?;
    if lines.next().is_some() {
        return Err("远端 Docker 连接环境无效。".into());
    }
    let endpoint = if !context.is_empty() {
        command(handle, "sh -c 'docker context inspect \"$DOCKER_CONTEXT\" --format \"{{json .Endpoints.docker.Host}}\"'")
            .await
            .map_err(|_| "无法核实远端 Docker context。")?
    } else if !host.is_empty() {
        return validate_local_endpoint(host);
    } else {
        command(
            handle,
            "docker context inspect --format '{{json .Endpoints.docker.Host}}'",
        )
        .await
        .map_err(|_| "无法核实远端 Docker context。")?
    };
    let endpoint: String =
        serde_json::from_str(endpoint.trim()).map_err(|_| "远端 Docker context 端点格式无效。")?;
    validate_local_endpoint(&endpoint)
}

fn safe_field(row: &Value, key: &str) -> Result<String, String> {
    let value = row
        .get(key)
        .and_then(Value::as_str)
        .ok_or("Docker 输出字段缺失。")?;
    if value.chars().any(char::is_control) {
        return Err("Docker 输出字段无效。".into());
    }
    let mut text: String = value.chars().take(512).collect();
    if value.chars().count() > 512 {
        text.push('…');
    }
    Ok(text)
}

fn safe_id(row: &Value) -> Result<String, String> {
    let id = safe_field(row, "ID")?;
    if id.is_empty() || id.len() > 64 || !id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("Docker 容器 ID 无效。".into());
    }
    Ok(id)
}

fn parse_rows(text: &str) -> Result<Vec<Value>, String> {
    let mut rows = Vec::new();
    for line in text.lines() {
        if rows.len() >= 2000 {
            return Err("Docker 列表过大。".into());
        }
        rows.push(serde_json::from_str(line).map_err(|_| "Docker JSON 输出无效。")?);
    }
    Ok(rows)
}

fn parse_inventory(containers: &str, images: &str, stats: &str) -> Result<Inventory, String> {
    let mut usage = HashMap::new();
    for row in parse_rows(stats)? {
        usage.insert(
            safe_id(&row)?,
            (safe_field(&row, "CPUPerc")?, safe_field(&row, "MemUsage")?),
        );
    }
    let containers = parse_rows(containers)?
        .into_iter()
        .map(|row| {
            let id = safe_id(&row)?;
            let (cpu, memory) = usage
                .get(&id)
                .cloned()
                .unwrap_or_else(|| ("—".into(), "—".into()));
            Ok(ContainerInfo {
                id,
                name: safe_field(&row, "Names")?,
                image: safe_field(&row, "Image")?,
                status: safe_field(&row, "Status")?,
                state: safe_field(&row, "State")?,
                ports: safe_field(&row, "Ports")?,
                cpu,
                memory,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    let images = parse_rows(images)?
        .into_iter()
        .map(|row| {
            let repository = safe_field(&row, "Repository")?;
            let tag = safe_field(&row, "Tag")?;
            Ok(ImageInfo {
                reference: format!("{repository}:{tag}"),
                size: safe_field(&row, "Size")?,
                created: safe_field(&row, "CreatedSince")?,
                dangling: repository == "<none>" && tag == "<none>",
                container_refs: row
                    .get("Containers")
                    .and_then(Value::as_str)
                    .and_then(|value| value.parse().ok()),
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    Ok(Inventory { containers, images })
}

fn safe_version(text: &str) -> Result<String, String> {
    let value = text.trim();
    if value.is_empty()
        || value.len() > 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'+'))
    {
        return Err("远端 Docker 版本信息无效。".into());
    }
    Ok(value.into())
}

fn linux_platform(architecture: &str) -> Option<&'static str> {
    match architecture.trim() {
        "x86_64" | "amd64" => Some("linux/amd64"),
        "aarch64" | "arm64" => Some("linux/arm64"),
        _ => None,
    }
}

fn disk_available(text: &str) -> Result<u64, String> {
    let available = text
        .lines()
        .nth(1)
        .and_then(|line| line.split_whitespace().rev().nth(2))
        .and_then(|value| value.parse::<u64>().ok())
        .ok_or("无法核实远端磁盘空间。")?;
    if available == 0 {
        return Err("远端磁盘没有可用空间。".into());
    }
    Ok(available)
}

fn cpu_counters(text: &str) -> Result<(u64, u64), String> {
    let line = text.lines().next().ok_or("无法读取 CPU 采样。")?;
    let mut parts = line.split_whitespace();
    if parts.next() != Some("cpu") {
        return Err("CPU 采样格式无效。".into());
    }
    let values = parts
        .take(8)
        .map(|value| value.parse::<u64>().map_err(|_| "CPU 采样格式无效。"))
        .collect::<Result<Vec<_>, _>>()?;
    if values.len() != 8 {
        return Err("CPU 采样格式无效。".into());
    }
    let total = values
        .iter()
        .try_fold(0u64, |sum, value| sum.checked_add(*value))
        .ok_or("CPU 采样溢出。")?;
    Ok((total, values[3].saturating_add(values[4])))
}

fn memory_kib(text: &str) -> Result<(u64, u64), String> {
    let read = |key: &str| {
        text.lines().find_map(|line| {
            let value = line.strip_prefix(key)?;
            value.split_whitespace().next()?.parse::<u64>().ok()
        })
    };
    let total = read("MemTotal:").ok_or("无法读取内存总量。")?;
    let available = read("MemAvailable:").ok_or("无法读取可用内存。")?;
    if total == 0 || available > total {
        return Err("内存采样格式无效。".into());
    }
    Ok((total - available, total))
}

fn disk_percent(text: &str) -> Result<f64, String> {
    let line = text.lines().nth(1).ok_or("无法读取磁盘采样。")?;
    let fields: Vec<_> = line.split_whitespace().collect();
    let total = fields
        .get(1)
        .and_then(|value| value.parse::<u64>().ok())
        .ok_or("磁盘采样格式无效。")?;
    let used = fields
        .get(2)
        .and_then(|value| value.parse::<u64>().ok())
        .ok_or("磁盘采样格式无效。")?;
    if total == 0 || used > total {
        return Err("磁盘采样格式无效。".into());
    }
    Ok(used as f64 * 100. / total as f64)
}

fn default_route_interface(text: &str) -> Option<String> {
    text.lines()
        .skip(1)
        .filter_map(|line| {
            let fields: Vec<_> = line.split_whitespace().collect();
            if fields.get(1) != Some(&"00000000") {
                return None;
            }
            Some((
                fields.first()?.to_string(),
                fields.get(6)?.parse::<u32>().ok()?,
            ))
        })
        .min_by_key(|(_, metric)| *metric)
        .map(|(name, _)| name)
}

fn network_bytes(text: &str, interface: &str) -> Result<(u64, u64), String> {
    for line in text.lines().skip(2) {
        let (name, counters) = line.split_once(':').ok_or("网络采样格式无效。")?;
        if name.trim() != interface {
            continue;
        }
        let fields: Vec<_> = counters.split_whitespace().collect();
        let rx = fields
            .first()
            .and_then(|value| value.parse::<u64>().ok())
            .ok_or("网络采样格式无效。")?;
        let tx = fields
            .get(8)
            .and_then(|value| value.parse::<u64>().ok())
            .ok_or("网络采样格式无效。")?;
        return Ok((rx, tx));
    }
    Err("未找到默认路由的网络接口。".into())
}

async fn fetch_monitor(material: ConnectionMaterial) -> Result<MonitorSnapshot, MonitorError> {
    if material.trusted_fingerprint.is_none() {
        return Err(MonitorError::Configuration(
            "请先在主机页确认 SSH 指纹。".into(),
        ));
    }
    let handle = tokio::time::timeout(Duration::from_secs(15), trusted_handle(material))
        .await
        .map_err(|_| MonitorError::Connection("SSH 连接或认证超时。".into()))?
        .map_err(MonitorError::Connection)?;
    let endpoint = local_docker_endpoint(&handle)
        .await
        .map_err(MonitorError::Configuration)?;
    let first_cpu = cpu_counters(&command(&handle, "head -n 1 /proc/stat").await?)?;
    let interface = command(&handle, "cat /proc/net/route")
        .await
        .ok()
        .and_then(|text| default_route_interface(&text));
    let first_network = if let Some(interface) = &interface {
        command_limited(&handle, "cat /proc/net/dev", 256 * 1024)
            .await
            .ok()
            .and_then(|text| network_bytes(&text, interface).ok())
    } else {
        None
    };
    let started = std::time::Instant::now();
    tokio::time::sleep(Duration::from_secs(1)).await;
    let second_cpu = cpu_counters(&command(&handle, "head -n 1 /proc/stat").await?)?;
    let second_network = if let Some(interface) = &interface {
        command_limited(&handle, "cat /proc/net/dev", 256 * 1024)
            .await
            .ok()
            .and_then(|text| network_bytes(&text, interface).ok())
    } else {
        None
    };
    let seconds = started.elapsed().as_secs_f64().max(0.001);
    let total = second_cpu.0.saturating_sub(first_cpu.0);
    if total == 0 {
        return Err(MonitorError::Sampling(
            "CPU 采样间隔内没有有效数据。".into(),
        ));
    }
    let idle = second_cpu.1.saturating_sub(first_cpu.1).min(total);
    let cpu_percent = (total - idle) as f64 * 100. / total as f64;
    let (memory_used_kib, memory_total_kib) =
        memory_kib(&command_limited(&handle, "cat /proc/meminfo", 16 * 1024).await?)?;
    let disk_percent = disk_percent(&command(&handle, "df -Pk /").await?)?;
    let list = tokio::time::timeout(
        Duration::from_secs(5),
        command_limited(
            &handle,
            &docker_command(
                &endpoint,
                "ps --filter status=running --format '{{json .}}'",
            ),
            2 * 1024 * 1024,
        ),
    )
    .await
    .ok()
    .and_then(Result::ok);
    let containers = if let Some(list) = list {
        tokio::time::timeout(
            Duration::from_secs(5),
            command_limited(
                &handle,
                &docker_command(&endpoint, "stats --no-stream --format '{{json .}}'"),
                2 * 1024 * 1024,
            ),
        )
        .await
        .ok()
        .and_then(Result::ok)
        .and_then(|stats| {
            parse_inventory(&list, "", &stats).ok().map(|inventory| {
                inventory
                    .containers
                    .into_iter()
                    .filter(|container| container.state == "running")
                    .collect()
            })
        })
    } else {
        None
    };
    let _ = handle.disconnect(Disconnect::ByApplication, "", "en").await;
    Ok(MonitorSnapshot {
        cpu_percent,
        memory_used_kib,
        memory_total_kib,
        disk_percent,
        network_bytes_per_sec: first_network.zip(second_network).map(|(first, second)| {
            (
                second.0.saturating_sub(first.0) as f64 / seconds,
                second.1.saturating_sub(first.1) as f64 / seconds,
            )
        }),
        containers,
    })
}

pub fn monitor_host(
    expected_host: Host,
    cancel: Arc<AtomicBool>,
) -> Result<MonitorSnapshot, MonitorError> {
    let material = material_for(&expected_host).map_err(MonitorError::Configuration)?;
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| MonitorError::Connection("无法初始化 SSH 运行环境。".into()))?
        .block_on(async {
            tokio::select! {
                _ = wait_cancel(&cancel) => Err(MonitorError::Connection("资源采样已取消。".into())),
                result = tokio::time::timeout(Duration::from_secs(30), fetch_monitor(material)) =>
                    result.map_err(|_| MonitorError::Sampling("资源监控超时，请核对主机连接。".into()))?,
            }
        })
}

enum ConnectionOutcome {
    NeedsTrust(TestResult),
    Authenticated(client::Handle<HostKeyChecker>),
}

async fn connect_authenticated(material: ConnectionMaterial) -> Result<ConnectionOutcome, String> {
    let observed = Arc::new(Mutex::new(None));
    let checker = HostKeyChecker {
        trusted: material.trusted_fingerprint.clone(),
        observed: observed.clone(),
    };
    let config = Arc::new(client::Config {
        inactivity_timeout: Some(Duration::from_secs(30)),
        ..Default::default()
    });
    let connection = client::connect(
        config,
        (material.host.address.as_str(), material.host.port),
        checker,
    )
    .await;
    let Some(mut handle) = connection.ok() else {
        let observed_key = observed
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        return match observed_key {
            Some((fingerprint, true)) => {
                Ok(ConnectionOutcome::NeedsTrust(TestResult::NeedsTrust {
                    address: material.host.address,
                    port: material.host.port,
                    previous: material.trusted_fingerprint,
                    fingerprint,
                }))
            }
            _ => Err("SSH 连接失败或超时；请检查地址、端口与网络。".into()),
        };
    };
    let user = material.host.username;
    let authenticated = match material.host.auth.as_str() {
        "password" => handle
            .authenticate_password(user, material.credential)
            .await
            .map_err(|_| "SSH 密码认证失败。")?
            .success(),
        "key" => {
            let key = PrivateKey::from_openssh(material.credential.as_bytes())
                .map_err(|_| "私钥格式无效或已加密；请使用未加密的 OpenSSH 私钥。")?;
            let hash = handle
                .best_supported_rsa_hash()
                .await
                .map_err(|_| "SSH 私钥算法协商失败。")?
                .flatten();
            handle
                .authenticate_publickey(user, PrivateKeyWithHashAlg::new(Arc::new(key), hash))
                .await
                .map_err(|_| "SSH 私钥认证失败。")?
                .success()
        }
        _ => return Err("主机认证方式无效。".into()),
    };
    if !authenticated {
        return Err("SSH 认证失败，请核对用户名与凭据。".into());
    }
    Ok(ConnectionOutcome::Authenticated(handle))
}

async fn probe(material: ConnectionMaterial) -> Result<TestResult, String> {
    let handle = match connect_authenticated(material).await? {
        ConnectionOutcome::NeedsTrust(challenge) => return Ok(challenge),
        ConnectionOutcome::Authenticated(handle) => handle,
    };
    let endpoint = local_docker_endpoint(&handle).await?;
    let docker_version = safe_version(
        &command(
            &handle,
            &docker_command(&endpoint, "info --format '{{.ServerVersion}}'"),
        )
        .await?,
    )?;
    let compose_version = safe_version(
        &command(
            &handle,
            &docker_command(&endpoint, "compose version --short"),
        )
        .await?,
    )?;
    let system_kernel = command(&handle, "uname -s -r").await?.trim().to_owned();
    if system_kernel.is_empty()
        || system_kernel.len() > 128
        || system_kernel.chars().any(char::is_control)
    {
        return Err("远端系统信息无效。".into());
    }
    let host_architecture = linux_platform(&command(&handle, "uname -m").await?);
    let daemon_architecture = match command(
        &handle,
        &docker_command(&endpoint, "info --format '{{.OSType}}/{{.Architecture}}'"),
    )
    .await?
    .trim()
    {
        "linux/x86_64" | "linux/amd64" => Some("linux/amd64"),
        "linux/aarch64" | "linux/arm64" => Some("linux/arm64"),
        _ => None,
    };
    if daemon_architecture.is_some() && host_architecture != daemon_architecture {
        return Err("Docker daemon 架构与 SSH 主机不一致；请核对远端 Docker 连接目标。".into());
    }
    let architecture = daemon_architecture.map(str::to_owned);
    let disk_available_kib = disk_available(&command(&handle, "df -Pk /").await?)?;
    let sftp_v3_handshake = tokio::time::timeout(Duration::from_secs(5), async {
        let channel = handle.channel_open_session().await.map_err(|_| ())?;
        channel
            .request_subsystem(true, "sftp")
            .await
            .map_err(|_| ())?;
        let sftp = RawSftpSession::new(channel.into_stream());
        if sftp.init().await.map_err(|_| ())?.version != 3 {
            return Err(());
        }
        Ok::<_, ()>(())
    })
    .await
    .is_ok_and(|result| result.is_ok());
    let _ = handle.disconnect(Disconnect::ByApplication, "", "en").await;
    Ok(TestResult::Connected {
        docker_version,
        compose_version,
        system_kernel,
        architecture,
        disk_available_kib,
        sftp_v3_handshake,
    })
}

pub fn test_host(id: i64) -> Result<TestResult, String> {
    let material = host_store::connection_material(id)?;
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| "无法初始化 SSH 运行环境。")?
        .block_on(async {
            tokio::time::timeout(Duration::from_secs(20), probe(material))
                .await
                .map_err(|_| "SSH 测试超时。")?
        })
}

pub fn test_host_snapshot(
    expected_host: Host,
    cancel: Arc<AtomicBool>,
) -> Result<TestResult, String> {
    let material = material_for(&expected_host)?;
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| "无法初始化 SSH 运行环境。")?
        .block_on(async {
            tokio::select! {
                _ = wait_cancel(&cancel) => Err("主机架构检查已取消。".into()),
                result = tokio::time::timeout(Duration::from_secs(20), probe(material)) =>
                    result.map_err(|_| "主机架构检查超时。")?,
            }
        })
}

async fn fetch_inventory(material: ConnectionMaterial) -> Result<Inventory, String> {
    let handle = trusted_handle(material).await?;
    let endpoint = local_docker_endpoint(&handle).await?;
    let containers = command_limited(
        &handle,
        &docker_command(&endpoint, "ps --all --format '{{json .}}'"),
        2 * 1024 * 1024,
    )
    .await?;
    let images = command_limited(
        &handle,
        &docker_command(&endpoint, "image ls --format '{{json .}}'"),
        2 * 1024 * 1024,
    )
    .await?;
    let stats = command_limited(
        &handle,
        &docker_command(&endpoint, "stats --no-stream --all --format '{{json .}}'"),
        2 * 1024 * 1024,
    )
    .await?;
    let _ = handle.disconnect(Disconnect::ByApplication, "", "en").await;
    parse_inventory(&containers, &images, &stats)
}

pub fn inventory(id: i64) -> Result<Inventory, String> {
    let material = host_store::connection_material(id)?;
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| "无法初始化 SSH 运行环境。")?
        .block_on(async {
            tokio::time::timeout(Duration::from_secs(35), fetch_inventory(material))
                .await
                .map_err(|_| "容器列表读取超时。")?
        })
}

async fn execute_container_action(
    material: ConnectionMaterial,
    container_id: &str,
    action: ContainerAction,
) -> Result<(), String> {
    validate_container_id(container_id)?;
    let handle = trusted_handle(material).await?;
    let endpoint = local_docker_endpoint(&handle).await?;
    let command_text = container_command(&endpoint, container_id, action)?;
    execute_container_command(&handle, &command_text, action).await?;
    let _ = handle.disconnect(Disconnect::ByApplication, "", "en").await;
    Ok(())
}

pub fn container_action(
    expected_host: Host,
    container_id: &str,
    action: ContainerAction,
) -> Result<(), String> {
    let material = material_for(&expected_host)?;
    let container_id = container_id.to_owned();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| "无法初始化 SSH 运行环境。")?
        .block_on(async {
            tokio::time::timeout(
                Duration::from_secs(45),
                execute_container_action(material, &container_id, action),
            )
            .await
            .map_err(|_| "容器操作超时，远端结果待核实；请刷新状态，不要盲目重试。")?
        })
}

async fn follow_logs(
    material: ConnectionMaterial,
    container_id: &str,
    cancel: &AtomicBool,
    sender: &SyncSender<LogEvent>,
) -> Result<(), String> {
    if material.trusted_fingerprint.is_none() {
        return Err("请先在主机页确认 SSH 指纹。".into());
    }
    let handle = tokio::select! {
        _ = wait_cancel(cancel) => return Ok(()),
        result = tokio::time::timeout(Duration::from_secs(20), connect_authenticated(material)) =>
            result.map_err(|_| "SSH 日志连接超时。")??,
    };
    let handle = match handle {
        ConnectionOutcome::NeedsTrust(_) => return Err("主机指纹已变化，日志读取已阻断。".into()),
        ConnectionOutcome::Authenticated(handle) => handle,
    };
    let endpoint = tokio::select! {
        _ = wait_cancel(cancel) => return Ok(()),
        result = local_docker_endpoint(&handle) => result?,
    };
    let mut channel = tokio::select! {
        _ = wait_cancel(cancel) => return Ok(()),
        result = tokio::time::timeout(Duration::from_secs(10), handle.channel_open_session()) =>
            result.map_err(|_| "SSH 日志会话建立超时。")?
                .map_err(|_| "无法打开 SSH 日志会话。")?,
    };
    tokio::select! {
        _ = wait_cancel(cancel) => { let _ = channel.close().await; return Ok(()); }
        result = tokio::time::timeout(Duration::from_secs(10), channel.exec(
            true, docker_command(&endpoint, &format!("logs --follow --tail 1000 --timestamps {container_id}")))) => {
            result.map_err(|_| "远端日志命令启动超时。")?
                .map_err(|_| "无法启动远端日志流。")?;
        }
    }
    let mut stdout = LogLineBuffer::new();
    let mut stderr = LogLineBuffer::new();
    let mut exit = None;
    loop {
        if cancel.load(Ordering::Relaxed) {
            let _ = channel.close().await;
            let _ = handle.disconnect(Disconnect::ByApplication, "", "en").await;
            return Ok(());
        }
        tokio::select! {
            message = channel.wait() => match message {
                Some(ChannelMsg::Data { data }) => if !stdout.push(&data, sender) { return Ok(()); },
                Some(ChannelMsg::ExtendedData { data, .. }) => if !stderr.push(&data, sender) { return Ok(()); },
                Some(ChannelMsg::ExitStatus { exit_status }) => exit = Some(exit_status),
                Some(_) => {},
                None => break,
            },
            _ = tokio::time::sleep(Duration::from_millis(250)) => {},
        }
    }
    if !stdout.pending.is_empty() && !stdout.flush(sender) {
        return Ok(());
    }
    if !stderr.pending.is_empty() && !stderr.flush(sender) {
        return Ok(());
    }
    match exit {
        Some(0) => Ok(()),
        Some(_) => Err("远端 Docker 日志命令失败；请核对容器状态与主机权限。".into()),
        None => Err("SSH 日志流已断开；请重新打开后核对。".into()),
    }
}

async fn wait_cancel(cancel: &AtomicBool) {
    while !cancel.load(Ordering::Relaxed) {
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

pub fn stream_logs(
    expected_host: Host,
    container_id: String,
    cancel: Arc<AtomicBool>,
    sender: SyncSender<LogEvent>,
) {
    let result = (|| -> Result<(), String> {
        validate_container_id(&container_id)?;
        let material = material_for(&expected_host)?;
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|_| "无法初始化 SSH 日志环境。")?
            .block_on(follow_logs(material, &container_id, &cancel, &sender))
    })();
    let _ = sender.send(match result {
        Ok(()) => LogEvent::Ended,
        Err(error) => LogEvent::Error(error),
    });
}

async fn archive_command(
    handle: &client::Handle<HostKeyChecker>,
    text: &str,
) -> Result<String, String> {
    archive_command_with_timeout(handle, text, Duration::from_secs(10)).await
}

async fn archive_command_with_timeout(
    handle: &client::Handle<HostKeyChecker>,
    text: &str,
    timeout: Duration,
) -> Result<String, String> {
    tokio::time::timeout(timeout, command_limited(handle, text, 4096))
        .await
        .map_err(|_| "远端文件操作超时。")?
}

async fn upload_to_directory(
    handle: &client::Handle<HostKeyChecker>,
    source: &std::path::Path,
    directory: &str,
    size: u64,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(u64, u64),
) -> Result<UploadedArchive, String> {
    let channel = tokio::time::timeout(Duration::from_secs(10), handle.channel_open_session())
        .await
        .map_err(|_| "SFTP 会话建立超时。")?
        .map_err(|_| "无法打开 SFTP 会话。")?;
    tokio::time::timeout(
        Duration::from_secs(10),
        channel.request_subsystem(true, "sftp"),
    )
    .await
    .map_err(|_| "SFTP 子系统启动超时。")?
    .map_err(|_| "远端 SFTP 服务不可用。")?;
    let sftp = tokio::time::timeout(
        Duration::from_secs(10),
        SftpSession::new(channel.into_stream()),
    )
    .await
    .map_err(|_| "SFTP 协议握手超时。")?
    .map_err(|_| "SFTP 协议握手失败。")?;
    let path = format!("{directory}/image.tar");
    let mut remote = tokio::time::timeout(
        Duration::from_secs(10),
        sftp.open_with_flags_and_attributes(
            path.clone(),
            OpenFlags::CREATE | OpenFlags::EXCLUDE | OpenFlags::WRITE,
            FileAttributes {
                permissions: Some(0o600),
                ..Default::default()
            },
        ),
    )
    .await
    .map_err(|_| "远端镜像文件创建超时。")?
    .map_err(|_| "无法在远端临时目录创建镜像文件。")?;
    let mut local = tokio::fs::File::open(source)
        .await
        .map_err(|_| "无法读取本地镜像归档。")?;
    let mut hasher = Sha256::new();
    let mut copied = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    let deadline = std::time::Instant::now() + Duration::from_secs(60 * 60);
    loop {
        if std::time::Instant::now() >= deadline {
            return Err("镜像传输超时，远端临时文件待清理。".into());
        }
        let read = tokio::select! {
            _ = wait_cancel(cancel) => return Err("镜像传输已取消。".into()),
            result = tokio::time::timeout(Duration::from_secs(60), local.read(&mut buffer)) =>
                result.map_err(|_| "本地镜像读取超时。")?
                    .map_err(|_| "无法读取本地镜像归档。")?,
        };
        if read == 0 {
            break;
        }
        tokio::select! {
            _ = wait_cancel(cancel) => return Err("镜像传输已取消。".into()),
            result = tokio::time::timeout(Duration::from_secs(60), remote.write_all(&buffer[..read])) =>
                result.map_err(|_| "远端镜像写入超时。")?
                    .map_err(|_| "远端镜像写入失败。")?,
        }
        hasher.update(&buffer[..read]);
        copied += read as u64;
        if copied > size {
            return Err("本地镜像归档在传输期间发生变化。".into());
        }
        progress(copied.min(size - 1), size);
    }
    if copied != size {
        return Err("本地镜像归档在传输期间发生变化。".into());
    }
    tokio::select! {
        _ = wait_cancel(cancel) => return Err("镜像传输已取消，远端结果待核实。".into()),
        result = tokio::time::timeout(Duration::from_secs(30), remote.close()) =>
            result.map_err(|_| "远端镜像文件关闭超时，结果待核实。")?
                .map_err(|_| "远端镜像文件关闭失败。")?,
    }
    let metadata = tokio::select! {
        _ = wait_cancel(cancel) => return Err("镜像传输已取消，远端结果待核实。".into()),
        result = tokio::time::timeout(Duration::from_secs(10), sftp.metadata(path.clone())) =>
            result.map_err(|_| "远端镜像属性检查超时。")?
                .map_err(|_| "无法核实远端镜像文件属性。")?,
    };
    let _ = sftp.close().await;
    if metadata.len() != size {
        return Err("远端镜像大小与本地不符。".into());
    }
    if metadata
        .permissions
        .is_none_or(|mode| mode & 0o777 != 0o600)
    {
        return Err("远端镜像文件权限未达到 600，传输已阻断。".into());
    }
    let local_digest = format!("{:x}", hasher.finalize());
    let checksum_command = format!("sha256sum -- {path}");
    let remote_digest = tokio::select! {
        _ = wait_cancel(cancel) => return Err("镜像校验已取消，远端结果待核实。".into()),
        result = archive_command_with_timeout(handle, &checksum_command, Duration::from_secs(60 * 60)) =>
            verified_sha256(&result?)?,
    };
    if local_digest != remote_digest {
        return Err("远端镜像 SHA-256 与本地不符。".into());
    }
    if cancel.load(Ordering::Relaxed) {
        return Err("镜像校验已取消，远端结果待核实。".into());
    }
    progress(size, size);
    Ok(UploadedArchive {
        directory: directory.to_owned(),
        path,
        bytes: size,
        sha256: local_digest,
    })
}

fn cleanup_archive_command(directory: &str) -> Result<String, String> {
    let directory = validated_upload_directory(directory)?;
    Ok(format!(
        "rm -f -- {directory}/image.tar && (rmdir -- {directory} || (test ! -e {directory} && test ! -L {directory}))"
    ))
}

async fn transfer_archive(
    material: ConnectionMaterial,
    source: &std::path::Path,
    size: u64,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(u64, u64),
) -> Result<UploadedArchive, String> {
    let handle = tokio::time::timeout(Duration::from_secs(20), trusted_handle(material))
        .await
        .map_err(|_| "SSH 连接超时，传输未开始。")??;
    let available_kib = disk_available(&archive_command(&handle, "df -Pk /tmp").await?)?;
    if size.saturating_add(64 * 1024 * 1024) > available_kib.saturating_mul(1024) {
        return Err("远端 /tmp 可用空间不足，传输未开始。".into());
    }
    if cancel.load(Ordering::Relaxed) {
        return Err("镜像传输已取消，传输未开始。".into());
    }
    let directory = validated_upload_directory(&format!(
        "/tmp/devingpanel.{}",
        uuid::Uuid::new_v4().simple()
    ))?;
    let create = archive_command(&handle, &format!(
        "mkdir -m 700 -- {directory} && chmod 700 -- {directory} && test \"$(stat -c %a -- {directory})\" = 700"
    )).await;
    if let Err(error) = create {
        // A timed-out mkdir may have succeeded; the random path is known, so try to remove it.
        let _ = archive_command(&handle, &format!("rmdir -- {directory}")).await;
        return Err(format!(
            "无法安全创建远端临时目录：{error}；请核对 {directory} 是否残留。"
        ));
    }
    let result = tokio::time::timeout(
        Duration::from_secs(2 * 60 * 60 + 60),
        upload_to_directory(&handle, source, &directory, size, cancel, progress),
    )
    .await
    .unwrap_or_else(|_| Err("镜像传输超时，远端结果待核实。".to_owned()));
    if result.is_err() {
        let cleanup = archive_command(&handle, &cleanup_archive_command(&directory)?).await;
        let _ = handle.disconnect(Disconnect::ByApplication, "", "en").await;
        return result.map_err(|error| match cleanup {
            Ok(_) => error,
            Err(_) => format!("{error}；临时目录 {directory} 可能残留，请到主机核对。"),
        });
    }
    let _ = handle.disconnect(Disconnect::ByApplication, "", "en").await;
    result
}

#[allow(dead_code)] // The release coordinator will call this after docker save is implemented.
pub fn upload_archive(
    expected_host: Host,
    source: &std::path::Path,
    cancel: Arc<AtomicBool>,
    mut progress: impl FnMut(u64, u64),
) -> Result<UploadedArchive, String> {
    let material = material_for(&expected_host)?;
    let size = std::fs::metadata(source)
        .map_err(|_| "本地镜像归档不存在。")?
        .len();
    if size == 0 {
        return Err("本地镜像归档为空。".into());
    }
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| "无法初始化传输环境。".to_owned())?
        .block_on(transfer_archive(
            material,
            source,
            size,
            &cancel,
            &mut progress,
        ))
}

#[allow(dead_code)] // The release coordinator calls this after docker load or on retry.
pub fn cleanup_uploaded_archive(
    expected_host: Host,
    archive: &UploadedArchive,
) -> Result<(), String> {
    let directory = validated_upload_directory(&archive.directory)?;
    if archive.path != format!("{directory}/image.tar") {
        return Err("远端归档路径无效，未执行清理。".into());
    }
    let material = material_for(&expected_host)?;
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| "无法初始化远端清理环境。".to_owned())?
        .block_on(async {
            let handle = tokio::time::timeout(Duration::from_secs(20), trusted_handle(material))
                .await
                .map_err(|_| "SSH 清理连接超时。")??;
            archive_command(&handle, &cleanup_archive_command(&directory)?).await?;
            let _ = handle.disconnect(Disconnect::ByApplication, "", "en").await;
            Ok(())
        })
}

fn validated_load_target(
    archive: &UploadedArchive,
    reference: &str,
    image_id: &str,
    platform: &str,
) -> Result<(), String> {
    let directory = validated_upload_directory(&archive.directory)?;
    if archive.path != format!("{directory}/image.tar")
        || archive.bytes == 0
        || archive.sha256.len() != 64
        || !archive.sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err("远端镜像归档信息无效。".into());
    }
    let (image, tag) = reference.rsplit_once(':').ok_or("镜像引用无效。")?;
    if !crate::project_store::valid_image_name(image)
        || tag.is_empty()
        || tag.len() > 128
        || !tag
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        || !tag
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-'))
        || image_id.len() != 71
        || !image_id.starts_with("sha256:")
        || !image_id[7..].bytes().all(|byte| byte.is_ascii_hexdigit())
        || !matches!(platform, "linux/amd64" | "linux/arm64")
    {
        return Err("待加载镜像引用、ID 或架构无效。".into());
    }
    Ok(())
}

fn verified_loaded_image(text: &str, image_id: &str, platform: &str) -> Result<(), String> {
    let row: Value = serde_json::from_str(text).map_err(|_| "远端镜像元数据无效，结果待核实。")?;
    let actual = format!(
        "{}/{}",
        row.get("Os").and_then(Value::as_str).unwrap_or_default(),
        row.get("Architecture")
            .and_then(Value::as_str)
            .unwrap_or_default()
    );
    if row.get("Id").and_then(Value::as_str) != Some(image_id) || actual != platform {
        return Err("远端镜像 ID 或架构与本机构建结果不一致，禁止部署。".into());
    }
    Ok(())
}

async fn load_archive(
    material: ConnectionMaterial,
    archive: &UploadedArchive,
    reference: &str,
    image_id: &str,
    platform: &str,
    cancel: &AtomicBool,
) -> Result<(), String> {
    validated_load_target(archive, reference, image_id, platform)?;
    let handle = tokio::time::timeout(Duration::from_secs(20), trusted_handle(material))
        .await
        .map_err(|_| "远端镜像加载连接超时。")??;
    let endpoint = tokio::time::timeout(Duration::from_secs(10), local_docker_endpoint(&handle))
        .await
        .map_err(|_| "远端 Docker 端点检查超时。")??;
    let host_platform = linux_platform(&archive_command(&handle, "uname -m").await?);
    let daemon_platform = archive_command(
        &handle,
        &docker_command(&endpoint, "info --format '{{.OSType}}/{{.Architecture}}'"),
    )
    .await?;
    let daemon_platform = match daemon_platform.trim() {
        "linux/x86_64" | "linux/amd64" => Some("linux/amd64"),
        "linux/aarch64" | "linux/arm64" => Some("linux/arm64"),
        _ => None,
    };
    if host_platform != Some(platform) || daemon_platform != Some(platform) {
        return Err("远端主机或 Docker daemon 架构与待加载镜像不一致。".into());
    }
    let checksum_command = format!("sha256sum -- {}", archive.path);
    let digest = tokio::select! {
        _ = wait_cancel(cancel) => return Err("远端归档复核已取消，尚未加载镜像。".into()),
        result = archive_command_with_timeout(&handle, &checksum_command, Duration::from_secs(60 * 60)) =>
            verified_sha256(&result?)?,
    };
    if digest != archive.sha256 {
        return Err("远端镜像归档校验值已变化，禁止加载。".into());
    }
    let existing = archive_command(
        &handle,
        &docker_command(
            &endpoint,
            &format!("image ls --filter reference={reference} --format '{{{{.Repository}}}}:{{{{.Tag}}}}'"),
        ),
    )
    .await?;
    if existing.lines().any(|line| line == reference) {
        return Err("远端镜像 tag 已存在，禁止覆盖。".into());
    }
    if cancel.load(Ordering::Relaxed) {
        return Err("远端镜像加载已取消，尚未执行。".into());
    }
    let load = docker_command(&endpoint, &format!("image load --input {}", archive.path));
    tokio::select! {
        _ = wait_cancel(cancel) => return Err("远端镜像加载已取消，结果待核实；请保留临时归档。".into()),
        result = tokio::time::timeout(Duration::from_secs(60 * 60), command_limited(&handle, &load, 64 * 1024)) =>
            result.map_err(|_| "远端镜像加载超时，结果待核实；请保留临时归档。")?
                .map_err(|_| "远端镜像加载失败或连接中断，结果待核实；请保留临时归档。")?,
    };
    let inspect_format = concat!(
        "image inspect --format '",
        r#"{"Id":{{json .Id}},"Os":{{json .Os}},"Architecture":{{json .Architecture}}}"#,
        "' "
    );
    let inspect = docker_command(&endpoint, &format!("{inspect_format}{reference}"));
    let result = archive_command(&handle, &inspect)
        .await
        .map_err(|_| "远端镜像加载后无法核对元数据，结果待核实。")?;
    verified_loaded_image(&result, image_id, platform)?;
    let _ = handle.disconnect(Disconnect::ByApplication, "", "en").await;
    Ok(())
}

#[allow(dead_code)] // The release coordinator will call this before deploying Compose.
pub fn load_uploaded_archive(
    expected_host: Host,
    archive: &UploadedArchive,
    reference: &str,
    image_id: &str,
    platform: &str,
    cancel: Arc<AtomicBool>,
) -> Result<(), String> {
    validated_load_target(archive, reference, image_id, platform)?;
    let material = material_for(&expected_host)?;
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| "无法初始化远端镜像加载环境。".to_owned())?
        .block_on(load_archive(
            material, archive, reference, image_id, platform, &cancel,
        ))
}

#[cfg(test)]
mod tests {
    use super::{
        ContainerAction, LogEvent, LogLineBuffer, TestResult, cleanup_archive_command,
        container_command, cpu_counters, default_route_interface, disk_available, disk_percent,
        execute_container_action, execute_disk_prune, fetch_disk_preview, fetch_inventory,
        fetch_monitor, follow_logs, image_size_bytes, memory_kib, network_bytes,
        parse_disk_preview, parse_inventory, probe, safe_version, same_connection_target,
        validated_upload_directory, verified_sha256,
    };
    use crate::host_store::{ConnectionMaterial, Host};
    use russh::server::{self, Auth, Msg, Server as _, Session};
    use russh::{Channel, ChannelId};
    use russh_sftp::protocol::{Attrs, FileAttributes, Handle, OpenFlags, Status, StatusCode};
    use sha2::Digest;
    use std::collections::HashMap;
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    };
    use std::time::Duration;

    #[test]
    fn upload_paths_and_checksums_reject_untrusted_remote_output() {
        let directory =
            validated_upload_directory("/tmp/devingpanel.a1b2c3d4e5f60718293a4b5c6d7e8f90")
                .unwrap();
        assert_eq!(
            cleanup_archive_command(&directory).unwrap(),
            "rm -f -- /tmp/devingpanel.a1b2c3d4e5f60718293a4b5c6d7e8f90/image.tar && (rmdir -- /tmp/devingpanel.a1b2c3d4e5f60718293a4b5c6d7e8f90 || (test ! -e /tmp/devingpanel.a1b2c3d4e5f60718293a4b5c6d7e8f90 && test ! -L /tmp/devingpanel.a1b2c3d4e5f60718293a4b5c6d7e8f90))"
        );
        for text in [
            "/tmp/other.a1b2c3d4e5f60718293a4b5c6d7e8f90",
            "/tmp/devingpanel.a1b2c3d4e5f60718293a4b5c6d7e8f9;",
            "/tmp/devingpanel.abcdefghi",
            "/tmp/devingpanel.a1b2c3d4e5f60718293a4b5c6d7e8f90/extra",
        ] {
            assert!(validated_upload_directory(text).is_err());
        }
        let digest = "a".repeat(64);
        assert_eq!(
            verified_sha256(&format!("{digest}  /tmp/archive\n")).unwrap(),
            digest
        );
        assert!(verified_sha256("oops /tmp/archive\n").is_err());
    }

    #[test]
    fn log_line_buffer_preserves_utf8_boundaries_and_limits_lines() {
        let (sender, receiver) = std::sync::mpsc::sync_channel(4);
        let mut buffer = LogLineBuffer::new();
        let text = "日志".as_bytes();
        assert!(buffer.push(&text[..2], &sender));
        assert!(buffer.push(&text[2..], &sender));
        assert!(buffer.push(b"\n", &sender));
        assert!(matches!(receiver.try_recv().unwrap(), LogEvent::Line(line) if line == "日志"));
        assert!(buffer.push(&vec![b'a'; 17 * 1024], &sender));
        assert!(buffer.push(b"\n", &sender));
        assert!(
            matches!(receiver.try_recv().unwrap(), LogEvent::Line(line) if line.len() < 17 * 1024 && line.ends_with("[单行截断]"))
        );
        let mut stdout = LogLineBuffer::new();
        let mut stderr = LogLineBuffer::new();
        assert!(stdout.push(b"stdout-part", &sender));
        assert!(stderr.push(b"stderr-line\n", &sender));
        assert!(stdout.push(b"\n", &sender));
        assert!(
            matches!(receiver.try_recv().unwrap(), LogEvent::Line(line) if line == "stderr-line")
        );
        assert!(
            matches!(receiver.try_recv().unwrap(), LogEvent::Line(line) if line == "stdout-part")
        );
    }

    #[tokio::test]
    async fn closing_logs_before_connect_does_not_wait_for_ssh() {
        let (sender, _) = std::sync::mpsc::sync_channel(1);
        let cancel = std::sync::atomic::AtomicBool::new(true);
        let material = ConnectionMaterial {
            host: Host {
                id: 1,
                name: "stub".into(),
                address: "127.0.0.1".into(),
                port: 1,
                username: "deploy".into(),
                group: String::new(),
                auth: "password".into(),
                trusted_fingerprint: Some("SHA256:synthetic".into()),
            },
            credential: "synthetic".into(),
            trusted_fingerprint: Some("SHA256:synthetic".into()),
        };
        tokio::time::timeout(
            Duration::from_secs(1),
            follow_logs(material, "abc123", &cancel, &sender),
        )
        .await
        .unwrap()
        .unwrap();
    }

    #[test]
    fn sanitizes_remote_version() {
        assert_eq!(safe_version("29.5.3\n").unwrap(), "29.5.3");
        assert!(safe_version("version\nSECRET=value").is_err());
        assert!(safe_version("\x1b[31m29.5.3").is_err());
        assert_eq!(disk_available("Filesystem 1024-blocks Used Available Capacity Mounted on\n/dev/sda 1000 200 800 20% /\n").unwrap(), 800);
        assert!(disk_available("invalid").is_err());
    }

    #[test]
    fn parses_only_container_and_image_display_fields() {
        let containers = r#"{"ID":"abc123","Names":"web","Image":"app:v1","Status":"Up 3 days","State":"running","Ports":"127.0.0.1:80->8080/tcp","Labels":"SECRET=hidden"}"#;
        let stats = r#"{"ID":"abc123","CPUPerc":"2.0%","MemUsage":"120MiB / 1GiB","Labels":"SECRET=hidden"}"#;
        let images = r#"{"Repository":"app","Tag":"v1","Size":"180MB","CreatedSince":"2 hours ago","Containers":"2","Labels":"SECRET=hidden"}"#;
        let inventory = parse_inventory(containers, images, stats).unwrap();
        assert_eq!(inventory.containers[0].cpu, "2.0%");
        assert_eq!(inventory.containers[0].memory, "120MiB / 1GiB");
        assert_eq!(inventory.images[0].reference, "app:v1");
        assert_eq!(inventory.images[0].container_refs, Some(2));
        assert!(
            !format!(
                "{} {}",
                inventory.containers[0].name, inventory.images[0].reference
            )
            .contains("SECRET")
        );
        assert!(
            parse_inventory(
                &containers.replace("Up 3 days", "Up\nmalicious"),
                images,
                stats
            )
            .is_err()
        );
        let long_ports = "80/tcp,".repeat(200);
        let many = containers.replace("127.0.0.1:80->8080/tcp", &long_ports);
        assert!(
            parse_inventory(&many, images, stats).unwrap().containers[0]
                .ports
                .ends_with('…')
        );
    }

    #[test]
    fn container_commands_accept_only_hex_ids() {
        assert_eq!(
            super::validate_local_endpoint("unix:///run/user/1000/docker.sock").unwrap(),
            "unix:///run/user/1000/docker.sock"
        );
        assert!(super::validate_local_endpoint("tcp://remote.example:2376").is_err());
        assert!(super::validate_local_endpoint("unix:///var/run/docker.sock;uname").is_err());
        assert_eq!(
            container_command(
                "unix:///var/run/docker.sock",
                "abc123",
                ContainerAction::Start
            )
            .unwrap(),
            "docker --host unix:///var/run/docker.sock container start abc123"
        );
        assert_eq!(
            container_command(
                "unix:///var/run/docker.sock",
                "abc123",
                ContainerAction::Stop
            )
            .unwrap(),
            "docker --host unix:///var/run/docker.sock container stop --time 10 abc123"
        );
        assert!(
            container_command(
                "unix:///var/run/docker.sock",
                "abc; rm -rf /",
                ContainerAction::Delete
            )
            .is_err()
        );
        assert!(
            container_command("unix:///var/run/docker.sock", "", ContainerAction::Restart).is_err()
        );
        let host = Host {
            id: 1,
            name: "web".into(),
            address: "web.example".into(),
            port: 22,
            username: "root".into(),
            group: String::new(),
            auth: "password".into(),
            trusted_fingerprint: Some("SHA256:known".into()),
        };
        assert!(same_connection_target(&host, &host));
        let mut changed = host.clone();
        changed.address = "other.example".into();
        assert!(!same_connection_target(&host, &changed));
        changed = host.clone();
        changed.trusted_fingerprint = None;
        assert!(!same_connection_target(&host, &changed));
    }

    #[test]
    fn disk_preview_ignores_volumes_and_rejects_invalid_networks() {
        let df = concat!(
            "{\"Type\":\"Images\",\"TotalCount\":\"3\",\"Reclaimable\":\"12MB (10%)\"}\n",
            "{\"Type\":\"Local Volumes\",\"TotalCount\":\"2\",\"Reclaimable\":\"2GB (100%)\"}\n",
            "{\"Type\":\"Build Cache\",\"TotalCount\":\"1\",\"Reclaimable\":\"3MB\"}\n"
        );
        let dangling = "{\"Size\":\"1.5GB\"}\n{\"Size\":\"500MB\"}\n";
        let preview = parse_disk_preview(df, "abc123\ndef456\n", dangling).unwrap();
        assert_eq!(preview.rows.len(), 2);
        assert_eq!(preview.rows[0].total, "2");
        assert!(preview.rows[0].reclaimable.contains("2000.0 MB"));
        assert_eq!(preview.unused_networks, 2);
        assert!(parse_disk_preview(df, "abc; command\n", dangling).is_err());
        assert!(parse_disk_preview("", "", dangling).is_err());
        assert!(image_size_bytes("1;GB").is_err());
    }

    #[test]
    fn parses_linux_resource_samples_without_guessing_missing_values() {
        assert_eq!(cpu_counters("cpu  1 2 3 4 5 6 7 8\n").unwrap(), (36, 9));
        assert!(cpu_counters("cpu  1 2\n").is_err());
        assert_eq!(
            memory_kib("MemTotal: 4000 kB\nMemAvailable: 1000 kB\n").unwrap(),
            (3000, 4000)
        );
        assert!(memory_kib("MemTotal: 4000 kB\n").is_err());
        assert_eq!(
            disk_percent("Filesystem 1024-blocks Used Available Capacity Mounted on\n/dev/sda 100 25 75 25% /\n").unwrap(),
            25.
        );
        let net = "Inter-| Receive | Transmit\n face |bytes ...\n lo: 100 0 0 0 0 0 0 0 200 0 0 0 0 0 0 0\n eth0: 1000 0 0 0 0 0 0 0 2000 0 0 0 0 0 0 0\n";
        assert_eq!(network_bytes(net, "eth0").unwrap(), (1000, 2000));
        assert!(network_bytes("invalid", "eth0").is_err());
        assert_eq!(
            default_route_interface(
                "Iface Destination Gateway Flags RefCnt Use Metric Mask MTU Window IRTT\neth0 00000000 0100007F 0003 0 0 100 00000000 0 0 0\n"
            ),
            Some("eth0".into())
        );
    }

    #[test]
    #[ignore = "requires a local Docker daemon"]
    fn local_docker_json_shape_matches_inventory_parser() {
        let output = |args: &[&str]| {
            let result = std::process::Command::new("docker")
                .args(args)
                .stderr(std::process::Stdio::null())
                .output()
                .unwrap();
            assert!(result.status.success());
            String::from_utf8(result.stdout).unwrap()
        };
        let containers = output(&["ps", "--all", "--format", "{{json .}}"]);
        let images = output(&["image", "ls", "--format", "{{json .}}"]);
        let stats = output(&["stats", "--no-stream", "--all", "--format", "{{json .}}"]);
        let _ = parse_inventory(&containers, &images, &stats).unwrap();
        let df = output(&["system", "df", "--format", "{{json .}}"]);
        let networks = output(&[
            "network",
            "ls",
            "--filter",
            "dangling=true",
            "--format",
            "{{.ID}}",
        ]);
        let dangling = output(&[
            "image",
            "ls",
            "--filter",
            "dangling=true",
            "--format",
            "{{json .}}",
        ]);
        let _ = parse_disk_preview(&df, &networks, &dangling).unwrap();
    }

    #[derive(Clone)]
    struct StubServer {
        auth_attempts: Arc<AtomicUsize>,
        allowed_public_key: russh::keys::PublicKey,
        operations: Arc<Mutex<Vec<String>>>,
        cpu_samples: Arc<AtomicUsize>,
        network_samples: Arc<AtomicUsize>,
        fail_docker_stats: Arc<AtomicBool>,
        wrong_daemon_arch: Arc<AtomicBool>,
        remote_context: Arc<AtomicBool>,
        endpoint_mode: Arc<AtomicUsize>,
        sftp_channels: Vec<ChannelId>,
        sftp_pending: Vec<u8>,
        sftp_version: Arc<AtomicUsize>,
        upload_sftp: Arc<AtomicBool>,
        upload_channels: Arc<tokio::sync::Mutex<HashMap<ChannelId, Channel<Msg>>>>,
        upload_files: Arc<Mutex<HashMap<String, Vec<u8>>>>,
        upload_fail_write: Arc<AtomicBool>,
        load_tag_exists: Arc<AtomicBool>,
        load_no_exit: Arc<AtomicBool>,
    }

    struct MemorySftp {
        files: Arc<Mutex<HashMap<String, Vec<u8>>>>,
        fail_write: Arc<AtomicBool>,
    }

    impl russh_sftp::server::Handler for MemorySftp {
        type Error = StatusCode;

        fn unimplemented(&self) -> Self::Error {
            StatusCode::OpUnsupported
        }

        async fn open(
            &mut self,
            id: u32,
            filename: String,
            flags: OpenFlags,
            attrs: FileAttributes,
        ) -> Result<Handle, Self::Error> {
            if !flags.contains(OpenFlags::CREATE | OpenFlags::EXCLUDE | OpenFlags::WRITE)
                || attrs.permissions != Some(0o600)
            {
                return Err(StatusCode::PermissionDenied);
            }
            let mut files = self.files.lock().unwrap();
            if files.contains_key(&filename) {
                return Err(StatusCode::Failure);
            }
            files.insert(filename.clone(), Vec::new());
            Ok(Handle {
                id,
                handle: filename,
            })
        }

        async fn write(
            &mut self,
            id: u32,
            handle: String,
            offset: u64,
            bytes: Vec<u8>,
        ) -> Result<Status, Self::Error> {
            if self.fail_write.load(Ordering::SeqCst) {
                return Err(StatusCode::PermissionDenied);
            }
            let mut files = self.files.lock().unwrap();
            let data = files.get_mut(&handle).ok_or(StatusCode::NoSuchFile)?;
            if offset != data.len() as u64 {
                return Err(StatusCode::Failure);
            }
            data.extend_from_slice(&bytes);
            Ok(Status {
                id,
                status_code: StatusCode::Ok,
                error_message: String::new(),
                language_tag: String::new(),
            })
        }

        async fn close(&mut self, id: u32, _: String) -> Result<Status, Self::Error> {
            Ok(Status {
                id,
                status_code: StatusCode::Ok,
                error_message: String::new(),
                language_tag: String::new(),
            })
        }

        async fn stat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
            let files = self.files.lock().unwrap();
            let data = files.get(&path).ok_or(StatusCode::NoSuchFile)?;
            Ok(Attrs {
                id,
                attrs: FileAttributes {
                    size: Some(data.len() as u64),
                    permissions: Some(0o600),
                    ..Default::default()
                },
            })
        }
    }

    impl server::Server for StubServer {
        type Handler = Self;
        fn new_client(&mut self, _: Option<std::net::SocketAddr>) -> Self {
            self.clone()
        }
    }

    impl server::Handler for StubServer {
        type Error = russh::Error;

        async fn subsystem_request(
            &mut self,
            channel: ChannelId,
            name: &str,
            session: &mut Session,
        ) -> Result<(), Self::Error> {
            if name == "sftp" && self.sftp_version.load(Ordering::SeqCst) != 0 {
                if self.upload_sftp.load(Ordering::SeqCst)
                    && let Some(stream) = self.upload_channels.lock().await.remove(&channel)
                {
                    session.channel_success(channel)?;
                    let files = self.upload_files.clone();
                    let fail_write = self.upload_fail_write.clone();
                    tokio::spawn(async move {
                        russh_sftp::server::run(
                            stream.into_stream(),
                            MemorySftp { files, fail_write },
                        )
                        .await;
                    });
                    return Ok(());
                }
                self.sftp_channels.push(channel);
                self.sftp_pending.clear();
                session.channel_success(channel)?;
            } else {
                session.channel_failure(channel)?;
            }
            Ok(())
        }

        async fn data(
            &mut self,
            channel: ChannelId,
            data: &[u8],
            session: &mut Session,
        ) -> Result<(), Self::Error> {
            if self.sftp_channels.contains(&channel) {
                self.sftp_pending.extend_from_slice(data);
                if self.sftp_pending.len() >= 9 && self.sftp_pending[..5] == [0, 0, 0, 5, 1] {
                    session.data(
                        channel,
                        vec![
                            0,
                            0,
                            0,
                            5,
                            2,
                            0,
                            0,
                            0,
                            self.sftp_version.load(Ordering::SeqCst) as u8,
                        ],
                    )?;
                    self.sftp_pending.clear();
                }
            }
            Ok(())
        }

        async fn auth_password(&mut self, user: &str, password: &str) -> Result<Auth, Self::Error> {
            self.auth_attempts.fetch_add(1, Ordering::SeqCst);
            Ok(if user == "deploy" && password == "test-password" {
                Auth::Accept
            } else {
                Auth::Reject {
                    proceed_with_methods: None,
                    partial_success: false,
                }
            })
        }

        async fn auth_publickey(
            &mut self,
            user: &str,
            key: &russh::keys::PublicKey,
        ) -> Result<Auth, Self::Error> {
            self.auth_attempts.fetch_add(1, Ordering::SeqCst);
            Ok(if user == "deploy" && key == &self.allowed_public_key {
                Auth::Accept
            } else {
                Auth::Reject {
                    proceed_with_methods: None,
                    partial_success: false,
                }
            })
        }

        async fn channel_open_session(
            &mut self,
            channel: Channel<Msg>,
            reply: server::ChannelOpenHandle,
            _: &mut Session,
        ) -> Result<(), Self::Error> {
            if self.upload_sftp.load(Ordering::SeqCst) {
                self.upload_channels
                    .lock()
                    .await
                    .insert(channel.id(), channel);
            }
            reply.accept().await;
            Ok(())
        }

        async fn exec_request(
            &mut self,
            channel: ChannelId,
            data: &[u8],
            session: &mut Session,
        ) -> Result<(), Self::Error> {
            if self.upload_sftp.load(Ordering::SeqCst)
                && let Some(path) = data.strip_prefix(b"sha256sum -- ")
                && let Ok(path) = std::str::from_utf8(path)
            {
                let files = self.upload_files.lock().unwrap();
                if let Some(bytes) = files.get(path) {
                    let digest = format!("{:x}  {path}\n", sha2::Sha256::digest(bytes));
                    session.channel_success(channel)?;
                    session.data(channel, digest.into_bytes())?;
                    session.exit_status_request(channel, 0)?;
                    session.eof(channel)?;
                    session.close(channel)?;
                    return Ok(());
                }
            }
            if data == b"docker --host unix:///run/user/1000/docker.sock container start abc123" {
                self.operations
                    .lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(data).into_owned());
                session.channel_success(channel)?;
                session.data(channel, b"abc123\n".to_vec())?;
                session.exit_status_request(channel, 0)?;
                session.eof(channel)?;
                session.close(channel)?;
                return Ok(());
            }
            if data == b"docker --host unix:///var/run/docker.sock stats --no-stream --format '{{json .}}'"
                && self.fail_docker_stats.load(Ordering::SeqCst)
            {
                session.channel_success(channel)?;
                session.extended_data(channel, 1, b"permission denied\n".to_vec())?;
                session.exit_status_request(channel, 1)?;
                session.eof(channel)?;
                session.close(channel)?;
                return Ok(());
            }
            if data == b"docker --host unix:///var/run/docker.sock container restart deadbeef" {
                session.channel_success(channel)?;
                session.close(channel)?;
                return Ok(());
            }
            if data == br#"docker --host unix:///var/run/docker.sock image inspect --format '{"Id":{{json .Id}},"Os":{{json .Os}},"Architecture":{{json .Architecture}}}' app:load-test"# {
                let response = format!(
                    "{{\"Id\":\"sha256:{}\",\"Os\":\"linux\",\"Architecture\":\"amd64\"}}\n",
                    "a".repeat(64)
                );
                session.channel_success(channel)?;
                session.data(channel, response.into_bytes())?;
                session.exit_status_request(channel, 0)?;
                session.eof(channel)?;
                session.close(channel)?;
                return Ok(());
            }
            if data.starts_with(b"docker --host unix:///var/run/docker.sock image load --input ")
                && self.load_no_exit.load(Ordering::SeqCst)
            {
                session.channel_success(channel)?;
                session.close(channel)?;
                return Ok(());
            }
            if data == b"docker --host unix:///var/run/docker.sock logs --follow --tail 1000 --timestamps abc123" {
                session.channel_success(channel)?;
                session.data(channel, b"2026-10-26 stdout".to_vec())?;
                session.extended_data(channel, 1, b"2026-10-26 stderr\n".to_vec())?;
                session.data(channel, b"\n".to_vec())?;
                session.exit_status_request(channel, 0)?;
                session.eof(channel)?;
                session.close(channel)?;
                return Ok(());
            }
            let response = match data {
                b"df -Pk /tmp" if self.upload_sftp.load(Ordering::SeqCst) =>
                    b"Filesystem 1024-blocks Used Available Capacity Mounted on\n/dev/stub 200000 10000 190000 5% /tmp\n".as_slice(),
                command if self.upload_sftp.load(Ordering::SeqCst) && command.starts_with(b"mkdir -m 700 -- ") => {
                    let text = std::str::from_utf8(command).map_err(|_| russh::Error::Disconnect)?;
                    let directory = text.strip_prefix("mkdir -m 700 -- ")
                        .and_then(|value| value.split_once(" && chmod 700 -- "))
                        .map(|(directory, _)| directory).ok_or(russh::Error::Disconnect)?;
                    super::validated_upload_directory(directory).map_err(|_| russh::Error::Disconnect)?;
                    if text != format!("mkdir -m 700 -- {directory} && chmod 700 -- {directory} && test \"$(stat -c %a -- {directory})\" = 700") {
                        return Err(russh::Error::Disconnect);
                    }
                    b"".as_slice()
                }
                command if self.upload_sftp.load(Ordering::SeqCst) && command.starts_with(b"rm -f -- ") => {
                    let text = std::str::from_utf8(command).map_err(|_| russh::Error::Disconnect)?;
                    let directory = text.strip_prefix("rm -f -- ")
                        .and_then(|value| value.split_once("/image.tar && (rmdir -- "))
                        .map(|(directory, _)| directory).ok_or(russh::Error::Disconnect)?;
                    if super::cleanup_archive_command(directory).as_deref() != Ok(text) {
                        return Err(russh::Error::Disconnect);
                    }
                    self.upload_files.lock().unwrap().remove(&format!("{directory}/image.tar"));
                    b"".as_slice()
                }
                b"sh -c 'printf \"%s\\n%s\\n\" \"$DOCKER_CONTEXT\" \"$DOCKER_HOST\"'" => match self.endpoint_mode.load(Ordering::SeqCst) {
                    1 => b"\nunix:///run/user/1000/docker.sock\n".as_slice(),
                    2 => b"local-alt\n\n".as_slice(),
                    _ => b"\n\n".as_slice(),
                },
                b"sh -c 'docker context inspect \"$DOCKER_CONTEXT\" --format \"{{json .Endpoints.docker.Host}}\"'" => b"\"unix:///run/user/1000/docker.sock\"\n".as_slice(),
                b"docker context inspect --format '{{json .Endpoints.docker.Host}}'" => if self.remote_context.load(Ordering::SeqCst) {
                    b"\"tcp://remote.example:2376\"\n".as_slice()
                } else {
                    b"\"unix:///var/run/docker.sock\"\n".as_slice()
                },
                b"docker --host unix:///var/run/docker.sock info --format '{{.ServerVersion}}'" => b"29.5.3\n".as_slice(),
                b"docker --host unix:///var/run/docker.sock compose version --short" => b"5.1.4\n".as_slice(),
                b"uname -m" => b"x86_64\n".as_slice(),
                b"docker --host unix:///var/run/docker.sock info --format '{{.OSType}}/{{.Architecture}}'" => if self.wrong_daemon_arch.load(Ordering::SeqCst) {
                    b"linux/aarch64\n".as_slice()
                } else {
                    b"linux/x86_64\n".as_slice()
                },
                b"uname -s -r" => b"Linux 6.8.0-stub\n".as_slice(),
                b"df -Pk /" => b"Filesystem 1024-blocks Used Available Capacity Mounted on\n/dev/stub 100000 20000 80000 20% /\n".as_slice(),
                b"docker --host unix:///var/run/docker.sock ps --all --format '{{json .}}'" => b"{\"ID\":\"abc123\",\"Names\":\"web\",\"Image\":\"app:v1\",\"Status\":\"Up\",\"State\":\"running\",\"Ports\":\"\"}\n".as_slice(),
                b"docker --host unix:///var/run/docker.sock ps --filter status=running --format '{{json .}}'" => b"{\"ID\":\"abc123\",\"Names\":\"web\",\"Image\":\"app:v1\",\"Status\":\"Up\",\"State\":\"running\",\"Ports\":\"\"}\n".as_slice(),
                b"docker --host unix:///var/run/docker.sock image ls --format '{{json .}}'" => b"{\"Repository\":\"app\",\"Tag\":\"v1\",\"Size\":\"180MB\",\"CreatedSince\":\"now\"}\n".as_slice(),
                b"docker --host unix:///var/run/docker.sock image ls --filter reference=app:load-test --format '{{.Repository}}:{{.Tag}}'" => if self.load_tag_exists.load(Ordering::SeqCst) {
                    b"app:load-test\n".as_slice()
                } else {
                    b"".as_slice()
                },
                command if command.starts_with(b"docker --host unix:///var/run/docker.sock image load --input ") => {
                    let path = std::str::from_utf8(command.strip_prefix(b"docker --host unix:///var/run/docker.sock image load --input ").unwrap()).map_err(|_| russh::Error::Disconnect)?;
                    if !self.upload_files.lock().unwrap().contains_key(path) {
                        return Err(russh::Error::Disconnect);
                    }
                    self.operations.lock().unwrap().push(String::from_utf8_lossy(data).into_owned());
                    b"Loaded image: app:load-test\n".as_slice()
                }
                b"docker --host unix:///var/run/docker.sock stats --no-stream --all --format '{{json .}}'" => b"{\"ID\":\"abc123\",\"CPUPerc\":\"1%\",\"MemUsage\":\"10MiB / 1GiB\"}\n".as_slice(),
                b"docker --host unix:///var/run/docker.sock stats --no-stream --format '{{json .}}'" => b"{\"ID\":\"abc123\",\"CPUPerc\":\"1%\",\"MemUsage\":\"10MiB / 1GiB\"}\n".as_slice(),
                b"cat /proc/net/route" => b"Iface Destination Gateway Flags RefCnt Use Metric Mask MTU Window IRTT\neth0 00000000 0100007F 0003 0 0 100 00000000 0 0 0\n".as_slice(),
                b"docker --host unix:///var/run/docker.sock system df --format '{{json .}}'" => b"{\"Type\":\"Images\",\"TotalCount\":\"1\",\"Reclaimable\":\"10MB (100%)\"}\n".as_slice(),
                b"head -n 1 /proc/stat" => if self.cpu_samples.fetch_add(1, Ordering::SeqCst).is_multiple_of(2) {
                    b"cpu 100 0 100 800 0 0 0 0\n".as_slice()
                } else {
                    b"cpu 130 0 130 840 0 0 0 0\n".as_slice()
                },
                b"cat /proc/net/dev" => if self.network_samples.fetch_add(1, Ordering::SeqCst).is_multiple_of(2) {
                    b"Inter-| Receive | Transmit\n face |bytes ...\n eth0: 1000 0 0 0 0 0 0 0 2000 0 0 0 0 0 0 0\n".as_slice()
                } else {
                    b"Inter-| Receive | Transmit\n face |bytes ...\n eth0: 3000 0 0 0 0 0 0 0 6000 0 0 0 0 0 0 0\n".as_slice()
                },
                b"cat /proc/meminfo" => b"MemTotal: 4000000 kB\nMemAvailable: 1000000 kB\n".as_slice(),
                b"docker --host unix:///var/run/docker.sock network ls --filter dangling=true --format '{{.ID}}'" => b"abc123\n".as_slice(),
                b"docker --host unix:///var/run/docker.sock image ls --filter dangling=true --format '{{json .}}'" => b"{\"Size\":\"10MB\"}\n".as_slice(),
                b"docker --host unix:///var/run/docker.sock system prune --force" => {
                    self.operations.lock().unwrap().push(String::from_utf8_lossy(data).into_owned());
                    b"Deleted Containers:\nabc123\nTotal reclaimed space: 10MB\n".as_slice()
                }
                b"docker --host unix:///var/run/docker.sock container start abc123" | b"docker --host unix:///var/run/docker.sock container stop --time 10 abc123"
                | b"docker --host unix:///var/run/docker.sock container restart --time 10 abc123" | b"docker --host unix:///var/run/docker.sock container rm abc123" => {
                    self.operations.lock().unwrap().push(String::from_utf8_lossy(data).into_owned());
                    b"abc123\n".as_slice()
                }
                _ => return Err(russh::Error::Disconnect),
            };
            session.channel_success(channel)?;
            session.data(channel, response.to_vec())?;
            session.exit_status_request(channel, 0)?;
            session.eof(channel)?;
            session.close(channel)?;
            Ok(())
        }
    }

    #[tokio::test]
    async fn first_contact_and_changed_key_never_auto_trust() {
        let key =
            russh::keys::PrivateKey::random(&mut rand::rng(), russh::keys::Algorithm::Ed25519)
                .unwrap();
        let user_key =
            russh::keys::PrivateKey::random(&mut rand::rng(), russh::keys::Algorithm::Ed25519)
                .unwrap();
        let attempts = Arc::new(AtomicUsize::new(0));
        let operations = Arc::new(Mutex::new(Vec::new()));
        let fail_docker_stats = Arc::new(AtomicBool::new(false));
        let wrong_daemon_arch = Arc::new(AtomicBool::new(false));
        let remote_context = Arc::new(AtomicBool::new(false));
        let endpoint_mode = Arc::new(AtomicUsize::new(0));
        let sftp_version = Arc::new(AtomicUsize::new(3));
        let upload_sftp = Arc::new(AtomicBool::new(false));
        let upload_files = Arc::new(Mutex::new(HashMap::new()));
        let upload_fail_write = Arc::new(AtomicBool::new(false));
        let load_tag_exists = Arc::new(AtomicBool::new(false));
        let load_no_exit = Arc::new(AtomicBool::new(false));
        let config = Arc::new(server::Config {
            keys: vec![key],
            auth_rejection_time: Duration::from_millis(10),
            ..Default::default()
        });
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let mut server = StubServer {
            auth_attempts: attempts.clone(),
            allowed_public_key: user_key.public_key().clone(),
            operations: operations.clone(),
            cpu_samples: Arc::new(AtomicUsize::new(0)),
            network_samples: Arc::new(AtomicUsize::new(0)),
            fail_docker_stats: fail_docker_stats.clone(),
            wrong_daemon_arch: wrong_daemon_arch.clone(),
            remote_context: remote_context.clone(),
            endpoint_mode: endpoint_mode.clone(),
            sftp_channels: Vec::new(),
            sftp_pending: Vec::new(),
            sftp_version: sftp_version.clone(),
            upload_sftp: upload_sftp.clone(),
            upload_channels: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            upload_files: upload_files.clone(),
            upload_fail_write: upload_fail_write.clone(),
            load_tag_exists: load_tag_exists.clone(),
            load_no_exit: load_no_exit.clone(),
        };
        let running = server.run_on_socket(config, &listener);
        let handle = running.handle();
        let client = async {
            let material = |trusted: Option<String>| ConnectionMaterial {
                host: Host {
                    id: 1,
                    name: "stub".into(),
                    address: "127.0.0.1".into(),
                    port: address.port(),
                    username: "deploy".into(),
                    group: String::new(),
                    auth: "password".into(),
                    trusted_fingerprint: trusted.clone(),
                },
                credential: "test-password".into(),
                trusted_fingerprint: trusted,
            };
            let fingerprint = match probe(material(None)).await.unwrap() {
                TestResult::NeedsTrust {
                    fingerprint,
                    previous: None,
                    ..
                } => fingerprint,
                _ => panic!("first contact must require trust"),
            };
            assert!(fingerprint.starts_with("SHA256:"));
            assert_eq!(attempts.load(Ordering::SeqCst), 0);
            assert!(fetch_inventory(material(None)).await.is_err());
            assert!(matches!(
                fetch_monitor(material(None)).await,
                Err(super::MonitorError::Configuration(_))
            ));
            assert_eq!(attempts.load(Ordering::SeqCst), 0);
            match probe(material(Some("SHA256:wrong".into()))).await.unwrap() {
                TestResult::NeedsTrust {
                    previous: Some(old),
                    fingerprint: new,
                    ..
                } => {
                    assert_eq!(old, "SHA256:wrong");
                    assert_eq!(new, fingerprint);
                }
                _ => panic!("changed key must block"),
            }
            assert_eq!(attempts.load(Ordering::SeqCst), 0);
            match probe(material(Some(fingerprint.clone()))).await.unwrap() {
                TestResult::Connected {
                    docker_version,
                    compose_version,
                    system_kernel,
                    architecture,
                    disk_available_kib,
                    sftp_v3_handshake,
                } => {
                    assert_eq!(docker_version, "29.5.3");
                    assert_eq!(compose_version, "5.1.4");
                    assert_eq!(system_kernel, "Linux 6.8.0-stub");
                    assert_eq!(architecture.as_deref(), Some("linux/amd64"));
                    assert_eq!(disk_available_kib, 80000);
                    assert!(sftp_v3_handshake);
                }
                _ => panic!("trusted host should connect"),
            }
            assert_eq!(attempts.load(Ordering::SeqCst), 1);
            for version in [0, 4] {
                sftp_version.store(version, Ordering::SeqCst);
                assert!(matches!(
                    probe(material(Some(fingerprint.clone()))).await.unwrap(),
                    TestResult::Connected {
                        sftp_v3_handshake: false,
                        ..
                    }
                ));
            }
            sftp_version.store(3, Ordering::SeqCst);
            for mode in [1, 2] {
                endpoint_mode.store(mode, Ordering::SeqCst);
                let connection = super::connect_authenticated(material(Some(fingerprint.clone())))
                    .await
                    .unwrap();
                let super::ConnectionOutcome::Authenticated(handle) = connection else {
                    panic!("trusted key required")
                };
                assert_eq!(
                    super::local_docker_endpoint(&handle).await.unwrap(),
                    "unix:///run/user/1000/docker.sock"
                );
                let _ = handle
                    .disconnect(russh::Disconnect::ByApplication, "", "en")
                    .await;
                if mode == 1 {
                    execute_container_action(
                        material(Some(fingerprint.clone())),
                        "abc123",
                        ContainerAction::Start,
                    )
                    .await
                    .unwrap();
                    assert!(
                        operations
                            .lock()
                            .unwrap()
                            .last()
                            .unwrap()
                            .contains("--host unix:///run/user/1000/docker.sock")
                    );
                    operations.lock().unwrap().clear();
                }
            }
            endpoint_mode.store(0, Ordering::SeqCst);
            remote_context.store(true, Ordering::SeqCst);
            assert!(
                probe(material(Some(fingerprint.clone())))
                    .await
                    .err()
                    .unwrap()
                    .contains("Unix socket")
            );
            assert!(
                fetch_inventory(material(Some(fingerprint.clone())))
                    .await
                    .is_err()
            );
            assert!(
                execute_disk_prune(material(Some(fingerprint.clone())))
                    .await
                    .is_err()
            );
            assert!(operations.lock().unwrap().is_empty());
            remote_context.store(false, Ordering::SeqCst);
            wrong_daemon_arch.store(true, Ordering::SeqCst);
            assert!(
                probe(material(Some(fingerprint.clone())))
                    .await
                    .err()
                    .unwrap()
                    .contains("架构与 SSH 主机不一致")
            );
            wrong_daemon_arch.store(false, Ordering::SeqCst);
            let inventory = fetch_inventory(material(Some(fingerprint.clone())))
                .await
                .unwrap();
            assert_eq!(inventory.containers[0].name, "web");
            assert_eq!(inventory.images[0].reference, "app:v1");
            let monitor = fetch_monitor(material(Some(fingerprint.clone())))
                .await
                .unwrap();
            assert_eq!(monitor.cpu_percent, 60.);
            assert_eq!(monitor.memory_used_kib, 3000000);
            assert_eq!(monitor.disk_percent, 20.);
            assert_eq!(monitor.containers.as_ref().unwrap().len(), 1);
            assert!(monitor.network_bytes_per_sec.unwrap().0 > 0.);
            fail_docker_stats.store(true, Ordering::SeqCst);
            let monitor_without_docker = fetch_monitor(material(Some(fingerprint.clone())))
                .await
                .unwrap();
            assert_eq!(monitor_without_docker.cpu_percent, 60.);
            assert!(monitor_without_docker.containers.is_none());
            fail_docker_stats.store(false, Ordering::SeqCst);
            assert!(fetch_disk_preview(material(None)).await.is_err());
            assert_eq!(
                fetch_disk_preview(material(Some(fingerprint.clone())))
                    .await
                    .unwrap()
                    .unused_networks,
                1
            );
            assert_eq!(
                execute_disk_prune(material(Some(fingerprint.clone())))
                    .await
                    .unwrap(),
                Some("10MB".into())
            );
            assert!(
                execute_container_action(
                    material(Some(fingerprint.clone())),
                    "abc;bad",
                    ContainerAction::Delete
                )
                .await
                .is_err()
            );
            for action in [
                ContainerAction::Start,
                ContainerAction::Stop,
                ContainerAction::Restart,
                ContainerAction::Delete,
            ] {
                execute_container_action(material(Some(fingerprint.clone())), "abc123", action)
                    .await
                    .unwrap();
            }
            assert_eq!(operations.lock().unwrap().len(), 5);
            let (sender, receiver) = std::sync::mpsc::sync_channel(8);
            let cancel = std::sync::atomic::AtomicBool::new(false);
            follow_logs(
                material(Some(fingerprint.clone())),
                "abc123",
                &cancel,
                &sender,
            )
            .await
            .unwrap();
            let lines: Vec<_> = receiver
                .try_iter()
                .filter_map(|event| match event {
                    LogEvent::Line(line) => Some(line),
                    _ => None,
                })
                .collect();
            assert_eq!(lines.len(), 2);
            assert!(lines.contains(&"2026-10-26 stdout".to_owned()));
            assert!(lines.contains(&"2026-10-26 stderr".to_owned()));
            upload_sftp.store(true, Ordering::SeqCst);
            let archive_path =
                std::env::temp_dir().join(format!("deving-sftp-{}", uuid::Uuid::new_v4()));
            let bytes = vec![0x5au8; 150_000];
            std::fs::write(&archive_path, &bytes).unwrap();
            let mut progress = Vec::new();
            let upload = super::transfer_archive(
                material(Some(fingerprint.clone())),
                &archive_path,
                bytes.len() as u64,
                &AtomicBool::new(false),
                &mut |copied, total| progress.push((copied, total)),
            )
            .await
            .unwrap();
            assert_eq!(upload.bytes, bytes.len() as u64);
            assert_eq!(upload.sha256, format!("{:x}", sha2::Sha256::digest(&bytes)));
            assert_eq!(upload_files.lock().unwrap().get(&upload.path), Some(&bytes));
            assert_eq!(
                progress.last(),
                Some(&(bytes.len() as u64, bytes.len() as u64))
            );
            Box::pin(async {
                let image_id = format!("sha256:{}", "a".repeat(64));
                assert!(
                    super::load_archive(
                        material(Some(fingerprint.clone())),
                        &upload,
                        "app;bad:tag",
                        &image_id,
                        "linux/amd64",
                        &AtomicBool::new(false),
                    )
                    .await
                    .is_err()
                );
                assert_eq!(operations.lock().unwrap().len(), 5);
                wrong_daemon_arch.store(true, Ordering::SeqCst);
                assert!(
                    super::load_archive(
                        material(Some(fingerprint.clone())),
                        &upload,
                        "app:load-test",
                        &image_id,
                        "linux/amd64",
                        &AtomicBool::new(false),
                    )
                    .await
                    .is_err()
                );
                wrong_daemon_arch.store(false, Ordering::SeqCst);
                let altered = super::UploadedArchive {
                    directory: upload.directory.clone(),
                    path: upload.path.clone(),
                    bytes: upload.bytes,
                    sha256: "b".repeat(64),
                };
                assert!(
                    super::load_archive(
                        material(Some(fingerprint.clone())),
                        &altered,
                        "app:load-test",
                        &image_id,
                        "linux/amd64",
                        &AtomicBool::new(false),
                    )
                    .await
                    .is_err()
                );
                assert_eq!(operations.lock().unwrap().len(), 5);
                load_tag_exists.store(true, Ordering::SeqCst);
                assert!(
                    super::load_archive(
                        material(Some(fingerprint.clone())),
                        &upload,
                        "app:load-test",
                        &image_id,
                        "linux/amd64",
                        &AtomicBool::new(false),
                    )
                    .await
                    .is_err()
                );
                load_tag_exists.store(false, Ordering::SeqCst);
                assert_eq!(operations.lock().unwrap().len(), 5);
                let cancelled = AtomicBool::new(true);
                assert!(
                    super::load_archive(
                        material(Some(fingerprint.clone())),
                        &upload,
                        "app:load-test",
                        &image_id,
                        "linux/amd64",
                        &cancelled,
                    )
                    .await
                    .unwrap_err()
                    .contains("尚未加载")
                );
                load_no_exit.store(true, Ordering::SeqCst);
                let uncertain = super::load_archive(
                    material(Some(fingerprint.clone())),
                    &upload,
                    "app:load-test",
                    &image_id,
                    "linux/amd64",
                    &AtomicBool::new(false),
                )
                .await
                .unwrap_err();
                load_no_exit.store(false, Ordering::SeqCst);
                assert!(uncertain.contains("待核实"));
                assert!(upload_files.lock().unwrap().contains_key(&upload.path));
                assert_eq!(operations.lock().unwrap().len(), 5);
                super::load_archive(
                    material(Some(fingerprint.clone())),
                    &upload,
                    "app:load-test",
                    &image_id,
                    "linux/amd64",
                    &AtomicBool::new(false),
                )
                .await
                .unwrap();
                assert_eq!(operations.lock().unwrap().len(), 6);
                let wrong_id = format!("sha256:{}", "b".repeat(64));
                assert!(
                    super::load_archive(
                        material(Some(fingerprint.clone())),
                        &upload,
                        "app:load-test",
                        &wrong_id,
                        "linux/amd64",
                        &AtomicBool::new(false),
                    )
                    .await
                    .unwrap_err()
                    .contains("禁止部署")
                );
            })
            .await;
            let transfer_handle = super::trusted_handle(material(Some(fingerprint.clone())))
                .await
                .unwrap();
            super::archive_command(
                &transfer_handle,
                &super::cleanup_archive_command(&upload.directory).unwrap(),
            )
            .await
            .unwrap();
            super::archive_command(
                &transfer_handle,
                &super::cleanup_archive_command(&upload.directory).unwrap(),
            )
            .await
            .unwrap();
            assert!(upload_files.lock().unwrap().is_empty());
            let _ = transfer_handle
                .disconnect(russh::Disconnect::ByApplication, "", "en")
                .await;
            upload_fail_write.store(true, Ordering::SeqCst);
            let failure = super::transfer_archive(
                material(Some(fingerprint.clone())),
                &archive_path,
                bytes.len() as u64,
                &AtomicBool::new(false),
                &mut |_, _| {},
            )
            .await;
            assert!(failure.is_err());
            assert!(upload_files.lock().unwrap().is_empty());
            upload_fail_write.store(false, Ordering::SeqCst);
            std::fs::remove_file(&archive_path).unwrap();
            upload_sftp.store(false, Ordering::SeqCst);
            let unknown = execute_container_action(
                material(Some(fingerprint.clone())),
                "deadbeef",
                ContainerAction::Restart,
            )
            .await
            .unwrap_err();
            assert!(unknown.contains("待核实"));
            let wrong_key =
                russh::keys::PrivateKey::random(&mut rand::rng(), russh::keys::Algorithm::Ed25519)
                    .unwrap();
            let mut wrong_material = material(Some(fingerprint.clone()));
            wrong_material.host.auth = "key".into();
            wrong_material.credential = wrong_key
                .to_openssh(russh::keys::ssh_key::LineEnding::LF)
                .unwrap()
                .to_string();
            assert!(probe(wrong_material).await.is_err());
            let mut key_material = material(Some(fingerprint));
            key_material.host.auth = "key".into();
            key_material.credential = user_key
                .to_openssh(russh::keys::ssh_key::LineEnding::LF)
                .unwrap()
                .to_string();
            assert!(matches!(
                probe(key_material).await.unwrap(),
                TestResult::Connected { .. }
            ));
            assert!(attempts.load(Ordering::SeqCst) >= 3);
            handle.shutdown("done".into());
        };
        tokio::time::timeout(Duration::from_secs(15), async {
            let (server_result, ()) = tokio::join!(running, client);
            server_result.unwrap();
        })
        .await
        .unwrap();
    }
}
