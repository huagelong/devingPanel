use crate::project_store::Project;
use serde_json::Value;
use std::fs::File;
use std::io::Read;
use std::path::{Component, Path};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};
use yaml_rust2::{Yaml, YamlLoader, yaml::Hash};

const MAX_CONFIG_BYTES: u64 = 16 * 1024 * 1024;

fn yaml_field<'a>(map: &'a Hash, key: &str, depth: usize) -> Result<Option<&'a Yaml>, String> {
    if depth > 16 {
        return Err("Compose YAML 合并层级过深。".into());
    }
    if let Some(value) = map
        .iter()
        .find_map(|(name, value)| (name.as_str() == Some(key)).then_some(value))
    {
        return Ok(Some(value));
    }
    let Some(merged) = map
        .iter()
        .find_map(|(name, value)| (name.as_str() == Some("<<")).then_some(value))
    else {
        return Ok(None);
    };
    match merged {
        Yaml::Hash(map) => yaml_field(map, key, depth + 1),
        Yaml::Array(maps) => {
            for map in maps.iter().filter_map(Yaml::as_hash) {
                if let Some(value) = yaml_field(map, key, depth + 1)? {
                    return Ok(Some(value));
                }
            }
            Ok(None)
        }
        _ => Ok(None),
    }
}

fn check_service_entries(services: &Hash, depth: usize) -> Result<(), String> {
    if depth > 16 {
        return Err("Compose YAML 合并层级过深。".into());
    }
    for (name, service) in services {
        if name.as_str() == Some("<<") {
            match service {
                Yaml::Hash(map) => check_service_entries(map, depth + 1)?,
                Yaml::Array(maps) => {
                    for map in maps.iter().filter_map(Yaml::as_hash) {
                        check_service_entries(map, depth + 1)?;
                    }
                }
                _ => return Err("Compose 服务合并格式无效。".into()),
            }
        } else if let Some(map) = service.as_hash()
            && yaml_field(map, "extends", 0)?.is_some()
        {
            return Err("暂不支持 Compose extends。".into());
        }
    }
    Ok(())
}

fn check_source_syntax(source: &str) -> Result<(), String> {
    let docs = YamlLoader::load_from_str(source).map_err(|_| "无法解析 Compose 源文件。")?;
    for doc in docs {
        let root = doc.as_hash().ok_or("Compose 源文件格式无效。")?;
        if yaml_field(root, "include", 0)?.is_some() {
            return Err("暂不支持 Compose include。".into());
        }
        if let Some(services) = yaml_field(root, "services", 0)?.and_then(Yaml::as_hash) {
            check_service_entries(services, 0)?;
        }
    }
    Ok(())
}

fn check_source_files(project: &Project) -> Result<(), String> {
    let root = Path::new(&project.path)
        .canonicalize()
        .map_err(|_| "项目本地目录不存在。")?;
    for (index, path) in project.compose_files.iter().enumerate() {
        let relative = Path::new(path);
        if !relative
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
            || (index == 0 && relative.components().count() != 1)
        {
            return Err("Compose 文件路径无效或首文件不在项目根目录。".into());
        }
        let resolved = root
            .join(relative)
            .canonicalize()
            .map_err(|_| "无法读取 Compose 源文件。")?;
        if !resolved.starts_with(&root) || !resolved.is_file() {
            return Err("Compose 文件必须位于项目目录内。".into());
        }
        let file = File::open(resolved).map_err(|_| "无法读取 Compose 源文件。")?;
        let mut bytes = Vec::new();
        file.take(MAX_CONFIG_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| "无法读取 Compose 源文件。")?;
        if bytes.len() as u64 > MAX_CONFIG_BYTES {
            return Err("Compose 源文件过大。".into());
        }
        let text = String::from_utf8(bytes).map_err(|_| "Compose 源文件编码无效。")?;
        check_source_syntax(&text)?;
    }
    Ok(())
}

#[derive(Clone)]
pub struct Service {
    pub name: String,
    pub uses_project_image: bool,
}

#[derive(Clone)]
pub struct ServiceSummary {
    pub name: String,
    pub image: String,
    pub ports: Vec<String>,
    pub hidden_ports: usize,
    pub volumes: usize,
}

pub fn inspect_summary(project: &Project) -> Result<Vec<ServiceSummary>, String> {
    if project.compose_files.is_empty() {
        return Ok(Vec::new());
    }
    let raw = config(project)?;
    validate_portability(&raw, &project.service)?;
    parse_summary(&raw)
}

pub fn inspect_services(project: &Project) -> Result<Vec<Service>, String> {
    if project.compose_files.is_empty() {
        return Ok(Vec::new());
    }
    parse_services(&config(project)?)
}

fn validate_portability(raw: &Value, target_service: &str) -> Result<(), String> {
    let services = raw
        .get("services")
        .and_then(Value::as_object)
        .ok_or("Compose 配置中没有服务。")?;
    if !target_service.is_empty()
        && services
            .get(target_service)
            .and_then(|service| service.get("image"))
            .and_then(Value::as_str)
            != Some("${DEVINGPANEL_IMAGE}")
    {
        return Err("部署服务必须精确使用 image: ${DEVINGPANEL_IMAGE}。".into());
    }
    if raw.get("include").is_some() {
        return Err("暂不支持 Compose include。".into());
    }
    for kind in ["configs", "secrets"] {
        if raw
            .get(kind)
            .and_then(Value::as_object)
            .is_some_and(|items| items.values().any(|item| item.get("file").is_some()))
        {
            return Err("暂不支持本地文件型 configs/secrets。".into());
        }
    }
    for service in services.values() {
        if service.get("build").is_some_and(|build| !build.is_null()) {
            return Err("所有远端服务都不能包含 build。".into());
        }
        if service
            .get("image")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
        {
            return Err("所有远端服务都必须指定可用的镜像引用。".into());
        }
        if service.get("label_file").is_some()
            || service
                .get("credential_spec")
                .and_then(|spec| spec.get("file"))
                .is_some()
        {
            return Err("暂不支持需要额外上传的 label_file 或 credential_spec.file。".into());
        }
        if service.get("develop").is_some() {
            return Err("暂不支持 Compose develop 文件同步。".into());
        }
        if service.get("extends").is_some() {
            return Err("暂不支持 Compose extends。".into());
        }
        if let Some(files) = service.get("env_file") {
            let files = files.as_array().ok_or("env_file 配置格式无法核实。")?;
            if files.iter().any(|file| {
                file.as_str()
                    .or_else(|| file.get("path").and_then(Value::as_str))
                    != Some(".env")
            }) {
                return Err("暂不支持 .env 以外的 env_file。".into());
            }
        }
        if let Some(volumes) = service.get("volumes") {
            let volumes = volumes.as_array().ok_or("卷配置格式无法核实。")?;
            if volumes.iter().any(|volume| {
                if volume.get("type").and_then(Value::as_str) != Some("bind") {
                    return false;
                }
                !volume
                    .get("source")
                    .and_then(Value::as_str)
                    .is_some_and(|source| source.starts_with('/') && !source.contains('\0'))
            }) {
                return Err("暂不支持相对路径或 Windows 本地路径的 bind mount。".into());
            }
        }
    }
    Ok(())
}

fn config(project: &Project) -> Result<Value, String> {
    check_source_files(project)?;
    let mut command = Command::new("docker");
    command
        .arg("compose")
        .arg("--project-directory")
        .arg(&project.path);
    for file in &project.compose_files {
        command.arg("-f").arg(Path::new(&project.path).join(file));
    }
    command.args([
        "config",
        "--no-interpolate",
        "--no-path-resolution",
        "--no-env-resolution",
    ]);
    command
        .args(["--format", "json"])
        .current_dir(&project.path)
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    command.env_remove("DEVINGPANEL_IMAGE");
    let mut child = command
        .spawn()
        .map_err(|_| "本地 Docker Compose 不可用。".to_owned())?;
    let stdout = child.stdout.take().expect("stdout was piped");
    let reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout
            .take(MAX_CONFIG_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| "无法读取 Compose 配置。")?;
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
                return Err("Compose 配置解析超时或输出过大。".into());
            }
        }
    };
    let bytes = reader.join().map_err(|_| "无法读取 Compose 配置。")??;
    if bytes.len() as u64 > MAX_CONFIG_BYTES {
        return Err("Compose 配置过大。".into());
    }
    if !status.success() {
        return Err("Compose 配置解析失败，请核对文件与变量。".into());
    }
    serde_json::from_slice(&bytes).map_err(|_| "Compose 配置不是可识别的 JSON。".into())
}

fn parse_services(raw: &Value) -> Result<Vec<Service>, String> {
    let services = raw
        .get("services")
        .and_then(Value::as_object)
        .ok_or("Compose 配置中没有服务。")?;
    let mut options = services
        .iter()
        .map(|(name, config)| Service {
            name: name.clone(),
            uses_project_image: config.get("image").and_then(Value::as_str)
                == Some("${DEVINGPANEL_IMAGE}")
                && !config.get("build").is_some_and(|build| !build.is_null()),
        })
        .collect::<Vec<_>>();
    options.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(options)
}

fn parse_summary(raw: &Value) -> Result<Vec<ServiceSummary>, String> {
    let services = raw
        .get("services")
        .and_then(Value::as_object)
        .ok_or("Compose 配置中没有服务。")?;
    let mut summary = Vec::new();
    for (name, service) in services {
        let project_image =
            service.get("image").and_then(Value::as_str) == Some("${DEVINGPANEL_IMAGE}");
        let port_count = service
            .get("ports")
            .and_then(Value::as_array)
            .map_or(0, Vec::len);
        let ports: Vec<String> = service
            .get("ports")
            .and_then(Value::as_array)
            .map(|ports| {
                ports
                    .iter()
                    .filter_map(|port| {
                        let published = port.get("published").and_then(Value::as_str)?;
                        let target = port.get("target").and_then(Value::as_u64)?;
                        if !published.bytes().all(|byte| byte.is_ascii_digit())
                            || target > u16::MAX as u64
                        {
                            return None;
                        }
                        Some(format!("{published}:{target}"))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let hidden_ports = port_count.saturating_sub(ports.len());
        summary.push(ServiceSummary {
            name: name.clone(),
            image: if project_image {
                "${DEVINGPANEL_IMAGE}"
            } else {
                "其他镜像（已脱敏）"
            }
            .into(),
            ports,
            hidden_ports,
            volumes: service
                .get("volumes")
                .and_then(Value::as_array)
                .map_or(0, Vec::len),
        });
    }
    summary.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::{
        check_source_syntax, inspect_services, inspect_summary, parse_services, parse_summary,
        validate_portability,
    };
    use crate::project_store::Project;
    use serde_json::json;

    #[test]
    fn only_project_image_services_are_selectable() {
        let raw = json!({"services": {
            "worker": {"image": "redis:7"},
            "web": {"image": "${DEVINGPANEL_IMAGE}"},
            "remote-build": {"image": "${DEVINGPANEL_IMAGE}", "build": {"context": "."}},
            "literal": {"image": "devingpanel/image-probe:local"},
            "default-form": {"image": "${DEVINGPANEL_IMAGE:-fallback}"}
        }});
        let services = parse_services(&raw).unwrap();
        assert_eq!(services.len(), 5);
        assert!(
            services
                .iter()
                .find(|service| service.name == "web")
                .unwrap()
                .uses_project_image
        );
        assert!(
            !services
                .iter()
                .find(|service| service.name == "worker")
                .unwrap()
                .uses_project_image
        );
        assert!(
            !services
                .iter()
                .find(|service| service.name == "remote-build")
                .unwrap()
                .uses_project_image
        );
        assert!(
            !services
                .iter()
                .find(|service| service.name == "literal")
                .unwrap()
                .uses_project_image
        );
        assert!(
            !services
                .iter()
                .find(|service| service.name == "default-form")
                .unwrap()
                .uses_project_image
        );
        assert!(parse_services(&json!({"services": null})).is_err());
    }

    #[test]
    fn summary_does_not_expose_interpolated_values() {
        let raw = json!({"services": {"web": {
            "image": "${DEVINGPANEL_IMAGE}",
            "environment": {"TOKEN": "top-secret"},
            "ports": [{"published": "8080", "target": 3000}, {"published": "secret", "target": 22}],
            "volumes": [{"source": "/secret/path", "target": "/data"}]
        }, "worker": {"image": "private.example/team/secret-tag"}}});
        let items = parse_summary(&raw).unwrap();
        assert_eq!(items[0].image, "${DEVINGPANEL_IMAGE}");
        assert_eq!(items[1].image, "其他镜像（已脱敏）");
        assert_eq!(items[0].ports, ["8080:3000"]);
        assert_eq!(items[0].hidden_ports, 1);
        assert_eq!(items[0].volumes, 1);
        let display = format!("{} {:?}", items[0].image, items[0].ports);
        assert!(!display.contains("top-secret"));
        assert!(!display.contains("/secret/path"));
    }

    #[test]
    fn rejects_unportable_compose_resources() {
        let base = json!({"services": {"web": {"image": "${DEVINGPANEL_IMAGE}",
            "volumes": [{"type": "volume", "source": "data", "target": "/data"},
                        {"type": "bind", "source": "/srv/shared", "target": "/shared"}],
            "env_file": [{"path": ".env"}]},
            "worker": {"image": "redis:7"}}});
        assert!(validate_portability(&base, "web").is_ok());
        for (field, value) in [
            ("build", json!({"context": "."})),
            ("label_file", json!(["./labels.txt"])),
            ("credential_spec", json!({"file": "./credentials.json"})),
            (
                "develop",
                json!({"watch": [{"path": "./src", "action": "sync"}]}),
            ),
            ("env_file", json!([{"path": "secret.env"}])),
            (
                "volumes",
                json!([{"type": "bind", "source": "./local", "target": "/data"}]),
            ),
            (
                "volumes",
                json!([{"type": "bind", "source": "C:\\local", "target": "/data"}]),
            ),
        ] {
            let mut modified = base.clone();
            modified["services"]["worker"][field] = value;
            assert!(validate_portability(&modified, "web").is_err());
        }
        let mut modified = base.clone();
        modified["configs"] = json!({"cfg": {"file": "config.txt"}});
        assert!(validate_portability(&modified, "web").is_err());
        assert!(validate_portability(&modified, "").is_err());
        assert!(validate_portability(&base, "").is_ok());
        let mut modified = base.clone();
        modified["services"]["web"]["image"] = json!("${DEVINGPANEL_IMAGE:-fallback}");
        assert!(validate_portability(&modified, "web").is_err());
        let mut modified = base.clone();
        modified["services"]["worker"]
            .as_object_mut()
            .unwrap()
            .remove("image");
        assert!(validate_portability(&modified, "web").is_err());
    }

    #[test]
    fn source_guard_rejects_expanded_include_and_extends() {
        assert!(check_source_syntax("services:\n  web:\n    image: app\n").is_ok());
        assert!(check_source_syntax("include:\n  - ./other.yml\nservices: {}\n").is_err());
        assert!(check_source_syntax("services:\n  web:\n    extends: base\n").is_err());
        assert!(
            check_source_syntax(
                "x-base: &base\n  extends: base\nservices:\n  web:\n    <<: *base\n"
            )
            .is_err()
        );
        assert!(
            check_source_syntax(
                "x-services: &services\n  web:\n    extends: base\nservices:\n  <<: *services\n"
            )
            .is_err()
        );
        assert!(
            check_source_syntax(
                "services:\n  web:\n    environment:\n      extends: ordinary-value\n"
            )
            .is_ok()
        );
    }

    #[test]
    #[ignore = "requires local Docker Compose CLI"]
    fn service_discovery_does_not_require_generated_env_file() {
        let dir =
            std::env::temp_dir().join(format!("deving-panel-compose-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(
            dir.join("compose.yml"),
            "services:\n  web:\n    image: ${DEVINGPANEL_IMAGE}\n    env_file: .env\n",
        )
        .unwrap();
        let mut project = Project {
            id: 1,
            name: "app".into(),
            path: dir.to_string_lossy().into_owned(),
            dockerfile: "Dockerfile".into(),
            image: "app".into(),
            tag_rule: "{timestamp}".into(),
            platform: "auto".into(),
            service: String::new(),
            compose_files: vec!["compose.yml".into()],
            missing_compose_files: Vec::new(),
            build_args: Vec::new(),
            host_ids: Vec::new(),
        };
        let services = inspect_services(&project).unwrap();
        assert_eq!(services.len(), 1);
        assert!(services[0].uses_project_image);
        project.service = "web".into();
        assert_eq!(
            inspect_summary(&project).unwrap()[0].image,
            "${DEVINGPANEL_IMAGE}"
        );
        std::fs::write(dir.join("compose.yml"),
            "services:\n  base:\n    image: busybox\n  web:\n    extends: base\n    image: ${DEVINGPANEL_IMAGE}\n").unwrap();
        assert!(inspect_services(&project).is_err());
        std::fs::remove_file(dir.join("compose.yml")).unwrap();
        std::fs::remove_dir(dir).unwrap();
    }
}
