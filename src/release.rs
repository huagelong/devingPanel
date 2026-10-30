use crate::build::{self, BuiltImage, SavedArchive};
use crate::compose;
use crate::docker;
use crate::env_store;
use crate::host_store;
use crate::host_store::Host;
use crate::project_config;
use crate::project_store::{self, NewProject, Project};
use crate::release_archive;
use crate::release_materials;
use crate::release_store::{self, HealthResult, Intent, Outcome, Phase};
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
    let builds = if project.manifest_mode {
        project_config::load(Path::new(&project.path))?
            .builds
            .into_iter()
            .map(|build| (build.image, build.platform))
            .collect::<Vec<_>>()
    } else {
        vec![(project.image.clone(), project.platform.clone())]
    };
    plan_build_images(builds, base_tag, targets)
}

pub(crate) fn plan_manifest_images(
    manifest: &project_config::Manifest,
    base_tag: &str,
    targets: &[(i64, String)],
) -> Result<Vec<PlannedImage>, String> {
    plan_build_images(
        manifest
            .builds
            .iter()
            .map(|build| (build.image.clone(), build.platform.clone())),
        base_tag,
        targets,
    )
}

fn plan_build_images(
    builds: impl IntoIterator<Item = (String, String)>,
    base_tag: &str,
    targets: &[(i64, String)],
) -> Result<Vec<PlannedImage>, String> {
    if targets.is_empty() {
        return Err("请先核对目标主机架构。".into());
    }
    let platforms = targets
        .iter()
        .map(|(_, platform)| platform.as_str())
        .collect::<BTreeSet<_>>();
    let mut planned = Vec::new();
    for (image, requested_platform) in builds {
        if !project_store::valid_image_name(&image) {
            return Err("项目镜像名无效。".into());
        }
        for platform in &platforms {
            if requested_platform != "auto" && requested_platform != *platform {
                return Err("构建目标平台与发布主机不匹配。".into());
            }
            let suffix = match *platform {
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
                platform: (*platform).to_owned(),
                reference: format!("{image}:{tag}"),
            });
        }
    }
    Ok(planned)
}

pub fn check_image_tags(
    project: &Project,
    base_tag: &str,
    targets: &[(i64, String)],
) -> Result<Vec<PlannedImage>, String> {
    let planned = plan_images(project, base_tag, targets)?;
    check_planned_tags(&planned)?;
    Ok(planned)
}

pub(crate) fn check_planned_tags(planned: &[PlannedImage]) -> Result<(), String> {
    for image in planned {
        if docker::image_reference_exists(&image.reference)? {
            return Err(format!(
                "本地镜像 tag 已存在：{}；请更换发布标识。",
                image.reference
            ));
        }
    }
    Ok(())
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
    task_id: &str,
    service: &str,
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
    release_archive::retain(task_id, service, &archive)?;
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

pub(crate) fn source_digest(project: &Project) -> Result<String, String> {
    let root = Path::new(&project.path)
        .canonicalize()
        .map_err(|_| "项目目录不存在。")?;
    let builds = if project.manifest_mode {
        project_config::load(&root)?.builds
    } else {
        Vec::new()
    };
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
        "builds": builds.iter().map(|build| serde_json::json!({
            "service": build.service, "dockerfile": build.dockerfile,
            "image": build.image, "platform": build.platform, "args": build.args,
        })).collect::<Vec<_>>(),
    });
    let mut digest = Sha256::new();
    digest.update(serde_json::to_vec(&metadata).map_err(|_| "无法核对项目配置。")?);
    let mut paths = Vec::new();
    if project.manifest_mode {
        paths.push(project_config::FILE_NAME.to_owned());
    }
    paths.push(project.dockerfile.clone());
    paths.extend(builds.iter().map(|build| build.dockerfile.clone()));
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

pub(crate) fn deployment_files(project: &Project) -> Result<Vec<ssh::DeploymentFile>, String> {
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
    if project.manifest_mode {
        return Err("YAML v2 请使用整栈发布协调器。".into());
    }
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
    let _release_guard = release_materials::acquire_guard(project_id)?;
    if project_store::effective(project_id)?.manifest_mode {
        return crate::stack_release::run(project_id, host_id, base_tag, None, cancel, on_event);
    }
    let (project, host, image, digest) =
        prepare_single_host(project_id, host_id, base_tag, cancel.clone())?;
    let files = deployment_files(&project)?;
    let env_file = env_store::deployment_env_file(project_id, host_id)?;
    let materials = release_materials::prepare(&files, &env_file)?;
    if source_digest(&project_store::effective(project_id)?)? != digest {
        return Err("项目文件在冻结部署材料期间变化，请重新开始。".into());
    }
    let (docker_endpoint, docker_daemon_id) = ssh::docker_identity_for_host(host.clone())?;
    let docker_identity = ssh::ReleaseDockerIdentity {
        endpoint: &docker_endpoint,
        daemon_id: &docker_daemon_id,
    };
    let id = uuid::Uuid::new_v4().to_string();
    release_store::create(&Intent {
        id: id.clone(),
        project_id,
        project_name: project.name.clone(),
        host: host.clone(),
        docker_endpoint: docker_endpoint.clone(),
        docker_daemon_id: docker_daemon_id.clone(),
        image_reference: image.reference.clone(),
        platform: image.platform.clone(),
        images: vec![(
            project.service.clone(),
            image.reference.clone(),
            image.platform.clone(),
        )],
        config_sha256: digest.clone(),
    })?;
    if let Err(error) = release_materials::save(&id, materials) {
        let outcome = if error.contains("部分凭据可能残留") {
            Outcome::Unknown
        } else {
            Outcome::Failed
        };
        let _ = release_store::settle(&id, Phase::Prepared, outcome, "本机版本材料保存失败");
        return Err(error);
    }
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
    let uploaded = match transfer_built_image(
        &id,
        &project.service,
        host.clone(),
        &built,
        cancel.clone(),
        |sent, total| on_event(&format!("镜像传输 {sent}/{total} 字节")),
    ) {
        Ok(uploaded) => uploaded,
        Err(error) => {
            let _ =
                release_store::settle(&id, Phase::Uploading, Outcome::Unknown, "传输结果待核实");
            return Err(error);
        }
    };
    release_store::advance(&id, Phase::Uploading, Phase::Uploaded)?;
    release_store::advance(&id, Phase::Uploaded, Phase::Loading)?;
    if let Err(error) = release_store::record_image_ids(&id, &project.service, &uploaded.image_ids)
    {
        let cleanup = ssh::cleanup_uploaded_archive(host.clone(), &uploaded);
        let _ = release_store::settle(
            &id,
            Phase::Loading,
            Outcome::Unknown,
            "镜像归档 ID 保存结果待核实",
        );
        return Err(if cleanup.is_ok() {
            error
        } else {
            format!("{error}；远端临时归档清理待核实")
        });
    }
    on_event("远端加载镜像");
    if let Err(error) = ssh::load_uploaded_archive(
        host.clone(),
        docker_identity,
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
        docker_identity,
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
    let target = ssh::StackService {
        service: project.service.clone(),
        reference: built.reference.clone(),
        image_ids: uploaded.image_ids.clone(),
    };
    on_event("远端启动 Compose 服务");
    if let Err(error) = ssh::start_staged_compose(
        host.clone(),
        docker_identity,
        &staged,
        project_id,
        &target,
        cancel.clone(),
    ) {
        // A read-only reconnect can reveal partial effects but cannot justify a retry.
        let observed = ssh::check_staged_compose(
            host,
            docker_identity,
            &staged,
            project_id,
            &target,
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
        docker_identity,
        &staged,
        project_id,
        &target,
        cancel,
    ) {
        Ok(state) => state,
        Err(error) => {
            let _ = release_store::settle(&id, Phase::Checking, Outcome::Unknown, "容器状态待核实");
            return Err(error);
        }
    };
    let (health, detail) = match state {
        ssh::DeploymentState::Healthy => (HealthResult::Healthy, "容器健康；回滚执行尚未实现"),
        ssh::DeploymentState::RunningUnverified => (
            HealthResult::RunningUnverified,
            "容器运行中但无健康检查；回滚执行尚未实现",
        ),
        ssh::DeploymentState::Failed => (
            HealthResult::Failed,
            "目标容器未运行、镜像不符或健康检查失败，需人工处理",
        ),
    };
    release_store::settle_checked(&id, health, detail)?;
    Ok(id)
}

pub fn run_single_host_confirmed(
    project_id: i64,
    host_id: i64,
    base_tag: &str,
    confirmed_digest: &str,
    cancel: Arc<AtomicBool>,
    on_event: impl FnMut(&str),
) -> Result<String, String> {
    let _release_guard = release_materials::acquire_guard(project_id)?;
    if !project_store::effective(project_id)?.manifest_mode {
        return Err("GUI 仅支持 YAML v2 发布。".into());
    }
    crate::stack_release::run(
        project_id,
        host_id,
        base_tag,
        Some(confirmed_digest),
        cancel,
        on_event,
    )
}

pub fn observe_task(
    id: &str,
) -> Result<(release_store::TaskSnapshot, ssh::ReleaseObservation), String> {
    let task = release_store::get_unresolved(id)?;
    let observation = ssh::observe_release_project(&task)?;
    Ok((task, observation))
}

pub fn observe_task_images(id: &str) -> Result<Vec<(String, bool)>, String> {
    let task = release_store::get_task(id)?;
    let images = release_store::historical_images(id)?;
    ssh::observe_historical_images(&task, &images)
}

pub struct CurrentVersionCheck {
    pub matched_task_id: Option<String>,
    pub containers: usize,
    pub running_containers: usize,
    pub all_healthy: bool,
}

fn matches_completed_services(
    expected: &[release_store::ConfirmedService],
    observed: &[ssh::ObservedContainer],
) -> bool {
    !expected.is_empty()
        && expected.len() == observed.len()
        && expected.iter().all(|service| {
            observed
                .iter()
                .filter(|container| {
                    container.service == service.service
                        && container.status == "running"
                        && container.config_image == service.reference
                        && container.image_id == service.image_id
                })
                .count()
                == 1
        })
}

pub fn inspect_current_version(id: &str) -> Result<CurrentVersionCheck, String> {
    let task = release_store::get_task(id)?;
    let candidates = release_store::completed_versions_for(&task)?;
    let observation = ssh::observe_release_project(&task)?;
    let mut matched_task_id = None;
    for candidate in candidates {
        let services = release_store::confirmed_services(&candidate.id)?;
        if matches_completed_services(&services, &observation.containers) {
            if matched_task_id.is_some() {
                return Err("远端容器同时匹配多条已完成版本，不能判定当前版本。".into());
            }
            matched_task_id = Some(candidate.id);
        }
    }
    Ok(CurrentVersionCheck {
        matched_task_id,
        containers: observation.containers.len(),
        running_containers: observation
            .containers
            .iter()
            .filter(|item| item.status == "running")
            .count(),
        all_healthy: !observation.containers.is_empty()
            && observation
                .containers
                .iter()
                .all(|item| item.status == "running" && item.health.as_deref() == Some("healthy")),
    })
}

pub struct PreviousVersionCheck {
    pub task_id: String,
    pub images: usize,
}

pub fn inspect_previous_version(id: &str) -> Result<PreviousVersionCheck, String> {
    let previous = release_store::previous_healthy(id)?;
    let _snapshot = release_materials::load(&previous.id)?;
    let images = release_store::historical_images(&previous.id)?;
    for image in &images {
        crate::release_archive::verify(&previous.id, &image.service)?;
    }
    let remote = ssh::observe_historical_images(&previous, &images)?;
    if remote.len() != images.len() || remote.iter().any(|(_, matched)| !matched) {
        return Err("上一健康版本的远端镜像缺失或已变化。".into());
    }
    if release_store::previous_healthy(id)?.id != previous.id {
        return Err("预检期间发布历史已变化，请重新检查。".into());
    }
    Ok(PreviousVersionCheck {
        task_id: previous.id,
        images: images.len(),
    })
}

pub fn confirm_absent_and_close(id: &str) -> Result<(), String> {
    let task = release_store::get_unresolved(id)?;
    if !matches!(
        (task.phase.as_str(), task.outcome.as_str()),
        ("checked", "blocked") | ("checking", "unknown")
    ) {
        return Err("该阶段可能仍有远端操作，禁止解除阻断。".into());
    }
    if task.docker_endpoint.is_none() || task.docker_daemon_id.is_none() {
        return Err("旧任务缺少 Docker daemon 身份快照，不能自动解除阻断。".into());
    }
    let observation = ssh::observe_release_project(&task)?;
    if !observation.containers.is_empty() {
        return Err("远端仍存在该项目的容器，未解除阻断。".into());
    }
    release_store::resolve_absent(&task)
}

pub fn confirm_predeploy_unchanged(id: &str) -> Result<usize, String> {
    let task = release_store::get_unresolved(id)?;
    if task.phase != "uploading" || task.outcome != "unknown" {
        return Err("仅上传结果不明、尚未进入镜像加载的任务可按旧版本关闭。".into());
    }
    let _guard = release_materials::acquire_guard(task.project_id)?;
    let previous = release_store::current_completed_for(&task)?;
    let snapshot = release_materials::load(&previous.id)?;
    let images = release_store::historical_images(&previous.id)?;
    let observed = ssh::verify_running_config_hashes(&previous, &snapshot, &images)?;
    let expected = release_store::confirmed_services(&previous.id)?;
    if !matches_completed_services(&expected, &observed.containers)
        || observed
            .containers
            .iter()
            .any(|item| item.health.as_deref() != Some("healthy"))
    {
        return Err("远端并非完整健康的已确认旧版本；仍保持阻断。".into());
    }
    release_store::resolve_predeploy_unchanged(&task, &previous.id, &observed.containers)?;
    Ok(observed.containers.len())
}

pub fn confirm_healthy_and_complete(id: &str) -> Result<usize, String> {
    let task = release_store::get_unresolved(id)?;
    let _guard = release_materials::acquire_guard(task.project_id)?;
    if task.phase != "checked"
        || task.outcome != "blocked"
        || task.health_result.as_deref() != Some("healthy")
    {
        return Err("仅新任务的整栈健康结果可人工确认完成。".into());
    }
    let observation = verify_healthy_release(&task)?;
    release_store::complete_checked(&task, &observation)?;
    Ok(observation.len())
}

pub struct UnknownUploadInspection {
    pub archives: Vec<ssh::RemoteArchiveObservation>,
    pub containers: Vec<ssh::ObservedContainer>,
}

pub fn inspect_unknown_upload(id: &str) -> Result<UnknownUploadInspection, String> {
    let task = release_store::get_task(id)?;
    if task.phase != "uploading" || task.outcome != "unknown" {
        return Err("仅上传结果不明的任务可核实远端临时归档。".into());
    }
    let records = release_store::uploaded_archives(id)?;
    let archives = if records.is_empty() {
        Vec::new()
    } else {
        ssh::inspect_uploaded_archives(&task, &records)?
    };
    let containers = ssh::observe_release_project(&task)?.containers;
    let current = release_store::get_task(id)?;
    if current.phase != task.phase
        || current.outcome != task.outcome
        || current.host != task.host
        || current.docker_endpoint != task.docker_endpoint
        || current.docker_daemon_id != task.docker_daemon_id
    {
        return Err("核实期间任务或目标身份已变化，请重新读取。".into());
    }
    Ok(UnknownUploadInspection {
        archives,
        containers,
    })
}

pub fn reconcile_interrupted_health(id: &str) -> Result<usize, String> {
    let task = release_store::get_unresolved(id)?;
    let _guard = release_materials::acquire_guard(task.project_id)?;
    let eligible = (task.phase == "checking"
        && matches!(task.outcome.as_str(), "unknown" | "running"))
        || (task.phase == "deploying" && task.outcome == "unknown");
    if !eligible || task.health_result.is_some() {
        return Err("仅启动或健康结果不明的任务可人工只读恢复。".into());
    }
    if task.phase == "deploying" {
        ssh::verify_start_completed(&task)?;
    }
    let observation = verify_healthy_release(&task)?;
    release_store::reconcile_healthy(&task, &observation)?;
    Ok(observation.len())
}

fn verify_healthy_release(
    task: &release_store::TaskSnapshot,
) -> Result<Vec<ssh::ObservedContainer>, String> {
    let id = &task.id;
    let snapshot = release_materials::load(id)?;
    if snapshot.compose_files.is_empty() {
        return Err("当次 Compose 快照不完整。".into());
    }
    let built = release_store::historical_images(id)?;
    let services = release_store::planned_services(id)?;
    for image in &built {
        crate::release_archive::verify(id, &image.service)?;
    }
    ssh::verify_staged_materials(task, &snapshot)?;
    let remote_images = ssh::observe_historical_images(task, &built)?;
    if remote_images.len() != built.len() || remote_images.iter().any(|(_, available)| !available) {
        return Err("远端自有镜像缺失或已变化，未确认完成。".into());
    }
    let observation = ssh::observe_release_project(task)?;
    if !matching_healthy_services(&services, &observation.containers) {
        return Err("远端整栈服务数量、镜像或健康状态与当次计划不符。".into());
    }
    Ok(observation.containers)
}

fn matching_healthy_services(
    services: &[release_store::PlannedService],
    observed: &[ssh::ObservedContainer],
) -> bool {
    !services.is_empty()
        && services.len() == observed.len()
        && services.iter().all(|expected| {
            observed
                .iter()
                .filter(|item| item.service == expected.service)
                .count()
                == 1
                && observed.iter().any(|item| {
                    item.service == expected.service
                        && item.status == "running"
                        && item.health.as_deref() == Some("healthy")
                        && item.config_image == expected.reference
                        && (expected.image_ids.is_empty()
                            || expected.image_ids.contains(&item.image_id))
                })
        })
}

#[cfg(test)]
mod tests {
    use super::{
        check_project_config, checked_platform, deployment_files, finish_transfer,
        matches_completed_services, matching_healthy_services, plan_images, source_digest,
        verify_transfer,
    };
    use crate::build::SavedArchive;
    use crate::project_store::Project;
    use crate::release_store::{ConfirmedService, PlannedService};
    use crate::ssh::ObservedContainer;
    use crate::ssh::TestResult;

    fn isolated_series_data_dir() -> std::path::PathBuf {
        let root = std::path::PathBuf::from(std::env::var_os("APPDATA").unwrap())
            .canonicalize()
            .unwrap();
        let temp = std::env::temp_dir().canonicalize().unwrap();
        assert_eq!(root.parent(), Some(temp.as_path()));
        assert!(
            root.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("devingpanel-v20-series-")
        );
        let data = root.join("devingPanel").canonicalize().unwrap();
        assert_eq!(data.parent(), Some(root.as_path()));
        assert!(data.join("state.sqlite3").is_file());
        assert!(
            !std::fs::symlink_metadata(data.join("state.sqlite3"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        data
    }

    fn create_isolated_stack_project() -> Project {
        let data = isolated_series_data_dir();
        let sentinel = 1_000_000_000_i64 + (uuid::Uuid::new_v4().as_u128() % 1_000_000_000) as i64;
        crate::database::connect().unwrap().execute(
            "INSERT INTO projects (id, name, path, dockerfile, image) VALUES (?1, ?2, ?3, 'Dockerfile', 'reserved')",
            rusqlite::params![sentinel, format!("reserved-smoke-{sentinel}"), data.join(format!("reserved-{sentinel}")).to_string_lossy()],
        ).unwrap();
        let source =
            std::path::PathBuf::from(std::env::var("DEVINGPANEL_TEST_STACK_FIXTURE").unwrap());
        let suffix = uuid::Uuid::new_v4().simple().to_string();
        let name = format!("stack-series-{}", &suffix[..12]);
        let path = data.join(&name);
        std::fs::create_dir(&path).unwrap();
        for file in [
            "compose.yaml",
            "Dockerfile.web",
            "Dockerfile.worker",
            "probe-web.txt",
            "probe-worker.txt",
        ] {
            std::fs::copy(source.join(file), path.join(file)).unwrap();
        }
        let manifest = std::fs::read_to_string(source.join("devingpanel.yaml"))
            .unwrap()
            .replace("stack-v2-success", &name);
        std::fs::write(path.join("devingpanel.yaml"), manifest).unwrap();
        let project = crate::project_store::create_from_manifest(&path).unwrap();
        assert!(project.id > 1_000_000_000);
        let host_id = *project.host_ids.first().unwrap();
        let host = crate::host_store::list()
            .unwrap()
            .0
            .into_iter()
            .find(|host| host.id == host_id)
            .unwrap();
        let (endpoint, daemon_id) = crate::ssh::docker_identity_for_host(host.clone()).unwrap();
        let probe = crate::release_store::TaskSnapshot {
            id: uuid::Uuid::new_v4().to_string(),
            project_id: project.id,
            project_name: project.name.clone(),
            host,
            docker_endpoint: Some(endpoint),
            docker_daemon_id: Some(daemon_id),
            phase: "prepared".into(),
            outcome: "running".into(),
            health_result: None,
            detail: String::new(),
            started_at: String::new(),
            execution_finished_at: None,
            image_reference: String::new(),
        };
        crate::ssh::assert_empty_project_namespace(&probe).unwrap();
        assert!(
            crate::ssh::observe_release_project(&probe)
                .unwrap()
                .containers
                .is_empty()
        );
        project
    }

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
    fn completion_requires_exact_service_set_and_healthy_images() {
        let expected = vec![
            PlannedService {
                service: "web".into(),
                reference: "app:v1".into(),
                image_ids: vec![format!("sha256:{}", "a".repeat(64))],
            },
            PlannedService {
                service: "db".into(),
                reference: "postgres:16".into(),
                image_ids: vec![],
            },
        ];
        let observed = vec![
            ObservedContainer {
                service: "web".into(),
                status: "running".into(),
                health: Some("healthy".into()),
                image_id: expected[0].image_ids[0].clone(),
                config_image: "app:v1".into(),
            },
            ObservedContainer {
                service: "db".into(),
                status: "running".into(),
                health: Some("healthy".into()),
                image_id: format!("sha256:{}", "b".repeat(64)),
                config_image: "postgres:16".into(),
            },
        ];
        assert!(matching_healthy_services(&expected, &observed));
        assert!(!matching_healthy_services(&expected, &observed[..1]));
        let mut changed = observed.clone();
        changed[1].service = "web".into();
        assert!(!matching_healthy_services(&expected, &changed));
        changed = observed.clone();
        changed[1].health = None;
        assert!(!matching_healthy_services(&expected, &changed));
        changed = observed.clone();
        changed[0].image_id = format!("sha256:{}", "c".repeat(64));
        assert!(!matching_healthy_services(&expected, &changed));
    }

    #[test]
    fn current_version_requires_exact_container_images_not_latest_task_order() {
        let expected = vec![
            ConfirmedService {
                service: "web".into(),
                reference: "app:v1".into(),
                image_id: format!("sha256:{}", "a".repeat(64)),
            },
            ConfirmedService {
                service: "db".into(),
                reference: "postgres:16".into(),
                image_id: format!("sha256:{}", "b".repeat(64)),
            },
        ];
        let observed = vec![
            ObservedContainer {
                service: "web".into(),
                status: "running".into(),
                health: Some("healthy".into()),
                image_id: expected[0].image_id.clone(),
                config_image: expected[0].reference.clone(),
            },
            ObservedContainer {
                service: "db".into(),
                status: "running".into(),
                health: Some("healthy".into()),
                image_id: expected[1].image_id.clone(),
                config_image: expected[1].reference.clone(),
            },
        ];
        assert!(matches_completed_services(&expected, &observed));
        let mut partial = observed.clone();
        partial[0].image_id = format!("sha256:{}", "c".repeat(64));
        assert!(!matches_completed_services(&expected, &partial));
        partial = observed.clone();
        partial[1].config_image = "postgres:17".into();
        assert!(!matches_completed_services(&expected, &partial));
        assert!(!matches_completed_services(&expected, &observed[..1]));
        partial = observed.clone();
        partial[0].status = "exited".into();
        assert!(!matches_completed_services(&expected, &partial));
        partial = observed.clone();
        partial[1].service = "web".into();
        assert!(!matches_completed_services(&expected, &partial));
    }

    #[test]
    #[ignore = "requires isolated APPDATA database copy, trusted VPS credential and local Docker; creates an isolated remote stack"]
    fn real_two_version_stack_release_and_completion() {
        let project = create_isolated_stack_project();
        let host_id = *project
            .host_ids
            .first()
            .expect("fixture must bind the trusted test host");
        let host = crate::host_store::list()
            .unwrap()
            .0
            .into_iter()
            .find(|host| host.id == host_id)
            .unwrap();
        assert_eq!(host.id, host_id);
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let confirmed_digest = super::source_digest(&project).unwrap();
        let first = super::run_single_host_confirmed(
            project.id,
            host_id,
            &format!("smoke{}", uuid::Uuid::new_v4().simple()),
            &confirmed_digest,
            cancel.clone(),
            |_| {},
        )
        .unwrap();
        assert_eq!(
            crate::release_store::get_task(&first)
                .unwrap()
                .health_result
                .as_deref(),
            Some("healthy")
        );
        assert_eq!(super::confirm_healthy_and_complete(&first).unwrap(), 4);
        assert_eq!(
            crate::release_store::get_task(&first).unwrap().outcome,
            "complete"
        );
        let second = super::run_single_host_confirmed(
            project.id,
            host_id,
            &format!("smoke{}", uuid::Uuid::new_v4().simple()),
            &confirmed_digest,
            cancel,
            |_| {},
        )
        .unwrap();
        assert_eq!(
            super::inspect_previous_version(&second).unwrap().task_id,
            first
        );
        assert_eq!(super::confirm_healthy_and_complete(&second).unwrap(), 4);
        assert_eq!(
            crate::release_store::get_task(&second).unwrap().outcome,
            "complete"
        );
        if std::env::var("DEVINGPANEL_TEST_FORCE_MISSING_TARGET_TAGS").as_deref() == Ok("1") {
            let source = crate::release_store::get_task(&second).unwrap();
            let target = crate::release_store::get_task(&first).unwrap();
            let images = crate::release_store::historical_images(&first).unwrap();
            crate::ssh::remove_isolated_target_image_tags(&source, &target, &images).unwrap();
        }
        let interrupted =
            std::env::var("DEVINGPANEL_TEST_INTERRUPT_ROLLBACK_CHECK").as_deref() == Ok("1");
        let preview = crate::rollback::preview(&second).unwrap();
        assert_eq!(preview.target_id, first);
        assert_eq!(preview.observed.len(), 4);
        let result = crate::rollback::run_confirmed(
            &second,
            Some(&preview.target_id),
            Some(&preview.observed),
            Some(&preview.missing_images),
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            |event| {
                if interrupted && event == "核对恢复后的全部服务健康状态" {
                    crate::ssh::inject_next_stack_check_disconnect(project.id);
                }
            },
        );
        let rollback_id = if interrupted {
            assert!(result.unwrap_err().contains("SSH 连接中断"));
            let pending = crate::rollback_store::recent(Some(project.id)).unwrap();
            assert_eq!(pending.len(), 1);
            assert_eq!(
                (pending[0].phase.as_str(), pending[0].outcome.as_str()),
                ("checking", "unknown")
            );
            let current: String = crate::database::connect()
                .unwrap()
                .query_row(
                    "SELECT task_id FROM project_current_version WHERE project_id = ?1",
                    [project.id],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(current, second);
            let source = crate::release_store::get_task(&second).unwrap();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
            loop {
                let remote = crate::ssh::observe_release_project(&source).unwrap();
                if remote.containers.len() == 4
                    && remote.containers.iter().all(|item| {
                        item.status == "running" && item.health.as_deref() == Some("healthy")
                    })
                {
                    break;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "restored stack never became healthy"
                );
                std::thread::sleep(std::time::Duration::from_secs(3));
            }
            assert_eq!(crate::rollback::reconcile(&pending[0].id).unwrap(), 4);
            let audit: i64 = crate::database::connect().unwrap().query_row(
                "SELECT observed_containers FROM rollback_reconciliations WHERE rollback_id = ?1",
                [&pending[0].id], |row| row.get(0)).unwrap();
            assert_eq!(audit, 4);
            pending[0].id.clone()
        } else {
            result.unwrap()
        };
        let conn = crate::database::connect().unwrap();
        let (outcome, target): (String, String) = conn
            .query_row(
                "SELECT outcome, target_task_id FROM rollback_tasks WHERE id = ?1",
                [&rollback_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            (outcome.as_str(), target.as_str()),
            ("complete", first.as_str())
        );
        assert_eq!(
            super::inspect_current_version(&second)
                .unwrap()
                .matched_task_id
                .as_deref(),
            Some(first.as_str())
        );
    }

    #[test]
    #[ignore = "read-only SSH observation of an explicit high-ID project in an isolated APPDATA copy"]
    fn real_isolated_series_observation() {
        let _data = isolated_series_data_dir();
        let project_id: i64 = std::env::var("DEVINGPANEL_TEST_SERIES_PROJECT_ID")
            .unwrap()
            .parse()
            .unwrap();
        assert!(project_id > 1_000_000_000);
        let tasks = crate::release_store::recent(Some(project_id)).unwrap();
        assert!(!tasks.is_empty());
        let current = super::inspect_current_version(&tasks[0].id).unwrap();
        eprintln!(
            "matched completed version {:?}, all healthy {}",
            current.matched_task_id, current.all_healthy
        );
        let retention = crate::release_store::retention_plan(project_id).unwrap();
        assert!(!retention.unresolved);
        assert_eq!(retention.entries.iter().filter(|entry| entry.class == crate::release_store::RetentionClass::LatestCompleted).count(), 1);
        assert_eq!(
            retention
                .entries
                .iter()
                .find(|entry| entry.class == crate::release_store::RetentionClass::LatestCompleted)
                .unwrap()
                .task_id,
            current.matched_task_id.as_ref().unwrap().as_str()
        );
        let local_archives = crate::release_archive::preview_local_reclaim(project_id).unwrap();
        assert!(!local_archives.blocked);
        if project_id == 1_210_048_068 {
            assert!(local_archives.unavailable.is_empty());
            assert_eq!(local_archives.candidates.len(), 2);
            let candidate_ids = local_archives
                .candidates
                .iter()
                .map(|entry| entry.task_id.as_str())
                .collect::<std::collections::HashSet<_>>();
            assert_eq!(
                candidate_ids,
                std::collections::HashSet::from([
                    "adaf8c60-f460-4f1d-9da6-1225c5fad7b7",
                    "ba6717cf-7d58-488a-8a31-9393bd3df38c",
                ])
            );
            assert!(
                local_archives
                    .candidates
                    .iter()
                    .all(|entry| entry.services == 2 && entry.verified_bytes > 0)
            );
        }
        assert!(
            local_archives
                .candidates
                .iter()
                .all(|entry| Some(&entry.task_id) != current.matched_task_id.as_ref())
        );
        eprintln!(
            "verified historical local archives: {} candidates, {} unavailable",
            local_archives.candidates.len(),
            local_archives.unavailable.len()
        );
        let completed = crate::release_store::current_completed_for(&tasks[0]).unwrap();
        let snapshot = crate::release_materials::load(&completed.id).unwrap();
        let images = crate::release_store::historical_images(&completed.id).unwrap();
        crate::ssh::verify_running_config_hashes(&completed, &snapshot, &images).unwrap();
        eprintln!("historical Compose hashes match current containers");
        for task in tasks {
            assert_eq!(task.project_id, project_id);
            let observed = crate::ssh::observe_release_project(&task).unwrap();
            eprintln!(
                "task {} {} / {}: {} containers",
                task.id,
                task.phase,
                task.outcome,
                observed.containers.len()
            );
            for container in observed.containers {
                eprintln!(
                    "{} {} {:?}",
                    container.service, container.status, container.health
                );
            }
        }
    }

    #[test]
    #[ignore = "requires explicit isolated high-ID VPS project with uploading/unknown and healthy prior version"]
    fn real_close_predeploy_unknown_without_remote_changes() {
        let _data = isolated_series_data_dir();
        let project_id: i64 = std::env::var("DEVINGPANEL_TEST_SERIES_PROJECT_ID")
            .unwrap()
            .parse()
            .unwrap();
        assert!(project_id > 1_000_000_000);
        let tasks = crate::release_store::recent(Some(project_id)).unwrap();
        let pending = tasks
            .iter()
            .find(|task| task.phase == "uploading" && task.outcome == "unknown")
            .expect("must select only the isolated unknown upload");
        let before = super::inspect_current_version(&pending.id).unwrap();
        assert_eq!(before.containers, 4);
        assert!(before.all_healthy);
        let previous_id = before
            .matched_task_id
            .expect("must match one completed version");
        assert_eq!(super::confirm_predeploy_unchanged(&pending.id).unwrap(), 4);
        let resolved = crate::release_store::get_task(&pending.id).unwrap();
        assert_eq!(
            (resolved.phase.as_str(), resolved.outcome.as_str()),
            ("checked", "failed")
        );
        let audit: (String, i64) = crate::database::connect().unwrap().query_row(
            "SELECT decision, observed_containers FROM release_task_resolutions WHERE task_id = ?1",
            [&pending.id], |row| Ok((row.get(0)?, row.get(1)?)),
        ).unwrap();
        assert_eq!(audit, ("confirmed_previous_running_predeploy".into(), 4));
        let after = super::inspect_current_version(&pending.id).unwrap();
        assert_eq!(after.matched_task_id.as_deref(), Some(previous_id.as_str()));
        assert!(after.all_healthy);
    }

    #[test]
    #[ignore = "one-shot isolated VPS release and confirmed rollback after manual upload resolution"]
    fn real_resume_after_predeploy_close_and_rollback() {
        let data = isolated_series_data_dir();
        let project_id: i64 = std::env::var("DEVINGPANEL_TEST_SERIES_PROJECT_ID")
            .unwrap()
            .parse()
            .unwrap();
        assert!(project_id > 1_000_000_000);
        let project = crate::project_store::effective(project_id).unwrap();
        assert!(project.manifest_mode && project.name.starts_with("stack-series-"));
        assert!(
            std::path::Path::new(&project.path)
                .canonicalize()
                .unwrap()
                .starts_with(&data)
        );
        assert_eq!(project.host_ids.len(), 1);
        let tasks = crate::release_store::recent(Some(project_id)).unwrap();
        let first = tasks
            .iter()
            .find(|task| {
                task.outcome == "complete" && task.health_result.as_deref() == Some("healthy")
            })
            .expect("prior healthy version required");
        assert!(tasks.iter().any(|task| task.phase == "checked"
            && task.outcome == "failed"
            && task.detail.contains("上传中断任务关闭")));
        let first_id = first.id.clone();
        let current = super::inspect_current_version(&first_id).unwrap();
        assert_eq!(current.matched_task_id.as_deref(), Some(first_id.as_str()));
        assert!(current.all_healthy);
        let digest = super::source_digest(&project).unwrap();
        let second = super::run_single_host_confirmed(
            project_id,
            project.host_ids[0],
            &format!("smoke{}", uuid::Uuid::new_v4().simple()),
            &digest,
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            |_| {},
        )
        .unwrap();
        assert_eq!(super::confirm_healthy_and_complete(&second).unwrap(), 4);
        let preview = crate::rollback::preview(&second).unwrap();
        assert_eq!(preview.target_id, first_id);
        let rollback_id = crate::rollback::run_confirmed(
            &second,
            Some(&preview.target_id),
            Some(&preview.observed),
            Some(&preview.missing_images),
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            |_| {},
        )
        .unwrap();
        assert_eq!(
            crate::rollback_store::get(&rollback_id).unwrap().outcome,
            "complete"
        );
        let after = super::inspect_current_version(&second).unwrap();
        assert_eq!(after.matched_task_id.as_deref(), Some(first_id.as_str()));
        assert!(after.all_healthy);
    }

    #[test]
    #[ignore = "requires isolated APPDATA database copy, trusted VPS credential and local Docker; injects one client-side SSH check failure after remote startup"]
    fn real_health_check_disconnect_and_read_only_reconciliation() {
        let project = create_isolated_stack_project();
        let host_id = project.host_ids[0];
        let mut injected = false;
        let result = super::run_single_host_until_checked(
            project.id,
            host_id,
            &format!("drop{}", uuid::Uuid::new_v4().simple()),
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            |event| {
                if event == "核对全部服务健康状态" {
                    crate::ssh::inject_next_stack_check_disconnect(project.id);
                    injected = true;
                }
            },
        );
        assert!(injected);
        assert!(result.unwrap_err().contains("SSH 连接中断"));
        let tasks = crate::release_store::recent(Some(project.id)).unwrap();
        assert_eq!(tasks.len(), 1);
        let task = &tasks[0];
        assert_eq!(
            (
                task.phase.as_str(),
                task.outcome.as_str(),
                task.health_result.as_deref()
            ),
            ("checking", "unknown", None)
        );
        assert!(super::confirm_healthy_and_complete(&task.id).is_err());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
        loop {
            let current = crate::ssh::observe_release_project(task).unwrap();
            if current.containers.len() == 4
                && current.containers.iter().all(|item| {
                    item.status == "running" && item.health.as_deref() == Some("healthy")
                })
            {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "remote stack never became healthy"
            );
            std::thread::sleep(std::time::Duration::from_secs(3));
        }
        assert_eq!(super::reconcile_interrupted_health(&task.id).unwrap(), 4);
        let recovered = crate::release_store::get_task(&task.id).unwrap();
        assert_eq!(
            (
                recovered.phase.as_str(),
                recovered.outcome.as_str(),
                recovered.health_result.as_deref()
            ),
            ("checked", "blocked", Some("healthy"))
        );
        let audit: i64 = crate::database::connect().unwrap().query_row(
            "SELECT observed_containers FROM release_task_health_reconciliations WHERE task_id = ?1",
            [&task.id], |row| row.get(0),
        ).unwrap();
        assert_eq!(audit, 4);
        assert_eq!(super::confirm_healthy_and_complete(&task.id).unwrap(), 4);
        assert_eq!(
            crate::release_store::get_task(&task.id).unwrap().outcome,
            "complete"
        );
        assert!(super::reconcile_interrupted_health(&task.id).is_err());
    }

    #[test]
    #[ignore = "continues an isolated two-version VPS test project; leaves its third release blocked/unhealthy"]
    fn real_failed_third_release_preserves_completed_versions() {
        let data = isolated_series_data_dir().canonicalize().unwrap();
        let project_id: i64 = std::env::var("DEVINGPANEL_TEST_SERIES_PROJECT_ID")
            .unwrap()
            .parse()
            .unwrap();
        assert!(project_id > 1_000_000_000);
        let source =
            std::path::PathBuf::from(std::env::var("DEVINGPANEL_TEST_FAILURE_FIXTURE").unwrap());
        let project = crate::project_store::effective(project_id).unwrap();
        assert!(project.name.starts_with("stack-series-"));
        assert!(std::path::Path::new(&project.path).starts_with(&data));
        let host_id = project.host_ids[0];
        let before: Vec<_> = crate::release_store::recent(Some(project_id))
            .unwrap()
            .into_iter()
            .filter(|task| task.outcome == "complete")
            .collect();
        assert_eq!(before.len(), 2);
        let faulty = std::fs::read_to_string(source.join("compose.yaml")).unwrap();
        let marker = "\nnetworks:\n";
        assert_eq!(faulty.matches(marker).count(), 1);
        let sidecar = "\n  sidecar:\n    image: redis:7-alpine\n    command: ['redis-server', '--save', '', '--appendonly', 'no']\n    networks: [isolated]\n    healthcheck:\n      test: ['CMD', 'redis-cli', 'ping']\n      interval: 3s\n      timeout: 2s\n      retries: 20\n";
        std::fs::write(
            std::path::Path::new(&project.path).join("compose.yaml"),
            faulty.replace(marker, &format!("{sidecar}{marker}")),
        )
        .unwrap();
        let id = super::run_single_host_until_checked(
            project_id,
            host_id,
            &format!("fault{}", uuid::Uuid::new_v4().simple()),
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            |_| {},
        )
        .unwrap();
        let task = crate::release_store::get_task(&id).unwrap();
        assert_eq!(
            (
                task.phase.as_str(),
                task.outcome.as_str(),
                task.health_result.as_deref()
            ),
            ("checked", "blocked", Some("failed"))
        );
        let rollback_candidate = crate::rollback_store::candidate(&id).unwrap();
        let (rolled_from, restored): (String, String) = crate::database::connect().unwrap().query_row(
            "SELECT source_task_id, target_task_id FROM rollback_tasks WHERE project_id = ?1 AND outcome = 'complete'",
            [project_id], |row| Ok((row.get(0)?, row.get(1)?)),
        ).unwrap();
        assert_eq!(rollback_candidate.target_task_id, restored);
        assert_ne!(rollback_candidate.target_task_id, rolled_from);
        let rollback_id = crate::rollback::run(
            &id,
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            |_| {},
        )
        .unwrap();
        let (status, target): (String, String) = crate::database::connect()
            .unwrap()
            .query_row(
                "SELECT outcome, target_task_id FROM rollback_tasks WHERE id = ?1",
                [&rollback_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            (status.as_str(), target.as_str()),
            ("complete", restored.as_str())
        );
        let restored_state = super::inspect_current_version(&id).unwrap();
        assert_eq!(
            restored_state.matched_task_id.as_deref(),
            Some(restored.as_str())
        );
        assert_eq!(restored_state.containers, 4);
        assert_eq!(
            crate::release_store::get_task(&id).unwrap().outcome,
            "failed"
        );
        assert!(super::confirm_healthy_and_complete(&id).is_err());
        let after: Vec<_> = crate::release_store::recent(Some(project_id))
            .unwrap()
            .into_iter()
            .filter(|task| task.outcome == "complete")
            .collect();
        assert_eq!(after.len(), 2);
        assert_eq!(
            after
                .iter()
                .map(|task| &task.id)
                .collect::<std::collections::HashSet<_>>(),
            before.iter().map(|task| &task.id).collect()
        );
        for old in &after {
            crate::release_materials::load(&old.id).unwrap();
            let images = crate::release_store::historical_images(&old.id).unwrap();
            assert_eq!(images.len(), 2);
            for image in &images {
                crate::release_archive::verify(&old.id, &image.service).unwrap();
            }
            let remote = crate::ssh::observe_historical_images(old, &images).unwrap();
            assert_eq!(remote.len(), images.len());
            assert!(remote.iter().all(|(_, available)| *available));
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
        std::fs::write(dir.join("devingpanel.yaml"), "version: 2\nname: app\ntag: '{timestamp}'\nbuilds:\n  - service: web\n    dockerfile: Dockerfile\n    image: app\n    platform: auto\n    args: {}\ndeploy:\n  compose:\n    files: [compose.yaml]\n  targets:\n    - group: test\n      host: web\n").unwrap();
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
    fn plans_each_manifest_build_with_shared_release_tag() {
        let dir = std::env::temp_dir().join(format!(
            "devingpanel-multibuild-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("Dockerfile"), "FROM scratch\n").unwrap();
        std::fs::write(dir.join("Dockerfile.worker"), "FROM scratch\n").unwrap();
        std::fs::write(dir.join("compose.yaml"), "services: {}\n").unwrap();
        std::fs::write(dir.join("devingpanel.yaml"), "version: 2\nname: app\ntag: '{timestamp}'\nbuilds:\n  - service: web\n    dockerfile: Dockerfile\n    image: app-web\n    platform: auto\n    args: {}\n  - service: worker\n    dockerfile: Dockerfile.worker\n    image: app-worker\n    platform: auto\n    args: {}\ndeploy:\n  compose:\n    files: [compose.yaml]\n  targets:\n    - group: test\n      host: web\n").unwrap();
        let mut project = project();
        project.path = dir.to_string_lossy().into_owned();
        project.manifest_mode = true;
        let targets = vec![(1, "linux/amd64".into()), (2, "linux/arm64".into())];
        let plan = plan_images(&project, "v3", &targets).unwrap();
        assert_eq!(plan.len(), 4);
        assert_eq!(
            plan.iter()
                .map(|image| image.reference.as_str())
                .collect::<Vec<_>>(),
            [
                "app-web:v3-amd64",
                "app-web:v3-arm64",
                "app-worker:v3-amd64",
                "app-worker:v3-arm64"
            ]
        );
        let before = source_digest(&project).unwrap();
        std::fs::write(dir.join("Dockerfile.worker"), "FROM busybox\n").unwrap();
        assert_ne!(before, source_digest(&project).unwrap());
        for file in [
            "Dockerfile",
            "Dockerfile.worker",
            "compose.yaml",
            "devingpanel.yaml",
        ] {
            std::fs::remove_file(dir.join(file)).unwrap();
        }
        std::fs::remove_dir(dir).unwrap();
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
