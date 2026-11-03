use crate::host_store::{self, ConnectionMaterial, Host};
use crate::release_store::TaskSnapshot;
use base64::Engine as _;
use russh::client;
use russh::keys::{HashAlg, PrivateKey, PrivateKeyWithHashAlg, PublicKeyOrCertificate};
use russh::{ChannelMsg, Disconnect};
use russh_sftp::client::{Config as SftpConfig, RawSftpSession, SftpSession};
use russh_sftp::protocol::{FileAttributes, OpenFlags};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::io::SeekFrom;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
    mpsc::SyncSender,
};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

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
    DockerMissing {
        os_id: String,
        os_version: String,
        system_kernel: String,
        architecture: Option<String>,
    },
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum DockerInstallSource {
    Tencent,
    Official,
}

impl DockerInstallSource {
    fn base_url(self) -> &'static str {
        match self {
            Self::Tencent => "https://mirrors.cloud.tencent.com/docker-ce/linux/debian",
            Self::Official => "https://download.docker.com/linux/debian",
        }
    }

    fn host(self) -> &'static str {
        match self {
            Self::Tencent => "mirrors.cloud.tencent.com",
            Self::Official => "download.docker.com",
        }
    }
}

const DOCKER_GPG_FINGERPRINT: &str = "9DC858229FC7DD38854AE2D88D81803C0EBFCD88";
pub const VERIFIED_REGISTRY_MIRRORS: [&str; 2] =
    ["https://docker.1panel.live", "https://docker.m.daocloud.io"];

fn sftp_upload_config() -> SftpConfig {
    // Bound pipelining and allow slow servers enough time to acknowledge each write.
    SftpConfig {
        max_concurrent_writes: 4,
        request_timeout_secs: 60,
        ..Default::default()
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum MirrorConfigResult {
    AlreadyConfigured,
    Updated,
}

fn merged_mirror_config(existing: Option<&str>) -> Result<Option<Vec<u8>>, String> {
    let mut config = match existing {
        Some(text) => serde_json::from_str::<Value>(text)
            .map_err(|_| "现有 Docker daemon.json 不是有效 JSON，未修改。")?,
        None => Value::Object(Default::default()),
    };
    let object = config
        .as_object_mut()
        .ok_or("现有 Docker daemon.json 不是对象，未修改。")?;
    let mirrors = VERIFIED_REGISTRY_MIRRORS
        .iter()
        .map(|mirror| Value::String((*mirror).into()))
        .collect::<Vec<_>>();
    if object.get("registry-mirrors") == Some(&Value::Array(mirrors.clone())) {
        return Ok(None);
    }
    object.insert("registry-mirrors".into(), Value::Array(mirrors));
    serde_json::to_vec_pretty(&config)
        .map(Some)
        .map_err(|_| "无法生成 Docker daemon.json。".into())
}

fn mirrors_active(text: &str) -> bool {
    serde_json::from_str::<Vec<String>>(text.trim()).is_ok_and(|actual| {
        actual
            .iter()
            .map(|url| url.trim_end_matches('/'))
            .eq(VERIFIED_REGISTRY_MIRRORS)
    })
}

fn valid_docker_signing_key(text: &str) -> bool {
    text.lines().filter(|line| line.starts_with("pub:")).count() == 1
        && text
            .lines()
            .find_map(|line| line.strip_prefix("fpr:::::::::"))
            .and_then(|line| line.split(':').next())
            == Some(DOCKER_GPG_FINGERPRINT)
}

fn apt_candidates_use_source(text: &str, source: DockerInstallSource) -> bool {
    let packages = [
        "docker-ce",
        "docker-ce-cli",
        "containerd.io",
        "docker-buildx-plugin",
        "docker-compose-plugin",
    ];
    let mut sections = std::collections::HashMap::new();
    let mut current = "";
    for line in text.lines() {
        if !line.starts_with(char::is_whitespace) && line.ends_with(':') {
            current = line.trim_end_matches(':');
        }
        sections.entry(current).or_insert_with(Vec::new).push(line);
    }
    packages.iter().all(|package| {
        let Some(lines) = sections.get(package) else {
            return false;
        };
        let Some(candidate) = lines
            .iter()
            .find_map(|line| line.trim().strip_prefix("Candidate: "))
            .filter(|value| *value != "(none)")
        else {
            return false;
        };
        let Some(index) = lines.iter().position(|line| {
            let parts: Vec<_> = line
                .trim()
                .trim_start_matches("*** ")
                .split_whitespace()
                .collect();
            parts.len() >= 2
                && parts[0] == candidate
                && parts[1]
                    .parse::<i32>()
                    .is_ok_and(|priority| priority >= 990)
        }) else {
            return false;
        };
        let mut selected = false;
        let source_url = format!("https://{}/docker-ce/linux/debian", source.host());
        for line in &lines[index + 1..] {
            let parts: Vec<_> = line.split_whitespace().collect();
            if parts.len() < 2 {
                continue;
            }
            if parts[1].parse::<i32>().is_ok() {
                break;
            }
            if parts[0].parse::<i32>().is_ok() {
                if parts[1] == source_url
                    || parts[1]
                        .strip_prefix(&source_url)
                        .is_some_and(|suffix| suffix.starts_with('/'))
                {
                    selected = true;
                } else if parts[1] != "/var/lib/dpkg/status" {
                    return false;
                }
            }
        }
        selected
    })
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
    pub image_ids: Vec<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct RemoteArchiveObservation {
    pub service: String,
    pub directory: String,
    pub state: String,
    pub expected_bytes: u64,
    pub actual_bytes: Option<u64>,
    pub digest_matches: Option<bool>,
}

pub struct DeploymentFile {
    pub path: String,
    pub bytes: Vec<u8>,
}

pub struct StagedDeployment {
    pub directory: String,
    pub compose_files: Vec<String>,
    pub env_path: String,
}

#[derive(Clone, Debug)]
pub struct StackService {
    pub service: String,
    pub reference: String,
    pub image_ids: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObservedContainer {
    pub service: String,
    pub status: String,
    pub health: Option<String>,
    pub image_id: String,
    pub config_image: String,
}

pub struct ReleaseObservation {
    pub containers: Vec<ObservedContainer>,
}

pub(crate) fn validated_release_file(path: &str) -> Result<(), String> {
    if path.is_empty()
        || path.len() > 512
        || path == ".env"
        || path.contains('\\')
        || path.chars().any(char::is_control)
        || path
            .split('/')
            .any(|part| part.is_empty() || matches!(part, "." | ".."))
    {
        return Err("发布文件相对路径无效。".into());
    }
    Ok(())
}

fn validated_release_directory(
    home: &str,
    project_id: i64,
    task_id: &str,
) -> Result<String, String> {
    if project_id <= 0
        || home.len() > 256
        || !home.starts_with('/')
        || home.split('/').skip(1).any(|part| {
            part.is_empty() || matches!(part, "." | "..") || part.chars().any(char::is_control)
        })
    {
        return Err("远端 HOME 路径不支持安全部署。".into());
    }
    let id = uuid::Uuid::parse_str(task_id).map_err(|_| "发布任务 ID 无效。")?;
    Ok(format!(
        "{home}/.local/share/devingpanel/projects/{project_id}/releases/{id}"
    ))
}

fn shell_quote(value: &str) -> Result<String, String> {
    if value.is_empty() || value.len() > 2048 || value.chars().any(char::is_control) {
        return Err("远端路径包含无效字符。".into());
    }
    Ok(format!("'{}'", value.replace('\'', "'\"'\"'")))
}

pub(crate) fn validated_upload_directory(raw: &str) -> Result<String, String> {
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

pub(crate) fn same_connection_target(current: &Host, expected: &Host) -> bool {
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

#[cfg(test)]
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
        return Err(command_exit_error(exit));
    }
    String::from_utf8(bytes).map_err(|_| "远端检查输出编码无效。".into())
}

fn command_exit_error(exit: Option<u32>) -> String {
    match exit {
        Some(code) => format!("远端命令以退出码 {code} 结束。"),
        None => "远端 SSH 会话未返回命令退出状态。".into(),
    }
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

#[derive(Clone, Copy)]
pub struct ReleaseDockerIdentity<'a> {
    pub endpoint: &'a str,
    pub daemon_id: &'a str,
}

async fn checked_release_endpoint(
    handle: &client::Handle<HostKeyChecker>,
    expected: ReleaseDockerIdentity<'_>,
) -> Result<String, String> {
    let actual = local_docker_endpoint(handle).await?;
    if actual != expected.endpoint {
        return Err("远端 Docker 端点与发布任务快照不同，操作已阻断。".into());
    }
    if docker_daemon_id(handle, &actual).await? != expected.daemon_id {
        return Err("远端 Docker daemon 身份与发布任务快照不同，操作已阻断。".into());
    }
    Ok(actual)
}

fn validated_daemon_id(raw: &str) -> Result<String, String> {
    let id = raw.trim();
    if id.is_empty()
        || id.len() > 128
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b':' | b'-' | b'_'))
    {
        return Err("远端 Docker daemon ID 无效。".into());
    }
    Ok(id.to_owned())
}

async fn docker_daemon_id(
    handle: &client::Handle<HostKeyChecker>,
    endpoint: &str,
) -> Result<String, String> {
    validated_daemon_id(
        &archive_command(handle, &docker_command(endpoint, "info --format '{{.ID}}'")).await?,
    )
}

pub fn docker_identity_for_host(expected_host: Host) -> Result<(String, String), String> {
    let material = material_for(&expected_host)?;
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| "无法初始化 Docker 端点预检环境。".to_owned())?
        .block_on(async {
            let handle = tokio::time::timeout(Duration::from_secs(20), trusted_handle(material))
                .await
                .map_err(|_| "Docker 端点预检连接超时。")??;
            let endpoint = local_docker_endpoint(&handle).await?;
            let daemon_id = docker_daemon_id(&handle, &endpoint).await?;
            let _ = handle.disconnect(Disconnect::ByApplication, "", "en").await;
            Ok((endpoint, daemon_id))
        })
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
        // Compose startup can be silent for several minutes; its command has a 10-minute limit.
        inactivity_timeout: Some(Duration::from_secs(11 * 60)),
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
    let docker_presence = command(
        &handle,
        "sh -c 'command -v docker >/dev/null 2>&1; printf \"%s\\n\" \"$?\"'",
    )
    .await?;
    if docker_presence.trim() != "0" {
        let release = command(&handle, "cat /etc/os-release")
            .await
            .unwrap_or_default();
        let field = |name: &str| {
            release
                .lines()
                .find_map(|line| line.strip_prefix(name))
                .map(|value| value.trim_matches('"'))
                .filter(|value| {
                    !value.is_empty()
                        && value.len() <= 32
                        && value.bytes().all(|byte| {
                            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_')
                        })
                })
                .map(str::to_owned)
        };
        let system_kernel = command(&handle, "uname -s -r")
            .await
            .unwrap_or_default()
            .trim()
            .to_owned();
        let system_kernel = if system_kernel.is_empty()
            || system_kernel.len() > 128
            || system_kernel.chars().any(char::is_control)
        {
            "unknown".into()
        } else {
            system_kernel
        };
        let architecture = command(&handle, "uname -m")
            .await
            .ok()
            .and_then(|value| linux_platform(&value).map(str::to_owned));
        let _ = handle.disconnect(Disconnect::ByApplication, "", "en").await;
        return Ok(TestResult::DockerMissing {
            os_id: field("ID=").unwrap_or_else(|| "unknown".into()),
            os_version: field("VERSION_ID=").unwrap_or_else(|| "unknown".into()),
            system_kernel,
            architecture,
        });
    }
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

fn docker_install_commands(source: DockerInstallSource) -> Vec<(&'static str, String, Duration)> {
    let url = source.base_url();
    let host = source.host();
    vec![
        ("更新现有软件源", "DEBIAN_FRONTEND=noninteractive apt-get -o APT::Update::Error-Mode=any update -qq > /dev/null".into(), Duration::from_secs(300)),
        ("安装仓库签名工具", "DEBIAN_FRONTEND=noninteractive apt-get install -y -qq --no-remove ca-certificates curl gnupg > /dev/null".into(), Duration::from_secs(300)),
        ("准备 Docker 仓库密钥", format!("install -m 0755 -d /etc/apt/keyrings && curl -fsSL --max-time 60 {url}/gpg -o /etc/apt/keyrings/devingpanel-docker.asc.tmp"), Duration::from_secs(90)),
        ("验证 Docker 官方密钥", "gpg --batch --show-keys --with-colons /etc/apt/keyrings/devingpanel-docker.asc.tmp".into(), Duration::from_secs(15)),
        ("配置 Docker 软件源", format!("chmod 0644 /etc/apt/keyrings/devingpanel-docker.asc.tmp && mv /etc/apt/keyrings/devingpanel-docker.asc.tmp /etc/apt/keyrings/devingpanel-docker.asc && printf 'Types: deb\\nURIs: {url}\\nSuites: trixie\\nComponents: stable\\nArchitectures: amd64\\nSigned-By: /etc/apt/keyrings/devingpanel-docker.asc\\n' > /etc/apt/sources.list.d/devingpanel-docker.sources && printf 'Package: docker-ce docker-ce-cli containerd.io docker-buildx-plugin docker-compose-plugin\\nPin: origin \"{host}\"\\nPin-Priority: 990\\n' > /etc/apt/preferences.d/devingpanel-docker.pref"), Duration::from_secs(15)),
        ("更新 Docker 软件源", "DEBIAN_FRONTEND=noninteractive apt-get -o APT::Update::Error-Mode=any update -qq > /dev/null".into(), Duration::from_secs(300)),
        ("核对软件包来源", "LC_ALL=C apt-cache policy docker-ce docker-ce-cli containerd.io docker-buildx-plugin docker-compose-plugin".into(), Duration::from_secs(15)),
        ("安装 Docker Engine 与 Compose", "DEBIAN_FRONTEND=noninteractive apt-get install -y -qq --no-remove docker-ce docker-ce-cli containerd.io docker-buildx-plugin docker-compose-plugin > /dev/null".into(), Duration::from_secs(900)),
        ("启动 Docker 服务", "systemctl enable --now docker > /dev/null".into(), Duration::from_secs(90)),
    ]
}

async fn install_docker_on(
    material: ConnectionMaterial,
    source: DockerInstallSource,
    on_stage: &mut dyn FnMut(&str),
) -> Result<(), String> {
    if material.host.username != "root" {
        return Err("一键安装仅支持 root 登录；请使用有权限的账号手动安装。".into());
    }
    let handle = tokio::time::timeout(Duration::from_secs(20), trusted_handle(material))
        .await
        .map_err(|_| "Docker 安装 SSH 连接超时。")??;
    if archive_command(&handle, "id -u").await?.trim() != "0" {
        return Err("远端登录用户不是 root，禁止安装。".into());
    }
    let release = archive_command(&handle, "cat /etc/os-release").await?;
    let supported = release.lines().any(|line| line == "ID=debian")
        && release
            .lines()
            .any(|line| line == "VERSION_ID=13" || line == "VERSION_ID=\"13\"");
    let architecture = archive_command(&handle, "uname -m").await?;
    if !supported || architecture.trim() != "x86_64" {
        return Err("一键安装目前仅支持 Debian 13 x86_64；其他系统请按官方文档安装。".into());
    }
    let tools = archive_command(
        &handle,
        "sh -c 'for tool in apt-get dpkg-query systemctl curl; do command -v \"$tool\" >/dev/null 2>&1 || exit 1; done; test -d /run/systemd/system && echo ready'",
    )
    .await
    .map_err(|_| "一键安装需要 apt/dpkg、curl 和运行中的 systemd；请手动安装。")?;
    if tools.trim() != "ready" {
        return Err("远端包管理器或 systemd 不可用，请手动安装。".into());
    }
    let docker = archive_command(
        &handle,
        "sh -c 'command -v docker >/dev/null 2>&1; printf \"%s\\n\" \"$?\"'",
    )
    .await?;
    if docker.trim() == "0" {
        return Err("Docker 命令已存在，请先检查 daemon 或权限，不应重复安装。".into());
    }
    if disk_available(&archive_command(&handle, "df -Pk /").await?)? < 2 * 1024 * 1024 {
        return Err("VPS 根目录可用空间不足 2 GiB，未开始安装。".into());
    }
    let existing = archive_command(
        &handle,
        "sh -c 'if test -e /etc/apt/sources.list.d/devingpanel-docker.sources || test -e /etc/apt/preferences.d/devingpanel-docker.pref; then echo occupied; fi'",
    )
    .await?;
    if !existing.trim().is_empty() {
        return Err("已有本应用创建的 Docker 软件源；请人工核对上次安装结果。".into());
    }
    for (stage, command, timeout) in docker_install_commands(source) {
        on_stage(stage);
        let output = tokio::time::timeout(
            timeout,
            command_limited(
                &handle,
                &command,
                if stage == "核对软件包来源" {
                    512 * 1024
                } else {
                    4096
                },
            ),
        )
        .await
        .map_err(|_| format!("{stage}超时，请在 VPS 上核对 apt 与 systemd 状态。"))?
        .map_err(|_| format!("{stage}失败，请在 VPS 上核对 apt 与 systemd 状态。"))?;
        if stage == "验证 Docker 官方密钥" && !valid_docker_signing_key(&output) {
            return Err("Docker 仓库密钥指纹不符，已停止安装；请核对镜像源。".into());
        }
        if stage == "核对软件包来源" && !apt_candidates_use_source(&output, source) {
            return Err("Docker 软件包候选版本不是来自所选签名仓库，已停止安装。".into());
        }
    }
    let _ = handle.disconnect(Disconnect::ByApplication, "", "en").await;
    Ok(())
}

#[allow(dead_code)] // The host details confirmation dialog invokes this in a background task.
pub fn install_docker(
    expected_host: Host,
    source: DockerInstallSource,
    mut on_stage: impl FnMut(&str),
) -> Result<(), String> {
    let material = material_for(&expected_host)?;
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| "无法初始化 Docker 安装环境。".to_owned())?
        .block_on(install_docker_on(material, source, &mut on_stage))?;
    if !matches!(
        test_host_snapshot(expected_host.clone(), Arc::new(AtomicBool::new(false)))?,
        TestResult::Connected { .. }
    ) {
        return Err("安装命令已结束，但 Docker 尚未通过连接测试；请核对 VPS 状态。".into());
    }
    on_stage("配置 Docker Hub 镜像源");
    configure_registry_mirrors(expected_host, |stage| on_stage(stage)).map_err(|error| {
        format!("Docker 已安装并通过连接测试，但镜像源配置未完成：{error} 请在主机详情中核对后单独配置镜像源。")
    })?;
    Ok(())
}

async fn configure_mirrors_on(
    material: ConnectionMaterial,
    on_stage: &mut dyn FnMut(&str),
) -> Result<MirrorConfigResult, String> {
    if material.host.username != "root" {
        return Err("配置 Docker 镜像源需要 root 登录。".into());
    }
    let handle = tokio::time::timeout(Duration::from_secs(20), trusted_handle(material))
        .await
        .map_err(|_| "镜像源配置 SSH 连接超时。")??;
    if archive_command(&handle, "id -u").await?.trim() != "0" {
        return Err("远端登录用户不是 root，未修改 Docker 配置。".into());
    }
    let endpoint = tokio::time::timeout(Duration::from_secs(10), local_docker_endpoint(&handle))
        .await
        .map_err(|_| "无法核对远端 Docker 端点。")??;
    if endpoint != "unix:///var/run/docker.sock"
        || archive_command(
            &handle,
            "sh -c 'test -S /var/run/docker.sock && systemctl is-active docker'",
        )
        .await?
        .trim()
            != "active"
    {
        return Err("当前 Docker context 不是系统 docker.service 的默认 socket，拒绝重启。".into());
    }
    let status_command = "sh -c 'if test -L /etc/docker/daemon.json; then echo symlink; elif test -f /etc/docker/daemon.json; then echo file; elif test -e /etc/docker/daemon.json; then echo other; else echo absent; fi'";
    let status = archive_command(&handle, status_command).await?;
    let original = match status.trim() {
        "file" => Some(
            tokio::time::timeout(
                Duration::from_secs(10),
                command_limited(&handle, "cat -- /etc/docker/daemon.json", 64 * 1024),
            )
            .await
            .map_err(|_| "读取现有 Docker 配置超时。")??,
        ),
        "absent" => None,
        _ => return Err("现有 Docker daemon.json 路径不是普通文件，未修改。".into()),
    };
    let update = merged_mirror_config(original.as_deref())?;
    if update.is_none() {
        let active = archive_command(
            &handle,
            &docker_command(
                &endpoint,
                "info --format '{{json .RegistryConfig.Mirrors}}'",
            ),
        )
        .await?;
        if mirrors_active(&active) {
            let _ = handle.disconnect(Disconnect::ByApplication, "", "en").await;
            return Ok(MirrorConfigResult::AlreadyConfigured);
        }
    }
    let bytes = match update {
        Some(bytes) => bytes,
        None => original.as_ref().unwrap().as_bytes().to_vec(),
    };
    if bytes.len() > 64 * 1024 {
        return Err("Docker daemon.json 太大，未修改。".into());
    }
    let running = archive_command(&handle, &docker_command(&endpoint, "ps -q")).await?;
    if !running.trim().is_empty() {
        return Err("主机存在运行中的容器，重启 Docker 会中断服务；未修改配置。".into());
    }
    if archive_command(&handle, "sh -c 'test -d /run/systemd/system && command -v base64 >/dev/null && command -v dockerd >/dev/null && echo ready'")
        .await?
        .trim() != "ready"
    {
        return Err("主机缺少 systemd、base64 或 dockerd，未修改配置。".into());
    }
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    let temp = format!("/etc/docker/.devingpanel-{nonce}.json");
    let backup = format!("/etc/docker/.devingpanel-{nonce}.bak");
    let new_digest = format!("{:x}", Sha256::digest(&bytes));
    let old_digest = original
        .as_ref()
        .map(|text| format!("{:x}", Sha256::digest(text.as_bytes())));
    let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
    on_stage("准备并验证 Docker 配置");
    if let Err(error) = archive_command(
        &handle,
        &format!("sh -c 'umask 077; test ! -e {temp} && printf %s {encoded} | base64 -d > {temp}'"),
    )
    .await
    {
        let _ = archive_command(&handle, &format!("rm -f -- {temp}")).await;
        return Err(error);
    }
    let lock = "/etc/docker/.devingpanel-mirror.lock";
    if archive_command(&handle, &format!("mkdir -m 700 -- {lock}"))
        .await
        .is_err()
    {
        let _ = archive_command(&handle, &format!("rm -f -- {temp}")).await;
        return Err(format!(
            "另一客户端正在配置 Docker，或上次操作残留锁 {lock}；未修改配置。"
        ));
    }
    let result = async {
        archive_command(&handle, &format!("dockerd --validate --config-file {temp}")).await?;
        let current = archive_command(&handle, status_command).await?;
        if current.trim() != status.trim() {
            return Err("Docker 配置在操作期间发生变化，未覆盖。".into());
        }
        if !archive_command(&handle, &docker_command(&endpoint, "ps -q"))
            .await?
            .trim()
            .is_empty()
        {
            return Err("配置期间出现运行中的容器，未重启 Docker。".into());
        }
        if let Some(original_text) = &original {
            let initial = verified_sha256(
                &archive_command(&handle, "sha256sum -- /etc/docker/daemon.json").await?,
            )?;
            let local = format!("{:x}", Sha256::digest(original_text.as_bytes()));
            if initial != local {
                return Err("Docker 配置在操作期间发生变化，未覆盖。".into());
            }
            archive_command(
                &handle,
                &format!("cp -p -- /etc/docker/daemon.json {backup} && chmod 600 -- {backup}"),
            )
            .await?;
        }
        archive_command(&handle, &format!("mv -T -- {temp} /etc/docker/daemon.json")).await?;
        Ok::<_, String>(())
    }
    .await;
    if let Err(error) = result {
        let current = async {
            match archive_command(&handle, status_command).await?.trim() {
                "absent" => Ok(None),
                "file" => verified_sha256(
                    &archive_command(&handle, "sha256sum -- /etc/docker/daemon.json").await?,
                )
                .map(Some),
                _ => Err("Docker 配置路径已变化。".into()),
            }
        }
        .await;
        match current {
            Ok(Some(digest)) if digest == new_digest => {}
            Ok(digest) if digest == old_digest => {
                let _ = archive_command(&handle, &format!("rm -f -- {temp} {backup}")).await;
                let _ = archive_command(&handle, &format!("rmdir -- {lock}")).await;
                return Err(error);
            }
            _ => {
                return Err(format!(
                    "{error}；Docker 配置结果待核实，请检查 {temp}、{backup}、daemon.json 和 {lock}。"
                ));
            }
        }
    }
    on_stage("重启并验证 Docker");
    let restart = tokio::time::timeout(
        Duration::from_secs(90),
        command_limited(&handle, "systemctl restart docker", 4096),
    )
    .await;
    if restart.is_err() {
        return Err(if original.is_some() {
            format!("Docker 重启超时，结果待核实；请核对备份 {backup} 和锁 {lock}。")
        } else {
            format!("Docker 重启超时，结果待核实；请核对 daemon.json、服务状态和锁 {lock}。")
        });
    }
    let verified = async {
        restart.unwrap()?;
        if archive_command(&handle, "systemctl is-active docker")
            .await?
            .trim()
            != "active"
        {
            return Err("Docker 服务未启动。".into());
        }
        let active = archive_command(
            &handle,
            &docker_command(
                &endpoint,
                "info --format '{{json .RegistryConfig.Mirrors}}'",
            ),
        )
        .await?;
        if !mirrors_active(&active) {
            return Err("Docker 未加载预期镜像源。".into());
        }
        Ok::<_, String>(())
    }
    .await;
    if let Err(error) = verified {
        let restore = if original.is_some() {
            format!("mv -T -- {backup} /etc/docker/daemon.json && systemctl restart docker")
        } else {
            "rm -f -- /etc/docker/daemon.json && systemctl restart docker".into()
        };
        return match archive_command_with_timeout(&handle, &restore, Duration::from_secs(90)).await
        {
            Ok(_) => {
                let _ = archive_command(&handle, &format!("rmdir -- {lock}")).await;
                Err(format!("{error}；原 Docker 配置已恢复。"))
            }
            Err(_) => Err(format!(
                "{error}；自动恢复失败，需在 VPS 核对 {backup} 和 {lock}。"
            )),
        };
    }
    if original.is_some() {
        let _ = archive_command(&handle, &format!("rm -f -- {backup}")).await;
    }
    archive_command(&handle, &format!("rmdir -- {lock}"))
        .await
        .map_err(|_| format!("Docker 镜像源已生效，但锁 {lock} 未清理；请人工核对。"))?;
    let _ = handle.disconnect(Disconnect::ByApplication, "", "en").await;
    Ok(MirrorConfigResult::Updated)
}

pub fn configure_registry_mirrors(
    expected_host: Host,
    mut on_stage: impl FnMut(&str),
) -> Result<MirrorConfigResult, String> {
    let material = material_for(&expected_host)?;
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| "无法初始化 Docker 镜像源配置环境。".to_owned())?
        .block_on(configure_mirrors_on(material, &mut on_stage))
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
        SftpSession::new_with_config(channel.into_stream(), sftp_upload_config()),
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
        image_ids: Vec::new(),
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
    Box::pin(transfer_archive_to(
        material,
        source,
        size,
        cancel,
        progress,
        ArchiveTransferOptions::default(),
    ))
    .await
}

#[derive(Default)]
struct ArchiveTransferOptions<'a> {
    directory: Option<&'a str>,
    preserve_on_error: bool,
    expected_identity: Option<ReleaseDockerIdentity<'a>>,
}

async fn transfer_archive_to(
    material: ConnectionMaterial,
    source: &std::path::Path,
    size: u64,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(u64, u64),
    options: ArchiveTransferOptions<'_>,
) -> Result<UploadedArchive, String> {
    let handle = tokio::time::timeout(Duration::from_secs(20), trusted_handle(material))
        .await
        .map_err(|_| "SSH 连接超时，传输未开始。")??;
    if let Some(identity) = options.expected_identity {
        checked_release_endpoint(&handle, identity).await?;
    }
    let available_kib = disk_available(&archive_command(&handle, "df -Pk /tmp").await?)?;
    if size.saturating_add(64 * 1024 * 1024) > available_kib.saturating_mul(1024) {
        return Err("远端 /tmp 可用空间不足，传输未开始。".into());
    }
    if cancel.load(Ordering::Relaxed) {
        return Err("镜像传输已取消，传输未开始。".into());
    }
    let directory = validated_upload_directory(
        &options
            .directory
            .map(str::to_owned)
            .unwrap_or_else(|| format!("/tmp/devingpanel.{}", uuid::Uuid::new_v4().simple())),
    )?;
    let create = if options.preserve_on_error {
        archive_command(&handle, &format!(
            "sh -c 'if test -L {directory}; then exit 1; \
             elif test -e {directory}; then test -d {directory} && test \"$(stat -c %a -- {directory})\" = 700; \
             else mkdir -m 700 -- {directory}; fi'"
        )).await
    } else {
        archive_command(&handle, &format!(
            "mkdir -m 700 -- {directory} && chmod 700 -- {directory} && test \"$(stat -c %a -- {directory})\" = 700"
        )).await
    };
    if let Err(error) = create {
        if !options.preserve_on_error {
            // A timed-out mkdir may have succeeded; the random path is known.
            let _ = archive_command(&handle, &format!("rmdir -- {directory}")).await;
        }
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
        if options.preserve_on_error {
            let _ = handle.disconnect(Disconnect::ByApplication, "", "en").await;
            return result
                .map_err(|error| format!("{error}；保留预留临时目录 {directory} 待核实。"));
        }
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

pub fn upload_archive_at(
    expected_host: Host,
    source: &std::path::Path,
    directory: &str,
    cancel: Arc<AtomicBool>,
    mut progress: impl FnMut(u64, u64),
) -> Result<UploadedArchive, String> {
    let directory = validated_upload_directory(directory)?;
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
        .map_err(|_| "无法初始化传输环境。")?
        .block_on(Box::pin(transfer_archive_to(
            material,
            source,
            size,
            &cancel,
            &mut progress,
            ArchiveTransferOptions {
                directory: Some(&directory),
                ..Default::default()
            },
        )))
}

pub fn upload_unknown_archive_at(
    expected_host: Host,
    identity: ReleaseDockerIdentity<'_>,
    source: &std::path::Path,
    directory: &str,
    cancel: Arc<AtomicBool>,
    mut progress: impl FnMut(u64, u64),
) -> Result<UploadedArchive, String> {
    let directory = validated_upload_directory(directory)?;
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
        .block_on(Box::pin(transfer_archive_to(
            material,
            source,
            size,
            &cancel,
            &mut progress,
            ArchiveTransferOptions {
                directory: Some(&directory),
                preserve_on_error: true,
                expected_identity: Some(identity),
            },
        )))
}

fn remote_archive_size(output: &str) -> Result<Option<u64>, String> {
    let status = output.trim();
    if status == "missing" {
        return Ok(None);
    }
    let size = status
        .strip_prefix("size:")
        .and_then(|text| text.parse::<u64>().ok())
        .ok_or("远端临时归档类型或大小无法核实。")?;
    Ok(Some(size))
}

async fn remote_archive_size_on(
    handle: &client::Handle<HostKeyChecker>,
    directory: &str,
) -> Result<Option<u64>, String> {
    let directory = validated_upload_directory(directory)?;
    let path = format!("{directory}/image.tar");
    let command = format!(
        "sh -c 'if test -L {directory}; then echo invalid; \
         elif test ! -e {directory}; then echo missing; \
         elif test ! -d {directory}; then echo invalid; \
         elif test \"$(stat -c %a -- {directory})\" != 700; then echo invalid; \
         elif test -L {path}; then echo invalid; \
         elif test -f {path}; then stat -c \"size:%s\" -- {path}; \
         elif test -e {path}; then echo invalid; else echo missing; fi'"
    );
    remote_archive_size(&archive_command(handle, &command).await?)
}

async fn resume_uploaded_archive_on(
    material: ConnectionMaterial,
    identity: ReleaseDockerIdentity<'_>,
    record: &crate::release_store::UploadedArchiveRecord,
    source: &std::path::Path,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(u64, u64),
) -> Result<UploadedArchive, String> {
    let directory = validated_upload_directory(&record.directory)?;
    let path = format!("{directory}/image.tar");
    let source_bytes = tokio::fs::metadata(source)
        .await
        .map_err(|_| "本地归档不存在。")?
        .len();
    if source_bytes != record.bytes
        || source_bytes == 0
        || record.sha256.len() != 64
        || !record.sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err("本地归档与上传意图不一致。".into());
    }
    let handle = tokio::time::timeout(Duration::from_secs(20), trusted_handle(material))
        .await
        .map_err(|_| "续传 SSH 连接超时。")??;
    checked_release_endpoint(&handle, identity).await?;
    let offset = remote_archive_size_on(&handle, &directory)
        .await?
        .ok_or("远端部分归档不存在，不能按原路径续传。")?;
    if offset > record.bytes {
        return Err("远端归档大于预期，禁止续传。".into());
    }
    let available = disk_available(&archive_command(&handle, "df -Pk /tmp").await?)?;
    if record.bytes - offset + 64 * 1024 * 1024 > available.saturating_mul(1024) {
        return Err("远端 /tmp 可用空间不足，未续传。".into());
    }
    let mut local = tokio::fs::File::open(source)
        .await
        .map_err(|_| "无法读取本地镜像归档。")?;
    let mut hasher = Sha256::new();
    let mut remaining = offset;
    let mut buffer = [0u8; 64 * 1024];
    while remaining > 0 {
        if cancel.load(Ordering::Relaxed) {
            return Err("续传预检已取消。".into());
        }
        let length = remaining.min(buffer.len() as u64) as usize;
        let read = tokio::time::timeout(Duration::from_secs(60), local.read(&mut buffer[..length]))
            .await
            .map_err(|_| "本地归档前缀读取超时。")?
            .map_err(|_| "本地归档前缀不可读取。")?;
        if read == 0 {
            return Err("本地归档在核对期间缩短。".into());
        }
        hasher.update(&buffer[..read]);
        remaining -= read as u64;
    }
    let remote_digest = archive_command_with_timeout(
        &handle,
        &format!("sha256sum -- {path}"),
        Duration::from_secs(60 * 60),
    )
    .await?;
    if format!("{:x}", hasher.clone().finalize()) != verified_sha256(&remote_digest)? {
        return Err("远端部分归档与本地前缀 SHA-256 不符，禁止续传。".into());
    }
    if offset < record.bytes {
        if cancel.load(Ordering::Relaxed) {
            return Err("续传已取消。".into());
        }
        let channel = handle
            .channel_open_session()
            .await
            .map_err(|_| "无法打开续传 SFTP 会话。")?;
        channel
            .request_subsystem(true, "sftp")
            .await
            .map_err(|_| "续传 SFTP 子系统不可用。")?;
        let sftp = SftpSession::new_with_config(channel.into_stream(), sftp_upload_config())
            .await
            .map_err(|_| "续传 SFTP 协议握手失败。")?;
        let metadata = sftp
            .metadata(path.clone())
            .await
            .map_err(|_| "远端归档属性不可读。")?;
        if metadata.len() != offset
            || metadata
                .permissions
                .is_none_or(|mode| mode & 0o777 != 0o600)
        {
            return Err("远端归档大小或权限已变化，禁止续传。".into());
        }
        let mut remote = sftp
            .open_with_flags_and_attributes(
                path.clone(),
                OpenFlags::WRITE,
                FileAttributes::default(),
            )
            .await
            .map_err(|_| "无法打开远端部分归档。")?;
        remote
            .seek(SeekFrom::Start(offset))
            .await
            .map_err(|_| "远端归档无法定位续传偏移。")?;
        let mut copied = offset;
        progress(copied, record.bytes);
        while copied < record.bytes {
            if cancel.load(Ordering::Relaxed) {
                return Err("镜像续传已取消，远端结果待核实。".into());
            }
            let read = tokio::time::timeout(Duration::from_secs(60), local.read(&mut buffer))
                .await
                .map_err(|_| "本地归档读取超时。")?
                .map_err(|_| "本地归档读取失败。")?;
            if read == 0 || copied + read as u64 > record.bytes {
                return Err("本地归档在续传期间变化。".into());
            }
            tokio::time::timeout(Duration::from_secs(60), remote.write_all(&buffer[..read]))
                .await
                .map_err(|_| "远端续传写入超时，结果待核实。")?
                .map_err(|_| "远端续传写入失败，结果待核实。")?;
            hasher.update(&buffer[..read]);
            copied += read as u64;
            progress(copied.min(record.bytes - 1), record.bytes);
        }
        tokio::time::timeout(Duration::from_secs(60), remote.close())
            .await
            .map_err(|_| "续传文件关闭超时，结果待核实。")?
            .map_err(|_| "续传文件关闭失败，结果待核实。")?;
        let _ = sftp.close().await;
    }
    if format!("{:x}", hasher.finalize()) != record.sha256.to_ascii_lowercase()
        || remote_archive_size_on(&handle, &directory).await? != Some(record.bytes)
        || verified_sha256(
            &archive_command_with_timeout(
                &handle,
                &format!("sha256sum -- {path}"),
                Duration::from_secs(60 * 60),
            )
            .await?,
        )? != record.sha256.to_ascii_lowercase()
    {
        return Err("续传后本机或远端完整归档校验不符，禁止加载。".into());
    }
    progress(record.bytes, record.bytes);
    let _ = handle.disconnect(Disconnect::ByApplication, "", "en").await;
    Ok(UploadedArchive {
        directory,
        path,
        bytes: record.bytes,
        sha256: record.sha256.to_ascii_lowercase(),
        image_ids: Vec::new(),
    })
}

pub fn resume_uploaded_archive(
    expected_host: Host,
    identity: ReleaseDockerIdentity<'_>,
    record: &crate::release_store::UploadedArchiveRecord,
    source: &std::path::Path,
    cancel: Arc<AtomicBool>,
    mut progress: impl FnMut(u64, u64),
) -> Result<UploadedArchive, String> {
    let material = material_for(&expected_host)?;
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| "无法初始化续传环境。".to_owned())?
        .block_on(resume_uploaded_archive_on(
            material,
            identity,
            record,
            source,
            &cancel,
            &mut progress,
        ))
}

pub fn inspect_uploaded_archives(
    task: &TaskSnapshot,
    records: &[crate::release_store::UploadedArchiveRecord],
) -> Result<Vec<RemoteArchiveObservation>, String> {
    if records.is_empty() || records.len() > 100 {
        return Err("任务没有可核实的远端归档记录。".into());
    }
    let endpoint = task
        .docker_endpoint
        .as_deref()
        .ok_or("任务缺少 Docker endpoint 快照。")?;
    let daemon_id = task
        .docker_daemon_id
        .as_deref()
        .ok_or("任务缺少 Docker daemon 身份快照。")?;
    let mut directories = HashSet::new();
    for record in records {
        validated_upload_directory(&record.directory)?;
        if record.service.is_empty()
            || record.service.len() > 128
            || record.service.chars().any(char::is_control)
            || record.bytes == 0
            || record.sha256.len() != 64
            || !record.sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
            || !directories.insert(&record.directory)
        {
            return Err("远端归档意图无效或路径重复。".into());
        }
    }
    let material = material_for(&task.host)?;
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| "无法初始化远端归档核实环境。".to_owned())?
        .block_on(async {
            let handle = tokio::time::timeout(Duration::from_secs(20), trusted_handle(material))
                .await
                .map_err(|_| "远端归档核实连接超时。")??;
            checked_release_endpoint(
                &handle,
                ReleaseDockerIdentity {
                    endpoint,
                    daemon_id,
                },
            )
            .await?;
            let mut observations = Vec::with_capacity(records.len());
            for record in records {
                let directory = &record.directory;
                let path = format!("{directory}/image.tar");
                let actual_bytes = remote_archive_size_on(&handle, directory).await?;
                let digest_matches = if actual_bytes == Some(record.bytes) {
                    let digest = archive_command_with_timeout(
                        &handle,
                        &format!("sha256sum -- {path}"),
                        Duration::from_secs(60),
                    )
                    .await?;
                    Some(verified_sha256(&digest)? == record.sha256.to_ascii_lowercase())
                } else {
                    None
                };
                observations.push(RemoteArchiveObservation {
                    service: record.service.clone(),
                    directory: directory.clone(),
                    state: record.state.clone(),
                    expected_bytes: record.bytes,
                    actual_bytes,
                    digest_matches,
                });
            }
            let _ = handle.disconnect(Disconnect::ByApplication, "", "en").await;
            Ok(observations)
        })
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

async fn write_staged_file(
    handle: &client::Handle<HostKeyChecker>,
    sftp: &SftpSession,
    path: &str,
    bytes: &[u8],
    cancel: &AtomicBool,
) -> Result<(), String> {
    let mut remote = tokio::time::timeout(
        Duration::from_secs(10),
        sftp.open_with_flags_and_attributes(
            path.to_owned(),
            OpenFlags::CREATE | OpenFlags::EXCLUDE | OpenFlags::WRITE,
            FileAttributes {
                permissions: Some(0o600),
                ..Default::default()
            },
        ),
    )
    .await
    .map_err(|_| "远端部署文件创建超时。")?
    .map_err(|_| "无法创建远端部署文件。")?;
    for chunk in bytes.chunks(64 * 1024) {
        tokio::select! {
            _ = wait_cancel(cancel) => return Err("部署文件上传已取消，结果待核实。".into()),
            result = tokio::time::timeout(Duration::from_secs(60), remote.write_all(chunk)) =>
                result.map_err(|_| "远端部署文件写入超时。")?
                    .map_err(|_| "远端部署文件写入失败。")?,
        }
    }
    tokio::time::timeout(Duration::from_secs(30), remote.close())
        .await
        .map_err(|_| "远端部署文件关闭超时。")?
        .map_err(|_| "远端部署文件关闭失败。")?;
    let metadata = tokio::time::timeout(Duration::from_secs(10), sftp.metadata(path.to_owned()))
        .await
        .map_err(|_| "远端部署文件属性检查超时。")?
        .map_err(|_| "无法核对远端部署文件属性。")?;
    if metadata.len() != bytes.len() as u64
        || metadata
            .permissions
            .is_none_or(|mode| mode & 0o777 != 0o600)
    {
        return Err("远端部署文件大小或权限不符合要求。".into());
    }
    let command = format!("sha256sum -- {}", shell_quote(path)?);
    let remote_digest = verified_sha256(
        &archive_command_with_timeout(handle, &command, Duration::from_secs(60)).await?,
    )?;
    if remote_digest != format!("{:x}", Sha256::digest(bytes)) {
        return Err("远端部署文件 SHA-256 校验失败。".into());
    }
    Ok(())
}

async fn stage_deployment_on(
    material: ConnectionMaterial,
    project_id: i64,
    task_id: &str,
    files: &[DeploymentFile],
    env_file: &[u8],
    cancel: &AtomicBool,
) -> Result<StagedDeployment, String> {
    if files.is_empty() || files[0].path.contains('/') || env_file.len() > 1024 * 1024 {
        return Err("部署文件列表或 .env 无效。".into());
    }
    let mut seen = HashSet::new();
    let mut total = env_file.len();
    for file in files {
        validated_release_file(&file.path)?;
        if !seen.insert(file.path.as_str()) || file.bytes.len() > 16 * 1024 * 1024 {
            return Err("部署文件重复或过大。".into());
        }
        total = total.saturating_add(file.bytes.len());
    }
    if total > 64 * 1024 * 1024 || cancel.load(Ordering::Relaxed) {
        return Err("部署文件总量过大或操作已取消。".into());
    }
    let handle = tokio::time::timeout(Duration::from_secs(20), trusted_handle(material))
        .await
        .map_err(|_| "部署文件 SSH 连接超时。")??;
    let home = archive_command(&handle, "sh -c 'printf %s \"$HOME\"'").await?;
    let directory = validated_release_directory(&home, project_id, task_id)?;
    let parent = directory.rsplit_once('/').unwrap().0;
    let quoted_parent = shell_quote(parent)?;
    let quoted_directory = shell_quote(&directory)?;
    let create = archive_command(&handle, &format!(
        "umask 077; mkdir -p -- {quoted_parent} && mkdir -m 700 -- {quoted_directory} && test \"$(stat -c %a -- {quoted_directory})\" = 700"
    )).await;
    if let Err(error) = create {
        let cleanup = archive_command(&handle, &format!("rmdir -- {quoted_directory}")).await;
        return Err(if cleanup.is_ok() {
            format!("创建远端发布目录失败：{error}。")
        } else {
            format!("创建远端发布目录失败：{error}；请核对 {directory} 是否残留。")
        });
    }
    let mut paths = Vec::new();
    let mut subdirs = Vec::new();
    let result = async {
        let channel = tokio::time::timeout(Duration::from_secs(10), handle.channel_open_session())
            .await
            .map_err(|_| "部署 SFTP 会话建立超时。")?
            .map_err(|_| "无法打开部署 SFTP 会话。")?;
        tokio::time::timeout(
            Duration::from_secs(10),
            channel.request_subsystem(true, "sftp"),
        )
        .await
        .map_err(|_| "部署 SFTP 子系统启动超时。")?
        .map_err(|_| "远端 SFTP 不可用。")?;
        let sftp = tokio::time::timeout(
            Duration::from_secs(10),
            SftpSession::new_with_config(channel.into_stream(), sftp_upload_config()),
        )
        .await
        .map_err(|_| "部署 SFTP 协议握手超时。")?
        .map_err(|_| "部署 SFTP 协议握手失败。")?;
        for file in files {
            let mut cursor = directory.clone();
            if let Some((parent, _)) = file.path.rsplit_once('/') {
                for part in parent.split('/') {
                    cursor.push('/');
                    cursor.push_str(part);
                    if !subdirs.contains(&cursor) {
                        subdirs.push(cursor.clone());
                        archive_command(
                            &handle,
                            &format!("mkdir -m 700 -- {}", shell_quote(&cursor)?),
                        )
                        .await?;
                    }
                }
            }
            let path = format!("{directory}/{}", file.path);
            paths.push(path.clone());
            write_staged_file(&handle, &sftp, &path, &file.bytes, cancel).await?;
        }
        let env_path = format!("{directory}/.env");
        paths.push(env_path.clone());
        write_staged_file(&handle, &sftp, &env_path, env_file, cancel).await?;
        let _ = sftp.close().await;
        Ok::<_, String>(StagedDeployment {
            directory: directory.clone(),
            compose_files: files.iter().map(|file| file.path.clone()).collect(),
            env_path,
        })
    }
    .await;
    if result.is_err() {
        let cleanup = cleanup_staged_paths(&handle, &directory, &paths, &subdirs).await;
        let _ = handle.disconnect(Disconnect::ByApplication, "", "en").await;
        return result.map_err(|error| match cleanup {
            Ok(()) => error,
            Err(_) => format!("{error}；发布目录 {directory} 可能残留，请人工核对。"),
        });
    }
    let _ = handle.disconnect(Disconnect::ByApplication, "", "en").await;
    result
}

async fn cleanup_staged_paths(
    handle: &client::Handle<HostKeyChecker>,
    directory: &str,
    files: &[String],
    subdirs: &[String],
) -> Result<(), String> {
    for path in files.iter().rev() {
        archive_command(handle, &format!("rm -f -- {}", shell_quote(path)?)).await?;
    }
    for path in subdirs.iter().rev() {
        let quoted = shell_quote(path)?;
        archive_command(
            handle,
            &format!("rmdir -- {quoted} || (test ! -e {quoted} && test ! -L {quoted})"),
        )
        .await?;
    }
    let quoted = shell_quote(directory)?;
    archive_command(
        handle,
        &format!("rmdir -- {quoted} || (test ! -e {quoted} && test ! -L {quoted})"),
    )
    .await?;
    Ok(())
}

#[allow(dead_code)] // The release coordinator will call this before Compose config/up.
pub fn stage_deployment(
    expected_host: Host,
    project_id: i64,
    task_id: &str,
    files: &[DeploymentFile],
    env_file: &[u8],
    cancel: Arc<AtomicBool>,
) -> Result<StagedDeployment, String> {
    let material = material_for(&expected_host)?;
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| "无法初始化部署文件传输环境。".to_owned())?
        .block_on(stage_deployment_on(
            material, project_id, task_id, files, env_file, &cancel,
        ))
}

fn compose_config_command(
    endpoint: &str,
    staged: &StagedDeployment,
    project_id: i64,
    reference: &str,
) -> Result<String, String> {
    let (image, tag) = reference.rsplit_once(':').ok_or("镜像引用无效。")?;
    if project_id <= 0
        || !crate::project_store::valid_image_name(image)
        || tag.is_empty()
        || tag.len() > 128
        || !tag
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-'))
        || staged.compose_files.is_empty()
        || staged.compose_files[0].contains('/')
        || staged.env_path != format!("{}/.env", staged.directory)
    {
        return Err("待校验部署配置无效。".into());
    }
    let directory = shell_quote(&staged.directory)?;
    let mut command = format!(
        "env DEVINGPANEL_IMAGE={} {} -p dp{project_id} --project-directory {directory} --env-file {}",
        shell_quote(reference)?,
        docker_command(endpoint, "compose"),
        shell_quote(&staged.env_path)?
    );
    for relative in &staged.compose_files {
        validated_release_file(relative)?;
        command.push_str(&format!(
            " -f {}",
            shell_quote(&format!("{}/{}", staged.directory, relative))?
        ));
    }
    Ok(command)
}

fn verified_compose_config(text: &str, service: &str, reference: &str) -> Result<(), String> {
    let config: Value = serde_json::from_str(text).map_err(|_| "远端 Compose 配置无法解析。")?;
    let services = config
        .get("services")
        .and_then(Value::as_object)
        .ok_or("远端 Compose 配置缺少服务。")?;
    let target = services.get(service).ok_or("目标服务不存在。")?;
    if target.get("image").and_then(Value::as_str) != Some(reference)
        || target
            .get("scale")
            .is_some_and(|value| value.as_u64() != Some(1))
        || target
            .pointer("/deploy/replicas")
            .is_some_and(|value| value.as_u64() != Some(1))
        || services.values().any(|value| {
            value.get("build").is_some_and(|build| !build.is_null())
                || value
                    .get("image")
                    .and_then(Value::as_str)
                    .is_none_or(str::is_empty)
        })
    {
        return Err("远端 Compose 镜像或构建配置与发布计划不一致。".into());
    }
    Ok(())
}

fn valid_compose_service(service: &str) -> bool {
    !service.is_empty()
        && service.len() <= 256
        && service
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_alphanumeric())
        && service
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}

async fn validate_staged_compose_on(
    material: ConnectionMaterial,
    identity: ReleaseDockerIdentity<'_>,
    staged: &StagedDeployment,
    project_id: i64,
    service: &str,
    reference: &str,
    cancel: &AtomicBool,
) -> Result<(), String> {
    if !valid_compose_service(service) || cancel.load(Ordering::Relaxed) {
        return Err("部署服务无效或配置校验已取消。".into());
    }
    let handle = tokio::time::timeout(Duration::from_secs(20), trusted_handle(material))
        .await
        .map_err(|_| "Compose 校验连接超时。")??;
    let endpoint = checked_release_endpoint(&handle, identity).await?;
    let command = format!(
        "{} config --format json",
        compose_config_command(&endpoint, staged, project_id, reference)?
    );
    let output = tokio::select! {
        _ = wait_cancel(cancel) => return Err("远端 Compose 校验已取消。".into()),
        result = tokio::time::timeout(Duration::from_secs(60), command_limited(&handle, &command, 128 * 1024 * 1024)) =>
            result.map_err(|_| "远端 Compose 校验超时。")?
                .map_err(|_| "远端 Compose 校验失败，请检查上传的文件和环境配置。")?,
    };
    verified_compose_config(&output, service, reference)?;
    let _ = handle.disconnect(Disconnect::ByApplication, "", "en").await;
    Ok(())
}

#[allow(dead_code)] // The release coordinator stops after validation until deployment is wired up.
pub fn validate_staged_compose(
    expected_host: Host,
    identity: ReleaseDockerIdentity<'_>,
    staged: &StagedDeployment,
    project_id: i64,
    service: &str,
    reference: &str,
    cancel: Arc<AtomicBool>,
) -> Result<(), String> {
    let material = material_for(&expected_host)?;
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| "无法初始化 Compose 校验环境。".to_owned())?
        .block_on(validate_staged_compose_on(
            material, identity, staged, project_id, service, reference, &cancel,
        ))
}

fn stack_command(
    endpoint: &str,
    staged: &StagedDeployment,
    project_id: i64,
    built: &[StackService],
) -> Result<String, String> {
    let first = built.first().ok_or("没有待构建的服务。")?;
    let base = compose_config_command(endpoint, staged, project_id, &first.reference)?;
    let mut variables = String::new();
    for image in built {
        if !valid_compose_service(&image.service) {
            return Err("构建服务名称无效。".into());
        }
        let variable = crate::project_config::image_variable(&image.service)?;
        variables.push_str(&format!("{variable}={} ", shell_quote(&image.reference)?));
    }
    Ok(base.replacen("env ", &format!("env {variables}"), 1))
}

fn verified_stack_config(text: &str, built: &[StackService]) -> Result<Vec<StackService>, String> {
    let config: Value = serde_json::from_str(text).map_err(|_| "远端 Compose 配置无法解析。")?;
    let services = config
        .get("services")
        .and_then(Value::as_object)
        .ok_or("远端 Compose 配置缺少服务。")?;
    if services.is_empty() || services.len() > 100 {
        return Err("Compose 服务数量不受支持。".into());
    }
    let mut resolved = Vec::new();
    for (name, entry) in services {
        if !valid_compose_service(name)
            || entry.get("build").is_some_and(|build| !build.is_null())
            || entry
                .get("profiles")
                .and_then(Value::as_array)
                .is_some_and(|profiles| !profiles.is_empty())
            || entry
                .get("scale")
                .is_some_and(|value| value.as_u64() != Some(1))
            || entry
                .pointer("/deploy/replicas")
                .is_some_and(|value| value.as_u64() != Some(1))
            || entry
                .pointer("/healthcheck/disable")
                .and_then(Value::as_bool)
                == Some(true)
        {
            return Err("Compose 服务包含不安全的构建、扩容或健康设置。".into());
        }
        let reference = entry
            .get("image")
            .and_then(Value::as_str)
            .ok_or("Compose 服务缺少镜像。")?;
        if reference.is_empty() || reference.len() > 512 || reference.chars().any(char::is_control)
        {
            return Err("Compose 镜像引用无效。".into());
        }
        let image_ids = if let Some(expected) = built.iter().find(|item| item.service == *name) {
            if reference != expected.reference
                || validate_deployment_image_ids(&expected.image_ids).is_err()
            {
                return Err("远端构建镜像与发布计划不一致。".into());
            }
            expected.image_ids.clone()
        } else {
            Vec::new()
        };
        resolved.push(StackService {
            service: name.clone(),
            reference: reference.to_owned(),
            image_ids,
        });
    }
    if built
        .iter()
        .any(|image| !resolved.iter().any(|entry| entry.service == image.service))
    {
        return Err("Compose 缺少已构建的服务。".into());
    }
    Ok(resolved)
}

async fn validate_stack_on(
    material: ConnectionMaterial,
    identity: ReleaseDockerIdentity<'_>,
    staged: &StagedDeployment,
    project_id: i64,
    built: &[StackService],
    cancel: &AtomicBool,
) -> Result<Vec<StackService>, String> {
    let handle = tokio::time::timeout(Duration::from_secs(20), trusted_handle(material))
        .await
        .map_err(|_| "Compose 校验连接超时。")??;
    let endpoint = checked_release_endpoint(&handle, identity).await?;
    let command = format!(
        "{} config --format json",
        stack_command(&endpoint, staged, project_id, built)?
    );
    let output = tokio::select! {
        _ = wait_cancel(cancel) => return Err("整栈配置校验已取消。".into()),
        result = tokio::time::timeout(Duration::from_secs(60), command_limited(&handle, &command, 128 * 1024 * 1024)) =>
            result.map_err(|_| "远端 Compose 校验超时。")?.map_err(|_| "远端 Compose 配置校验失败。")?,
    };
    verified_stack_config(&output, built)
}

pub fn validate_staged_stack(
    host: Host,
    identity: ReleaseDockerIdentity<'_>,
    staged: &StagedDeployment,
    project_id: i64,
    built: &[StackService],
    cancel: Arc<AtomicBool>,
) -> Result<Vec<StackService>, String> {
    let material = material_for(&host)?;
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| "无法初始化整栈校验环境。".to_owned())?
        .block_on(validate_stack_on(
            material, identity, staged, project_id, built, &cancel,
        ))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeploymentState {
    Healthy,
    RunningUnverified,
    Failed,
}

fn validate_deployment_image_ids(ids: &[String]) -> Result<(), String> {
    if ids.is_empty()
        || ids.len() > 4
        || ids.iter().any(|id| {
            id.len() != 71
                || !id.starts_with("sha256:")
                || !id[7..].bytes().all(|byte| byte.is_ascii_hexdigit())
        })
    {
        return Err("镜像 ID 无效。".into());
    }
    Ok(())
}

fn deployment_inspect_command(endpoint: &str, ids: &[String]) -> Result<String, String> {
    if ids.is_empty() || ids.len() > 100 || ids.iter().any(|id| validate_container_id(id).is_err())
    {
        return Err("目标服务容器标识无效。".into());
    }
    let template = r#"{"Id":{{json .Id}},"Service":{{json (index .Config.Labels "com.docker.compose.service")}},"Image":{{json .Image}},"ConfigImage":{{json .Config.Image}},"Status":{{json .State.Status}},"Health":{{with index .State "Health"}}{{json .Status}}{{else}}null{{end}}}"#;
    Ok(docker_command(
        endpoint,
        &format!(
            "container inspect --format {} {}",
            shell_quote(template)?,
            ids.join(" ")
        ),
    ))
}

fn parse_release_observation(ids: &[String], output: &str) -> Result<ReleaseObservation, String> {
    let mut seen = HashSet::new();
    let mut containers = Vec::new();
    for line in output.lines() {
        let row: Value = serde_json::from_str(line).map_err(|_| "远端容器状态无法解析。")?;
        let id = row
            .get("Id")
            .and_then(Value::as_str)
            .ok_or("容器 ID 缺失。")?;
        if !ids.iter().any(|expected| expected == id) || !seen.insert(id.to_owned()) {
            return Err("远端容器检查结果不完整或重复。".into());
        }
        let service = row
            .get("Service")
            .and_then(Value::as_str)
            .ok_or("Compose 服务标签缺失。")?;
        let status = row
            .get("Status")
            .and_then(Value::as_str)
            .ok_or("容器运行状态缺失。")?;
        let health = row.get("Health").and_then(Value::as_str);
        let image_id = row
            .get("Image")
            .and_then(Value::as_str)
            .ok_or("容器镜像 ID 缺失。")?;
        let config_image = row
            .get("ConfigImage")
            .and_then(Value::as_str)
            .ok_or("容器镜像引用缺失。")?;
        if !valid_compose_service(service)
            || !matches!(
                status,
                "running" | "exited" | "created" | "restarting" | "paused" | "dead"
            )
            || health.is_some_and(|value| !matches!(value, "healthy" | "unhealthy" | "starting"))
            || image_id.len() != 71
            || !image_id.starts_with("sha256:")
            || !image_id[7..].bytes().all(|byte| byte.is_ascii_hexdigit())
            || config_image.is_empty()
            || config_image.len() > 512
            || config_image.chars().any(char::is_control)
        {
            return Err("Compose 服务状态无法核实。".into());
        }
        containers.push(ObservedContainer {
            service: service.into(),
            status: status.into(),
            health: health.map(str::to_owned),
            image_id: image_id.to_owned(),
            config_image: config_image.to_owned(),
        });
    }
    if seen.len() != ids.len() {
        return Err("远端容器检查结果不完整或重复。".into());
    }
    containers.sort_by(|left, right| left.service.cmp(&right.service));
    Ok(ReleaseObservation { containers })
}

async fn observe_project_on(
    material: ConnectionMaterial,
    project_id: i64,
    expected_endpoint: Option<&str>,
    expected_daemon_id: Option<&str>,
) -> Result<ReleaseObservation, String> {
    if project_id <= 0 {
        return Err("项目 ID 无效。".into());
    }
    let handle = tokio::time::timeout(Duration::from_secs(20), trusted_handle(material))
        .await
        .map_err(|_| "只读核实连接超时。")??;
    let endpoint = match (expected_endpoint, expected_daemon_id) {
        (Some(endpoint), Some(daemon_id)) => {
            checked_release_endpoint(
                &handle,
                ReleaseDockerIdentity {
                    endpoint,
                    daemon_id,
                },
            )
            .await?
        }
        (None, None) => local_docker_endpoint(&handle).await?,
        _ => return Err("发布任务 Docker 身份快照不完整，核实已阻断。".into()),
    };
    let list = docker_command(
        &endpoint,
        &format!(
            "ps -a --no-trunc --filter label=com.docker.compose.project=dp{project_id} --format '{{{{.ID}}}}'"
        ),
    );
    let output = tokio::time::timeout(
        Duration::from_secs(15),
        command_limited(&handle, &list, 8192),
    )
    .await
    .map_err(|_| "远端容器列表核实超时。")??;
    let ids: Vec<String> = output.lines().map(str::to_owned).collect();
    if ids.len() > 100
        || ids
            .iter()
            .any(|id| id.len() != 64 || validate_container_id(id).is_err())
    {
        return Err("远端项目容器列表无效。".into());
    }
    if ids.is_empty() {
        return Ok(ReleaseObservation {
            containers: Vec::new(),
        });
    }
    let inspect = deployment_inspect_command(&endpoint, &ids)?;
    let rows = tokio::time::timeout(
        Duration::from_secs(15),
        command_limited(&handle, &inspect, 64 * 1024),
    )
    .await
    .map_err(|_| "远端容器元数据核实超时。")??;
    parse_release_observation(&ids, &rows)
}

pub fn observe_release_project(task: &TaskSnapshot) -> Result<ReleaseObservation, String> {
    let material = material_for(&task.host)?;
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| "无法初始化只读核实环境。".to_owned())?
        .block_on(observe_project_on(
            material,
            task.project_id,
            task.docker_endpoint.as_deref(),
            task.docker_daemon_id.as_deref(),
        ))
}

#[cfg(test)]
pub(crate) fn assert_empty_project_namespace(task: &TaskSnapshot) -> Result<(), String> {
    if task.project_id <= 1_000_000_000 {
        return Err("隔离测试项目 ID 不够独立。".into());
    }
    let endpoint = task
        .docker_endpoint
        .as_deref()
        .ok_or("测试缺少 Docker endpoint。")?;
    let daemon_id = task
        .docker_daemon_id
        .as_deref()
        .ok_or("测试缺少 Docker daemon ID。")?;
    let material = material_for(&task.host)?;
    tokio::runtime::Builder::new_current_thread().enable_all().build()
        .map_err(|_| "无法初始化隔离资源检查。")?
        .block_on(async {
            let handle = tokio::time::timeout(Duration::from_secs(20), trusted_handle(material))
                .await.map_err(|_| "隔离资源检查连接超时。")??;
            let endpoint = checked_release_endpoint(&handle,
                ReleaseDockerIdentity { endpoint, daemon_id }).await?;
            for command in [
                format!("network ls --filter label=com.docker.compose.project=dp{} --format '{{{{.ID}}}}'", task.project_id),
                format!("volume ls --filter label=com.docker.compose.project=dp{} --format '{{{{.Name}}}}'", task.project_id),
            ] {
                if !archive_command(&handle, &docker_command(&endpoint, &command)).await?.trim().is_empty() {
                    return Err("远端已存在同名 Compose 网络或卷，拒绝隔离测试。".into());
                }
            }
            let _ = handle.disconnect(Disconnect::ByApplication, "", "en").await;
            Ok(())
        })
}

async fn verify_snapshot_files(
    handle: &client::Handle<HostKeyChecker>,
    directory: &str,
    snapshot: &crate::release_materials::RestoredMaterials,
) -> Result<(), String> {
    for file in &snapshot.compose_files {
        validated_release_file(&file.path)?;
        let path = shell_quote(&format!("{directory}/{}", file.path))?;
        let output = archive_command_with_timeout(
            handle,
            &format!("sha256sum -- {path}"),
            Duration::from_secs(30),
        )
        .await?;
        if verified_sha256(&output)? != format!("{:x}", Sha256::digest(&file.bytes)) {
            return Err("远端 Compose 文件已变化或丢失。".into());
        }
    }
    let env_path = shell_quote(&format!("{directory}/.env"))?;
    let output = archive_command_with_timeout(
        handle,
        &format!("sha256sum -- {env_path}"),
        Duration::from_secs(30),
    )
    .await?;
    if verified_sha256(&output)? != format!("{:x}", Sha256::digest(&snapshot.env_file)) {
        return Err("远端环境快照已变化或丢失。".into());
    }
    Ok(())
}

pub fn verify_staged_materials(
    task: &TaskSnapshot,
    snapshot: &crate::release_materials::RestoredMaterials,
) -> Result<(), String> {
    let endpoint = task
        .docker_endpoint
        .as_deref()
        .ok_or("任务缺少 Docker endpoint 身份。")?;
    let daemon_id = task
        .docker_daemon_id
        .as_deref()
        .ok_or("任务缺少 Docker daemon 身份。")?;
    if snapshot.compose_files.is_empty() || snapshot.compose_files.len() > 16 {
        return Err("部署文件快照无效。".into());
    }
    let material = material_for(&task.host)?;
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| "无法初始化部署文件核实环境。")?
        .block_on(async {
            let handle = tokio::time::timeout(Duration::from_secs(20), trusted_handle(material))
                .await
                .map_err(|_| "部署文件核实连接超时。")??;
            checked_release_endpoint(
                &handle,
                ReleaseDockerIdentity {
                    endpoint,
                    daemon_id,
                },
            )
            .await?;
            let home = archive_command(&handle, "sh -c 'printf %s \"$HOME\"'").await?;
            let directory = validated_release_directory(&home, task.project_id, &task.id)?;
            verify_snapshot_files(&handle, &directory, snapshot).await?;
            let _ = handle.disconnect(Disconnect::ByApplication, "", "en").await;
            Ok(())
        })
}

pub fn verify_start_completed(task: &TaskSnapshot) -> Result<(), String> {
    let endpoint = task
        .docker_endpoint
        .as_deref()
        .ok_or("任务缺少 Docker endpoint 身份。")?;
    let daemon_id = task
        .docker_daemon_id
        .as_deref()
        .ok_or("任务缺少 Docker daemon 身份。")?;
    let material = material_for(&task.host)?;
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| "无法初始化启动完成核实环境。".to_owned())?
        .block_on(async {
            let handle = tokio::time::timeout(Duration::from_secs(20), trusted_handle(material))
                .await
                .map_err(|_| "启动完成标记连接超时。")??;
            checked_release_endpoint(
                &handle,
                ReleaseDockerIdentity {
                    endpoint,
                    daemon_id,
                },
            )
            .await?;
            let home = archive_command(&handle, "sh -c 'printf %s \"$HOME\"'").await?;
            let directory = validated_release_directory(&home, task.project_id, &task.id)?;
            let marker = shell_quote(&format!("{directory}/.compose-start-complete"))?;
            let directory = shell_quote(&directory)?;
            let command = format!(
                "test -d {directory} && test ! -L {directory} && \
                 test \"$(stat -c %a -- {directory})\" = 700 && \
                 test -f {marker} && test ! -L {marker} && \
                 test \"$(stat -c %a -- {marker})\" = 600 && \
                 test \"$(stat -c %u -- {marker})\" = \"$(id -u)\" && cat -- {marker}"
            );
            let content = archive_command(&handle, &command)
                .await
                .map_err(|error| format!("远端 Compose 启动完成标记不可核实：{error}"))?;
            let expected = format!(
                "{}:{}",
                task.project_id,
                uuid::Uuid::parse_str(&task.id).map_err(|_| "发布任务 ID 无效。")?
            );
            if content.trim() != expected {
                return Err("远端 Compose 启动完成标记缺失或不匹配，仍保持待核实。".into());
            }
            let _ = handle.disconnect(Disconnect::ByApplication, "", "en").await;
            Ok(())
        })
}

fn parse_compose_hashes(text: &str) -> Result<HashMap<String, String>, String> {
    let mut hashes = HashMap::new();
    for line in text.lines() {
        let (service, hash) = line.split_once(' ').ok_or("Compose 配置哈希格式无效。")?;
        if !valid_compose_service(service)
            || !valid_config_hash(hash)
            || hashes.insert(service.to_owned(), hash.to_owned()).is_some()
        {
            return Err("Compose 配置哈希无效或重复。".into());
        }
    }
    if hashes.is_empty() || hashes.len() > 100 {
        return Err("Compose 配置哈希集合无效。".into());
    }
    Ok(hashes)
}

fn valid_config_hash(hash: &str) -> bool {
    hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn verify_container_config_hashes(
    ids: &[String],
    rows: &str,
    expected: &HashMap<String, String>,
) -> Result<(), String> {
    let mut seen_ids = HashSet::new();
    let mut seen_services = HashSet::new();
    for line in rows.lines() {
        let row: Value = serde_json::from_str(line).map_err(|_| "容器配置哈希无法解析。")?;
        let id = row
            .get("Id")
            .and_then(Value::as_str)
            .ok_or("容器标识缺失。")?;
        let service = row
            .get("Service")
            .and_then(Value::as_str)
            .ok_or("容器服务标签缺失。")?;
        let hash = row
            .get("ConfigHash")
            .and_then(Value::as_str)
            .ok_or("容器缺少 Compose 配置哈希。")?;
        if !ids.iter().any(|item| item == id)
            || !seen_ids.insert(id.to_owned())
            || !seen_services.insert(service.to_owned())
            || !valid_config_hash(hash)
            || expected.get(service).map(String::as_str) != Some(hash)
        {
            return Err("容器配置与历史 Compose 快照不一致。".into());
        }
    }
    if seen_ids.len() != ids.len() || seen_services.len() != expected.len() {
        return Err("历史 Compose 服务集合与当前容器不一致。".into());
    }
    Ok(())
}

pub fn verify_running_config_hashes(
    task: &TaskSnapshot,
    snapshot: &crate::release_materials::RestoredMaterials,
    images: &[crate::release_store::HistoricalImage],
) -> Result<ReleaseObservation, String> {
    if snapshot.compose_files.is_empty() || snapshot.compose_files.len() > 16 {
        return Err("部署文件快照无效。".into());
    }
    let endpoint = task
        .docker_endpoint
        .as_deref()
        .ok_or("历史版本缺少 Docker endpoint。")?;
    let daemon = task
        .docker_daemon_id
        .as_deref()
        .ok_or("历史版本缺少 Docker daemon。")?;
    let built = images
        .iter()
        .map(|image| StackService {
            service: image.service.clone(),
            reference: image.reference.clone(),
            image_ids: image.image_ids.clone(),
        })
        .collect::<Vec<_>>();
    let material = material_for(&task.host)?;
    tokio::runtime::Builder::new_current_thread().enable_all().build()
        .map_err(|_| "无法初始化配置哈希核实环境。".to_owned())?
        .block_on(async {
            let handle = tokio::time::timeout(Duration::from_secs(20), trusted_handle(material))
                .await.map_err(|_| "配置哈希核实连接超时。")??;
            let checked = checked_release_endpoint(&handle, ReleaseDockerIdentity { endpoint, daemon_id: daemon }).await?;
            let home = archive_command(&handle, "sh -c 'printf %s \"$HOME\"'").await?;
            let directory = validated_release_directory(&home, task.project_id, &task.id)?;
            verify_snapshot_files(&handle, &directory, snapshot).await?;
            let staged = StagedDeployment {
                env_path: format!("{directory}/.env"),
                compose_files: snapshot.compose_files.iter().map(|file| file.path.clone()).collect(),
                directory,
            };
            let command = format!("{} config --hash '*'", stack_command(&checked, &staged, task.project_id, &built)?);
            let hashes = parse_compose_hashes(&tokio::time::timeout(Duration::from_secs(30),
                command_limited(&handle, &command, 16 * 1024)).await.map_err(|_| "历史 Compose 哈希计算超时。")??)?;
            let list = docker_command(&checked, &format!(
                "ps -a --no-trunc --filter label=com.docker.compose.project=dp{} --format '{{{{.ID}}}}'", task.project_id));
            let output = tokio::time::timeout(Duration::from_secs(15), command_limited(&handle, &list, 8192))
                .await.map_err(|_| "当前容器列表核实超时。")??;
            let ids = output.lines().map(str::to_owned).collect::<Vec<_>>();
            if ids.is_empty() || ids.len() != hashes.len() || ids.len() > 100
                || ids.iter().any(|id| validate_container_id(id).is_err())
            {
                return Err("当前容器与历史 Compose 服务数量不一致。".into());
            }
            let template = r#"{"Id":{{json .Id}},"Service":{{json (index .Config.Labels "com.docker.compose.service")}},"ConfigHash":{{json (index .Config.Labels "com.docker.compose.config-hash")}},"Image":{{json .Image}},"ConfigImage":{{json .Config.Image}},"Status":{{json .State.Status}},"Health":{{with index .State "Health"}}{{json .Status}}{{else}}null{{end}}}"#;
            let inspect = docker_command(&checked, &format!("container inspect --format {} {}", shell_quote(template)?, ids.join(" ")));
            let rows = tokio::time::timeout(Duration::from_secs(15), command_limited(&handle, &inspect, 32 * 1024))
                .await.map_err(|_| "当前容器配置哈希核实超时。")??;
            verify_container_config_hashes(&ids, &rows, &hashes)?;
            let observed = parse_release_observation(&ids, &rows)?;
            verify_snapshot_files(&handle, &staged.directory, snapshot).await?;
            Ok(observed)
        })
}

fn historical_image_matches(
    text: &str,
    image: &crate::release_store::HistoricalImage,
) -> Result<bool, String> {
    let row: Value = serde_json::from_str(text).map_err(|_| "远端历史镜像元数据无效。")?;
    let id = row
        .get("Id")
        .and_then(Value::as_str)
        .ok_or("远端镜像 ID 缺失。")?;
    let os = row
        .get("Os")
        .and_then(Value::as_str)
        .ok_or("远端镜像系统信息缺失。")?;
    let arch = row
        .get("Architecture")
        .and_then(Value::as_str)
        .ok_or("远端镜像架构信息缺失。")?;
    Ok(image.image_ids.iter().any(|expected| expected == id)
        && format!("{os}/{arch}") == image.platform)
}

async fn observe_historical_images_on(
    material: ConnectionMaterial,
    identity: ReleaseDockerIdentity<'_>,
    images: &[crate::release_store::HistoricalImage],
) -> Result<Vec<(String, HistoricalImageState)>, String> {
    if images.is_empty() || images.len() > 16 {
        return Err("历史镜像计划无效。".into());
    }
    for image in images {
        let (name, tag) = image
            .reference
            .rsplit_once(':')
            .ok_or("历史镜像引用无效。")?;
        if !crate::project_store::valid_image_name(name)
            || tag.is_empty()
            || tag.len() > 128
            || !tag
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-'))
            || !matches!(image.platform.as_str(), "linux/amd64" | "linux/arm64")
            || image.service.is_empty()
            || image.service.len() > 128
            || image.service.chars().any(char::is_control)
        {
            return Err("历史镜像计划无效。".into());
        }
        validate_deployment_image_ids(&image.image_ids)?;
    }
    let handle = tokio::time::timeout(Duration::from_secs(20), trusted_handle(material))
        .await
        .map_err(|_| "历史镜像核实连接超时。")??;
    let endpoint = checked_release_endpoint(&handle, identity).await?;
    let mut results = Vec::with_capacity(images.len());
    for image in images {
        let reference = shell_quote(&image.reference)?;
        let list = docker_command(
            &endpoint,
            &format!(
                "image ls --filter reference={reference} --format '{{{{.Repository}}}}:{{{{.Tag}}}}'"
            ),
        );
        let output = tokio::time::timeout(
            Duration::from_secs(15),
            command_limited(&handle, &list, 8192),
        )
        .await
        .map_err(|_| "历史镜像列表核实超时。")??;
        if !output.lines().any(|line| line == image.reference) {
            results.push((image.service.clone(), HistoricalImageState::Missing));
            continue;
        }
        let format = concat!(
            "image inspect --format '",
            r#"{"Id":{{json .Id}},"Os":{{json .Os}},"Architecture":{{json .Architecture}}}"#,
            "' "
        );
        let inspect = docker_command(&endpoint, &format!("{format}{reference}"));
        let output = tokio::time::timeout(
            Duration::from_secs(15),
            command_limited(&handle, &inspect, 8192),
        )
        .await
        .map_err(|_| "历史镜像元数据核实超时。")??;
        let state = if historical_image_matches(&output, image)? {
            HistoricalImageState::Matching
        } else {
            HistoricalImageState::Changed
        };
        results.push((image.service.clone(), state));
    }
    let _ = handle.disconnect(Disconnect::ByApplication, "", "en").await;
    Ok(results)
}

pub fn observe_historical_images(
    task: &TaskSnapshot,
    images: &[crate::release_store::HistoricalImage],
) -> Result<Vec<(String, bool)>, String> {
    Ok(historical_image_states(task, images)?
        .into_iter()
        .map(|(service, state)| (service, state == HistoricalImageState::Matching))
        .collect())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HistoricalImageState {
    Matching,
    Missing,
    Changed,
}

pub fn historical_image_states(
    task: &TaskSnapshot,
    images: &[crate::release_store::HistoricalImage],
) -> Result<Vec<(String, HistoricalImageState)>, String> {
    let endpoint = task
        .docker_endpoint
        .as_deref()
        .ok_or("旧任务缺少 Docker endpoint 快照。")?;
    let daemon_id = task
        .docker_daemon_id
        .as_deref()
        .ok_or("旧任务缺少 Docker daemon 身份快照。")?;
    let material = material_for(&task.host)?;
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| "无法初始化历史镜像核实环境。")?
        .block_on(observe_historical_images_on(
            material,
            ReleaseDockerIdentity {
                endpoint,
                daemon_id,
            },
            images,
        ))
}

fn inspect_deployment_state(
    output: &str,
    ids: &[String],
    reference: &str,
    image_ids: &[String],
) -> Result<Option<DeploymentState>, String> {
    let mut seen = HashSet::new();
    let mut pending = false;
    let mut unverified = false;
    for line in output.lines() {
        let row: Value = serde_json::from_str(line).map_err(|_| "容器状态无法解析。")?;
        let id = row
            .get("Id")
            .and_then(Value::as_str)
            .ok_or("容器 ID 缺失。")?;
        if !ids.iter().any(|expected| id == expected) || !seen.insert(id.to_owned()) {
            return Err("容器检查结果不完整或重复。".into());
        }
        let actual_image = row
            .get("Image")
            .and_then(Value::as_str)
            .ok_or("容器镜像 ID 缺失。")?;
        if actual_image.len() != 71
            || !actual_image.starts_with("sha256:")
            || !actual_image[7..]
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
            || (!image_ids.is_empty()
                && !row
                    .get("Image")
                    .and_then(Value::as_str)
                    .is_some_and(|id| image_ids.iter().any(|expected| expected == id)))
            || row.get("ConfigImage").and_then(Value::as_str) != Some(reference)
        {
            return Ok(Some(DeploymentState::Failed));
        }
        if row.get("Status").and_then(Value::as_str) != Some("running") {
            return Ok(Some(DeploymentState::Failed));
        }
        match row.get("Health").and_then(Value::as_str) {
            Some("healthy") => {}
            Some("starting") => pending = true,
            Some("unhealthy") => return Ok(Some(DeploymentState::Failed)),
            None if row.get("Health").is_none_or(Value::is_null) => unverified = true,
            _ => return Err("容器健康状态无法核实。".into()),
        }
    }
    if seen.len() != ids.len() {
        return Err("容器检查结果不完整或重复。".into());
    }
    if pending {
        Ok(None)
    } else if unverified {
        Ok(Some(DeploymentState::RunningUnverified))
    } else {
        Ok(Some(DeploymentState::Healthy))
    }
}

async fn inspect_target_containers(
    handle: &client::Handle<HostKeyChecker>,
    endpoint: &str,
    staged: &StagedDeployment,
    project_id: i64,
    service: &str,
    reference: &str,
    image_ids: &[String],
) -> Result<Option<DeploymentState>, String> {
    let prefix = compose_config_command(endpoint, staged, project_id, reference)?;
    let list = format!("{prefix} ps -a -q --no-trunc {}", shell_quote(service)?);
    let output = tokio::time::timeout(
        Duration::from_secs(15),
        command_limited(handle, &list, 8192),
    )
    .await
    .map_err(|_| "容器列表检查超时。")??;
    let ids: Vec<String> = output.lines().map(str::to_owned).collect();
    if ids.is_empty() {
        return Ok(None);
    }
    if ids.len() != 1 {
        return Ok(Some(DeploymentState::Failed));
    }
    let inspect = deployment_inspect_command(endpoint, &ids)?;
    let output = tokio::time::timeout(
        Duration::from_secs(15),
        command_limited(handle, &inspect, 64 * 1024),
    )
    .await
    .map_err(|_| "容器检查超时。")??;
    inspect_deployment_state(&output, &ids, reference, image_ids)
}

async fn start_staged_compose_on(
    material: ConnectionMaterial,
    identity: ReleaseDockerIdentity<'_>,
    staged: &StagedDeployment,
    project_id: i64,
    target: &StackService,
    cancel: &AtomicBool,
) -> Result<(), String> {
    if !valid_compose_service(&target.service) {
        return Err("部署服务无效。".into());
    }
    validate_deployment_image_ids(&target.image_ids)?;
    if cancel.load(Ordering::Relaxed) {
        return Err("部署已取消，尚未启动容器。".into());
    }
    let handle = tokio::time::timeout(Duration::from_secs(20), trusted_handle(material))
        .await
        .map_err(|_| "部署连接超时。")??;
    let endpoint = checked_release_endpoint(&handle, identity).await?;
    let prefix = compose_config_command(&endpoint, staged, project_id, &target.reference)?;
    let inspect = docker_command(
        &endpoint,
        &format!(
            "image inspect --format '{{{{.Id}}}}' {}",
            shell_quote(&target.reference)?
        ),
    );
    let remote_id = archive_command(&handle, &inspect).await?;
    if !target.image_ids.iter().any(|id| id == remote_id.trim()) {
        return Err("远端镜像 tag 已变化，禁止启动。".into());
    }
    if cancel.load(Ordering::Relaxed) {
        return Err("部署已取消，尚未启动容器。".into());
    }
    let up = format!(
        "{prefix} up -d --no-build --pull never --no-deps {}",
        shell_quote(&target.service)?
    );
    tokio::select! {
        _ = wait_cancel(cancel) => Err("远端启动已取消，执行结果待核实。".into()),
        result = tokio::time::timeout(Duration::from_secs(5 * 60), command_limited(&handle, &up, 64 * 1024)) =>
            result.map_err(|_| "远端启动超时，执行结果待核实。".to_owned())?
                .map(|_| ())
                .map_err(|_| "远端启动未确认成功，需核实容器状态。".into()),
    }
}

async fn check_staged_compose_on(
    material: ConnectionMaterial,
    identity: ReleaseDockerIdentity<'_>,
    staged: &StagedDeployment,
    project_id: i64,
    target: &StackService,
    cancel: &AtomicBool,
) -> Result<DeploymentState, String> {
    if !valid_compose_service(&target.service) {
        return Err("部署服务无效。".into());
    }
    let handle = tokio::time::timeout(Duration::from_secs(20), trusted_handle(material))
        .await
        .map_err(|_| "状态核实连接超时。")??;
    let endpoint = checked_release_endpoint(&handle, identity).await?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
    loop {
        let state = inspect_target_containers(
            &handle,
            &endpoint,
            staged,
            project_id,
            &target.service,
            &target.reference,
            &target.image_ids,
        )
        .await?;
        if let Some(state) = state {
            return Ok(state);
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(DeploymentState::Failed);
        }
        tokio::select! {
            _ = wait_cancel(cancel) => return Err("健康检查已取消，状态待核实。".into()),
            _ = tokio::time::sleep(Duration::from_secs(3)) => {}
        }
    }
}

#[allow(dead_code)] // The GUI release flow remains disabled until history and recovery are wired up.
pub fn start_staged_compose(
    expected_host: Host,
    identity: ReleaseDockerIdentity<'_>,
    staged: &StagedDeployment,
    project_id: i64,
    target: &StackService,
    cancel: Arc<AtomicBool>,
) -> Result<(), String> {
    let material = material_for(&expected_host)?;
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| "无法初始化远端部署环境。".to_owned())?
        .block_on(start_staged_compose_on(
            material, identity, staged, project_id, target, &cancel,
        ))
}

#[allow(dead_code)] // Explicit read-only check, including after an uncertain start result.
pub fn check_staged_compose(
    expected_host: Host,
    identity: ReleaseDockerIdentity<'_>,
    staged: &StagedDeployment,
    project_id: i64,
    target: &StackService,
    cancel: Arc<AtomicBool>,
) -> Result<DeploymentState, String> {
    let material = material_for(&expected_host)?;
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| "无法初始化状态核实环境。".to_owned())?
        .block_on(check_staged_compose_on(
            material, identity, staged, project_id, target, &cancel,
        ))
}

struct StackStartPolicy<'a> {
    expected_images: &'a [StackService],
    pull_missing: bool,
    remove_orphans: bool,
    record_completion: bool,
}

fn stack_up_command(
    endpoint: &str,
    staged: &StagedDeployment,
    project_id: i64,
    built: &[StackService],
    policy: &StackStartPolicy<'_>,
) -> Result<String, String> {
    let pull = if policy.pull_missing {
        "missing"
    } else {
        "never"
    };
    let orphans = if policy.remove_orphans {
        " --remove-orphans"
    } else {
        ""
    };
    Ok(format!(
        "{} up -d --no-build --pull {pull}{orphans}",
        stack_command(endpoint, staged, project_id, built)?
    ))
}

fn stack_start_command(
    endpoint: &str,
    staged: &StagedDeployment,
    project_id: i64,
    built: &[StackService],
    policy: &StackStartPolicy<'_>,
) -> Result<String, String> {
    let up = stack_up_command(endpoint, staged, project_id, built, policy)?;
    if !policy.record_completion {
        return Ok(up);
    }
    let task_id = staged
        .directory
        .rsplit('/')
        .next()
        .ok_or("远端发布目录缺少任务 ID。")?;
    let task_id = uuid::Uuid::parse_str(task_id).map_err(|_| "远端发布目录任务 ID 无效。")?;
    let marker = shell_quote(&format!("{}/.compose-start-complete", staged.directory))?;
    let temporary = shell_quote(&format!("{}/.compose-start-complete.tmp", staged.directory))?;
    let directory = shell_quote(&staged.directory)?;
    let content = shell_quote(&format!("{project_id}:{task_id}"))?;
    let completion =
        format!("(umask 077; printf %s {content} > {temporary} && mv -- {temporary} {marker})");
    #[cfg(test)]
    let completion = if project_id > 1_000_000_000
        && TEST_FAIL_NEXT_STACK_COMPLETION
            .compare_exchange(project_id, 0, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    {
        "false".to_owned()
    } else {
        completion
    };
    Ok(format!(
        "test -d {directory} && test ! -L {directory} && \
         test ! -e {marker} && test ! -L {marker} && \
         test ! -e {temporary} && test ! -L {temporary} && \
         {up} && {completion}"
    ))
}

async fn start_stack_on(
    material: ConnectionMaterial,
    identity: ReleaseDockerIdentity<'_>,
    staged: &StagedDeployment,
    project_id: i64,
    built: &[StackService],
    policy: StackStartPolicy<'_>,
    cancel: &AtomicBool,
) -> Result<(), String> {
    if cancel.load(Ordering::Relaxed) {
        return Err("整栈启动已取消。".into());
    }
    let handle = tokio::time::timeout(Duration::from_secs(20), trusted_handle(material))
        .await
        .map_err(|_| "整栈启动连接超时。")??;
    let endpoint = checked_release_endpoint(&handle, identity).await?;
    for image in policy.expected_images {
        validate_deployment_image_ids(&image.image_ids)?;
        let inspect = docker_command(
            &endpoint,
            &format!(
                "image inspect --format '{{{{.Id}}}}' {}",
                shell_quote(&image.reference)?
            ),
        );
        let remote_id = archive_command(&handle, &inspect).await?;
        if !image.image_ids.iter().any(|id| id == remote_id.trim()) {
            return Err("远端构建镜像 tag 已变化，禁止启动。".into());
        }
    }
    if cancel.load(Ordering::Relaxed) {
        return Err("整栈启动已取消。".into());
    }
    let command = stack_start_command(&endpoint, staged, project_id, built, &policy)?;
    tokio::select! {
        _ = wait_cancel(cancel) => Err("整栈启动已取消，结果待核实。".into()),
        result = tokio::time::timeout(Duration::from_secs(10 * 60), command_limited(&handle, &command, 64 * 1024)) =>
            result.map_err(|_| "整栈启动超时，结果待核实。".to_owned())?
                .map(|_| ())
                .map_err(|error| format!("整栈启动未确认成功：{error} 远端状态待核实。")),
    }
}

pub fn start_staged_stack(
    host: Host,
    identity: ReleaseDockerIdentity<'_>,
    staged: &StagedDeployment,
    project_id: i64,
    built: &[StackService],
    cancel: Arc<AtomicBool>,
) -> Result<(), String> {
    let material = material_for(&host)?;
    let result = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| "无法初始化整栈部署环境。".to_owned())?
        .block_on(start_stack_on(
            material,
            identity,
            staged,
            project_id,
            built,
            StackStartPolicy {
                expected_images: built,
                pull_missing: true,
                remove_orphans: false,
                record_completion: true,
            },
            &cancel,
        ));
    #[cfg(test)]
    if result.is_ok()
        && project_id > 1_000_000_000
        && TEST_DISCONNECT_NEXT_STACK_START
            .compare_exchange(project_id, 0, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    {
        return Err("测试注入：整栈启动响应丢失，远端结果待核实。".into());
    }
    result
}

pub fn start_staged_rollback(
    host: Host,
    identity: ReleaseDockerIdentity<'_>,
    staged: &StagedDeployment,
    project_id: i64,
    built: &[StackService],
    expected_images: &[StackService],
    cancel: Arc<AtomicBool>,
) -> Result<(), String> {
    if expected_images.is_empty() || expected_images.len() > 100 {
        return Err("回滚服务镜像列表无效。".into());
    }
    let material = material_for(&host)?;
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| "无法初始化回滚环境。")?
        .block_on(start_stack_on(
            material,
            identity,
            staged,
            project_id,
            built,
            StackStartPolicy {
                expected_images,
                pull_missing: false,
                remove_orphans: true,
                record_completion: false,
            },
            &cancel,
        ))
}

async fn check_stack_on(
    material: ConnectionMaterial,
    identity: ReleaseDockerIdentity<'_>,
    staged: &StagedDeployment,
    project_id: i64,
    built: &[StackService],
    services: &[StackService],
    cancel: &AtomicBool,
) -> Result<DeploymentState, String> {
    if services.is_empty() || services.len() > 100 {
        return Err("整栈服务列表无效。".into());
    }
    let handle = tokio::time::timeout(Duration::from_secs(20), trusted_handle(material))
        .await
        .map_err(|_| "整栈核实连接超时。")??;
    let endpoint = checked_release_endpoint(&handle, identity).await?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
    loop {
        let mut states = Vec::with_capacity(services.len());
        for service in services {
            let prefix = stack_command(&endpoint, staged, project_id, built)?;
            let list = format!(
                "{prefix} ps -a -q --no-trunc {}",
                shell_quote(&service.service)?
            );
            let output = tokio::time::timeout(
                Duration::from_secs(15),
                command_limited(&handle, &list, 8192),
            )
            .await
            .map_err(|_| "整栈容器列表检查超时。")??;
            let ids: Vec<String> = output.lines().map(str::to_owned).collect();
            if ids.is_empty() {
                states.push(None);
                continue;
            }
            if ids.len() != 1 {
                return Ok(DeploymentState::Failed);
            }
            let inspect = deployment_inspect_command(&endpoint, &ids)?;
            let rows = tokio::time::timeout(
                Duration::from_secs(15),
                command_limited(&handle, &inspect, 64 * 1024),
            )
            .await
            .map_err(|_| "整栈容器检查超时。")??;
            states.push(inspect_deployment_state(
                &rows,
                &ids,
                &service.reference,
                &service.image_ids,
            )?);
        }
        if let Some(state) = aggregate_stack_states(&states) {
            return Ok(state);
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(DeploymentState::Failed);
        }
        tokio::select! {
            _ = wait_cancel(cancel) => return Err("整栈健康核实已取消，状态待核实。".into()),
            _ = tokio::time::sleep(Duration::from_secs(3)) => {}
        }
    }
}

fn aggregate_stack_states(states: &[Option<DeploymentState>]) -> Option<DeploymentState> {
    if states.contains(&Some(DeploymentState::Failed)) {
        return Some(DeploymentState::Failed);
    }
    if states.contains(&None) {
        return None;
    }
    if states.contains(&Some(DeploymentState::RunningUnverified)) {
        Some(DeploymentState::RunningUnverified)
    } else {
        Some(DeploymentState::Healthy)
    }
}

pub fn check_staged_stack(
    host: Host,
    identity: ReleaseDockerIdentity<'_>,
    staged: &StagedDeployment,
    project_id: i64,
    built: &[StackService],
    services: &[StackService],
    cancel: Arc<AtomicBool>,
) -> Result<DeploymentState, String> {
    #[cfg(test)]
    if project_id > 1_000_000_000
        && TEST_DISCONNECT_NEXT_STACK_CHECK
            .compare_exchange(project_id, 0, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    {
        return Err("测试注入：健康核实 SSH 连接中断，结果待核实。".into());
    }
    let material = material_for(&host)?;
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| "无法初始化整栈核实环境。".to_owned())?
        .block_on(check_stack_on(
            material, identity, staged, project_id, built, services, &cancel,
        ))
}

#[cfg(test)]
static TEST_DISCONNECT_NEXT_STACK_CHECK: std::sync::atomic::AtomicI64 =
    std::sync::atomic::AtomicI64::new(0);

#[cfg(test)]
static TEST_DISCONNECT_NEXT_STACK_START: std::sync::atomic::AtomicI64 =
    std::sync::atomic::AtomicI64::new(0);

#[cfg(test)]
static TEST_FAIL_NEXT_STACK_COMPLETION: std::sync::atomic::AtomicI64 =
    std::sync::atomic::AtomicI64::new(0);

#[cfg(test)]
pub(crate) fn inject_next_stack_completion_failure(project_id: i64) {
    assert!(project_id > 1_000_000_000);
    assert_eq!(
        TEST_FAIL_NEXT_STACK_COMPLETION.swap(project_id, Ordering::SeqCst),
        0
    );
}

#[cfg(test)]
pub(crate) fn inject_next_stack_start_ack_loss(project_id: i64) {
    assert!(project_id > 1_000_000_000);
    assert_eq!(
        TEST_DISCONNECT_NEXT_STACK_START.swap(project_id, Ordering::SeqCst),
        0
    );
}

#[cfg(test)]
pub(crate) fn inject_next_stack_check_disconnect(project_id: i64) {
    assert!(project_id > 1_000_000_000);
    assert_eq!(
        TEST_DISCONNECT_NEXT_STACK_CHECK.swap(project_id, Ordering::SeqCst),
        0
    );
}

#[cfg(test)]
pub(crate) fn remove_isolated_target_image_tags(
    source: &TaskSnapshot,
    target: &TaskSnapshot,
    images: &[crate::release_store::HistoricalImage],
) -> Result<(), String> {
    if source.project_id <= 1_000_000_000
        || source.project_id != target.project_id
        || images.len() != 2
        || source.outcome != "complete"
        || target.outcome != "complete"
        || source.docker_daemon_id != target.docker_daemon_id
    {
        return Err("仅允许隔离测试版本的两份自有镜像 tag。".into());
    }
    let root = std::path::PathBuf::from(std::env::var_os("APPDATA").ok_or("APPDATA 缺失。")?)
        .canonicalize()
        .map_err(|_| "隔离目录缺失。")?;
    if root.parent()
        != Some(
            std::env::temp_dir()
                .canonicalize()
                .map_err(|_| "临时目录缺失。")?
                .as_path(),
        )
        || !root.file_name().is_some_and(|name| {
            name.to_string_lossy()
                .starts_with("devingpanel-v20-series-")
        })
        || root
            .join("devingPanel")
            .canonicalize()
            .ok()
            .and_then(|data| data.parent().map(|parent| parent == root))
            != Some(true)
    {
        return Err("必须使用隔离 APPDATA。".into());
    }
    let project = crate::project_store::effective(source.project_id)?;
    if !project.name.starts_with("stack-series-")
        || images
            .iter()
            .any(|image| !image.reference.starts_with(&format!("{}-", project.name)))
    {
        return Err("测试镜像引用不属于隔离项目。".into());
    }
    let running = observe_release_project(source)?;
    if running.containers.len() != 4
        || images.iter().any(|image| {
            running
                .containers
                .iter()
                .any(|container| container.config_image == image.reference)
        })
    {
        return Err("目标镜像 tag 仍被当前容器使用。".into());
    }
    let material = material_for(&source.host)?;
    let endpoint = source
        .docker_endpoint
        .as_deref()
        .ok_or("Docker endpoint 缺失。")?;
    let daemon_id = source
        .docker_daemon_id
        .as_deref()
        .ok_or("Docker daemon 缺失。")?;
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| "无法初始化镜像测试。")?
        .block_on(async {
            let handle = tokio::time::timeout(Duration::from_secs(20), trusted_handle(material))
                .await
                .map_err(|_| "测试 SSH 连接超时。")??;
            checked_release_endpoint(
                &handle,
                ReleaseDockerIdentity {
                    endpoint,
                    daemon_id,
                },
            )
            .await?;
            for image in images {
                let reference = shell_quote(&image.reference)?;
                let inspect_format = concat!(
                    "image inspect --format '",
                    r#"{"Id":{{json .Id}},"Os":{{json .Os}},"Architecture":{{json .Architecture}}}"#,
                    "' "
                );
                let metadata = archive_command(&handle,
                    &docker_command(endpoint, &format!("{inspect_format}{reference}"))).await?;
                if !historical_image_matches(&metadata, image)? {
                    return Err("测试目标镜像 tag 已变化，禁止删除。".into());
                }
                let all = archive_command(&handle,
                    &docker_command(endpoint, "ps -a --no-trunc --format '{{.ID}}'")) .await?;
                let ids: Vec<String> = all.lines().map(str::to_owned).collect();
                if ids.len() > 100 || ids.iter().any(|id| id.len() != 64 || validate_container_id(id).is_err()) {
                    return Err("无法完整核实 daemon 上的容器引用。".into());
                }
                if !ids.is_empty() {
                    let output = archive_command_with_timeout(&handle,
                        &deployment_inspect_command(endpoint, &ids)?, Duration::from_secs(20)).await?;
                    for line in output.lines() {
                        let row: Value = serde_json::from_str(line).map_err(|_| "容器镜像引用无法解析。")?;
                        if row.get("ConfigImage").and_then(Value::as_str) == Some(&image.reference) {
                            return Err("其他容器仍使用目标镜像 tag，禁止测试移除。".into());
                        }
                    }
                    if output.lines().count() != ids.len() {
                        return Err("daemon 容器元数据不完整。".into());
                    }
                }
                let command = docker_command(
                    endpoint,
                    &format!("image rm -- {reference}"),
                );
                archive_command(&handle, &command).await?;
            }
            let _ = handle.disconnect(Disconnect::ByApplication, "", "en").await;
            Ok::<(), String>(())
        })?;
    if historical_image_states(source, images)?
        .iter()
        .any(|(_, state)| *state != HistoricalImageState::Missing)
    {
        return Err("测试目标镜像 tag 未全部移除。".into());
    }
    Ok(())
}

fn validated_load_target(
    archive: &UploadedArchive,
    reference: &str,
    platform: &str,
) -> Result<(), String> {
    let directory = validated_upload_directory(&archive.directory)?;
    if archive.path != format!("{directory}/image.tar")
        || archive.bytes == 0
        || archive.sha256.len() != 64
        || !archive.sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
        || archive.image_ids.is_empty()
        || archive.image_ids.iter().any(|id| {
            id.len() != 71
                || !id.starts_with("sha256:")
                || !id[7..].bytes().all(|byte| byte.is_ascii_hexdigit())
        })
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
        || !matches!(platform, "linux/amd64" | "linux/arm64")
    {
        return Err("待加载镜像引用或架构无效。".into());
    }
    Ok(())
}

fn verified_loaded_image(text: &str, image_ids: &[String], platform: &str) -> Result<(), String> {
    let row: Value = serde_json::from_str(text).map_err(|_| "远端镜像元数据无效，结果待核实。")?;
    let actual = format!(
        "{}/{}",
        row.get("Os").and_then(Value::as_str).unwrap_or_default(),
        row.get("Architecture")
            .and_then(Value::as_str)
            .unwrap_or_default()
    );
    if !row
        .get("Id")
        .and_then(Value::as_str)
        .is_some_and(|id| image_ids.iter().any(|expected| expected == id))
        || actual != platform
    {
        return Err("远端镜像 ID 或架构与本机构建结果不一致，禁止部署。".into());
    }
    Ok(())
}

async fn load_archive(
    material: ConnectionMaterial,
    identity: ReleaseDockerIdentity<'_>,
    archive: &UploadedArchive,
    reference: &str,
    platform: &str,
    cancel: &AtomicBool,
) -> Result<(), String> {
    validated_load_target(archive, reference, platform)?;
    let handle = tokio::time::timeout(Duration::from_secs(20), trusted_handle(material))
        .await
        .map_err(|_| "远端镜像加载连接超时。")??;
    let endpoint = tokio::time::timeout(
        Duration::from_secs(10),
        checked_release_endpoint(&handle, identity),
    )
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
    verified_loaded_image(&result, &archive.image_ids, platform)?;
    let _ = handle.disconnect(Disconnect::ByApplication, "", "en").await;
    Ok(())
}

#[allow(dead_code)] // The release coordinator will call this before deploying Compose.
pub fn load_uploaded_archive(
    expected_host: Host,
    identity: ReleaseDockerIdentity<'_>,
    archive: &UploadedArchive,
    reference: &str,
    platform: &str,
    cancel: Arc<AtomicBool>,
) -> Result<(), String> {
    validated_load_target(archive, reference, platform)?;
    let material = material_for(&expected_host)?;
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| "无法初始化远端镜像加载环境。".to_owned())?
        .block_on(load_archive(
            material, identity, archive, reference, platform, &cancel,
        ))
}

#[cfg(test)]
mod tests {
    #[test]
    #[ignore = "one-shot VPS network outage with a host-local timed restore and read-only unknown-task audit"]
    fn isolated_lan_host_outage_keeps_upload_unknown() {
        use std::net::{SocketAddr, TcpStream};
        use std::time::{Duration, Instant};

        let appdata = std::path::PathBuf::from(std::env::var_os("APPDATA").unwrap())
            .canonicalize()
            .unwrap();
        assert_eq!(
            appdata.parent().unwrap(),
            std::env::current_dir()
                .unwrap()
                .join("target/isolated-tests/ui-lan-crash-07df86259bea430e9bd0dc9c57cedf5b")
                .canonicalize()
                .unwrap()
        );
        let batch_id = "17d64ab0-18f7-4d17-b881-db907a3312b3";
        let batch = crate::batch_store::get(batch_id).unwrap();
        assert_eq!(batch.project_id, 1856624467);
        assert_eq!(batch.state, "unknown");
        assert_eq!(batch.targets[0].state, "unknown");
        assert_eq!(batch.targets[1].state, "waiting");
        let target = &batch.targets[0];
        assert_eq!(target.host.address, "192.168.63.129");
        assert_eq!(
            target.docker_daemon_id,
            "882622c2-a12a-4ac3-99d4-c3ab389f2641"
        );
        let task_id = target.release_task_id.as_ref().unwrap();
        let task = crate::release_store::get_task(task_id).unwrap();
        assert_eq!(
            (task.phase.as_str(), task.outcome.as_str()),
            ("uploading", "unknown")
        );
        let before = super::inventory(target.host.id).unwrap();
        let mut container_ids = before
            .containers
            .iter()
            .map(|container| container.id.clone())
            .collect::<Vec<_>>();
        container_ids.sort();
        assert_eq!(
            super::docker_identity_for_host(target.host.clone())
                .unwrap()
                .1,
            target.docker_daemon_id
        );
        let address: SocketAddr = "192.168.63.129:22".parse().unwrap();
        assert!(TcpStream::connect_timeout(&address, Duration::from_secs(3)).is_ok());

        let material = super::material_for(&target.host).unwrap();
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let handle = super::trusted_handle(material).await.unwrap();
                let route = super::command_limited(&handle, "ip -o -4 route show default", 4096)
                    .await
                    .unwrap();
                assert!(route.contains("dev ens33") && route.contains("src 192.168.63.129"));
                let unit = format!(
                    "devingpanel-link-restore-1856624467-{}",
                    uuid::Uuid::new_v4().simple()
                );
                super::command_limited(
                    &handle,
                    &format!(
                        "systemd-run --unit={unit} --on-active=45s /usr/sbin/ip link set dev ens33 up"
                    ),
                    4096,
                )
                .await
                .unwrap();
                assert_eq!(
                    super::command_limited(
                        &handle,
                        &format!("systemctl is-active {unit}.timer"),
                        4096,
                    )
                    .await
                    .unwrap()
                    .trim(),
                    "active"
                );
                let _ = tokio::time::timeout(
                    Duration::from_secs(3),
                    super::command_limited(&handle, "ip link set dev ens33 down", 4096),
                )
                .await;
            });
        assert!(TcpStream::connect_timeout(&address, Duration::from_secs(3)).is_err());
        assert!(crate::release::inspect_unknown_upload(task_id).is_err());
        assert_eq!(crate::batch_store::get(batch_id).unwrap().state, "unknown");

        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            match super::docker_identity_for_host(target.host.clone()) {
                Ok((_, daemon_id)) => {
                    assert_eq!(daemon_id, target.docker_daemon_id);
                    break;
                }
                Err(_) => {
                    assert!(
                        Instant::now() < deadline,
                        "VPS SSH and Docker did not recover on schedule"
                    );
                    std::thread::sleep(Duration::from_secs(2));
                }
            }
        }
        let inspection = crate::release::inspect_unknown_upload(task_id).unwrap();
        assert!(inspection.containers.is_empty());
        assert_eq!(inspection.archives.len(), 1);
        assert_eq!(crate::batch_store::get(batch_id).unwrap().state, "unknown");
        let mut after = super::inventory(target.host.id)
            .unwrap()
            .containers
            .iter()
            .map(|container| container.id.clone())
            .collect::<Vec<_>>();
        after.sort();
        assert_eq!(after, container_ids);
    }

    #[test]
    fn remote_command_exit_error_keeps_status_without_output() {
        assert_eq!(
            super::command_exit_error(Some(17)),
            "远端命令以退出码 17 结束。"
        );
        assert_eq!(
            super::command_exit_error(None),
            "远端 SSH 会话未返回命令退出状态。"
        );
    }

    #[tokio::test]
    #[ignore = "read-only Docker state audit of an isolated LAN startup with no completion marker"]
    async fn isolated_lan_second_unknown_container_states() {
        let appdata = std::path::PathBuf::from(std::env::var_os("APPDATA").unwrap())
            .canonicalize()
            .unwrap();
        assert_eq!(
            appdata.parent().unwrap(),
            std::env::current_dir()
                .unwrap()
                .join("target/isolated-tests/ui-lan-recovery-5c842ae5a3de461684a8f2b1bd0400d3")
                .canonicalize()
                .unwrap()
        );
        let batch = crate::batch_store::get("0e1ffb0e-25ba-4537-a76e-a3c164ffaf0b").unwrap();
        assert_eq!(batch.project_id, 1856624454);
        assert_eq!(batch.state, "unknown");
        let target = &batch.targets[1];
        assert_eq!(target.host.address, "192.168.63.130");
        assert_eq!(
            target.docker_daemon_id,
            "ac70e19f-a489-49f3-82f0-0a4ad66ca2e6"
        );
        let handle = super::trusted_handle(super::material_for(&target.host).unwrap())
            .await
            .unwrap();
        let endpoint = super::checked_release_endpoint(
            &handle,
            super::ReleaseDockerIdentity {
                endpoint: &target.docker_endpoint,
                daemon_id: &target.docker_daemon_id,
            },
        )
        .await
        .unwrap();
        let output = super::command_limited(
            &handle,
            &super::docker_command(
                &endpoint,
                "ps --all --no-trunc --filter label=com.docker.compose.project=dp1856624454 --format '{{json .}}'",
            ),
            65536,
        )
        .await
        .unwrap();
        let rows = output
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(rows.len(), 4);
        for row in rows {
            let id = row["ID"].as_str().unwrap();
            assert_eq!(id.len(), 64);
            assert!(id.bytes().all(|byte| byte.is_ascii_hexdigit()));
            let labels = super::archive_command(
                &handle,
                &super::docker_command(
                    &endpoint,
                    &format!("inspect --format '{{{{json .Config.Labels}}}}' {id}"),
                ),
            )
            .await
            .unwrap();
            let labels: serde_json::Value = serde_json::from_str(labels.trim()).unwrap();
            assert_eq!(labels["com.docker.compose.project"], "dp1856624454");
            let state = super::archive_command(
                &handle,
                &super::docker_command(
                    &endpoint,
                    &format!("inspect --format '{{{{json .State}}}}' {id}"),
                ),
            )
            .await
            .unwrap();
            let state: serde_json::Value = serde_json::from_str(state.trim()).unwrap();
            eprintln!(
                "{}: status={}, error={}, started={}, finished={}",
                labels["com.docker.compose.service"],
                state["Status"],
                state["Error"],
                state["StartedAt"],
                state["FinishedAt"]
            );
            if let Some(health) = state["Health"].as_object() {
                let checks = health["Log"].as_array().unwrap();
                eprintln!(
                    "{} health: status={}, checks={:?}",
                    labels["com.docker.compose.service"],
                    health["Status"],
                    checks
                        .iter()
                        .map(|check| (
                            check["Start"].as_str().unwrap_or_default(),
                            check["ExitCode"].as_i64().unwrap_or_default()
                        ))
                        .collect::<Vec<_>>()
                );
            }
        }
        let active = super::archive_command(
            &handle,
            "sh -c \"pgrep -af 'docker.*[d]p1856624454' || true\"",
        )
        .await
        .unwrap();
        eprintln!("matching Compose processes: {}", active.trim());
        let events = super::command_limited(
            &handle,
            &super::docker_command(
                &endpoint,
                "events --since '2026-10-30T01:44:30Z' --until '2026-10-30T01:44:50Z' --filter label=com.docker.compose.project=dp1856624454 --format '{{json .}}'",
            ),
            65536,
        )
        .await
        .unwrap();
        eprintln!("bounded project events: {}", events.lines().count());
        for line in events.lines() {
            let event: serde_json::Value = serde_json::from_str(line).unwrap();
            eprintln!(
                "event {} {} {}",
                event["timeNano"],
                event["Action"],
                event["Actor"]["Attributes"]["com.docker.compose.service"]
            );
        }
        let _ = handle
            .disconnect(russh::Disconnect::ByApplication, "", "en")
            .await;
    }

    use super::{
        ContainerAction, DeploymentState, DockerInstallSource, LogEvent, LogLineBuffer, TestResult,
        apt_candidates_use_source, cleanup_archive_command, container_command, cpu_counters,
        default_route_interface, disk_available, disk_percent, docker_install_commands,
        execute_container_action, execute_disk_prune, fetch_disk_preview, fetch_inventory,
        fetch_monitor, follow_logs, image_size_bytes, inspect_deployment_state, memory_kib,
        merged_mirror_config, mirrors_active, network_bytes, parse_compose_hashes,
        parse_disk_preview, parse_inventory, probe, remote_archive_size, safe_version,
        same_connection_target, sftp_upload_config, shell_quote, valid_docker_signing_key,
        validated_release_directory, validated_release_file, validated_upload_directory,
        verified_compose_config, verified_sha256, verify_container_config_hashes,
    };
    use crate::host_store::{ConnectionMaterial, Host};
    use crate::release_store::HistoricalImage;
    use russh::server::{self, Auth, Msg, Server as _, Session};
    use russh::{Channel, ChannelId};
    use russh_sftp::protocol::{Attrs, FileAttributes, Handle, OpenFlags, Status, StatusCode};

    #[test]
    fn remote_uploads_bound_inflight_writes_and_ack_timeout() {
        assert_eq!(sftp_upload_config().max_concurrent_writes, 4);
        assert_eq!(sftp_upload_config().request_timeout_secs, 60);
    }

    #[test]
    fn remote_archive_probe_rejects_ambiguous_file_types_and_sizes() {
        assert_eq!(remote_archive_size("missing\n").unwrap(), None);
        assert_eq!(remote_archive_size("size:262144\n").unwrap(), Some(262144));
        assert!(remote_archive_size("invalid\n").is_err());
        assert!(remote_archive_size("size:-1\n").is_err());
        assert!(remote_archive_size("size:12\nsize:13\n").is_err());
    }
    use sha2::Digest;
    use std::collections::HashMap;
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    };
    use std::time::Duration;

    #[test]
    fn compose_hashes_reject_drift_missing_and_duplicate_services() {
        let web = "a".repeat(64);
        let db = "b".repeat(64);
        let expected = parse_compose_hashes(&format!("web {web}\ndb {db}\n")).unwrap();
        let ids = vec!["c".repeat(64), "d".repeat(64)];
        let rows = format!(
            "{{\"Id\":\"{}\",\"Service\":\"web\",\"ConfigHash\":\"{web}\"}}\n{{\"Id\":\"{}\",\"Service\":\"db\",\"ConfigHash\":\"{db}\"}}",
            ids[0], ids[1]
        );
        assert!(verify_container_config_hashes(&ids, &rows, &expected).is_ok());
        assert!(verify_container_config_hashes(&ids, &rows.replace(&db, &web), &expected).is_err());
        assert!(
            verify_container_config_hashes(&ids, &rows.replace("\"db\"", "\"web\""), &expected)
                .is_err()
        );
        assert!(parse_compose_hashes(&format!("web {web}\nweb {web}")).is_err());
        assert!(parse_compose_hashes("web bad").is_err());
    }

    #[test]
    fn deployment_paths_are_private_and_cannot_escape_the_release_root() {
        let id = uuid::Uuid::new_v4().to_string();
        assert!(
            validated_release_directory("/root", 1, &id)
                .unwrap()
                .ends_with(&format!("/projects/1/releases/{id}"))
        );
        for home in ["/root/../other", "/", "relative", "/root\n"] {
            assert!(validated_release_directory(home, 1, &id).is_err());
        }
        for path in [
            ".env",
            "../config.yml",
            "dir/../config.yml",
            "C:\\config.yml",
            "a//b",
            "/absolute.yml",
        ] {
            assert!(validated_release_file(path).is_err());
        }
        assert!(validated_release_file("compose.yml").is_ok());
        assert!(validated_release_file("deploy/compose.prod.yml").is_ok());
        assert!(validated_release_file("deploy/发布 配置's.yaml").is_ok());
        assert_eq!(
            shell_quote("/root user/发布's.yaml").unwrap(),
            "'/root user/发布'\"'\"'s.yaml'"
        );
        assert!(validated_release_directory("/root user", 1, &id).is_ok());
    }

    #[test]
    fn compose_validation_checks_exact_target_without_exposing_environment() {
        let staged = super::StagedDeployment {
            directory: "/root/release's".into(),
            compose_files: vec!["compose.yaml".into(), "deploy/发布 配置's.yaml".into()],
            env_path: "/root/release's/.env".into(),
        };
        let command = super::compose_config_command(
            "unix:///var/run/docker.sock",
            &staged,
            7,
            "sample/app:v1",
        )
        .unwrap();
        assert!(command.contains("DEVINGPANEL_IMAGE='sample/app:v1'"));
        assert!(command.contains("-p dp7"));
        assert!(command.contains("--env-file '/root/release'\"'\"'s/.env'"));
        assert!(format!("{command} config --format json").contains("config --format json"));
        assert!(
            super::compose_config_command("unix:///var/run/docker.sock", &staged, 7, "app:tag;rm")
                .is_err()
        );
        let json = r#"{"services":{"web":{"image":"sample/app:v1","environment":{"PASSWORD":"secret"}},"worker":{"image":"redis:7"}}}"#;
        assert!(verified_compose_config(json, "web", "sample/app:v1").is_ok());
        assert!(verified_compose_config(json, "web", "sample/app:v2").is_err());
        assert!(verified_compose_config(json, "missing", "sample/app:v1").is_err());
        assert!(verified_compose_config("not json", "web", "sample/app:v1").is_err());
        assert!(
            verified_compose_config(
                r#"{"services":{"web":{"image":"sample/app:v1","deploy":{"replicas":2}}}}"#,
                "web",
                "sample/app:v1"
            )
            .is_err()
        );
        assert!(
            !verified_compose_config(json, "web", "sample/app:v2")
                .unwrap_err()
                .contains("secret")
        );
        assert!(
            verified_compose_config(
                r#"{"services":{"web":{"image":"sample/app:v1","build":"."}}}"#,
                "web",
                "sample/app:v1"
            )
            .is_err()
        );
    }

    #[test]
    fn stack_config_requires_each_built_image_and_checks_all_services() {
        let built = vec![
            super::StackService {
                service: "web".into(),
                reference: "app:v1".into(),
                image_ids: vec![format!("sha256:{}", "a".repeat(64))],
            },
            super::StackService {
                service: "worker".into(),
                reference: "worker:v1".into(),
                image_ids: vec![format!("sha256:{}", "b".repeat(64))],
            },
        ];
        let staged = super::StagedDeployment {
            directory: "/root/test".into(),
            compose_files: vec!["compose.yaml".into()],
            env_path: "/root/test/.env".into(),
        };
        let command =
            super::stack_command("unix:///var/run/docker.sock", &staged, 1, &built).unwrap();
        let rollback_command = super::stack_up_command(
            "unix:///var/run/docker.sock",
            &staged,
            1,
            &built,
            &super::StackStartPolicy {
                expected_images: &built,
                pull_missing: false,
                remove_orphans: true,
                record_completion: false,
            },
        )
        .unwrap();
        assert!(rollback_command.ends_with("up -d --no-build --pull never --remove-orphans"));
        let normal_command = super::stack_up_command(
            "unix:///var/run/docker.sock",
            &staged,
            1,
            &built,
            &super::StackStartPolicy {
                expected_images: &built,
                pull_missing: true,
                remove_orphans: false,
                record_completion: true,
            },
        )
        .unwrap();
        assert!(normal_command.ends_with("up -d --no-build --pull missing"));
        let task_id = uuid::Uuid::new_v4().to_string();
        let release_dir =
            format!("/root/.local/share/devingpanel/projects/1001/releases/{task_id}");
        let staged_release = super::StagedDeployment {
            env_path: format!("{release_dir}/.env"),
            directory: release_dir.clone(),
            compose_files: vec!["compose.yaml".into()],
        };
        let start = super::stack_start_command(
            "unix:///var/run/docker.sock",
            &staged_release,
            1001,
            &built,
            &super::StackStartPolicy {
                expected_images: &built,
                pull_missing: true,
                remove_orphans: false,
                record_completion: true,
            },
        )
        .unwrap();
        assert!(start.contains("up -d --no-build --pull missing && (umask 077;"));
        assert!(start.contains(&format!("{release_dir}/.compose-start-complete")));
        assert!(start.contains(&format!("1001:{task_id}")));
        assert!(
            super::stack_start_command(
                "unix:///var/run/docker.sock",
                &staged,
                1,
                &built,
                &super::StackStartPolicy {
                    expected_images: &built,
                    pull_missing: true,
                    remove_orphans: false,
                    record_completion: true,
                },
            )
            .is_err()
        );
        assert!(command.contains("DEVINGPANEL_IMAGE_WEB='app:v1'"));
        assert!(command.contains("DEVINGPANEL_IMAGE_WORKER='worker:v1'"));
        let config = r#"{"services":{"web":{"image":"app:v1"},"worker":{"image":"worker:v1"},"db":{"image":"postgres:16"}}}"#;
        assert_eq!(
            super::verified_stack_config(config, &built).unwrap().len(),
            3
        );
        assert!(
            super::verified_stack_config(&config.replace("worker:v1", "worker:old"), &built)
                .is_err()
        );
        assert!(
            super::verified_stack_config(
                &config.replace(
                    "\"db\":{\"image\":\"postgres:16\"}",
                    "\"db\":{\"image\":\"postgres:16\",\"build\":\".\"}"
                ),
                &built
            )
            .is_err()
        );
        assert!(
            super::verified_stack_config(
                &config.replace("\"worker\":{\"image\":\"worker:v1\"},", ""),
                &built
            )
            .is_err()
        );
        assert!(
            super::verified_stack_config(
                &config.replace(
                    "\"db\":{\"image\":\"postgres:16\"}",
                    "\"db\":{\"image\":\"postgres:16\",\"profiles\":[\"optional\"]}"
                ),
                &built,
            )
            .is_err()
        );
    }

    #[test]
    fn historical_image_check_distinguishes_matching_changed_and_invalid_metadata() {
        let expected = HistoricalImage {
            service: "web".into(),
            reference: "app:v1".into(),
            platform: "linux/amd64".into(),
            image_ids: vec![format!("sha256:{}", "a".repeat(64))],
        };
        let matching = format!(
            r#"{{"Id":"sha256:{}","Os":"linux","Architecture":"amd64"}}"#,
            "a".repeat(64)
        );
        assert!(super::historical_image_matches(&matching, &expected).unwrap());
        assert!(
            !super::historical_image_matches(&matching.replace("amd64", "arm64"), &expected)
                .unwrap()
        );
        assert!(
            !super::historical_image_matches(
                &matching.replace(&"a".repeat(64), &"b".repeat(64)),
                &expected
            )
            .unwrap()
        );
        assert!(super::historical_image_matches("not-json", &expected).is_err());
    }

    #[test]
    fn stack_health_preserves_running_without_healthcheck() {
        use super::{DeploymentState as State, aggregate_stack_states};
        assert_eq!(
            aggregate_stack_states(&[Some(State::Healthy), Some(State::Healthy)]),
            Some(State::Healthy)
        );
        assert_eq!(
            aggregate_stack_states(&[Some(State::Healthy), Some(State::RunningUnverified)]),
            Some(State::RunningUnverified)
        );
        assert_eq!(
            aggregate_stack_states(&[Some(State::RunningUnverified), None]),
            None
        );
        assert_eq!(
            aggregate_stack_states(&[None, Some(State::Failed)]),
            Some(State::Failed)
        );
    }

    #[test]
    fn release_observation_requires_exact_compose_container_set() {
        let id = "a".repeat(64);
        let image = format!("sha256:{}", "b".repeat(64));
        let row = format!(
            r#"{{"Id":"{id}","Service":"web","Image":"{image}","ConfigImage":"app:v1","Status":"running","Health":"healthy"}}"#
        );
        let observed = super::parse_release_observation(std::slice::from_ref(&id), &row).unwrap();
        assert_eq!(observed.containers[0].service, "web");
        assert_eq!(observed.containers[0].health.as_deref(), Some("healthy"));
        assert!(super::parse_release_observation(std::slice::from_ref(&id), "").is_err());
        assert!(
            super::parse_release_observation(std::slice::from_ref(&id), &format!("{row}\n{row}"))
                .is_err()
        );
        assert!(
            super::parse_release_observation(&[id], &row.replace("\"web\"", "\"invalid/name\""))
                .is_err()
        );
    }

    #[test]
    fn container_check_rejects_wrong_image_and_distinguishes_health() {
        let id = "a".repeat(64);
        let image = format!("sha256:{}", "b".repeat(64));
        let archived_ids = vec![format!("sha256:{}", "c".repeat(64)), image.clone()];
        assert!(super::validate_deployment_image_ids(&archived_ids).is_ok());
        assert!(super::validate_deployment_image_ids(&[]).is_err());
        let row = |status: &str, health: &str, image: &str| {
            format!(
                r#"{{"Id":"{id}","Image":"{image}","ConfigImage":"app:v1","Status":"{status}","Health":{health}}}"#
            )
        };
        let ids = vec![id.clone()];
        assert_eq!(
            inspect_deployment_state(
                &row("running", "\"healthy\"", &image),
                &ids,
                "app:v1",
                &archived_ids
            )
            .unwrap(),
            Some(DeploymentState::Healthy)
        );
        assert_eq!(
            inspect_deployment_state(
                &row("running", "null", &image),
                &ids,
                "app:v1",
                std::slice::from_ref(&image)
            )
            .unwrap(),
            Some(DeploymentState::RunningUnverified)
        );
        assert_eq!(
            inspect_deployment_state(
                &row("running", "\"starting\"", &image),
                &ids,
                "app:v1",
                std::slice::from_ref(&image)
            )
            .unwrap(),
            None
        );
        assert_eq!(
            inspect_deployment_state(
                &row("running", "\"unhealthy\"", &image),
                &ids,
                "app:v1",
                std::slice::from_ref(&image)
            )
            .unwrap(),
            Some(DeploymentState::Failed)
        );
        assert_eq!(
            inspect_deployment_state(
                &row("exited", "null", &image),
                &ids,
                "app:v1",
                std::slice::from_ref(&image)
            )
            .unwrap(),
            Some(DeploymentState::Failed)
        );
        assert_eq!(
            inspect_deployment_state(
                &row("running", "null", "sha256:wrong"),
                &ids,
                "app:v1",
                std::slice::from_ref(&image)
            )
            .unwrap(),
            Some(DeploymentState::Failed)
        );
        assert!(
            inspect_deployment_state("", &ids, "app:v1", std::slice::from_ref(&image)).is_err()
        );
        assert!(
            inspect_deployment_state(
                &row("running", "null", &image),
                &[id, "c".repeat(64)],
                "app:v1",
                std::slice::from_ref(&image)
            )
            .is_err()
        );
        assert!(
            super::deployment_inspect_command("unix:///var/run/docker.sock", &["bad;id".into()])
                .is_err()
        );
    }

    #[test]
    fn mirror_config_preserves_other_keys_and_recognizes_active_values() {
        let original = r#"{"log-driver":"json-file","registry-mirrors":["https://old.example"]}"#;
        let updated = merged_mirror_config(Some(original)).unwrap().unwrap();
        let row: serde_json::Value = serde_json::from_slice(&updated).unwrap();
        assert_eq!(row["log-driver"], "json-file");
        assert_eq!(row["registry-mirrors"][0], "https://docker.1panel.live");
        assert_eq!(row["registry-mirrors"][1], "https://docker.m.daocloud.io");
        assert!(
            merged_mirror_config(Some(std::str::from_utf8(&updated).unwrap()))
                .unwrap()
                .is_none()
        );
        assert!(merged_mirror_config(Some("[]")).is_err());
        assert!(merged_mirror_config(Some("not json")).is_err());
        assert!(mirrors_active(
            r#"["https://docker.1panel.live/","https://docker.m.daocloud.io/"]"#
        ));
        assert!(!mirrors_active(r#"["https://untrusted.example/"]"#));
    }

    #[test]
    #[ignore = "configures mirrors only on the two audited LAN test hosts"]
    fn isolated_lan_hosts_configure_registry_mirrors() {
        let appdata = std::path::PathBuf::from(std::env::var_os("APPDATA").unwrap())
            .canonicalize()
            .unwrap();
        assert_eq!(
            appdata.parent().unwrap(),
            std::env::current_dir()
                .unwrap()
                .join("target/isolated-tests/ui-lan-crash-20261030-b")
                .canonicalize()
                .unwrap()
        );
        let hosts = crate::host_store::list().unwrap().0;
        for (id, address, daemon) in [
            (1, "192.168.63.129", "882622c2-a12a-4ac3-99d4-c3ab389f2641"),
            (2, "192.168.63.130", "ac70e19f-a489-49f3-82f0-0a4ad66ca2e6"),
        ] {
            let host = hosts.iter().find(|host| host.id == id).unwrap().clone();
            assert_eq!(host.address, address);
            assert_eq!(host.username, "root");
            assert_eq!(
                super::docker_identity_for_host(host.clone()).unwrap().1,
                daemon
            );
            assert!(
                super::inventory(id)
                    .unwrap()
                    .containers
                    .iter()
                    .all(|container| container.state != "running")
            );
            let result = super::configure_registry_mirrors(host.clone(), |_| {}).unwrap();
            assert!(matches!(
                result,
                super::MirrorConfigResult::Updated | super::MirrorConfigResult::AlreadyConfigured
            ));
            assert_eq!(super::docker_identity_for_host(host).unwrap().1, daemon);
            eprintln!("LAN mirror configuration verified: host={address}, result={result:?}");
        }
    }

    #[test]
    #[ignore = "requires an isolated copy of the real host database and its existing vault credential; read-only SSH"]
    fn real_historical_images_read_only() {
        let task_id = std::env::var("DEVINGPANEL_TEST_HISTORY_TASK").unwrap();
        let mut task = crate::release_store::get_task(&task_id).unwrap();
        let conn = crate::database::connect().unwrap();
        let mut query = conn.prepare(
            "SELECT service, reference, platform FROM release_task_images WHERE task_id = ?1 ORDER BY ordinal"
        ).unwrap();
        let plans = query
            .query_map([&task_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(
            plans.len(),
            2,
            "the isolated stack smoke test needs both built services"
        );
        let (endpoint, daemon_id) = super::docker_identity_for_host(task.host.clone()).unwrap();
        task.docker_endpoint = Some(endpoint.clone());
        task.docker_daemon_id = Some(daemon_id.clone());
        let material = super::material_for(&task.host).unwrap();
        let images = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let handle = super::trusted_handle(material).await?;
                super::checked_release_endpoint(
                    &handle,
                    super::ReleaseDockerIdentity {
                        endpoint: &endpoint,
                        daemon_id: &daemon_id,
                    },
                )
                .await?;
                let mut images = Vec::new();
                for (service, reference, platform) in plans {
                    let reference_arg = super::shell_quote(&reference)?;
                    let command = super::docker_command(
                        &endpoint,
                        &format!("image inspect --format '{{{{json .Id}}}}' {reference_arg}"),
                    );
                    let result = super::archive_command(&handle, &command).await?;
                    let image_id: String = serde_json::from_str(result.trim())
                        .map_err(|_| "测试镜像 ID 格式无效。")?;
                    images.push(HistoricalImage {
                        service,
                        reference,
                        platform,
                        image_ids: vec![image_id],
                    });
                }
                let _ = handle
                    .disconnect(russh::Disconnect::ByApplication, "", "en")
                    .await;
                Ok::<_, String>(images)
            })
            .unwrap();
        assert_eq!(
            super::observe_historical_images(&task, &images).unwrap(),
            images
                .iter()
                .map(|item| (item.service.clone(), true))
                .collect::<Vec<_>>()
        );
        let mut changed = images.clone();
        changed[1].image_ids = vec![format!("sha256:{}", "0".repeat(64))];
        assert_eq!(
            super::observe_historical_images(&task, &changed).unwrap(),
            vec![
                (images[0].service.clone(), true),
                (images[1].service.clone(), false)
            ]
        );
        let mut missing = images.clone();
        missing[0].reference =
            format!("devingpanel-absent-{}:smoke", uuid::Uuid::new_v4().simple());
        assert_eq!(
            super::observe_historical_images(&task, &missing).unwrap(),
            vec![
                (images[0].service.clone(), false),
                (images[1].service.clone(), true)
            ]
        );
        task.docker_daemon_id = Some("changed-daemon".into());
        assert!(super::observe_historical_images(&task, &images).is_err());
    }

    #[test]
    #[ignore = "requires an explicitly configured real VPS; creates and removes one private temporary upload"]
    fn real_vps_probe_and_temporary_upload() {
        let address = std::env::var("DEVINGPANEL_TEST_VPS_HOST").unwrap();
        let fingerprint = std::env::var("DEVINGPANEL_TEST_VPS_FINGERPRINT").unwrap();
        let credential = std::env::var("DEVINGPANEL_TEST_VPS_PASSWORD").unwrap();
        let material = || ConnectionMaterial {
            host: Host {
                id: 0,
                name: "integration target".into(),
                address: address.clone(),
                port: 22,
                username: "root".into(),
                group: String::new(),
                auth: "password".into(),
                trusted_fingerprint: Some(fingerprint.clone()),
            },
            credential: credential.clone(),
            trusted_fingerprint: Some(fingerprint.clone()),
        };
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                assert!(matches!(
                    probe(material()).await.unwrap(),
                    TestResult::Connected {
                        architecture: Some(platform),
                        sftp_v3_handshake: true,
                        ..
                    } if platform == "linux/amd64"
                ));
                let inventory = fetch_inventory(material()).await.unwrap();
                assert!(inventory.containers.is_empty());
                let monitor = fetch_monitor(material()).await.unwrap();
                assert!(monitor.memory_total_kib > 0);
                let policy_handle = super::trusted_handle(material()).await.unwrap();
                let policy = super::command_limited(
                    &policy_handle,
                    "apt-cache policy docker-ce docker-ce-cli containerd.io docker-buildx-plugin docker-compose-plugin",
                    512 * 1024,
                )
                .await
                .unwrap();
                assert!(apt_candidates_use_source(&policy, DockerInstallSource::Tencent));
                let _ = policy_handle
                    .disconnect(russh::Disconnect::ByApplication, "", "en")
                    .await;
                assert_eq!(
                    super::configure_mirrors_on(material(), &mut |_| panic!("must not restart"))
                        .await
                        .unwrap(),
                    super::MirrorConfigResult::AlreadyConfigured
                );
                let path = std::env::temp_dir().join(format!(
                    "devingpanel-vps-upload-{}",
                    uuid::Uuid::new_v4().simple()
                ));
                let bytes = vec![0x5au8; 8192];
                std::fs::write(&path, &bytes).unwrap();
                let mut progress = Vec::new();
                let uploaded = super::transfer_archive(
                    material(),
                    &path,
                    bytes.len() as u64,
                    &AtomicBool::new(false),
                    &mut |copied, total| progress.push((copied, total)),
                )
                .await
                .unwrap();
                let handle = super::trusted_handle(material()).await.unwrap();
                super::archive_command(
                    &handle,
                    &super::cleanup_archive_command(&uploaded.directory).unwrap(),
                )
                .await
                .unwrap_or_else(|error| panic!("{}: {error}", uploaded.directory));
                let _ = handle
                    .disconnect(russh::Disconnect::ByApplication, "", "en")
                    .await;
                std::fs::remove_file(path).unwrap();
                assert_eq!(
                    uploaded.sha256,
                    format!("{:x}", sha2::Sha256::digest(&bytes))
                );
                assert_eq!(
                    progress.last(),
                    Some(&(bytes.len() as u64, bytes.len() as u64))
                );
                let task_id = uuid::Uuid::new_v4().to_string();
                let files = vec![
                    super::DeploymentFile {
                        path: "compose.yaml".into(),
                        bytes: b"services:\n  web:\n    image: ${DEVINGPANEL_IMAGE}\n".to_vec(),
                    },
                    super::DeploymentFile {
                        path: "deploy/发布 配置's.yaml".into(),
                        bytes: b"services:\n  web:\n    restart: unless-stopped\n".to_vec(),
                    },
                ];
                let staged = super::stage_deployment_on(
                    material(),
                    987654321,
                    &task_id,
                    &files,
                    b"MODE=\"smoke\"\n",
                    &AtomicBool::new(false),
                )
                .await
                .unwrap();
                let handle = super::trusted_handle(material()).await.unwrap();
                let endpoint = super::local_docker_endpoint(&handle).await.unwrap();
                let daemon_id = super::docker_daemon_id(&handle, &endpoint).await.unwrap();
                super::validate_staged_compose_on(
                    material(),
                    super::ReleaseDockerIdentity { endpoint: &endpoint, daemon_id: &daemon_id },
                    &staged,
                    987654321,
                    "web",
                    "devingpanel/smoke:v1",
                    &AtomicBool::new(false),
                )
                .await
                .unwrap();
                let paths = files
                    .iter()
                    .map(|file| format!("{}/{}", staged.directory, file.path))
                    .chain(std::iter::once(staged.env_path.clone()))
                    .collect::<Vec<_>>();
                super::cleanup_staged_paths(
                    &handle,
                    &staged.directory,
                    &paths,
                    &[format!("{}/deploy", staged.directory)],
                )
                .await
                .unwrap_or_else(|error| panic!("{}: {error}", staged.directory));
                let project_dir = staged.directory.rsplit_once("/releases/").unwrap().0;
                super::archive_command(
                    &handle,
                    &format!("rmdir -- {project_dir}/releases {project_dir}"),
                )
                .await
                .unwrap();
                let _ = handle
                    .disconnect(russh::Disconnect::ByApplication, "", "en")
                    .await;
            });
    }

    #[test]
    fn installer_uses_fixed_signed_apt_sources() {
        let key = "pub:-:4096:1:0EBFCD88:0::::::\nfpr:::::::::9DC858229FC7DD38854AE2D88D81803C0EBFCD88:\nsub:-:4096:1:xxx:\nfpr:::::::::AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA:\n";
        assert!(valid_docker_signing_key(key));
        assert!(!valid_docker_signing_key(&key.replace(
            "9DC858229FC7DD38854AE2D88D81803C0EBFCD88",
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        )));
        assert!(!valid_docker_signing_key(&format!("{key}{key}")));
        for (source, host) in [
            (DockerInstallSource::Tencent, "mirrors.cloud.tencent.com"),
            (DockerInstallSource::Official, "download.docker.com"),
        ] {
            let commands = docker_install_commands(source);
            assert_eq!(commands.len(), 9);
            assert!(commands[2].1.contains(host));
            assert!(
                commands[4]
                    .1
                    .contains("Signed-By: /etc/apt/keyrings/devingpanel-docker.asc")
            );
            assert!(commands[4].1.contains(&format!("Pin: origin \"{host}\"")));
            assert!(commands[5].1.contains("APT::Update::Error-Mode=any"));
            assert!(commands[6].1.starts_with("LC_ALL=C apt-cache policy"));
            assert!(commands[7].1.contains("--no-remove docker-ce docker-ce-cli containerd.io docker-buildx-plugin docker-compose-plugin"));
            assert!(
                !commands
                    .iter()
                    .any(|(_, command, _)| command.contains("| sh"))
            );
        }
        let output = ["docker-ce", "docker-ce-cli", "containerd.io", "docker-buildx-plugin", "docker-compose-plugin"]
            .iter()
            .map(|package| format!("{package}:\n  Installed: (none)\n  Candidate: 1.0\n  Version table:\n     1.0 990\n       500 https://mirrors.cloud.tencent.com/docker-ce/linux/debian trixie/stable amd64 Packages\n"))
            .collect::<String>();
        assert!(apt_candidates_use_source(
            &output,
            DockerInstallSource::Tencent
        ));
        assert!(!apt_candidates_use_source(
            &output.replace("1.0 990", "1.0 500"),
            DockerInstallSource::Tencent
        ));
        assert!(!apt_candidates_use_source(
            &output,
            DockerInstallSource::Official
        ));
        assert!(!apt_candidates_use_source(
            &output.replace(
                "500 https://mirrors.cloud.tencent.com",
                "500 https://untrusted.example"
            ),
            DockerInstallSource::Tencent
        ));
        assert!(!apt_candidates_use_source(
            &output.replace(
                "       500 https://mirrors.cloud.tencent.com",
                "       1000 https://untrusted.example/docker-ce/linux/debian trixie/stable amd64 Packages\n       500 https://mirrors.cloud.tencent.com"
            ),
            DockerInstallSource::Tencent
        ));
        assert!(!apt_candidates_use_source(
            &output.replace(
                "       500 https://mirrors.cloud.tencent.com",
                "       500 file:/srv/packages trixie/stable amd64 Packages\n       500 https://mirrors.cloud.tencent.com"
            ),
            DockerInstallSource::Tencent
        ));
    }

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
            group: "test".into(),
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
        changed = host.clone();
        changed.group.clear();
        assert!(same_connection_target(&host, &changed));
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
        upload_fail_after: Arc<AtomicUsize>,
        load_tag_exists: Arc<AtomicBool>,
        load_no_exit: Arc<AtomicBool>,
        docker_missing: Arc<AtomicBool>,
    }

    struct MemorySftp {
        files: Arc<Mutex<HashMap<String, Vec<u8>>>>,
        fail_write: Arc<AtomicBool>,
        fail_after: Arc<AtomicUsize>,
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
            let mut files = self.files.lock().unwrap();
            if flags.contains(OpenFlags::WRITE)
                && !flags.contains(OpenFlags::CREATE | OpenFlags::EXCLUDE)
                && attrs.permissions.is_none()
            {
                if !files.contains_key(&filename) {
                    return Err(StatusCode::NoSuchFile);
                }
            } else if flags.contains(OpenFlags::CREATE | OpenFlags::EXCLUDE | OpenFlags::WRITE)
                && attrs.permissions == Some(0o600)
            {
                if files.contains_key(&filename) {
                    return Err(StatusCode::Failure);
                }
                files.insert(filename.clone(), Vec::new());
            } else {
                return Err(StatusCode::PermissionDenied);
            }
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
            let mut files = self.files.lock().unwrap();
            let data = files.get_mut(&handle).ok_or(StatusCode::NoSuchFile)?;
            let fail_after = self.fail_after.load(Ordering::SeqCst);
            if self.fail_write.load(Ordering::SeqCst)
                || (fail_after != 0 && data.len() >= fail_after)
            {
                return Err(StatusCode::PermissionDenied);
            }
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
                    let fail_after = self.upload_fail_after.clone();
                    tokio::spawn(async move {
                        russh_sftp::server::run(
                            stream.into_stream(),
                            MemorySftp {
                                files,
                                fail_write,
                                fail_after,
                            },
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
            Ok(
                if (user == "deploy" || user == "root") && password == "test-password" {
                    Auth::Accept
                } else {
                    Auth::Reject {
                        proceed_with_methods: None,
                        partial_success: false,
                    }
                },
            )
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
            if data == b"diagnostic-exit-17" {
                session.channel_success(channel)?;
                session.data(channel, b"SECRET=not-for-errors\n".to_vec())?;
                session.exit_status_request(channel, 17)?;
                session.eof(channel)?;
                session.close(channel)?;
                return Ok(());
            }
            if data == b"diagnostic-no-exit" {
                session.channel_success(channel)?;
                session.data(channel, b"SECRET=not-for-errors\n".to_vec())?;
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
            if self.upload_sftp.load(Ordering::SeqCst)
                && let Ok(text) = std::str::from_utf8(data)
                && let Some(directory) = text
                    .strip_prefix("sh -c 'if test -L ")
                    .and_then(|value| value.split_once(';').map(|(directory, _)| directory))
            {
                super::validated_upload_directory(directory)
                    .map_err(|_| russh::Error::Disconnect)?;
                let response =
                    if text.contains("then echo invalid") && text.contains("stat -c \"size:%s\"") {
                        self.upload_files
                            .lock()
                            .unwrap()
                            .get(&format!("{directory}/image.tar"))
                            .map_or_else(
                                || "missing\n".to_owned(),
                                |bytes| format!("size:{}\n", bytes.len()),
                            )
                    } else if text.contains("then exit 1") && text.contains("mkdir -m 700") {
                        String::new()
                    } else {
                        return Err(russh::Error::Disconnect);
                    };
                session.channel_success(channel)?;
                session.data(channel, response.into_bytes())?;
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
                b"sh -c 'command -v docker >/dev/null 2>&1; printf \"%s\\n\" \"$?\"'" => if self.docker_missing.load(Ordering::SeqCst) {
                    b"127\n".as_slice()
                } else {
                    b"0\n".as_slice()
                },
                b"cat /etc/os-release" => b"ID=debian\nVERSION_ID=\"13\"\n".as_slice(),
                b"id -u" => b"0\n".as_slice(),
                b"sh -c 'for tool in apt-get dpkg-query systemctl curl; do command -v \"$tool\" >/dev/null 2>&1 || exit 1; done; test -d /run/systemd/system && echo ready'" => b"ready\n".as_slice(),
                b"docker --host unix:///var/run/docker.sock info --format '{{.ServerVersion}}'" => b"29.5.3\n".as_slice(),
                b"docker --host unix:///var/run/docker.sock info --format '{{.ID}}'" => b"TEST:ID\n".as_slice(),
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
        let upload_fail_after = Arc::new(AtomicUsize::new(0));
        let load_tag_exists = Arc::new(AtomicBool::new(false));
        let load_no_exit = Arc::new(AtomicBool::new(false));
        let docker_missing = Arc::new(AtomicBool::new(false));
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
            upload_fail_after: upload_fail_after.clone(),
            load_tag_exists: load_tag_exists.clone(),
            load_no_exit: load_no_exit.clone(),
            docker_missing: docker_missing.clone(),
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
            docker_missing.store(true, Ordering::SeqCst);
            assert!(matches!(
                probe(material(Some(fingerprint.clone()))).await.unwrap(),
                TestResult::DockerMissing {
                    os_id,
                    os_version,
                    architecture: Some(platform),
                    ..
                } if os_id == "debian" && os_version == "13" && platform == "linux/amd64"
            ));
            docker_missing.store(false, Ordering::SeqCst);
            Box::pin(async {
                let mut root = material(Some(fingerprint.clone()));
                root.host.username = "root".into();
                let mut phases = Vec::new();
                let error =
                    super::install_docker_on(root, DockerInstallSource::Tencent, &mut |phase| {
                        phases.push(phase.to_owned())
                    })
                    .await
                    .unwrap_err();
                assert!(error.contains("命令已存在"));
                assert!(phases.is_empty());
                assert!(operations.lock().unwrap().is_empty());
            })
            .await;
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
            let mut upload = super::transfer_archive(
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
                let identity = super::ReleaseDockerIdentity {
                    endpoint: "unix:///var/run/docker.sock",
                    daemon_id: "TEST:ID",
                };
                upload.image_ids = vec![image_id.clone()];
                assert!(
                    super::load_archive(
                        material(Some(fingerprint.clone())),
                        identity,
                        &upload,
                        "app;bad:tag",
                        "linux/amd64",
                        &AtomicBool::new(false),
                    )
                    .await
                    .is_err()
                );
                assert_eq!(operations.lock().unwrap().len(), 5);
                let wrong_identity = super::ReleaseDockerIdentity {
                    endpoint: "unix:///var/run/docker.sock",
                    daemon_id: "OTHER:ID",
                };
                assert!(
                    super::load_archive(
                        material(Some(fingerprint.clone())),
                        wrong_identity,
                        &upload,
                        "app:load-test",
                        "linux/amd64",
                        &AtomicBool::new(false),
                    )
                    .await
                    .unwrap_err()
                    .contains("daemon 身份")
                );
                assert_eq!(operations.lock().unwrap().len(), 5);
                wrong_daemon_arch.store(true, Ordering::SeqCst);
                assert!(
                    super::load_archive(
                        material(Some(fingerprint.clone())),
                        identity,
                        &upload,
                        "app:load-test",
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
                    image_ids: upload.image_ids.clone(),
                };
                assert!(
                    super::load_archive(
                        material(Some(fingerprint.clone())),
                        identity,
                        &altered,
                        "app:load-test",
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
                        identity,
                        &upload,
                        "app:load-test",
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
                        identity,
                        &upload,
                        "app:load-test",
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
                    identity,
                    &upload,
                    "app:load-test",
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
                    identity,
                    &upload,
                    "app:load-test",
                    "linux/amd64",
                    &AtomicBool::new(false),
                )
                .await
                .unwrap();
                assert_eq!(operations.lock().unwrap().len(), 6);
                let wrong_id = format!("sha256:{}", "b".repeat(64));
                let mut wrong_archive = super::UploadedArchive {
                    directory: upload.directory.clone(),
                    path: upload.path.clone(),
                    bytes: upload.bytes,
                    sha256: upload.sha256.clone(),
                    image_ids: vec![wrong_id],
                };
                assert!(
                    super::load_archive(
                        material(Some(fingerprint.clone())),
                        identity,
                        &wrong_archive,
                        "app:load-test",
                        "linux/amd64",
                        &AtomicBool::new(false),
                    )
                    .await
                    .unwrap_err()
                    .contains("禁止部署")
                );
                wrong_archive.image_ids.clear();
                assert!(
                    super::validated_load_target(&wrong_archive, "app:load-test", "linux/amd64")
                        .is_err()
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
            let reserved = format!("/tmp/devingpanel.{}", uuid::Uuid::new_v4().simple());
            let identity = super::ReleaseDockerIdentity {
                endpoint: "unix:///var/run/docker.sock",
                daemon_id: "TEST:ID",
            };
            upload_fail_write.store(true, Ordering::SeqCst);
            let interrupted = Box::pin(super::transfer_archive_to(
                material(Some(fingerprint.clone())),
                &archive_path,
                bytes.len() as u64,
                &AtomicBool::new(false),
                &mut |_, _| {},
                super::ArchiveTransferOptions {
                    directory: Some(&reserved),
                    preserve_on_error: true,
                    expected_identity: Some(identity),
                },
            ))
            .await;
            assert!(interrupted.is_err());
            let path = format!("{reserved}/image.tar");
            assert!(upload_files.lock().unwrap().contains_key(&path));
            upload_fail_write.store(false, Ordering::SeqCst);
            let record = crate::release_store::UploadedArchiveRecord {
                service: "web".into(),
                directory: reserved.clone(),
                bytes: bytes.len() as u64,
                sha256: format!("{:x}", sha2::Sha256::digest(&bytes)),
                state: "reserved".into(),
            };
            upload_files
                .lock()
                .unwrap()
                .insert(path.clone(), bytes[..65_536].to_vec());
            upload_files.lock().unwrap().get_mut(&path).unwrap()[0] ^= 1;
            assert!(
                Box::pin(super::resume_uploaded_archive_on(
                    material(Some(fingerprint.clone())),
                    identity,
                    &record,
                    &archive_path,
                    &AtomicBool::new(false),
                    &mut |_, _| {},
                ))
                .await
                .err()
                .unwrap()
                .contains("前缀 SHA-256 不符")
            );
            assert_eq!(
                upload_files.lock().unwrap().get(&path).unwrap().len(),
                65_536
            );
            upload_files.lock().unwrap().get_mut(&path).unwrap()[0] ^= 1;
            upload_fail_after.store(100_000, Ordering::SeqCst);
            assert!(
                Box::pin(super::resume_uploaded_archive_on(
                    material(Some(fingerprint.clone())),
                    identity,
                    &record,
                    &archive_path,
                    &AtomicBool::new(false),
                    &mut |_, _| {},
                ))
                .await
                .is_err()
            );
            let partial = upload_files.lock().unwrap().get(&path).unwrap().clone();
            assert!(partial.len() >= 100_000 && partial.len() < bytes.len());
            assert_eq!(partial, bytes[..partial.len()]);
            upload_fail_after.store(0, Ordering::SeqCst);
            Box::pin(super::resume_uploaded_archive_on(
                material(Some(fingerprint.clone())),
                identity,
                &record,
                &archive_path,
                &AtomicBool::new(false),
                &mut |_, _| {},
            ))
            .await
            .unwrap();
            assert_eq!(upload_files.lock().unwrap().get(&path), Some(&bytes));
            let mut mismatched = record;
            mismatched.sha256 = "0".repeat(64);
            assert!(
                Box::pin(super::resume_uploaded_archive_on(
                    material(Some(fingerprint.clone())),
                    identity,
                    &mismatched,
                    &archive_path,
                    &AtomicBool::new(false),
                    &mut |_, _| {},
                ))
                .await
                .is_err()
            );
            assert_eq!(upload_files.lock().unwrap().get(&path), Some(&bytes));
            let cleanup_handle = super::trusted_handle(material(Some(fingerprint.clone())))
                .await
                .unwrap();
            super::archive_command(
                &cleanup_handle,
                &super::cleanup_archive_command(&reserved).unwrap(),
            )
            .await
            .unwrap();
            assert!(upload_files.lock().unwrap().is_empty());
            let missing = format!("/tmp/devingpanel.{}", uuid::Uuid::new_v4().simple());
            upload_fail_write.store(true, Ordering::SeqCst);
            assert!(
                Box::pin(super::transfer_archive_to(
                    material(Some(fingerprint.clone())),
                    &archive_path,
                    bytes.len() as u64,
                    &AtomicBool::new(false),
                    &mut |_, _| {},
                    super::ArchiveTransferOptions {
                        directory: Some(&missing),
                        preserve_on_error: true,
                        expected_identity: Some(identity),
                    },
                ))
                .await
                .is_err()
            );
            let missing_path = format!("{missing}/image.tar");
            assert!(upload_files.lock().unwrap().remove(&missing_path).is_some());
            upload_fail_write.store(false, Ordering::SeqCst);
            Box::pin(super::transfer_archive_to(
                material(Some(fingerprint.clone())),
                &archive_path,
                bytes.len() as u64,
                &AtomicBool::new(false),
                &mut |_, _| {},
                super::ArchiveTransferOptions {
                    directory: Some(&missing),
                    preserve_on_error: true,
                    expected_identity: Some(identity),
                },
            ))
            .await
            .unwrap();
            assert_eq!(
                upload_files.lock().unwrap().get(&missing_path),
                Some(&bytes)
            );
            super::archive_command(
                &cleanup_handle,
                &super::cleanup_archive_command(&missing).unwrap(),
            )
            .await
            .unwrap();
            assert!(upload_files.lock().unwrap().is_empty());
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
            let mut key_material = material(Some(fingerprint.clone()));
            key_material.host.auth = "key".into();
            key_material.credential = user_key
                .to_openssh(russh::keys::ssh_key::LineEnding::LF)
                .unwrap()
                .to_string();
            assert!(matches!(
                probe(key_material).await.unwrap(),
                TestResult::Connected { .. }
            ));
            let diagnostic = super::trusted_handle(material(Some(fingerprint)))
                .await
                .unwrap();
            assert_eq!(
                super::command_limited(&diagnostic, "diagnostic-exit-17", 1024)
                    .await
                    .unwrap_err(),
                "远端命令以退出码 17 结束。"
            );
            assert_eq!(
                super::command_limited(&diagnostic, "diagnostic-no-exit", 1024)
                    .await
                    .unwrap_err(),
                "远端 SSH 会话未返回命令退出状态。"
            );
            let _ = diagnostic
                .disconnect(russh::Disconnect::ByApplication, "", "en")
                .await;
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
