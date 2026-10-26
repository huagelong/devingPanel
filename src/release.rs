use crate::build::{self, BuiltImage, SavedArchive};
use crate::compose;
use crate::docker;
use crate::env_store;
use crate::host_store;
use crate::host_store::Host;
use crate::project_config;
use crate::project_store::{self, NewProject, Project};
use crate::release_store::{self, Intent, Outcome, Phase};
use crate::ssh::{self, TestResult};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::collections::HashSet;
use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

pub fn check_project_config(project: &Project, host_ids: &[i64]) -> Result<(), String> {
    let current = if project.manifest_mode {
        project_store::effective(project.id)?
    } else {
        project.clone()
    };
    let project = &current;
    if project.manifest_mode && host_ids != project.host_ids {
        return Err("YAML 项目的目标主机由开发者配置，不能在发布草稿中改动。".into());
    }
    if let Some(error) = &project.config_error {
        return Err(format!("devingpanel.yaml 待修复：{error}"));
    }
    if host_ids.is_empty() {
        return Err("请先选择目标主机。".into());
    }
    let mut seen = HashSet::new();
    if host_ids
        .iter()
        .any(|id| !project.host_ids.contains(id) || !seen.insert(*id))
    {
        return Err("本次目标主机必须来自项目已绑定的主机。".into());
    }
    if !project_store::valid_image_name(&project.image) {
        return Err("请先在构建配置中设置合法镜像名。".into());
    }
    NewProject {
        name: project.name.clone(),
        path: project.path.clone(),
        dockerfile: project.dockerfile.clone(),
    }
    .normalize()?;
    if project.compose_files.is_empty() || project.service.is_empty() {
        return Err("请先配置 Compose 文件和部署服务。".into());
    }
    compose::inspect_summary(project)?;
    Ok(())
}

fn checked_platform(
    result: TestResult,
    host_name: &str,
    requested: &str,
) -> Result<String, String> {
    match result {
        TestResult::NeedsTrust { previous, .. } => Err(format!(
            "主机 {host_name} 的 SSH 指纹{}；请到主机页独立核对后再试。",
            if previous.is_some() {
                "已变化"
            } else {
                "尚未确认"
            }
        )),
        TestResult::Connected {
            architecture: Some(platform),
            sftp_v3_handshake,
            ..
        } => {
            if !sftp_v3_handshake {
                return Err(format!(
                    "主机 {host_name} 未通过 SFTP v3 协议握手；无法开始直连传输预检。"
                ));
            }
            if requested != "auto" && requested != platform {
                return Err(format!(
                    "主机 {host_name} 的架构为 {platform}，与项目目标平台 {requested} 不匹配。"
                ));
            }
            Ok(platform)
        }
        TestResult::Connected {
            architecture: None, ..
        } => Err(format!("主机 {host_name} 的架构尚未支持。")),
        TestResult::DockerMissing { .. } => Err(format!(
            "主机 {host_name} 的 SSH 已连接，但 Docker 未安装；请先到主机详情安装。"
        )),
    }
}

pub fn probe_targets(
    project: &Project,
    host_ids: &[i64],
    hosts: &[Host],
    cancel: Arc<AtomicBool>,
) -> Result<Vec<(i64, String)>, String> {
    if host_ids.is_empty() {
        return Err("请先选择目标主机。".into());
    }
    let mut seen = HashSet::new();
    let mut result = Vec::new();
    for id in host_ids {
        if cancel.load(Ordering::Relaxed) {
            return Err("主机架构检查已取消。".into());
        }
        if !project.host_ids.contains(id) || !seen.insert(*id) {
            return Err("本次目标主机不属于项目绑定，或存在重复。".into());
        }
        let host = hosts
            .iter()
            .find(|host| host.id == *id)
            .ok_or("目标主机配置待读取。")?;
        let probe = ssh::test_host_snapshot(host.clone(), cancel.clone())?;
        result.push((*id, checked_platform(probe, &host.name, &project.platform)?));
    }
    Ok(result)
}

#[derive(Clone)]
pub struct PlannedImage {
    pub platform: String,
    pub reference: String,
}

pub fn plan_images(
    project: &Project,
    base_tag: &str,
    targets: &[(i64, String)],
) -> Result<Vec<PlannedImage>, String> {
    if !project_store::valid_image_name(&project.image) {
        return Err("项目镜像名无效。".into());
    }
    if targets.is_empty() {
        return Err("请先核对目标主机架构。".into());
    }
    let platforms = targets
        .iter()
        .map(|(_, platform)| platform.as_str())
        .collect::<BTreeSet<_>>();
    let mut planned = Vec::new();
    for platform in platforms {
        let suffix = match platform {
            "linux/amd64" => "-amd64",
            "linux/arm64" => "-arm64",
            _ => return Err("目标平台不受支持。".into()),
        };
        let tag = format!("{base_tag}{suffix}");
        if tag.len() > 128
            || !tag
                .bytes()
                .next()
                .is_some_and(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
            || !tag
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-'))
        {
            return Err("发布标识无法生成合法的 Docker tag。".into());
        }
        planned.push(PlannedImage {
            platform: platform.to_owned(),
            reference: format!("{}:{tag}", project.image),
        });
    }
    Ok(planned)
}

pub fn check_image_tags(
    project: &Project,
    base_tag: &str,
    targets: &[(i64, String)],
) -> Result<Vec<PlannedImage>, String> {
    let planned = plan_images(project, base_tag, targets)?;
    for image in &planned {
        if docker::image_reference_exists(&image.reference)? {
            return Err(format!(
                "本地镜像 tag 已存在：{}；请更换发布标识。",
                image.reference
            ));
        }
    }
    Ok(planned)
}

fn verify_transfer(archive: &SavedArchive, uploaded: &ssh::UploadedArchive) -> Result<(), String> {
    if archive.bytes != uploaded.bytes || archive.sha256 != uploaded.sha256 {
        return Err("镜像归档传输前后校验值不一致，远端文件不可用于部署。".into());
    }
    Ok(())
}

fn finish_transfer(
    archive: &SavedArchive,
    uploaded: ssh::UploadedArchive,
    cleanup: impl FnOnce(&ssh::UploadedArchive) -> Result<(), String>,
) -> Result<ssh::UploadedArchive, String> {
    if let Err(error) = verify_transfer(archive, &uploaded) {
        return Err(match cleanup(&uploaded) {
            Ok(()) => error,
            Err(reason) => format!(
                "{error}；远端临时目录 {} 清理失败：{reason}",
                uploaded.directory
            ),
        });
    }
    Ok(uploaded)
}

#[allow(dead_code)] // The release coordinator will load this archive before deployment.
pub fn transfer_built_image(
    host: Host,
    image: &BuiltImage,
    cancel: Arc<AtomicBool>,
    mut progress: impl FnMut(u64, u64),
) -> Result<ssh::UploadedArchive, String> {
    let probe = ssh::test_host_snapshot(host.clone(), cancel.clone())?;
    let platform = checked_platform(probe, &host.name, &image.platform)?;
    if platform != image.platform {
        return Err("目标主机架构与已构建镜像不匹配。".into());
    }
    let archive = build::save_image(image, cancel.clone())?;
    if cancel.load(Ordering::Relaxed) {
        return Err("镜像传输已取消。".into());
    }
    let uploaded = ssh::upload_archive(host.clone(), &archive.path, cancel, &mut progress)?;
    let mut uploaded = finish_transfer(&archive, uploaded, |uploaded| {
        ssh::cleanup_uploaded_archive(host, uploaded)
    })?;
    uploaded.image_ids = archive.image_ids.clone();
    Ok(uploaded)
}

fn source_digest(project: &Project) -> Result<String, String> {
    let root = Path::new(&project.path)
        .canonicalize()
        .map_err(|_| "项目目录不存在。")?;
    let metadata = serde_json::json!({
        "name": project.name,
        "dockerfile": project.dockerfile,
        "image": project.image,
        "tag_rule": project.tag_rule,
        "platform": project.platform,
        "service": project.service,
        "compose_files": project.compose_files,
        "build_args": project.build_args,
        "host_ids": project.host_ids,
    });
    let mut digest = Sha256::new();
    digest.update(serde_json::to_vec(&metadata).map_err(|_| "无法核对项目配置。")?);
    let mut paths = Vec::new();
    if project.manifest_mode {
        paths.push(project_config::FILE_NAME.to_owned());
    }
    paths.push(project.dockerfile.clone());
    paths.extend(project.compose_files.iter().cloned());
    for relative in paths {
        let file = root
            .join(&relative)
            .canonicalize()
            .map_err(|_| format!("项目文件不存在：{relative}"))?;
        if !file.starts_with(&root) || !file.is_file() {
            return Err("项目文件越出代码目录。".into());
        }
        let mut source = File::open(file)
            .map_err(|_| "无法读取项目文件。")?
            .take(16 * 1024 * 1024 + 1);
        let mut bytes = Vec::new();
        source
            .read_to_end(&mut bytes)
            .map_err(|_| "无法读取项目文件。")?;
        if bytes.len() > 16 * 1024 * 1024 {
            return Err("项目文件过大。".into());
        }
        digest.update((relative.len() as u64).to_le_bytes());
        digest.update(relative.as_bytes());
        digest.update((bytes.len() as u64).to_le_bytes());
        digest.update(bytes);
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn deployment_files(project: &Project) -> Result<Vec<ssh::DeploymentFile>, String> {
    let root = Path::new(&project.path)
        .canonicalize()
        .map_err(|_| "项目目录不存在。")?;
    let mut remote_paths = HashSet::new();
    project
        .compose_files
        .iter()
        .map(|relative| {
            let remote_path = if cfg!(windows) {
                relative.replace('\\', "/")
            } else {
                relative.clone()
            };
            ssh::validated_release_file(&remote_path)?;
            if !remote_paths.insert(remote_path.clone()) {
                return Err("Compose 文件映射到重复的远端路径。".into());
            }
            let path = root
                .join(relative)
                .canonicalize()
                .map_err(|_| "Compose 文件不存在。")?;
            if !path.starts_with(&root) || !path.is_file() {
                return Err("Compose 文件越出项目目录。".into());
            }
            let mut bytes = Vec::new();
            File::open(path)
                .map_err(|_| "无法读取 Compose 文件。")?
                .take(16 * 1024 * 1024 + 1)
                .read_to_end(&mut bytes)
                .map_err(|_| "无法读取 Compose 文件。")?;
            if bytes.len() > 16 * 1024 * 1024 {
                return Err("Compose 文件过大。".into());
            }
            Ok(ssh::DeploymentFile {
                path: remote_path,
                bytes,
            })
        })
        .collect()
}

#[allow(dead_code)] // The GUI release coordinator will call this after deployment is wired up.
pub fn prepare_single_host(
    project_id: i64,
    host_id: i64,
    base_tag: &str,
    cancel: Arc<AtomicBool>,
) -> Result<(Project, Host, PlannedImage, String), String> {
    let project = project_store::effective(project_id)?;
    check_project_config(&project, &[host_id])?;
    env_store::check_deployment_environment(project_id, &[host_id])?;
    let (hosts, _) = host_store::list()?;
    let host = hosts
        .iter()
        .find(|host| host.id == host_id)
        .cloned()
        .ok_or("目标主机不存在。")?;
    let initial_digest = source_digest(&project)?;
    let targets = probe_targets(&project, &[host_id], &hosts, cancel.clone())?;
    let image = plan_images(&project, base_tag, &targets)?
        .into_iter()
        .next()
        .ok_or("无法规划构建镜像。")?;
    if !docker::build_platforms()?.contains(&image.platform) {
        return Err("本机 default 构建器不支持目标平台。".into());
    }
    if docker::image_reference_exists(&image.reference)? {
        return Err("本地镜像 tag 已存在，请更换发布标识。".into());
    }
    if cancel.load(Ordering::Relaxed)
        || source_digest(&project_store::effective(project_id)?)? != initial_digest
    {
        return Err("项目配置在预检期间变化或预检已取消，请重新开始。".into());
    }
    Ok((project, host, image, initial_digest))
}

#[allow(dead_code)] // This stops before persisting a completed release and never reports success.
pub fn run_single_host_until_checked(
    project_id: i64,
    host_id: i64,
    base_tag: &str,
    cancel: Arc<AtomicBool>,
    mut on_event: impl FnMut(&str),
) -> Result<String, String> {
    let (project, host, image, digest) =
        prepare_single_host(project_id, host_id, base_tag, cancel.clone())?;
    let files = deployment_files(&project)?;
    let env_file = env_store::deployment_env_file(project_id, host_id)?;
    if source_digest(&project_store::effective(project_id)?)? != digest {
        return Err("项目文件在冻结部署材料期间变化，请重新开始。".into());
    }
    let id = uuid::Uuid::new_v4().to_string();
    release_store::create(&Intent {
        id: id.clone(),
        project_id,
        project_name: project.name.clone(),
        host: host.clone(),
        image_reference: image.reference.clone(),
        platform: image.platform.clone(),
        config_sha256: digest.clone(),
    })?;
    release_store::advance(&id, Phase::Prepared, Phase::Building)?;
    on_event("本机构建");
    let built = match build::build_image(&project, &image, cancel.clone(), |line| on_event(line)) {
        Ok(built) => built,
        Err(error) => {
            let _ = release_store::settle(&id, Phase::Building, Outcome::Unknown, "构建结果待核实");
            return Err(error.message);
        }
    };
    release_store::advance(&id, Phase::Building, Phase::Built)?;
    if project_store::effective(project_id)
        .and_then(|current| source_digest(&current))
        .as_deref()
        != Ok(digest.as_str())
    {
        release_store::settle(&id, Phase::Built, Outcome::Failed, "项目文件在构建期间变化")?;
        return Err("项目文件在构建期间变化，未开始上传。".into());
    }
    release_store::advance(&id, Phase::Built, Phase::Uploading)?;
    on_event("传输镜像");
    let uploaded =
        match transfer_built_image(host.clone(), &built, cancel.clone(), |sent, total| {
            on_event(&format!("镜像传输 {sent}/{total} 字节"))
        }) {
            Ok(uploaded) => uploaded,
            Err(error) => {
                let _ = release_store::settle(
                    &id,
                    Phase::Uploading,
                    Outcome::Unknown,
                    "传输结果待核实",
                );
                return Err(error);
            }
        };
    release_store::advance(&id, Phase::Uploading, Phase::Uploaded)?;
    release_store::advance(&id, Phase::Uploaded, Phase::Loading)?;
    on_event("远端加载镜像");
    if let Err(error) = ssh::load_uploaded_archive(
        host.clone(),
        &uploaded,
        &built.reference,
        &built.platform,
        cancel.clone(),
    ) {
        let _ = release_store::settle(
            &id,
            Phase::Loading,
            Outcome::Unknown,
            "远端镜像加载结果待核实",
        );
        return Err(error);
    }
    release_store::advance(&id, Phase::Loading, Phase::Loaded)?;
    if let Err(error) = ssh::cleanup_uploaded_archive(host.clone(), &uploaded) {
        release_store::settle(
            &id,
            Phase::Loaded,
            Outcome::Unknown,
            "镜像已加载，但临时归档清理待核实",
        )?;
        return Err(error);
    }
    release_store::advance(&id, Phase::Loaded, Phase::AwaitingDeployment)?;
    release_store::advance(&id, Phase::AwaitingDeployment, Phase::Staging)?;
    on_event("上传 Compose 和环境配置");
    let staged = match ssh::stage_deployment(
        host.clone(),
        project_id,
        &id,
        &files,
        &env_file,
        cancel.clone(),
    ) {
        Ok(staged) => staged,
        Err(error) => {
            let _ = release_store::settle(
                &id,
                Phase::Staging,
                Outcome::Unknown,
                "部署文件上传结果待核实",
            );
            return Err(error);
        }
    };
    if staged.compose_files.len() != files.len()
        || staged.env_path.is_empty()
        || staged.directory.is_empty()
    {
        release_store::settle(&id, Phase::Staging, Outcome::Unknown, "远端部署文件待核实")?;
        return Err("远端部署文件状态不完整。".into());
    }
    release_store::advance(&id, Phase::Staging, Phase::Staged)?;
    release_store::advance(&id, Phase::Staged, Phase::Validating)?;
    on_event("远端校验 Compose 配置");
    if let Err(error) = ssh::validate_staged_compose(
        host.clone(),
        &staged,
        project_id,
        &project.service,
        &built.reference,
        cancel.clone(),
    ) {
        let _ = release_store::settle(
            &id,
            Phase::Validating,
            Outcome::Unknown,
            "远端 Compose 配置待核实",
        );
        return Err(error);
    }
    release_store::advance(&id, Phase::Validating, Phase::Validated)?;
    release_store::advance(&id, Phase::Validated, Phase::Deploying)?;
    on_event("远端启动 Compose 服务");
    if let Err(error) = ssh::start_staged_compose(
        host.clone(),
        &staged,
        project_id,
        &project.service,
        &built.reference,
        &built.image_id,
        cancel.clone(),
    ) {
        // A read-only reconnect can reveal partial effects but cannot justify a retry.
        let observed = ssh::check_staged_compose(
            host,
            &staged,
            project_id,
            &project.service,
            &built.reference,
            &built.image_id,
            Arc::new(AtomicBool::new(false)),
        );
        let detail = match observed {
            Ok(ssh::DeploymentState::Healthy) => "启动命令未确认，但目标容器健康；发布记录待核实",
            Ok(ssh::DeploymentState::RunningUnverified) => {
                "启动命令未确认，目标容器运行但就绪待核实"
            }
            Ok(ssh::DeploymentState::Failed) => "启动命令未确认，目标容器状态异常，需人工核实",
            Err(_) => "启动结果待核实，禁止自动重试",
        };
        let _ = release_store::settle(&id, Phase::Deploying, Outcome::Unknown, detail);
        return Err(error);
    }
    release_store::advance(&id, Phase::Deploying, Phase::Checking)?;
    on_event("核对容器与健康状态");
    let state = match ssh::check_staged_compose(
        host,
        &staged,
        project_id,
        &project.service,
        &built.reference,
        &built.image_id,
        cancel,
    ) {
        Ok(state) => state,
        Err(error) => {
            let _ = release_store::settle(&id, Phase::Checking, Outcome::Unknown, "容器状态待核实");
            return Err(error);
        }
    };
    release_store::advance(&id, Phase::Checking, Phase::Checked)?;
    let (outcome, detail) = match state {
        ssh::DeploymentState::Healthy => (Outcome::Blocked, "容器健康，历史材料与恢复入口尚未实现"),
        ssh::DeploymentState::RunningUnverified => (
            Outcome::Blocked,
            "容器运行中但无健康检查，历史材料与恢复入口尚未实现",
        ),
        ssh::DeploymentState::Failed => (
            Outcome::Blocked,
            "目标容器未运行、镜像不符或健康检查失败，需人工处理",
        ),
    };
    release_store::settle(&id, Phase::Checked, outcome, detail)?;
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::{
        check_project_config, checked_platform, deployment_files, finish_transfer, plan_images,
        source_digest, verify_transfer,
    };
    use crate::build::SavedArchive;
    use crate::project_store::Project;
    use crate::ssh::TestResult;

    fn project() -> Project {
        Project {
            id: 1,
            name: "app".into(),
            path: ".".into(),
            dockerfile: "Dockerfile".into(),
            image: "app".into(),
            tag_rule: "{timestamp}".into(),
            platform: "auto".into(),
            service: "web".into(),
            compose_files: vec!["compose.yaml".into()],
            missing_compose_files: Vec::new(),
            build_args: Vec::new(),
            host_ids: vec![1],
            manifest_mode: false,
            config_error: None,
        }
    }

    #[test]
    fn rejects_duplicate_normalized_compose_destinations() {
        let dir = std::env::temp_dir().join(format!(
            "devingpanel-compose-path-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(dir.join("deploy")).unwrap();
        std::fs::write(dir.join("deploy/compose.yaml"), "services: {}\n").unwrap();
        let mut project = project();
        project.path = dir.to_string_lossy().into_owned();
        project.compose_files = vec!["deploy/compose.yaml".into(), "deploy/compose.yaml".into()];
        assert!(deployment_files(&project).err().unwrap().contains("重复"));
        if cfg!(windows) {
            project.compose_files[1] = "deploy\\compose.yaml".into();
            assert!(deployment_files(&project).err().unwrap().contains("重复"));
        }
        project.compose_files.pop();
        assert_eq!(
            deployment_files(&project).unwrap()[0].path,
            "deploy/compose.yaml"
        );
        std::fs::remove_file(dir.join("deploy/compose.yaml")).unwrap();
        std::fs::remove_dir(dir.join("deploy")).unwrap();
        std::fs::remove_dir(dir).unwrap();
    }

    #[test]
    fn source_digest_changes_with_compose_and_manifest_inputs() {
        let dir = std::env::temp_dir().join(format!(
            "devingpanel-digest-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("Dockerfile"), "FROM scratch\n").unwrap();
        std::fs::write(dir.join("compose.yaml"), "services: {}\n").unwrap();
        let mut project = project();
        project.path = dir.to_string_lossy().into_owned();
        let before = source_digest(&project).unwrap();
        std::fs::write(dir.join("compose.yaml"), "services: {web: {image: app}}\n").unwrap();
        assert_ne!(before, source_digest(&project).unwrap());
        project.manifest_mode = true;
        assert!(source_digest(&project).is_err());
        std::fs::write(dir.join("devingpanel.yaml"), "version: 1\n").unwrap();
        assert_ne!(before, source_digest(&project).unwrap());
        std::fs::remove_file(dir.join("devingpanel.yaml")).unwrap();
        std::fs::remove_file(dir.join("compose.yaml")).unwrap();
        std::fs::remove_file(dir.join("Dockerfile")).unwrap();
        std::fs::remove_dir(dir).unwrap();
    }

    #[test]
    fn rejects_unbound_or_missing_release_configuration() {
        let mut project = project();
        assert!(check_project_config(&project, &[]).is_err());
        assert!(check_project_config(&project, &[2]).is_err());
        assert!(check_project_config(&project, &[1, 1]).is_err());
        project.image.clear();
        assert!(check_project_config(&project, &[1]).is_err());
        project.image = "app".into();
        project.compose_files.clear();
        assert!(check_project_config(&project, &[1]).is_err());
    }

    #[test]
    fn target_architecture_requires_trust_and_matches_fixed_platform() {
        let connected = || TestResult::Connected {
            docker_version: "29.0".into(),
            compose_version: "5.0".into(),
            system_kernel: "Linux 6".into(),
            architecture: Some("linux/arm64".into()),
            disk_available_kib: 100,
            sftp_v3_handshake: true,
        };
        assert_eq!(
            checked_platform(connected(), "web", "auto").unwrap(),
            "linux/arm64"
        );
        assert!(checked_platform(connected(), "web", "linux/amd64").is_err());
        assert!(
            checked_platform(
                TestResult::Connected {
                    docker_version: "29.0".into(),
                    compose_version: "5.0".into(),
                    system_kernel: "Linux 6".into(),
                    architecture: Some("linux/arm64".into()),
                    disk_available_kib: 100,
                    sftp_v3_handshake: false,
                },
                "web",
                "auto"
            )
            .is_err()
        );
        assert!(
            checked_platform(
                TestResult::NeedsTrust {
                    address: "example.com".into(),
                    port: 22,
                    previous: None,
                    fingerprint: "SHA256:test".into(),
                },
                "web",
                "auto"
            )
            .is_err()
        );
    }

    #[test]
    fn transfer_requires_matching_archive_size_and_digest() {
        let archive = SavedArchive {
            path: std::env::temp_dir().join(format!(
                "devingpanel-nonexistent-{}",
                uuid::Uuid::new_v4().simple()
            )),
            bytes: 100,
            sha256: "a".repeat(64),
            image_ids: vec![format!("sha256:{}", "a".repeat(64))],
        };
        let mut uploaded = crate::ssh::UploadedArchive {
            directory: "/tmp/devingpanel.0123456789abcdef0123456789abcdef".into(),
            path: "/tmp/devingpanel.0123456789abcdef0123456789abcdef/image.tar".into(),
            bytes: 100,
            sha256: "a".repeat(64),
            image_ids: Vec::new(),
        };
        assert!(verify_transfer(&archive, &uploaded).is_ok());
        uploaded.bytes += 1;
        assert!(verify_transfer(&archive, &uploaded).is_err());
        uploaded.bytes -= 1;
        uploaded.sha256 = "b".repeat(64);
        assert!(verify_transfer(&archive, &uploaded).is_err());
        let mut cleanup_calls = 0;
        let error = finish_transfer(&archive, uploaded, |_| {
            cleanup_calls += 1;
            Err("连接中断".into())
        })
        .err()
        .unwrap();
        assert_eq!(cleanup_calls, 1);
        assert!(error.contains("清理失败"));
        assert!(error.contains("/tmp/devingpanel."));

        let uploaded = crate::ssh::UploadedArchive {
            directory: "/tmp/devingpanel.0123456789abcdef0123456789abcdef".into(),
            path: "/tmp/devingpanel.0123456789abcdef0123456789abcdef/image.tar".into(),
            bytes: archive.bytes,
            sha256: archive.sha256.clone(),
            image_ids: Vec::new(),
        };
        assert!(
            finish_transfer(&archive, uploaded, |_| panic!(
                "must not clean successful upload"
            ))
            .is_ok()
        );
    }

    #[test]
    fn plans_distinct_architecture_tags_and_rejects_invalid_input() {
        let project = project();
        let targets = vec![
            (1, "linux/amd64".into()),
            (2, "linux/arm64".into()),
            (3, "linux/amd64".into()),
        ];
        let plan = plan_images(&project, "v1.2", &targets).unwrap();
        assert_eq!(plan.len(), 2);
        assert_eq!(plan[0].reference, "app:v1.2-amd64");
        assert_eq!(plan[1].reference, "app:v1.2-arm64");
        assert!(plan_images(&project, "bad tag", &targets).is_err());
        assert!(plan_images(&project, &"a".repeat(128), &targets).is_err());
    }

    #[test]
    #[ignore = "requires local Docker Compose CLI"]
    fn validates_portable_project_before_release() {
        let dir =
            std::env::temp_dir().join(format!("deving-panel-release-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("Dockerfile"), "FROM scratch\n").unwrap();
        std::fs::write(
            dir.join("compose.yaml"),
            "services:\n  web:\n    image: ${DEVINGPANEL_IMAGE}\n",
        )
        .unwrap();
        let mut project = project();
        project.path = dir.to_string_lossy().into_owned();
        assert!(check_project_config(&project, &[1]).is_ok());
        std::fs::write(
            dir.join("compose.yaml"),
            "services:\n  web:\n    build: .\n    image: ${DEVINGPANEL_IMAGE}\n",
        )
        .unwrap();
        assert!(check_project_config(&project, &[1]).is_err());
        std::fs::write(dir.join("compose.yaml"),
            "services:\n  web:\n    image: ${DEVINGPANEL_IMAGE}\n  worker:\n    command: echo hello\n").unwrap();
        assert!(check_project_config(&project, &[1]).is_err());
        std::fs::remove_file(dir.join("compose.yaml")).unwrap();
        std::fs::remove_file(dir.join("Dockerfile")).unwrap();
        std::fs::remove_dir(dir).unwrap();
    }
}
