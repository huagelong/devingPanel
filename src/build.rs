use crate::docker;
use crate::preferences;
use crate::project_store::{NewProject, Project};
use crate::release::PlannedImage;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;

#[allow(dead_code)] // Wired into release execution after remote transfer is available.
pub struct BuiltImage {
    pub reference: String,
    pub platform: String,
    pub image_id: String,
    pub has_healthcheck: bool,
    pub logs: Vec<String>,
}

#[allow(dead_code)] // The release coordinator will pass this to the SFTP adapter.
pub struct SavedArchive {
    pub path: PathBuf,
    pub bytes: u64,
    pub sha256: String,
    pub image_ids: Vec<String>,
}

impl Drop for SavedArchive {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[allow(dead_code)] // Wired into release execution after remote transfer is available.
pub struct BuildFailure {
    pub message: String,
    pub logs: Vec<String>,
}

fn sanitized_line(bytes: &[u8], build_args: &[(String, String)]) -> String {
    let mut line: String = String::from_utf8_lossy(bytes)
        .chars()
        .filter(|ch| *ch == '\t' || !ch.is_control())
        .collect();
    for (_, value) in build_args {
        for fragment in value.lines() {
            let fragment: String = fragment
                .chars()
                .filter(|ch| *ch == '\t' || !ch.is_control())
                .collect();
            if !fragment.is_empty() {
                line = line.replace(&fragment, "[已隐藏]");
            }
        }
    }
    line
}

#[derive(Default)]
struct BuildLineBuffer {
    pending: Vec<u8>,
    truncated: bool,
}

impl BuildLineBuffer {
    fn push(
        &mut self,
        data: &[u8],
        args: &[(String, String)],
        logs: &mut VecDeque<String>,
        on_log: &mut dyn FnMut(&str),
    ) {
        for byte in data {
            if *byte == b'\n' {
                self.flush(args, logs, on_log);
            } else if self.pending.len() < 16 * 1024 {
                self.pending.push(*byte);
            } else {
                self.truncated = true;
            }
        }
    }

    fn flush(
        &mut self,
        args: &[(String, String)],
        logs: &mut VecDeque<String>,
        on_log: &mut dyn FnMut(&str),
    ) {
        let text = if self.truncated {
            "[单行过长，已隐藏]".into()
        } else {
            sanitized_line(&self.pending, args)
        };
        on_log(&text);
        if logs.len() == 1000 {
            logs.pop_front();
        }
        logs.push_back(text);
        self.pending.clear();
        self.truncated = false;
    }

    fn finish(
        &mut self,
        args: &[(String, String)],
        logs: &mut VecDeque<String>,
        on_log: &mut dyn FnMut(&str),
    ) {
        if !self.pending.is_empty() || self.truncated {
            self.flush(args, logs, on_log);
        }
    }
}

fn acquire_build_lock(cancel: &AtomicBool) -> Result<File, String> {
    let directory = preferences::data_dir().ok_or("无法确定本机配置目录。")?;
    std::fs::create_dir_all(&directory).map_err(|_| "无法准备构建锁目录。")?;
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(directory.join("local-build.lock"))
        .map_err(|_| "无法打开本机构建锁。")?;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err("构建已取消。".into());
        }
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(std::fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50))
            }
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err("另一客户端正在构建，请稍后重试。".into());
            }
            Err(_) => return Err("无法锁定本机构建。".into()),
        }
    }
}

fn inspect_built_image(
    endpoint: &str,
    reference: &str,
    platform: &str,
) -> Result<(String, bool), String> {
    let mut command = docker::command_for_endpoint(endpoint);
    command
        .args(["image", "inspect", "--format",
            r#"{"Id":{{json .Id}},"Os":{{json .Os}},"Architecture":{{json .Architecture}},"Healthcheck":{{json (index .Config "Healthcheck")}}}"#,
            reference])
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let output = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| "无法初始化镜像核验环境。")?
        .block_on(async {
            let mut command = tokio::process::Command::from(command);
            command.kill_on_drop(true);
            let mut child = command.spawn().map_err(|_| "无法检查已构建镜像。")?;
            let stdout = child.stdout.take().expect("stdout was piped");
            let mut limited = stdout.take(64 * 1024 + 1);
            let mut bytes = Vec::new();
            tokio::time::timeout(Duration::from_secs(10), async {
                let (read, status) =
                    tokio::try_join!(limited.read_to_end(&mut bytes), child.wait(),)
                        .map_err(|_| "无法读取镜像元数据。")?;
                Ok::<_, &str>((read, status, bytes))
            })
            .await
            .map_err(|_| "构建后镜像核验超时，结果待核实。")?
        })?;
    let (read, status, bytes) = output;
    if read > 64 * 1024 || !status.success() {
        return Err("构建后镜像元数据不可用，结果待核实。".into());
    }
    let row: Value = serde_json::from_slice(&bytes).map_err(|_| "构建后镜像元数据无效。")?;
    let image_id = row
        .get("Id")
        .and_then(Value::as_str)
        .ok_or("镜像 ID 缺失。")?;
    if image_id.len() != 71
        || !image_id.starts_with("sha256:")
        || !image_id[7..].bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err("镜像 ID 无效。".into());
    }
    let actual = format!(
        "{}/{}",
        row.get("Os")
            .and_then(Value::as_str)
            .ok_or("镜像系统信息缺失。")?,
        row.get("Architecture")
            .and_then(Value::as_str)
            .ok_or("镜像架构信息缺失。")?
    );
    if actual != platform {
        return Err(format!(
            "构建镜像架构为 {actual}，与目标 {platform} 不一致。"
        ));
    }
    let healthcheck = row.pointer("/Healthcheck/Test").and_then(Value::as_array);
    let has_healthcheck = healthcheck.is_some_and(|test| {
        !test.is_empty() && test.first().and_then(Value::as_str) != Some("NONE")
    });
    Ok((image_id.to_owned(), has_healthcheck))
}

fn archive_image_ids(path: &Path, reference: &str, platform: &str) -> Result<Vec<String>, String> {
    let file = File::open(path).map_err(|_| "无法读取镜像归档元数据。")?;
    let mut archive = tar::Archive::new(file);
    let mut index = None;
    let mut manifest = None;
    for entry in archive.entries().map_err(|_| "镜像归档格式无效。")? {
        let entry = entry.map_err(|_| "镜像归档内容无效。")?;
        let target = match entry.path_bytes().as_ref() {
            b"index.json" => &mut index,
            b"manifest.json" => &mut manifest,
            _ => continue,
        };
        if target.is_some() {
            return Err("镜像归档元数据重复。".into());
        }
        let mut bytes = Vec::new();
        entry
            .take(64 * 1024 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| "无法读取镜像归档元数据。")?;
        if bytes.len() > 64 * 1024 {
            return Err("镜像归档元数据过大。".into());
        }
        *target = Some(bytes);
    }
    let manifest: Value = serde_json::from_slice(&manifest.ok_or("镜像归档缺少 manifest。")?)
        .map_err(|_| "镜像归档 manifest 无效。")?;
    let rows = manifest.as_array().ok_or("镜像归档 manifest 无效。")?;
    if rows.len() != 1
        || !rows[0]
            .get("RepoTags")
            .and_then(Value::as_array)
            .is_some_and(|tags| tags.iter().any(|tag| tag.as_str() == Some(reference)))
    {
        return Err("镜像归档引用与构建结果不一致。".into());
    }
    let config = rows[0]
        .get("Config")
        .and_then(Value::as_str)
        .ok_or("镜像归档配置标识缺失。")?;
    let config_hash = config
        .strip_prefix("blobs/sha256/")
        .or_else(|| config.strip_suffix(".json"))
        .ok_or("镜像归档配置标识无效。")?;
    let mut ids = vec![format!("sha256:{config_hash}")];
    if let Some(index) = index {
        let index: Value = serde_json::from_slice(&index).map_err(|_| "镜像归档索引无效。")?;
        let expected = platform.split_once('/').ok_or("镜像归档平台无效。")?;
        let matches = index
            .get("manifests")
            .and_then(Value::as_array)
            .ok_or("镜像归档索引缺少 manifest。")?
            .iter()
            .filter(|item| {
                item.pointer("/platform/os").and_then(Value::as_str) == Some(expected.0)
                    && item
                        .pointer("/platform/architecture")
                        .and_then(Value::as_str)
                        == Some(expected.1)
            })
            .filter_map(|item| item.get("digest").and_then(Value::as_str))
            .collect::<Vec<_>>();
        if matches.len() != 1 {
            return Err("镜像归档平台索引不唯一。".into());
        }
        ids.push(matches[0].to_owned());
    }
    if ids.iter().any(|id| {
        id.len() != 71
            || !id.starts_with("sha256:")
            || !id[7..].bytes().all(|byte| byte.is_ascii_hexdigit())
    }) {
        return Err("镜像归档标识无效。".into());
    }
    Ok(ids)
}

async fn execute_build(
    command: std::process::Command,
    args: &[(String, String)],
    cancel: &AtomicBool,
    on_log: &mut dyn FnMut(&str),
) -> Result<Vec<String>, BuildFailure> {
    let mut command = tokio::process::Command::from(command);
    command.kill_on_drop(true);
    let mut child = command.spawn().map_err(|_| BuildFailure {
        message: "无法启动本地 Docker 构建。".into(),
        logs: Vec::new(),
    })?;
    let mut stdout = child.stdout.take().expect("stdout was piped");
    let mut stderr = child.stderr.take().expect("stderr was piped");
    let mut out_buffer = BuildLineBuffer::default();
    let mut err_buffer = BuildLineBuffer::default();
    let mut logs = VecDeque::new();
    let mut out_open = true;
    let mut err_open = true;
    let mut status = None;
    let mut exited_at = None;
    let deadline = Instant::now() + Duration::from_secs(30 * 60);
    let mut stopped = None;
    loop {
        if cancel.load(Ordering::Relaxed) {
            stopped = Some("构建已取消，镜像结果待核实。");
            break;
        }
        if Instant::now() >= deadline {
            stopped = Some("构建超时，镜像结果待核实。");
            break;
        }
        if status.is_none() {
            match child.try_wait() {
                Ok(Some(exit)) => {
                    status = Some(exit);
                    exited_at = Some(Instant::now());
                }
                Ok(None) => {}
                Err(_) => {
                    stopped = Some("本地构建进程状态待核实。");
                    break;
                }
            }
        }
        if status.is_some() && !out_open && !err_open {
            break;
        }
        if exited_at.is_some_and(|when: Instant| when.elapsed() >= Duration::from_secs(2)) {
            stopped = Some("构建进程已结束但日志管道未关闭，镜像结果待核实。");
            break;
        }
        let mut out_chunk = [0u8; 4096];
        let mut err_chunk = [0u8; 4096];
        tokio::select! {
            result = stdout.read(&mut out_chunk), if out_open => match result {
                Ok(0) => out_open = false,
                Ok(size) => out_buffer.push(&out_chunk[..size], args, &mut logs, on_log),
                Err(_) => { stopped = Some("无法读取构建日志，镜像结果待核实。"); break; }
            },
            result = stderr.read(&mut err_chunk), if err_open => match result {
                Ok(0) => err_open = false,
                Ok(size) => err_buffer.push(&err_chunk[..size], args, &mut logs, on_log),
                Err(_) => { stopped = Some("无法读取构建日志，镜像结果待核实。"); break; }
            },
            _ = tokio::time::sleep(Duration::from_millis(50)) => {},
        }
    }
    if stopped.is_some() {
        let _ = child.start_kill();
        let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
    }
    out_buffer.finish(args, &mut logs, on_log);
    err_buffer.finish(args, &mut logs, on_log);
    let logs: Vec<_> = logs.into_iter().collect();
    if let Some(message) = stopped {
        return Err(BuildFailure {
            message: message.into(),
            logs,
        });
    }
    if !status.is_some_and(|status: std::process::ExitStatus| status.success()) {
        return Err(BuildFailure {
            message: "本地 Docker 构建失败；请核对构建日志。".into(),
            logs,
        });
    }
    Ok(logs)
}

#[allow(dead_code)] // Wired into release execution after remote transfer is available.
pub fn build_image(
    project: &Project,
    image: &PlannedImage,
    cancel: Arc<AtomicBool>,
    mut on_log: impl FnMut(&str),
) -> Result<BuiltImage, BuildFailure> {
    let fail = |message: String, logs: Vec<String>| BuildFailure { message, logs };
    NewProject {
        name: project.name.clone(),
        path: project.path.clone(),
        dockerfile: project.dockerfile.clone(),
    }
    .normalize()
    .map_err(|message| fail(message, Vec::new()))?;
    if !matches!(image.platform.as_str(), "linux/amd64" | "linux/arm64")
        || !image.reference.starts_with(&format!("{}:", project.image))
    {
        return Err(fail("构建目标镜像或平台无效。".into(), Vec::new()));
    }
    let _build_lock = acquire_build_lock(&cancel).map_err(|message| fail(message, Vec::new()))?;
    let cancelled = || cancel.load(Ordering::Relaxed);
    let endpoint = docker::local_engine_endpoint().map_err(|message| fail(message, Vec::new()))?;
    if cancelled() {
        return Err(fail("构建已取消。".into(), Vec::new()));
    }
    let platforms =
        docker::build_platforms_on(&endpoint).map_err(|message| fail(message, Vec::new()))?;
    if cancelled() {
        return Err(fail("构建已取消。".into(), Vec::new()));
    }
    if !platforms.contains(&image.platform) {
        return Err(fail(
            "本机 default 构建器不支持目标平台。".into(),
            Vec::new(),
        ));
    }
    if docker::image_reference_exists_on(&endpoint, &image.reference)
        .map_err(|message| fail(message, Vec::new()))?
    {
        return Err(fail("本地镜像 tag 已存在，构建已阻断。".into(), Vec::new()));
    }
    if cancelled() {
        return Err(fail("构建已取消。".into(), Vec::new()));
    }
    let mut command = docker::command_for_endpoint(&endpoint);
    command
        .args([
            "buildx",
            "build",
            "--builder",
            "default",
            "--progress",
            "plain",
            "--load",
            "--platform",
            &image.platform,
            "--tag",
            &image.reference,
            "--file",
        ])
        .arg(std::path::Path::new(&project.path).join(&project.dockerfile));
    for (name, value) in &project.build_args {
        if value.contains('\0') {
            return Err(fail("Build args 包含无效字符。".into(), Vec::new()));
        }
        command.arg("--build-arg").arg(format!("{name}={value}"));
    }
    command
        .arg(".")
        .current_dir(&project.path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let logs = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| fail("无法初始化本机构建环境。".into(), Vec::new()))?
        .block_on(execute_build(
            command,
            &project.build_args,
            &cancel,
            &mut on_log,
        ))?;
    if cancelled() {
        return Err(fail("构建已取消，镜像结果待核实。".into(), logs));
    }
    let (image_id, has_healthcheck) =
        inspect_built_image(&endpoint, &image.reference, &image.platform)
            .map_err(|message| fail(message, logs.clone()))?;
    if cancelled() {
        return Err(fail("构建已取消，镜像结果待核实。".into(), logs));
    }
    Ok(BuiltImage {
        reference: image.reference.clone(),
        platform: image.platform.clone(),
        image_id,
        has_healthcheck,
        logs,
    })
}

async fn write_archive(
    mut command: std::process::Command,
    path: &Path,
    cancel: &AtomicBool,
) -> Result<(), String> {
    command.stdout(Stdio::piped()).stderr(Stdio::null());
    let mut child = tokio::process::Command::from(command)
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| "无法启动本地 Docker 镜像导出。")?;
    let mut stdout = child.stdout.take().expect("stdout was piped");
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path).map_err(|_| "无法创建本地镜像归档。")?;
    let mut file = tokio::fs::File::from_std(file);
    let deadline = Instant::now() + Duration::from_secs(60 * 60);
    let result = {
        let copy = tokio::io::copy(&mut stdout, &mut file);
        tokio::pin!(copy);
        loop {
            if cancel.load(Ordering::Relaxed) {
                break Err("镜像导出已取消。".into());
            }
            if Instant::now() >= deadline {
                break Err("镜像导出超时。".into());
            }
            tokio::select! {
                result = &mut copy => break result.map(|_| ()).map_err(|_| "本地镜像归档写入失败。".into()),
                _ = tokio::time::sleep(Duration::from_millis(100)) => {},
            }
        }
    };
    if let Err(error) = result {
        let _ = child.start_kill();
        let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
        return Err(error);
    }
    let exit_deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if cancel.load(Ordering::Relaxed) {
            let _ = child.start_kill();
            let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
            return Err("镜像导出已取消。".into());
        }
        if Instant::now() >= exit_deadline {
            let _ = child.start_kill();
            let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
            return Err("Docker 导出进程退出超时，结果待核实。".into());
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => tokio::time::sleep(Duration::from_millis(100)).await,
            Err(_) => return Err("Docker 导出进程状态待核实。".into()),
        }
    };
    if !status.success() {
        return Err("本地 Docker 镜像导出失败。".into());
    }
    file.flush().await.map_err(|_| "无法刷新镜像归档。")?;
    file.sync_all().await.map_err(|_| "无法保存镜像归档。")?;
    Ok(())
}

#[allow(dead_code)] // Called by the release coordinator after a successful local build.
pub fn save_image(image: &BuiltImage, cancel: Arc<AtomicBool>) -> Result<SavedArchive, String> {
    if cancel.load(Ordering::Relaxed) {
        return Err("镜像导出已取消。".into());
    }
    let endpoint = docker::local_engine_endpoint()?;
    let (before, _) = inspect_built_image(&endpoint, &image.reference, &image.platform)?;
    if before != image.image_id {
        return Err("本地镜像引用已变化，导出已阻断。".into());
    }
    let path = std::env::temp_dir().join(format!(
        "devingpanel-image-{}.tar",
        uuid::Uuid::new_v4().simple()
    ));
    let mut archive = SavedArchive {
        path,
        bytes: 0,
        sha256: String::new(),
        image_ids: Vec::new(),
    };
    let mut command = docker::command_for_endpoint(&endpoint);
    command.args([
        "image",
        "save",
        "--platform",
        &image.platform,
        &image.reference,
    ]);
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| "无法初始化镜像导出环境。")?
        .block_on(write_archive(command, &archive.path, &cancel))?;
    if cancel.load(Ordering::Relaxed) {
        return Err("镜像导出已取消。".into());
    }
    let (after, _) = inspect_built_image(&endpoint, &image.reference, &image.platform)?;
    if after != before {
        return Err("导出期间本地镜像引用已变化，归档不可信。".into());
    }
    let mut file = File::open(&archive.path).map_err(|_| "无法校验镜像归档。")?;
    let mut digest = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err("镜像归档校验已取消。".into());
        }
        let size = std::io::Read::read(&mut file, &mut buffer).map_err(|_| "无法读取镜像归档。")?;
        if size == 0 {
            break;
        }
        archive.bytes += size as u64;
        digest.update(&buffer[..size]);
    }
    if archive.bytes == 0 {
        return Err("本地镜像归档为空。".into());
    }
    archive.image_ids = archive_image_ids(&archive.path, &image.reference, &image.platform)?;
    archive.sha256 = format!("{:x}", digest.finalize());
    Ok(archive)
}

#[cfg(test)]
mod tests {
    use super::{
        BuildLineBuffer, SavedArchive, archive_image_ids, build_image, execute_build,
        sanitized_line, save_image, write_archive,
    };
    use crate::project_store::Project;
    use crate::release::PlannedImage;
    use std::collections::VecDeque;
    use std::fs::File;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    use std::time::{Duration, Instant};

    #[test]
    fn streams_output_and_cancels_build_promptly() {
        let cancel = AtomicBool::new(false);
        #[cfg(windows)]
        let mut command = {
            let mut command = std::process::Command::new("powershell");
            command.args([
                "-NoProfile",
                "-Command",
                "Write-Output ready; Start-Sleep -Seconds 5",
            ]);
            command
        };
        #[cfg(not(windows))]
        let mut command = {
            let mut command = std::process::Command::new("sh");
            command.args(["-c", "printf 'ready\\n'; sleep 5"]);
            command
        };
        command
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let start = Instant::now();
        let result = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(execute_build(command, &[], &cancel, &mut |line| {
                if line == "ready" {
                    cancel.store(true, Ordering::Relaxed);
                }
            }));
        assert!(start.elapsed() < Duration::from_secs(4));
        let failure = match result {
            Err(failure) => failure,
            Ok(_) => panic!("cancelled build must not succeed"),
        };
        assert!(failure.message.contains("取消"));
        assert!(failure.logs.iter().any(|line| line == "ready"));
    }

    #[test]
    fn masks_build_arg_values_in_retained_log() {
        assert_eq!(
            sanitized_line(
                b"TOKEN=private-value\x1b[31m",
                &[("TOKEN".into(), "private-value".into())]
            ),
            "TOKEN=[已隐藏][31m"
        );
    }

    #[test]
    fn fragmented_and_long_build_lines_never_echo_unredacted_fragments() {
        let mut buffer = BuildLineBuffer::default();
        let mut logs = VecDeque::new();
        let mut shown = Vec::new();
        let args = [("TOKEN".into(), "first\nsecond".into())];
        let mut display = |line: &str| shown.push(line.to_owned());
        buffer.push(b"value=fir", &args, &mut logs, &mut display);
        buffer.push(b"st\nvalue=second\n", &args, &mut logs, &mut display);
        assert_eq!(
            logs.iter().map(String::as_str).collect::<Vec<_>>(),
            ["value=[已隐藏]", "value=[已隐藏]"]
        );
        buffer.push(&vec![b'x'; 17 * 1024], &args, &mut logs, &mut display);
        buffer.push(b"\n", &args, &mut logs, &mut display);
        assert_eq!(shown.last().unwrap(), "[单行过长，已隐藏]");
        assert_eq!(logs.len(), 3);
    }

    #[test]
    fn cancelled_archive_export_stops_promptly() {
        let path = std::env::temp_dir().join(format!(
            "devingpanel-archive-cancel-{}.tar",
            uuid::Uuid::new_v4().simple()
        ));
        let cancel = AtomicBool::new(true);
        #[cfg(windows)]
        let command = {
            let mut command = std::process::Command::new("powershell");
            command.args(["-NoProfile", "-Command", "Start-Sleep -Seconds 5"]);
            command
        };
        #[cfg(not(windows))]
        let command = {
            let mut command = std::process::Command::new("sh");
            command.args(["-c", "sleep 5"]);
            command
        };
        let start = Instant::now();
        let result = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(write_archive(command, &path, &cancel));
        assert!(result.unwrap_err().contains("取消"));
        assert!(start.elapsed() < Duration::from_secs(4));
        drop(SavedArchive {
            path: path.clone(),
            bytes: 0,
            sha256: String::new(),
            image_ids: Vec::new(),
        });
        assert!(!path.exists());
    }

    #[test]
    fn export_metadata_tracks_platform_manifest_and_config_ids() {
        let path = std::env::temp_dir().join(format!(
            "devingpanel-index-test-{}.tar",
            uuid::Uuid::new_v4().simple()
        ));
        let config = "a".repeat(64);
        let manifest_id = "b".repeat(64);
        let reference = "devingpanel/test:one";
        {
            let mut archive = tar::Builder::new(File::create(&path).unwrap());
            for (name, bytes) in [
                (
                    "manifest.json",
                    format!(
                        r#"[{{"Config":"blobs/sha256/{config}","RepoTags":["{reference}"],"Layers":[]}}]"#
                    ),
                ),
                (
                    "index.json",
                    format!(
                        r#"{{"manifests":[{{"digest":"sha256:{manifest_id}","platform":{{"os":"linux","architecture":"amd64"}}}}]}}"#
                    ),
                ),
            ] {
                let mut header = tar::Header::new_gnu();
                header.set_size(bytes.len() as u64);
                header.set_mode(0o644);
                header.set_cksum();
                archive
                    .append_data(&mut header, name, bytes.as_bytes())
                    .unwrap();
            }
            archive.finish().unwrap();
        }
        assert_eq!(
            archive_image_ids(&path, reference, "linux/amd64").unwrap(),
            [format!("sha256:{config}"), format!("sha256:{manifest_id}")]
        );
        assert!(archive_image_ids(&path, reference, "linux/arm64").is_err());
        assert!(archive_image_ids(&path, "different:tag", "linux/amd64").is_err());
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    #[ignore = "creates and removes one uniquely tagged local Docker image"]
    fn builds_and_inspects_isolated_local_image() {
        let dir = std::env::temp_dir().join(format!("deving-panel-build-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(
            dir.join("Dockerfile"),
            "FROM scratch\nCOPY marker.txt /marker.txt\n",
        )
        .unwrap();
        std::fs::write(dir.join("marker.txt"), "smoke\n").unwrap();
        let reference = format!(
            "devingpanel/build-smoke:{}-amd64",
            uuid::Uuid::new_v4().simple()
        );
        let project = Project {
            id: 1,
            name: "smoke".into(),
            path: dir.to_string_lossy().into_owned(),
            dockerfile: "Dockerfile".into(),
            image: "devingpanel/build-smoke".into(),
            tag_rule: "{timestamp}".into(),
            platform: "linux/amd64".into(),
            service: "web".into(),
            compose_files: Vec::new(),
            missing_compose_files: Vec::new(),
            build_args: Vec::new(),
            host_ids: Vec::new(),
            manifest_mode: false,
            config_error: None,
        };
        let image = PlannedImage {
            platform: "linux/amd64".into(),
            reference: reference.clone(),
        };
        let endpoint = crate::docker::local_engine_endpoint().unwrap();
        let mut streamed_lines = 0;
        let result = build_image(&project, &image, Arc::new(AtomicBool::new(false)), |_| {
            streamed_lines += 1;
        });
        let saved = result.as_ref().ok().map(|built| {
            let archive = save_image(built, Arc::new(AtomicBool::new(false))).unwrap();
            assert!(archive.bytes > 0);
            use sha2::Digest;
            let actual = sha2::Sha256::digest(std::fs::read(&archive.path).unwrap());
            assert_eq!(archive.sha256, format!("{actual:x}"));
            assert!(!archive.image_ids.is_empty());
            let path = archive.path.clone();
            drop(archive);
            assert!(!path.exists());
        });
        let removal = crate::docker::command_for_endpoint(&endpoint)
            .args(["image", "rm", &reference])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        std::fs::remove_file(dir.join("Dockerfile")).unwrap();
        std::fs::remove_file(dir.join("marker.txt")).unwrap();
        std::fs::remove_dir(dir).unwrap();
        let built =
            result.unwrap_or_else(|error| panic!("{}\n{}", error.message, error.logs.join("\n")));
        assert_eq!(built.platform, "linux/amd64");
        assert!(built.image_id.starts_with("sha256:"));
        assert!(!built.has_healthcheck);
        assert!(streamed_lines > 0);
        assert!(saved.is_some());
        assert!(removal.success(), "isolated test image was not removed");
    }
}
