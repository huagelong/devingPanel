use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

pub enum DockerStatus {
    Checking,
    Available(String),
    Unavailable,
}

impl DockerStatus {
    pub fn label(&self) -> String {
        match self {
            Self::Checking => "正在检测 Docker".into(),
            Self::Available(_) => "本地 Docker 可用".into(),
            Self::Unavailable => "未检测到 Docker".into(),
        }
    }
}

pub fn executable_path() -> Option<PathBuf> {
    let name = if cfg!(windows) {
        "docker.exe"
    } else {
        "docker"
    };
    std::env::split_paths(&std::env::var_os("PATH")?).find_map(|directory| {
        let path = directory.join(name);
        path.is_file().then_some(path)
    })
}

fn validate_local_endpoint(endpoint: &str) -> Result<String, String> {
    let local = if cfg!(windows) {
        endpoint.starts_with("npipe:////./pipe/") || endpoint.starts_with("unix:///")
    } else {
        endpoint.starts_with("unix:///")
    };
    if !local || endpoint.len() > 1024 || endpoint.chars().any(char::is_control) {
        return Err("本地 Docker 连接目标不是本机 socket，构建已阻断。".into());
    }
    Ok(endpoint.to_owned())
}

pub fn local_engine_endpoint() -> Result<String, String> {
    if std::env::var_os("DOCKER_CONTEXT").is_none()
        && let Some(endpoint) = std::env::var_os("DOCKER_HOST")
    {
        return validate_local_endpoint(&endpoint.to_string_lossy());
    }
    let mut child = crate::local_process::command("docker")
        .args([
            "context",
            "inspect",
            "--format",
            "{{json .Endpoints.docker.Host}}",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| "无法核实本地 Docker context。")?;
    let stdout = child.stdout.take().expect("stdout was piped");
    let reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout
            .take(4097)
            .read_to_end(&mut bytes)
            .map_err(|_| "无法读取 Docker context。")?;
        Ok::<_, &str>(bytes)
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(50)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("Docker context 检查超时。".into());
            }
        }
    };
    let bytes = reader.join().map_err(|_| "无法读取 Docker context。")??;
    if bytes.len() > 4096 || !status.success() {
        return Err("无法核实本地 Docker context。".into());
    }
    let endpoint: String =
        serde_json::from_slice(&bytes).map_err(|_| "Docker context 端点格式无效。")?;
    validate_local_endpoint(&endpoint)
}

pub fn command_for_endpoint(endpoint: &str) -> Command {
    let mut command = crate::local_process::command("docker");
    command
        .arg("--host")
        .arg(endpoint)
        .env_remove("DOCKER_HOST")
        .env_remove("DOCKER_CONTEXT")
        .env_remove("BUILDX_BUILDER");
    command
}

fn parse_build_platforms(text: &str) -> Result<Vec<String>, String> {
    let mut platforms = std::collections::BTreeSet::new();
    let mut current = false;
    for line in text.lines() {
        let row: serde_json::Value =
            serde_json::from_str(line).map_err(|_| "Buildx 构建器信息不是有效 JSON。")?;
        if row.get("Name").and_then(serde_json::Value::as_str) != Some("default")
            || row.get("Driver").and_then(serde_json::Value::as_str) != Some("docker")
        {
            continue;
        }
        current = true;
        let nodes = row
            .get("Nodes")
            .and_then(serde_json::Value::as_array)
            .ok_or("Buildx 构建器节点信息无效。")?;
        for node in nodes {
            if node.get("Status").and_then(serde_json::Value::as_str) != Some("running")
                || node.get("Endpoint").and_then(serde_json::Value::as_str) != Some("default")
            {
                continue;
            }
            for platform in node
                .get("Platforms")
                .and_then(serde_json::Value::as_array)
                .ok_or("Buildx 平台信息无效。")?
            {
                if let Some(platform @ ("linux/amd64" | "linux/arm64")) = platform.as_str() {
                    platforms.insert(platform.to_owned());
                }
            }
        }
    }
    if !current || platforms.is_empty() {
        return Err("当前 Buildx 构建器没有可用的 linux/amd64 或 linux/arm64 节点。".into());
    }
    Ok(platforms.into_iter().collect())
}

pub fn build_platforms() -> Result<Vec<String>, String> {
    let endpoint = local_engine_endpoint()?;
    build_platforms_on(&endpoint)
}

pub fn build_platforms_on(endpoint: &str) -> Result<Vec<String>, String> {
    let mut child = command_for_endpoint(endpoint)
        .args(["buildx", "ls", "--format", "{{json .}}"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| "本地 Docker Buildx 不可用。")?;
    let stdout = child.stdout.take().expect("stdout was piped");
    let reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout
            .take(2 * 1024 * 1024 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| "无法读取 Buildx 能力。")?;
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
                return Err("Buildx 能力检测超时。".into());
            }
        }
    };
    let bytes = reader.join().map_err(|_| "无法读取 Buildx 能力。")??;
    if bytes.len() > 2 * 1024 * 1024 || !status.success() {
        return Err("Buildx 能力检测失败或输出过大。".into());
    }
    let text = String::from_utf8(bytes).map_err(|_| "Buildx 输出编码无效。")?;
    parse_build_platforms(&text)
}

pub fn image_reference_exists(reference: &str) -> Result<bool, String> {
    image_reference_exists_on(&local_engine_endpoint()?, reference)
}

pub fn image_reference_exists_on(endpoint: &str, reference: &str) -> Result<bool, String> {
    let (_, tag) = reference.rsplit_once(':').ok_or("镜像引用无效。")?;
    let mut child = command_for_endpoint(endpoint)
        .args([
            "image",
            "ls",
            "--filter",
            &format!("reference={reference}"),
            "--format",
            "{{json .}}",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| "无法查询本地镜像。")?;
    let stdout = child.stdout.take().expect("stdout was piped");
    let reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout
            .take(64 * 1024 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| "无法读取本地镜像列表。")?;
        Ok::<_, &str>(bytes)
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(50)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("本地镜像检查超时。".into());
            }
        }
    };
    let bytes = reader.join().map_err(|_| "无法读取本地镜像列表。")??;
    if bytes.len() > 64 * 1024 || !status.success() {
        return Err("本地镜像检查失败或输出过大。".into());
    }
    let text = String::from_utf8(bytes).map_err(|_| "本地镜像列表编码无效。")?;
    filtered_rows_contain_tag(&text, tag)
}

fn filtered_rows_contain_tag(text: &str, tag: &str) -> Result<bool, String> {
    for line in text.lines() {
        let row: serde_json::Value =
            serde_json::from_str(line).map_err(|_| "本地镜像列表格式无效。")?;
        if row
            .get("Repository")
            .and_then(serde_json::Value::as_str)
            .is_none()
        {
            return Err("本地镜像列表格式无效。".into());
        }
        let listed_tag = row
            .get("Tag")
            .and_then(serde_json::Value::as_str)
            .ok_or("本地镜像列表格式无效。")?;
        if listed_tag == tag {
            return Ok(true);
        }
    }
    Ok(false)
}

pub fn probe() -> DockerStatus {
    let Ok(endpoint) = local_engine_endpoint() else {
        return DockerStatus::Unavailable;
    };
    let Ok(mut child) = command_for_endpoint(&endpoint)
        .args(["info", "--format", "{{.ServerVersion}}"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    else {
        return DockerStatus::Unavailable;
    };

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(50)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return DockerStatus::Unavailable;
            }
        }
    }

    match child.wait_with_output() {
        Ok(output) if output.status.success() => {
            let version = String::from_utf8_lossy(&output.stdout).trim().to_owned();
            if version.is_empty() {
                DockerStatus::Unavailable
            } else {
                DockerStatus::Available(version)
            }
        }
        _ => DockerStatus::Unavailable,
    }
}

#[cfg(test)]
mod tests {
    use super::{filtered_rows_contain_tag, parse_build_platforms};
    use std::process::{Command, Stdio};

    #[test]
    fn reads_only_local_default_builder_platforms() {
        let rows = concat!(
            "{\"Name\":\"remote\",\"Driver\":\"remote\",\"Current\":true,\"Nodes\":[{\"Status\":\"running\",\"Endpoint\":\"remote\",\"Platforms\":[\"linux/arm64\"]}]}\n",
            "{\"Name\":\"default\",\"Driver\":\"docker\",\"Current\":false,\"Nodes\":[{\"Status\":\"stopped\",\"Endpoint\":\"default\",\"Platforms\":[\"linux/arm64\"]},{\"Status\":\"running\",\"Endpoint\":\"default\",\"Platforms\":[\"linux/amd64\",\"linux/amd64/v2\"]}]}\n"
        );
        assert_eq!(parse_build_platforms(rows).unwrap(), vec!["linux/amd64"]);
        assert!(parse_build_platforms("not-json").is_err());
        assert!(
            parse_build_platforms("{\"Name\":\"default\",\"Driver\":\"docker\",\"Nodes\":[]}\n")
                .is_err()
        );
        assert!(super::validate_local_endpoint("tcp://remote:2376").is_err());
        assert!(super::validate_local_endpoint("unix:///run/user/1000/docker.sock").is_ok());
    }

    #[test]
    fn qualified_docker_hub_alias_is_a_collision() {
        let filtered = "{\"Repository\":\"postgres\",\"Tag\":\"16-alpine\"}\n";
        assert!(filtered_rows_contain_tag(filtered, "16-alpine").unwrap());
        assert!(!filtered_rows_contain_tag(filtered, "17-alpine").unwrap());
        assert!(filtered_rows_contain_tag("not-json", "16-alpine").is_err());
    }

    #[test]
    #[ignore = "requires local Docker Buildx"]
    fn local_buildx_json_shape() {
        assert!(!super::build_platforms().unwrap().is_empty());
    }

    #[test]
    #[ignore = "requires local Docker daemon"]
    fn unique_image_reference_is_absent() {
        let reference = format!("devingpanel/image-probe:{}", uuid::Uuid::new_v4().simple());
        assert!(!super::image_reference_exists(&reference).unwrap());
    }

    #[test]
    #[ignore = "requires an optional local postgres image"]
    fn docker_hub_qualified_reference_detects_existing_tag() {
        let image = "docker.io/library/postgres:16-alpine";
        let installed = Command::new("docker")
            .args(["image", "inspect", image])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        if installed.success() {
            assert!(super::image_reference_exists(image).unwrap());
        }
    }
}
