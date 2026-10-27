use crate::host_store::Host;
use crate::project_store::{self, NewProject, Project};
use std::collections::HashSet;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::path::{Component, Path};
use yaml_rust2::yaml::Hash;
use yaml_rust2::{Yaml, YamlLoader};

const MAX_MANIFEST_BYTES: u64 = 64 * 1024;
pub const FILE_NAME: &str = "devingpanel.yaml";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub group: String,
    pub host: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BuildSpec {
    pub service: String,
    pub dockerfile: String,
    pub image: String,
    pub platform: String,
    pub args: Vec<(String, String)>,
}

pub fn image_variable(service: &str) -> Result<String, String> {
    if service.is_empty()
        || service.len() > 100
        || !service
            .bytes()
            .next()
            .is_some_and(|ch| ch.is_ascii_alphanumeric())
        || !service
            .bytes()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, b'-' | b'_'))
    {
        return Err("构建服务名不能映射为安全的镜像变量。".into());
    }
    Ok(format!(
        "DEVINGPANEL_IMAGE_{}",
        service.to_ascii_uppercase().replace('-', "_")
    ))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Manifest {
    pub name: String,
    pub builds: Vec<BuildSpec>,
    pub dockerfile: String,
    pub image: String,
    pub tag_rule: String,
    pub platform: String,
    pub build_args: Vec<(String, String)>,
    pub compose_files: Vec<String>,
    pub service: String,
    pub targets: Vec<Target>,
}

fn mapping<'a>(value: &'a Yaml, label: &str, allowed: &[&str]) -> Result<&'a Hash, String> {
    let map = value
        .as_hash()
        .ok_or_else(|| format!("{label} 必须是映射。"))?;
    for key in map.keys() {
        let name = key
            .as_str()
            .ok_or_else(|| format!("{label} 字段名必须是字符串。"))?;
        if !allowed.contains(&name) {
            return Err(format!("{label} 包含未知字段：{name}。"));
        }
    }
    Ok(map)
}

fn field<'a>(map: &'a Hash, key: &str, label: &str) -> Result<&'a Yaml, String> {
    map.get(&Yaml::String(key.into()))
        .ok_or_else(|| format!("{label} 缺少 {key}。"))
}

fn text(map: &Hash, key: &str, label: &str, allow_empty: bool) -> Result<String, String> {
    let value = field(map, key, label)?
        .as_str()
        .ok_or_else(|| format!("{label}.{key} 必须是字符串。"))?;
    if value.len() > 255
        || value.chars().any(char::is_control)
        || (!allow_empty && value.is_empty())
    {
        return Err(format!("{label}.{key} 无效。"));
    }
    Ok(value.to_owned())
}

fn relative_file(value: &str, label: &str) -> Result<(), String> {
    let path = Path::new(value);
    if value.is_empty()
        || value.contains(['\\', ':'])
        || !path
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
    {
        return Err(format!("{label} 必须是项目内相对路径。"));
    }
    Ok(())
}

fn valid_arg_name(value: &str) -> bool {
    let mut bytes = value.bytes();
    bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

pub fn parse(source: &str) -> Result<Manifest, String> {
    let docs = YamlLoader::load_from_str(source)
        .map_err(|_| "devingpanel.yaml YAML 格式无效或字段重复。")?;
    if docs.len() != 1 {
        return Err("devingpanel.yaml 必须且只能包含一份文档。".into());
    }
    let root = mapping(
        &docs[0],
        "配置",
        &["version", "name", "tag", "builds", "deploy"],
    )?;
    if field(root, "version", "配置")?.as_i64() != Some(2) {
        return Err("devingpanel.yaml 仅支持 version: 2。".into());
    }
    let name = text(root, "name", "配置", false)?;
    let tag_rule = text(root, "tag", "配置", false)?;
    if !project_store::valid_tag_rule(&tag_rule) {
        return Err("tag 不是合法的 Tag 规则。".into());
    }
    let rows = field(root, "builds", "配置")?
        .as_vec()
        .ok_or("builds 必须是数组。")?;
    if rows.is_empty() || rows.len() > 16 {
        return Err("builds 必须包含 1 至 16 个构建。".into());
    }
    let mut builds = Vec::new();
    let mut services = HashSet::new();
    let mut images = HashSet::new();
    let mut variables = HashSet::new();
    for row in rows {
        let build = mapping(
            row,
            "builds 条目",
            &["service", "dockerfile", "image", "platform", "args"],
        )?;
        let service = text(build, "service", "builds 条目", false)?;
        let variable = image_variable(&service)?;
        let dockerfile = text(build, "dockerfile", "builds 条目", false)?;
        relative_file(&dockerfile, "Dockerfile")?;
        let image = text(build, "image", "builds 条目", false)?;
        let platform = text(build, "platform", "builds 条目", false)?;
        if !project_store::valid_image_name(&image)
            || !matches!(platform.as_str(), "auto" | "linux/amd64" | "linux/arm64")
            || !services.insert(service.clone())
            || !images.insert(image.clone())
            || !variables.insert(variable)
        {
            return Err("构建服务、镜像名、平台重复或无效。".into());
        }
        let args = field(build, "args", "builds 条目")?
            .as_hash()
            .ok_or("builds.args 必须是键值映射。")?;
        let mut parsed_args = Vec::new();
        for (key, value) in args {
            let name = key.as_str().ok_or("Build args 名称必须是字符串。")?;
            let value = value.as_str().ok_or("Build args 值必须是字符串。")?;
            if !valid_arg_name(name) || value.contains('\0') {
                return Err("Build args 名称或值无效。".into());
            }
            parsed_args.push((name.into(), value.into()));
        }
        builds.push(BuildSpec {
            service,
            dockerfile,
            image,
            platform,
            args: parsed_args,
        });
    }
    let first = &builds[0];
    let deploy = mapping(
        field(root, "deploy", "配置")?,
        "deploy",
        &["compose", "targets"],
    )?;
    let compose = mapping(field(deploy, "compose", "deploy")?, "compose", &["files"])?;
    let files = field(compose, "files", "compose")?
        .as_vec()
        .ok_or("compose.files 必须是文件列表。")?;
    if files.is_empty() {
        return Err("compose.files 不能为空。".into());
    }
    let mut seen_files = HashSet::new();
    let mut compose_files = Vec::new();
    for (index, file) in files.iter().enumerate() {
        let file = file.as_str().ok_or("Compose 文件路径必须是字符串。")?;
        relative_file(file, "Compose 文件")?;
        if (index == 0 && Path::new(file).components().count() != 1) || !seen_files.insert(file) {
            return Err("首个 Compose 文件必须在项目根目录，且文件不可重复。".into());
        }
        compose_files.push(file.into());
    }
    let target_rows = field(deploy, "targets", "deploy")?
        .as_vec()
        .ok_or("deploy.targets 必须是主机列表。")?;
    if target_rows.is_empty() {
        return Err("deploy.targets 不能为空。".into());
    }
    let mut seen_targets = HashSet::new();
    let mut targets = Vec::new();
    for value in target_rows {
        let row = mapping(value, "目标主机", &["group", "host"])?;
        let group = text(row, "group", "目标主机", true)?;
        let host = text(row, "host", "目标主机", false)?;
        if !seen_targets.insert((group.clone(), host.clone())) {
            return Err("目标主机不可重复。".into());
        }
        targets.push(Target { group, host });
    }
    if targets
        .iter()
        .any(|target| target.group != targets[0].group)
    {
        return Err("目标主机必须属于同一主机组。".into());
    }
    Ok(Manifest {
        name,
        builds: builds.clone(),
        dockerfile: first.dockerfile.clone(),
        image: first.image.clone(),
        tag_rule,
        platform: first.platform.clone(),
        build_args: first.args.clone(),
        compose_files,
        service: first.service.clone(),
        targets,
    })
}

pub fn load(root: &Path) -> Result<Manifest, String> {
    let root = root.canonicalize().map_err(|_| "项目目录不存在。")?;
    let path = root.join(FILE_NAME);
    let canonical = path
        .canonicalize()
        .map_err(|_| "项目缺少 devingpanel.yaml。")?;
    if !canonical.starts_with(&root) || !canonical.is_file() {
        return Err("devingpanel.yaml 必须位于项目目录内。".into());
    }
    let mut bytes = Vec::new();
    File::open(canonical)
        .map_err(|_| "无法读取 devingpanel.yaml。")?
        .take(MAX_MANIFEST_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "无法读取 devingpanel.yaml。")?;
    if bytes.len() as u64 > MAX_MANIFEST_BYTES {
        return Err("devingpanel.yaml 超过 64 KiB。".into());
    }
    let source = String::from_utf8(bytes).map_err(|_| "devingpanel.yaml 必须是 UTF-8。")?;
    let manifest = parse(&source)?;
    for build in &manifest.builds {
        NewProject {
            name: manifest.name.clone(),
            path: root.to_string_lossy().into_owned(),
            dockerfile: build.dockerfile.clone(),
        }
        .normalize()?;
    }
    for file in &manifest.compose_files {
        let resolved = root
            .join(file)
            .canonicalize()
            .map_err(|_| format!("Compose 文件不存在：{file}"))?;
        if !resolved.starts_with(&root) || !resolved.is_file() {
            return Err(format!("Compose 文件必须位于项目目录内：{file}"));
        }
    }
    Ok(manifest)
}

pub fn resolve_targets(manifest: &Manifest, hosts: &[Host]) -> Result<Vec<i64>, String> {
    manifest
        .targets
        .iter()
        .map(|target| {
            let matches = hosts
                .iter()
                .filter(|host| host.group == target.group && host.name == target.host)
                .collect::<Vec<_>>();
            if matches.len() != 1 {
                return Err(format!(
                    "目标主机 {}/{} 在本机不存在或名称不唯一。",
                    target.group, target.host
                ));
            }
            Ok(matches[0].id)
        })
        .collect()
}

pub fn apply(
    project: &Project,
    manifest: &Manifest,
    host_ids: Vec<i64>,
) -> Result<Project, String> {
    if project.name != manifest.name {
        return Err("devingpanel.yaml 项目名与本机项目不一致。".into());
    }
    let mut effective = project.clone();
    effective.dockerfile = manifest.dockerfile.clone();
    effective.image = manifest.image.clone();
    effective.tag_rule = manifest.tag_rule.clone();
    effective.platform = manifest.platform.clone();
    effective.build_args = manifest.build_args.clone();
    effective.compose_files = manifest.compose_files.clone();
    effective.missing_compose_files.clear();
    effective.service = manifest.service.clone();
    effective.host_ids = host_ids;
    Ok(effective)
}

fn quoted(value: &str) -> String {
    serde_json::to_string(value).expect("JSON strings are serializable")
}

pub fn export_legacy(project: &Project, hosts: &[Host]) -> Result<(), String> {
    if project.image.is_empty()
        || project.service.is_empty()
        || project.compose_files.is_empty()
        || project.host_ids.is_empty()
    {
        return Err("请先补齐构建配置、Compose 服务和目标主机，再导出 YAML。".into());
    }
    let variable = format!("${{{}}}", image_variable(&project.service)?);
    if !project.compose_files.iter().any(|file| {
        std::fs::read_to_string(Path::new(&project.path).join(file))
            .is_ok_and(|source| source.contains(&variable))
    }) {
        return Err(format!(
            "请先将 Compose 部署服务的 image 改为 {variable}，再导出 YAML。"
        ));
    }
    let mut targets = Vec::new();
    let mut seen = HashSet::new();
    for id in &project.host_ids {
        let host = hosts
            .iter()
            .find(|host| host.id == *id)
            .ok_or("绑定主机不存在。")?;
        if hosts
            .iter()
            .filter(|candidate| candidate.group == host.group && candidate.name == host.name)
            .count()
            != 1
            || !seen.insert((host.group.clone(), host.name.clone()))
        {
            return Err("主机组与别名组合不唯一，无法导出稳定的目标引用。".into());
        }
        targets.push(host);
    }
    let mut source = format!(
        "version: 2\nname: {}\ntag: {}\nbuilds:\n  - service: {}\n    dockerfile: {}\n    image: {}\n    platform: {}\n    args:\n",
        quoted(&project.name),
        quoted(&project.tag_rule),
        quoted(&project.service),
        quoted(&project.dockerfile),
        quoted(&project.image),
        quoted(&project.platform)
    );
    if project.build_args.is_empty() {
        source.push_str("      {}\n");
    } else {
        for (name, value) in &project.build_args {
            source.push_str(&format!("      {}: {}\n", quoted(name), quoted(value)));
        }
    }
    source.push_str("deploy:\n  compose:\n    files:\n");
    for file in &project.compose_files {
        source.push_str(&format!("      - {}\n", quoted(file)));
    }
    source.push_str("  targets:\n");
    for host in targets {
        source.push_str(&format!(
            "    - group: {}\n      host: {}\n",
            quoted(&host.group),
            quoted(&host.name)
        ));
    }
    parse(&source)?;
    let path = Path::new(&project.path).join(FILE_NAME);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|_| "devingpanel.yaml 已存在或无法创建，未覆盖原文件。")?;
    if let Err(error) = file
        .write_all(source.as_bytes())
        .and_then(|_| file.sync_all())
    {
        drop(file);
        let _ = std::fs::remove_file(&path);
        return Err(format!("写入 devingpanel.yaml 失败：{error}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{Target, export_legacy, load, parse};
    use crate::host_store::Host;
    use crate::project_store::Project;

    const VALID: &str = r#"
version: 2
name: myapp
tag: "{timestamp}"
builds:
  - service: web
    dockerfile: Dockerfile
    image: myapp
    platform: auto
    args:
      NODE_ENV: production
deploy:
  compose:
    files: [compose.yaml, compose.prod.yaml]
  targets:
    - group: prod
      host: web-1
"#;

    #[test]
    fn parses_strict_manifest_and_ordered_targets() {
        let parsed = parse(VALID).unwrap();
        assert_eq!(parsed.name, "myapp");
        assert_eq!(
            parsed.build_args,
            [("NODE_ENV".into(), "production".into())]
        );
        assert_eq!(parsed.compose_files, ["compose.yaml", "compose.prod.yaml"]);
        assert_eq!(
            parsed.targets,
            [Target {
                group: "prod".into(),
                host: "web-1".into()
            }]
        );
    }

    #[test]
    fn parses_multiple_builds_and_rejects_colliding_image_variables() {
        let second = "  - service: worker\n    dockerfile: Dockerfile.worker\n    image: myapp-worker\n    platform: auto\n    args: {}\n";
        let source = VALID.replace("deploy:\n", &format!("{second}deploy:\n"));
        let parsed = parse(&source).unwrap();
        assert_eq!(parsed.builds.len(), 2);
        assert_eq!(parsed.builds[1].service, "worker");
        let collision = source.replace("service: worker", "service: WEB");
        assert!(parse(&collision).is_err());
        let collision = source.replace("service: worker", "service: web");
        assert!(parse(&collision).is_err());
    }

    #[test]
    fn rejects_ambiguous_and_unsafe_configuration() {
        assert!(parse(&VALID.replace("version: 2", "version: 1")).is_err());
        assert!(parse(&VALID.replace("name: myapp", "name: myapp\nname: other")).is_err());
        assert!(parse(&VALID.replace("name: myapp", "name: myapp\nextra: yes")).is_err());
        assert!(parse(&VALID.replace("Dockerfile", "../Dockerfile")).is_err());
        assert!(parse(&VALID.replace("compose.yaml", "../compose.yaml")).is_err());
        assert!(
            parse(&VALID.replace(
                "host: web-1",
                "host: web-1\n    - group: prod\n      host: web-1"
            ))
            .is_err()
        );
        assert!(parse(&VALID.replace("NODE_ENV: production", "NODE_ENV: 42")).is_err());
        assert!(parse(&VALID.replace("service: web", "service: a-b\n  - service: a_b")).is_err());
        assert!(
            parse(&VALID.replace(
                "host: web-1",
                "host: web-1\n    - group: staging\n      host: web-2"
            ))
            .is_err()
        );
        assert!(parse(&format!("{VALID}\n---\nversion: 2")).is_err());
    }

    #[test]
    fn exports_legacy_project_without_overwriting_an_existing_manifest() {
        let root = std::env::temp_dir().join(format!(
            "devingpanel-export-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("Dockerfile"), "FROM scratch\n").unwrap();
        std::fs::write(
            root.join("compose.yaml"),
            "services:\n  web:\n    image: ${DEVINGPANEL_IMAGE_WEB}\n",
        )
        .unwrap();
        let project = Project {
            id: 1,
            name: "myapp".into(),
            path: root.to_string_lossy().into_owned(),
            dockerfile: "Dockerfile".into(),
            image: "myapp".into(),
            tag_rule: "{timestamp}".into(),
            platform: "auto".into(),
            service: "web".into(),
            compose_files: vec!["compose.yaml".into()],
            missing_compose_files: Vec::new(),
            build_args: vec![("NODE_ENV".into(), "production".into())],
            host_ids: vec![4],
            manifest_mode: false,
            config_error: None,
        };
        let host = Host {
            id: 4,
            name: "web-1".into(),
            address: "203.0.113.10".into(),
            port: 22,
            username: "deploy".into(),
            group: "prod".into(),
            auth: "password".into(),
            trusted_fingerprint: None,
        };
        std::fs::write(
            root.join("compose.yaml"),
            "services:\n  web:\n    image: ${DEVINGPANEL_IMAGE}\n",
        )
        .unwrap();
        assert!(export_legacy(&project, std::slice::from_ref(&host)).is_err());
        assert!(!root.join("devingpanel.yaml").exists());
        std::fs::write(
            root.join("compose.yaml"),
            "services:\n  web:\n    image: ${DEVINGPANEL_IMAGE_WEB}\n",
        )
        .unwrap();
        export_legacy(&project, std::slice::from_ref(&host)).unwrap();
        let exported = load(&root).unwrap();
        assert_eq!(exported.targets[0].host, "web-1");
        assert_eq!(
            exported.build_args,
            [("NODE_ENV".into(), "production".into())]
        );
        assert!(export_legacy(&project, &[host]).is_err());
        std::fs::remove_file(root.join("devingpanel.yaml")).unwrap();
        std::fs::remove_file(root.join("Dockerfile")).unwrap();
        std::fs::remove_file(root.join("compose.yaml")).unwrap();
        std::fs::remove_dir(root).unwrap();
    }
}
