use crate::docker;
use crate::project_store::{NewProject, Project};
use crate::release::PlannedImage;
use serde_json::Value;
use std::collections::VecDeque;
use std::io::Read;
use std::process::Stdio;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread;
use std::time::{Duration, Instant};
use tokio::io::AsyncReadExt;

#[allow(dead_code)] // Wired into release execution after remote transfer is available.
pub struct BuiltImage {
    pub reference: String,
    pub platform: String,
    pub image_id: String,
    pub has_healthcheck: bool,
    pub logs: Vec<String>,
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
        if !value.is_empty() {
            line = line.replace(value, "[已隐藏]");
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
        let mut text = sanitized_line(&self.pending, args);
        if self.truncated {
            text.push_str(" …[单行截断]");
        }
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

fn inspect_built_image(
    endpoint: &str,
    reference: &str,
    platform: &str,
) -> Result<(String, bool), String> {
    let mut child = docker::command_for_endpoint(endpoint)
        .args(["image", "inspect", "--format",
            r#"{"Id":{{json .Id}},"Os":{{json .Os}},"Architecture":{{json .Architecture}},"Healthcheck":{{json (index .Config "Healthcheck")}}}"#,
            reference])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| "无法检查已构建镜像。")?;
    let stdout = child.stdout.take().expect("stdout was piped");
    let reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout
            .take(64 * 1024 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| "无法读取镜像元数据。")?;
        Ok::<_, &str>(bytes)
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(50)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("构建后镜像核验超时，结果待核实。".into());
            }
        }
    };
    let bytes = reader.join().map_err(|_| "无法读取镜像元数据。")??;
    if bytes.len() > 64 * 1024 || !status.success() {
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
    let endpoint = docker::local_engine_endpoint().map_err(|message| fail(message, Vec::new()))?;
    if docker::image_reference_exists_on(&endpoint, &image.reference)
        .map_err(|message| fail(message, Vec::new()))?
    {
        return Err(fail("本地镜像 tag 已存在，构建已阻断。".into(), Vec::new()));
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
    let (image_id, has_healthcheck) =
        inspect_built_image(&endpoint, &image.reference, &image.platform)
            .map_err(|message| fail(message, logs.clone()))?;
    Ok(BuiltImage {
        reference: image.reference.clone(),
        platform: image.platform.clone(),
        image_id,
        has_healthcheck,
        logs,
    })
}

#[cfg(test)]
mod tests {
    use super::{build_image, sanitized_line};
    use crate::project_store::Project;
    use crate::release::PlannedImage;
    use std::sync::{Arc, atomic::AtomicBool};

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
        assert!(removal.success(), "isolated test image was not removed");
    }
}
