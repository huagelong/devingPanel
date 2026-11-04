use crate::build;
use crate::compose;
use crate::docker;
use crate::env_store;
use crate::host_store;
use crate::project_config::{self, BuildSpec, Manifest};
use crate::project_store::{self, Project};
use crate::registry;
use crate::release;
use crate::release_materials;
use crate::release_store::{self, HealthResult, Intent, Outcome, Phase};
use crate::ssh::{self, DeploymentState, StackService};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::path::Path;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

const UNKNOWN_REGISTRY_PUSH: &str = "镜像仓库推送结果待核实";

fn build_project(project: &Project, spec: &BuildSpec) -> Project {
    let mut item = project.clone();
    item.dockerfile = spec.dockerfile.clone();
    item.image = spec.image.clone();
    item.platform = spec.platform.clone();
    item.build_args = spec.args.clone();
    item.service = spec.service.clone();
    item
}

fn cleanup_known_archives<T>(
    archives: &[T],
    skip: Option<usize>,
    mut cleanup: impl FnMut(&T) -> Result<(), String>,
) -> Result<(), String> {
    let mut failed = false;
    for (index, archive) in archives.iter().enumerate() {
        if Some(index) != skip && cleanup(archive).is_err() {
            failed = true;
        }
    }
    if failed {
        Err("已上传归档清理不完整，需核对远端临时目录。".into())
    } else {
        Ok(())
    }
}

#[derive(Clone)]
pub struct BatchBuild {
    pub service: String,
    pub platform: String,
    pub reference: String,
}

#[derive(Clone)]
pub struct BatchHost {
    pub host_id: i64,
    pub platform: String,
    pub images: Vec<(String, String)>,
}

#[derive(Clone)]
pub struct BatchPlan {
    pub builds: Vec<BatchBuild>,
    pub hosts: Vec<BatchHost>,
}

#[allow(dead_code)] // Retained for the batch executor; the GUI still exposes only the read-only matrix.
pub struct BatchTargetIdentity {
    pub host_id: i64,
    pub host: host_store::Host,
    pub platform: String,
    pub docker_endpoint: String,
    pub docker_daemon_id: String,
}

#[allow(dead_code)] // The executable batch path will consume the verified snapshots.
pub struct BatchPreflight {
    pub plan: BatchPlan,
    pub config_sha256: String,
    pub builds: Vec<BuildSpec>,
    pub compose_files: Vec<ssh::DeploymentFile>,
    pub env_files: Vec<Vec<u8>>,
    pub targets: Vec<BatchTargetIdentity>,
}

fn batch_env_file(input: &[u8], images: &[(String, String)]) -> Result<Vec<u8>, String> {
    if input
        .split(|byte| *byte == b'\n')
        .any(|line| line.starts_with(b"DEVINGPANEL_IMAGE_"))
    {
        return Err("环境变量与构建镜像变量冲突。".into());
    }
    let mut result = input.to_vec();
    if !result.is_empty() && !result.ends_with(b"\n") {
        result.push(b'\n');
    }
    for (service, reference) in images {
        let variable = project_config::image_variable(service)?;
        result.extend_from_slice(format!("{variable}=\"{reference}\"\n").as_bytes());
    }
    Ok(result)
}

fn env_digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn ensure_distinct_daemons(targets: &[BatchTargetIdentity]) -> Result<(), String> {
    let mut seen = HashSet::new();
    if targets
        .iter()
        .any(|target| !seen.insert(&target.docker_daemon_id))
    {
        return Err("YAML 多台目标指向同一 Docker daemon，请只保留其中一台主机别名。".into());
    }
    Ok(())
}

fn same_batch_hosts(targets: &[BatchTargetIdentity], hosts: &[host_store::Host]) -> bool {
    targets
        .iter()
        .all(|target| hosts.iter().find(|host| host.id == target.host_id) == Some(&target.host))
}

fn batch_plan_from(
    builds: &[BuildSpec],
    host_ids: &[i64],
    targets: &[(i64, String)],
    planned: &[release::PlannedImage],
) -> Result<BatchPlan, String> {
    if builds.is_empty()
        || targets.is_empty()
        || host_ids.len() != targets.len()
        || !host_ids
            .iter()
            .zip(targets)
            .all(|(expected, (actual, _))| expected == actual)
        || targets
            .iter()
            .map(|(id, _)| id)
            .collect::<HashSet<_>>()
            .len()
            != targets.len()
    {
        return Err("批次目标顺序或构建服务无效。".into());
    }
    let mut platforms = targets
        .iter()
        .map(|(_, platform)| platform.as_str())
        .collect::<Vec<_>>();
    platforms.sort_unstable();
    platforms.dedup();
    if planned.len() != builds.len() * platforms.len() {
        return Err("批次构建镜像矩阵不完整。".into());
    }
    let mut matrix = Vec::new();
    for spec in builds {
        for platform in &platforms {
            let matching = planned
                .iter()
                .filter(|image| {
                    image.platform == *platform
                        && image
                            .reference
                            .rsplit_once(':')
                            .is_some_and(|(name, _)| name == spec.image)
                })
                .collect::<Vec<_>>();
            if matching.len() != 1 {
                return Err("批次服务与架构的镜像引用不唯一。".into());
            }
            matrix.push(BatchBuild {
                service: spec.service.clone(),
                platform: (*platform).into(),
                reference: matching[0].reference.clone(),
            });
        }
    }
    let hosts = targets
        .iter()
        .map(|(host_id, platform)| BatchHost {
            host_id: *host_id,
            platform: platform.clone(),
            images: matrix
                .iter()
                .filter(|image| image.platform == *platform)
                .map(|image| (image.service.clone(), image.reference.clone()))
                .collect(),
        })
        .collect();
    Ok(BatchPlan {
        builds: matrix,
        hosts,
    })
}

pub fn plan_batch(
    project: &Project,
    base_tag: &str,
    targets: &[(i64, String)],
) -> Result<BatchPlan, String> {
    if !project.manifest_mode {
        return Err("批次规划仅支持 devingpanel.yaml v2。".into());
    }
    let manifest = project_config::load(Path::new(&project.path))?;
    let (hosts, _) = host_store::list()?;
    plan_batch_from_manifest(project, &manifest, &hosts, base_tag, targets)
}

#[allow(dead_code)] // Requires a new human confirmation after restart; never resumes automatically.
pub fn reconfirm_batch(batch_id: &str, cancel: Arc<AtomicBool>) -> Result<BatchPreflight, String> {
    let batch = crate::batch_store::get(batch_id)?;
    if !matches!(
        batch.state.as_str(),
        "ready" | "running" | "paused" | "unknown"
    ) || batch.targets.len() < 2
    {
        return Err("批次不处于可重新确认的状态。".into());
    }
    let project = project_store::effective(batch.project_id)?;
    if !project.manifest_mode
        || release::source_digest(&project)? != batch.config_sha256
        || project.host_ids
            != batch
                .targets
                .iter()
                .map(|target| target.host.id)
                .collect::<Vec<_>>()
    {
        return Err("批次源文件或目标绑定已变化，不能沿用原批次。".into());
    }
    let manifest = project_config::load(Path::new(&project.path))?;
    let (hosts, _) = host_store::list()?;
    if project_config::resolve_targets(&manifest, &hosts)? != project.host_ids
        || batch
            .targets
            .iter()
            .any(|target| hosts.iter().find(|host| host.id == target.host.id) != Some(&target.host))
    {
        return Err("批次可信主机资料已变化。".into());
    }
    let first = batch.images.first().ok_or("批次没有共享镜像计划。")?;
    let tag = first
        .reference
        .rsplit_once(':')
        .ok_or("批次镜像引用无效。")?
        .1;
    let suffix = match first.platform.as_str() {
        "linux/amd64" => "-amd64",
        "linux/arm64" => "-arm64",
        _ => return Err("批次镜像平台无效。".into()),
    };
    let base_tag = tag
        .strip_suffix(suffix)
        .filter(|tag| !tag.is_empty())
        .ok_or("批次发布标识无法还原。")?;
    let platforms = batch
        .targets
        .iter()
        .map(|target| (target.host.id, target.platform.clone()))
        .collect::<Vec<_>>();
    let plan = plan_batch_from_manifest(&project, &manifest, &hosts, base_tag, &platforms)?;
    if plan.builds.len() != batch.images.len()
        || plan.builds.iter().any(|image| {
            !batch.images.iter().any(|saved| {
                image.service == saved.service
                    && image.platform == saved.platform
                    && image.reference == saved.reference
                    && saved.archive_bytes.is_some()
                    && saved.archive_sha256.is_some()
                    && saved.image_ids.is_some()
            })
        })
    {
        return Err("批次共享镜像矩阵或归档记录已变化。".into());
    }
    for image in &plan.builds {
        if cancel.load(Ordering::Relaxed) {
            return Err("批次重新确认已取消。".into());
        }
        crate::release_archive::verify_shared(batch_id, &image.service, &image.platform)?;
    }
    let compose_files = release::deployment_files(&project)?;
    let mut env_files = Vec::new();
    let mut targets = Vec::new();
    for (saved, host_plan) in batch.targets.iter().zip(&plan.hosts) {
        if cancel.load(Ordering::Relaxed) {
            return Err("批次重新确认已取消。".into());
        }
        let env = batch_env_file(
            &env_store::deployment_env_file(batch.project_id, saved.host.id)?,
            &host_plan.images,
        )?;
        if saved.env_sha256.as_deref() != Some(env_digest(&env).as_str()) {
            return Err("批次冻结环境已变化，不能以新环境沿用旧批次。".into());
        }
        release_materials::prepare(&compose_files, &env)?;
        env_files.push(env);
        targets.push(BatchTargetIdentity {
            host_id: saved.host.id,
            host: saved.host.clone(),
            platform: saved.platform.clone(),
            docker_endpoint: saved.docker_endpoint.clone(),
            docker_daemon_id: saved.docker_daemon_id.clone(),
        });
    }
    let fresh = project_store::effective(batch.project_id)?;
    if cancel.load(Ordering::Relaxed)
        || fresh.host_ids != project.host_ids
        || release::source_digest(&fresh)? != batch.config_sha256
    {
        return Err("批次重新确认期间项目配置已变化。".into());
    }
    Ok(BatchPreflight {
        plan,
        config_sha256: batch.config_sha256,
        builds: manifest.builds,
        compose_files,
        env_files,
        targets,
    })
}

fn plan_batch_from_manifest(
    project: &Project,
    manifest: &Manifest,
    hosts: &[host_store::Host],
    base_tag: &str,
    targets: &[(i64, String)],
) -> Result<BatchPlan, String> {
    let current_ids = project_config::resolve_targets(manifest, hosts)?;
    if manifest.name != project.name || current_ids != project.host_ids {
        return Err("YAML 项目或目标列表在批次规划期间变化。".into());
    }
    let planned = release::plan_manifest_images(manifest, base_tag, targets)?;
    batch_plan_from(&manifest.builds, &current_ids, targets, &planned)
}

#[allow(dead_code)] // The batch execution entry point is deliberately disabled until persistence is ready.
pub fn preflight_batch(
    project_id: i64,
    base_tag: &str,
    cancel: Arc<AtomicBool>,
) -> Result<BatchPreflight, String> {
    let project = project_store::effective(project_id)?;
    if !project.manifest_mode || project.host_ids.len() < 2 {
        return Err("批次预检仅支持 YAML v2 多主机项目。".into());
    }
    release::check_project_config(&project, &project.host_ids)?;
    env_store::check_deployment_environment(project_id, &project.host_ids)?;
    let manifest = project_config::load(Path::new(&project.path))?;
    let (hosts, _) = host_store::list()?;
    let ids = project_config::resolve_targets(&manifest, &hosts)?;
    if manifest.name != project.name || ids != project.host_ids {
        return Err("YAML 项目或目标主机绑定已变化，请重新预检。".into());
    }
    let digest = release::source_digest(&project)?;
    if project_config::load(Path::new(&project.path))? != manifest {
        return Err("YAML 在批次预检期间变化，请重新预检。".into());
    }
    let platforms = release::probe_targets(&project, &ids, &hosts, cancel.clone())?;
    let supported = docker::build_platforms()?;
    if platforms
        .iter()
        .any(|(_, platform)| !supported.contains(platform))
    {
        return Err("本机 default 构建器不支持部分目标平台。".into());
    }
    let plan = plan_batch_from_manifest(&project, &manifest, &hosts, base_tag, &platforms)?;
    let images = plan
        .builds
        .iter()
        .map(|build| release::PlannedImage {
            platform: build.platform.clone(),
            reference: build.reference.clone(),
        })
        .collect::<Vec<_>>();
    release::check_planned_tags(&images)?;
    let compose_files = release::deployment_files(&project)?;
    let mut targets = Vec::with_capacity(platforms.len());
    let mut env_files = Vec::with_capacity(platforms.len());
    for (host_id, platform) in platforms {
        if cancel.load(Ordering::Relaxed) {
            return Err("批次预检已取消。".into());
        }
        let host = hosts
            .iter()
            .find(|host| host.id == host_id)
            .ok_or("目标主机已变化。")?;
        let host_plan = plan
            .hosts
            .iter()
            .find(|target| target.host_id == host_id)
            .ok_or("批次目标镜像计划缺失。")?;
        let env_file = batch_env_file(
            &env_store::deployment_env_file(project_id, host_id)?,
            &host_plan.images,
        )?;
        release_materials::prepare(&compose_files, &env_file)?;
        let resolved = compose::config_snapshot(&compose_files, &env_file)?;
        verify_resolved_batch_images(&resolved, host_plan)?;
        let (docker_endpoint, docker_daemon_id) = ssh::docker_identity_for_host(host.clone())?;
        targets.push(BatchTargetIdentity {
            host_id,
            host: host.clone(),
            platform,
            docker_endpoint,
            docker_daemon_id,
        });
        env_files.push(env_file);
    }
    ensure_distinct_daemons(&targets)?;
    let fresh = project_store::effective(project_id)?;
    if cancel.load(Ordering::Relaxed)
        || release::source_digest(&fresh)? != digest
        || fresh.host_ids != ids
        || project_config::load(Path::new(&project.path))? != manifest
    {
        return Err("项目配置在批次预检期间变化，请重新预检。".into());
    }
    let (current_hosts, _) = host_store::list()?;
    if !same_batch_hosts(&targets, &current_hosts) {
        return Err("目标主机连接资料在批次预检期间变化，请重新预检。".into());
    }
    for (host_plan, frozen) in plan.hosts.iter().zip(&env_files) {
        if cancel.load(Ordering::Relaxed) {
            return Err("批次预检已取消。".into());
        }
        let current = batch_env_file(
            &env_store::deployment_env_file(project_id, host_plan.host_id)?,
            &host_plan.images,
        )?;
        if &current != frozen {
            return Err("环境变量在批次预检期间变化，请重新预检。".into());
        }
    }
    if release::source_digest(&project_store::effective(project_id)?)? != digest {
        return Err("项目源文件在批次预检期间变化，请重新预检。".into());
    }
    Ok(BatchPreflight {
        plan,
        config_sha256: digest,
        builds: manifest.builds,
        compose_files,
        env_files,
        targets,
    })
}

#[allow(dead_code)] // Held until every target has retained its own version metadata.
pub struct SharedBuild {
    pub service: String,
    pub platform: String,
    pub image: build::BuiltImage,
    pub archive: build::SavedArchive,
}

fn verify_resolved_batch_images(config: &Value, host: &BatchHost) -> Result<(), String> {
    let services = config
        .get("services")
        .and_then(Value::as_object)
        .filter(|services| !services.is_empty() && services.len() <= 100)
        .ok_or("批次 Compose 服务集合无效。")?;
    if host.images.is_empty()
        || host.images.iter().any(|(name, reference)| {
            services
                .get(name)
                .and_then(|service| service.get("image"))
                .and_then(Value::as_str)
                != Some(reference.as_str())
        })
    {
        return Err("主机解析后的自有镜像与批次计划不同。".into());
    }
    if services.values().any(|service| {
        service.get("build").is_some_and(|build| !build.is_null())
            || service
                .get("image")
                .and_then(Value::as_str)
                .is_none_or(str::is_empty)
    }) {
        return Err("批次远端服务镜像或构建配置无效。".into());
    }
    Ok(())
}

#[allow(dead_code)] // Called after frozen Compose snapshots are resolved by the batch executor.
fn verify_batch_stack_config(
    config: &Value,
    host: &BatchHost,
    built_health: &[(String, String, bool)],
) -> Result<(), String> {
    let services = config
        .get("services")
        .and_then(Value::as_object)
        .filter(|services| !services.is_empty() && services.len() <= 100)
        .ok_or("批次 Compose 服务集合无效。")?;
    if host.images.is_empty() || host.images.len() != built_health.len() {
        return Err("批次主机镜像与构建结果不完整。".into());
    }
    if host.images.iter().any(|(service, reference)| {
        built_health
            .iter()
            .filter(|(name, image, _)| name == service && image == reference)
            .count()
            != 1
    }) {
        return Err("批次构建结果与目标镜像不一致。".into());
    }
    for (name, entry) in services {
        if entry.get("build").is_some_and(|value| !value.is_null())
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
            || entry.pointer("/healthcheck/test").is_some_and(|test| {
                test.as_str() == Some("NONE")
                    || test
                        .as_array()
                        .is_some_and(|parts| parts.first().and_then(Value::as_str) == Some("NONE"))
            })
        {
            return Err(format!(
                "服务 {name} 的构建、扩容或健康配置不支持批次发布。"
            ));
        }
        let image = entry
            .get("image")
            .and_then(Value::as_str)
            .ok_or("Compose 服务缺少镜像引用。")?;
        let project_image = host.images.iter().find(|(service, _)| service == name);
        if let Some((_, expected)) = project_image {
            if image != expected {
                return Err(format!("服务 {name} 的解析镜像与批次计划不同。"));
            }
        } else if image.is_empty() || image.chars().any(char::is_control) {
            return Err("Compose 外部服务镜像引用无效。".into());
        }
        let compose_health = entry.pointer("/healthcheck/test").is_some_and(|test| {
            test.as_array()
                .is_some_and(|parts| !parts.is_empty() && parts[0].as_str() != Some("NONE"))
                || test
                    .as_str()
                    .is_some_and(|text| !text.is_empty() && text != "NONE")
        });
        let image_health = built_health
            .iter()
            .find(|(service, reference, _)| service == name && reference == image)
            .is_some_and(|(_, _, healthy)| *healthy);
        if !compose_health && !image_health {
            return Err(format!(
                "服务 {name} 缺少有效 HEALTHCHECK，批次传输已阻断。"
            ));
        }
    }
    if host
        .images
        .iter()
        .any(|(service, _)| !services.contains_key(service))
    {
        return Err("Compose 缺少批次自有镜像服务。".into());
    }
    Ok(())
}

fn spec_for_batch_build<'a>(
    specs: &'a [BuildSpec],
    planned: &BatchBuild,
) -> Result<&'a BuildSpec, String> {
    let mut matches = specs.iter().filter(|spec| {
        spec.service == planned.service
            && planned
                .reference
                .rsplit_once(':')
                .is_some_and(|(name, _)| name == spec.image)
            && (spec.platform == "auto" || spec.platform == planned.platform)
    });
    let spec = matches.next().ok_or("批次构建计划与 YAML 服务不一致。")?;
    if matches.next().is_some() {
        return Err("批次构建服务不唯一。".into());
    }
    Ok(spec)
}

#[allow(dead_code)] // Batch execution remains disabled until shared archives and per-host intent persist.
pub fn build_shared_images(
    project_id: i64,
    batch_id: &str,
    preflight: &BatchPreflight,
    cancel: Arc<AtomicBool>,
    mut on_event: impl FnMut(&str),
) -> Result<Vec<SharedBuild>, String> {
    let _guard = release_materials::acquire_guard(project_id)?;
    crate::batch_store::require_building(batch_id, project_id, &preflight.config_sha256)?;
    revalidate_batch(project_id, preflight, cancel.clone())?;
    let project = project_store::effective(project_id)?;
    build_shared_images_local(&project, batch_id, preflight, cancel, &mut on_event)
}

fn build_shared_images_local(
    project: &Project,
    batch_id: &str,
    preflight: &BatchPreflight,
    cancel: Arc<AtomicBool>,
    on_event: &mut impl FnMut(&str),
) -> Result<Vec<SharedBuild>, String> {
    let mut result = Vec::with_capacity(preflight.plan.builds.len());
    for planned in &preflight.plan.builds {
        if cancel.load(Ordering::Relaxed) {
            return Err("批次构建已取消。".into());
        }
        let spec = spec_for_batch_build(&preflight.builds, planned)?;
        let image = release::PlannedImage {
            platform: planned.platform.clone(),
            reference: planned.reference.clone(),
        };
        on_event(&format!("构建镜像：{} {}", spec.service, planned.platform));
        let built = build::build_image(
            &build_project(project, spec),
            &image,
            cancel.clone(),
            |_| {},
        )
        .map_err(|error| error.message)?;
        let archive = build::save_image(&built, cancel.clone())?;
        result.push(SharedBuild {
            service: spec.service.clone(),
            platform: planned.platform.clone(),
            image: built,
            archive,
        });
    }
    if preflight.plan.hosts.len() != preflight.env_files.len() {
        return Err("批次主机环境快照数量不一致。".into());
    }
    for (host, env_file) in preflight.plan.hosts.iter().zip(&preflight.env_files) {
        if cancel.load(Ordering::Relaxed) {
            return Err("批次构建后预检已取消。".into());
        }
        on_event(&format!("核对构建后 Compose：主机 {}", host.host_id));
        let config = compose::config_snapshot(&preflight.compose_files, env_file)?;
        let health = result
            .iter()
            .filter(|image| image.platform == host.platform)
            .map(|image| {
                (
                    image.service.clone(),
                    image.image.reference.clone(),
                    image.image.has_healthcheck,
                )
            })
            .collect::<Vec<_>>();
        verify_batch_stack_config(&config, host, &health)?;
    }
    if cancel.load(Ordering::Relaxed)
        || release::source_digest(&project_store::effective(project.id)?)?
            != preflight.config_sha256
    {
        return Err("项目文件在批次构建期间变化，未开始上传。".into());
    }
    for built in &result {
        if cancel.load(Ordering::Relaxed) {
            return Err("批次归档留存已取消。".into());
        }
        crate::release_archive::retain_shared(
            batch_id,
            &built.service,
            &built.platform,
            &built.archive,
        )?;
    }
    if crate::batch_store::registry_mode(batch_id)? {
        for built in &result {
            on_event(&format!("推送镜像：{} {}", built.service, built.platform));
            if let Err(error) = registry::push(&built.image.reference, &cancel) {
                let state = crate::batch_store::mark_unknown_registry_push(batch_id)
                    .err()
                    .map(|reason| format!("；批次状态保存失败：{reason}"))
                    .unwrap_or_default();
                return Err(format!("{error}；{UNKNOWN_REGISTRY_PUSH}{state}。"));
            }
        }
    }
    crate::batch_store::ready(batch_id)?;
    Ok(result)
}

#[allow(dead_code)] // The guard stays held when the coordinator proceeds to transfer.
pub struct PreparedBatchHost {
    pub task_id: String,
    _guard: Arc<release_materials::ReleaseGuard>,
}

#[allow(dead_code)] // Local-only preparation; no SSH upload or Compose start occurs here.
pub fn prepare_batch_host(
    project_id: i64,
    batch_id: &str,
    preflight: &BatchPreflight,
    host_index: usize,
    guard: Arc<release_materials::ReleaseGuard>,
    cancel: Arc<AtomicBool>,
) -> Result<PreparedBatchHost, String> {
    revalidate_batch_inputs(
        project_id,
        preflight,
        cancel.clone(),
        false,
        Some(host_index),
    )?;
    let snapshot = crate::batch_store::get(batch_id)?;
    let identity = preflight
        .targets
        .get(host_index)
        .ok_or("批次目标序号无效。")?;
    let planned = preflight
        .plan
        .hosts
        .get(host_index)
        .ok_or("批次目标计划缺失。")?;
    let env_file = preflight
        .env_files
        .get(host_index)
        .ok_or("批次环境快照缺失。")?;
    if snapshot.project_id != project_id
        || snapshot.config_sha256 != preflight.config_sha256
        || !matches!(snapshot.state.as_str(), "ready" | "running")
        || snapshot.targets.get(host_index).is_none_or(|target| {
            target.ordinal != host_index
                || target.state != "waiting"
                || target.host != identity.host
                || target.platform != identity.platform
                || target.docker_endpoint != identity.docker_endpoint
                || target.docker_daemon_id != identity.docker_daemon_id
                || target.env_sha256.as_deref() != Some(env_digest(env_file).as_str())
        })
        || planned.host_id != identity.host_id
    {
        return Err("批次目标或身份快照已变化。".into());
    }
    if let Some(previous_id) = snapshot.targets[host_index].attempt_task_ids.last() {
        let previous = release_store::get_task(previous_id)?;
        if previous.outcome != "failed"
            || previous.project_id != project_id
            || previous.host.id != identity.host.id
            || previous.docker_daemon_id.as_deref() != Some(identity.docker_daemon_id.as_str())
            || !ssh::observe_release_project(&previous)?
                .containers
                .is_empty()
        {
            return Err("重试前未能确认失败任务已关闭且远端项目无容器。".into());
        }
    }
    let materials = release_materials::prepare(&preflight.compose_files, env_file)?;
    let project = project_store::effective(project_id)?;
    let first = planned.images.first().ok_or("批次目标没有自有镜像。")?;
    let id = uuid::Uuid::new_v4().to_string();
    release_store::create_for_batch(
        &Intent {
            id: id.clone(),
            project_id,
            project_name: project.name,
            host: identity.host.clone(),
            docker_endpoint: identity.docker_endpoint.clone(),
            docker_daemon_id: identity.docker_daemon_id.clone(),
            image_reference: first.1.clone(),
            platform: identity.platform.clone(),
            images: planned
                .images
                .iter()
                .map(|(service, reference)| {
                    (
                        service.clone(),
                        reference.clone(),
                        identity.platform.clone(),
                    )
                })
                .collect(),
            config_sha256: preflight.config_sha256.clone(),
        },
        batch_id,
        &env_digest(env_file),
    )?;
    if let Err(error) = release_materials::save(&id, materials) {
        let _ = release_store::settle(
            &id,
            Phase::Prepared,
            Outcome::Failed,
            "批次目标本机版本材料保存失败；凭据残留需单独清理",
        );
        return Err(error);
    }
    release_store::advance(&id, Phase::Prepared, Phase::Building)?;
    release_store::advance(&id, Phase::Building, Phase::Built)?;
    let prepared = (|| {
        for (service, _) in &planned.images {
            if cancel.load(Ordering::Relaxed) {
                return Err("批次目标归档准备已取消。".to_owned());
            }
            let archive = crate::release_archive::copy_verified_shared(
                batch_id,
                service,
                &identity.platform,
            )?;
            crate::release_archive::retain(&id, service, &archive)?;
        }
        Ok::<_, String>(())
    })();
    if let Err(error) = prepared {
        let _ = release_store::settle(&id, Phase::Built, Outcome::Failed, "本机批次归档准备失败");
        return Err(error);
    }
    Ok(PreparedBatchHost {
        task_id: id,
        _guard: guard,
    })
}

#[allow(dead_code)] // Stops at Uploaded; Docker load and Compose execution are separate stages.
pub fn upload_batch_host_archives(
    prepared: &PreparedBatchHost,
    batch_id: &str,
    preflight: &BatchPreflight,
    host_index: usize,
    cancel: Arc<AtomicBool>,
    mut on_event: impl FnMut(&str),
) -> Result<Vec<ssh::UploadedArchive>, String> {
    let target = preflight
        .targets
        .get(host_index)
        .ok_or("批次主机序号无效。")?;
    let batch = crate::batch_store::get(batch_id)?;
    revalidate_batch_inputs(
        batch.project_id,
        preflight,
        cancel.clone(),
        false,
        Some(host_index),
    )?;
    let task = release_store::get_task(&prepared.task_id)?;
    if batch.targets.get(host_index).is_none_or(|row| {
        row.release_task_id.as_deref() != Some(&prepared.task_id)
            || row.state != "running"
            || row.host != target.host
    }) || !matches!(batch.state.as_str(), "running" | "paused" | "unknown")
        || task.phase != "built"
        || task.outcome != "running"
        || task.host.id != target.host.id
        || task.docker_daemon_id.as_deref() != Some(target.docker_daemon_id.as_str())
    {
        return Err("批次主机任务或预备阶段已变化。".into());
    }
    let planned = preflight
        .plan
        .hosts
        .get(host_index)
        .ok_or("批次镜像分配缺失。")?;
    if cancel.load(Ordering::Relaxed) {
        return Err("批次传输已取消。".into());
    }
    release_store::advance(&prepared.task_id, Phase::Built, Phase::Uploading)?;
    if crate::batch_store::registry_mode(batch_id)? {
        release_store::advance(&prepared.task_id, Phase::Uploading, Phase::Uploaded)?;
        return Ok(Vec::new());
    }
    let mut uploaded = Vec::new();
    for (service, reference) in &planned.images {
        let result = (|| {
            if cancel.load(Ordering::Relaxed) {
                return Err("批次传输已取消。".into());
            }
            let (_, _, ids, saved_reference) =
                crate::batch_store::archive_record(batch_id, service, &target.platform)?;
            if saved_reference != *reference {
                return Err("批次共享镜像引用已变化。".into());
            }
            let archive = crate::release_archive::copy_verified(&prepared.task_id, service, ids)?;
            if crate::build::archive_image_ids(&archive.path, reference, &target.platform)?
                != archive.image_ids
            {
                return Err("目标版本归档镜像身份与批次不一致。".into());
            }
            let directory = format!("/tmp/devingpanel.{}", uuid::Uuid::new_v4().simple());
            release_store::reserve_uploaded_archive(
                &prepared.task_id,
                service,
                &directory,
                archive.bytes,
                &archive.sha256,
            )?;
            on_event(&format!("传输镜像：{service}"));
            // Keep the reserved path if this first upload becomes uncertain.
            let remote = ssh::upload_unknown_archive_at(
                target.host.clone(),
                ssh::ReleaseDockerIdentity {
                    endpoint: &target.docker_endpoint,
                    daemon_id: &target.docker_daemon_id,
                },
                &archive.path,
                &directory,
                cancel.clone(),
                |sent, total| on_event(&format!("镜像传输 {sent}/{total} 字节")),
            )?;
            release_store::confirm_uploaded_archive(&prepared.task_id, service, &remote)?;
            let mut remote = remote;
            remote.image_ids = archive.image_ids.clone();
            Ok::<_, String>(remote)
        })();
        match result {
            Ok(archive) => uploaded.push(archive),
            Err(error) => {
                let _ = release_store::settle(
                    &prepared.task_id,
                    Phase::Uploading,
                    Outcome::Unknown,
                    "批次镜像上传或归档状态待核实",
                );
                return Err(error);
            }
        }
    }
    release_store::advance(&prepared.task_id, Phase::Uploading, Phase::Uploaded)?;
    Ok(uploaded)
}

#[allow(dead_code)] // Stops before Compose staging; all remote tar locations are recorded first.
pub fn load_batch_host_archives(
    prepared: &PreparedBatchHost,
    batch_id: &str,
    preflight: &BatchPreflight,
    host_index: usize,
    cancel: Arc<AtomicBool>,
    mut on_event: impl FnMut(&str),
) -> Result<Vec<StackService>, String> {
    let target = preflight
        .targets
        .get(host_index)
        .ok_or("批次目标序号无效。")?;
    let plan = preflight
        .plan
        .hosts
        .get(host_index)
        .ok_or("批次镜像分配缺失。")?;
    let task = release_store::get_task(&prepared.task_id)?;
    let batch = crate::batch_store::get(batch_id)?;
    if task.phase != "uploaded"
        || task.outcome != "running"
        || !matches!(batch.state.as_str(), "running" | "paused" | "unknown")
        || task.host.id != target.host_id
        || task.docker_daemon_id.as_deref() != Some(target.docker_daemon_id.as_str())
        || batch.targets.get(host_index).is_none_or(|host| {
            host.release_task_id.as_deref() != Some(&prepared.task_id)
                || host.state != "running"
                || host.host != target.host
        })
    {
        return Err("批次镜像加载前主机任务状态已变化。".into());
    }
    if crate::batch_store::registry_mode(batch_id)? {
        let credentials =
            registry::enabled_credentials()?.ok_or("镜像仓库配置已失效，批次无法继续。")?;
        let mut services = Vec::new();
        for (service, reference) in &plan.images {
            let (_, _, image_ids, saved_reference) =
                crate::batch_store::archive_record(batch_id, service, &target.platform)?;
            if saved_reference != *reference
                || !reference.starts_with(&format!("{}/", credentials.config.address))
            {
                return Err("批次镜像与镜像仓库配置不一致。".into());
            }
            services.push(StackService {
                service: service.clone(),
                reference: reference.clone(),
                image_ids,
            });
        }
        release_store::advance(&prepared.task_id, Phase::Uploaded, Phase::Loading)?;
        for service in &services {
            let result = (|| {
                release_store::record_image_ids(
                    &prepared.task_id,
                    &service.service,
                    &service.image_ids,
                )?;
                on_event(&format!("远端拉取镜像：{}", service.service));
                ssh::pull_registry_image(
                    target.host.clone(),
                    ssh::ReleaseDockerIdentity {
                        endpoint: &target.docker_endpoint,
                        daemon_id: &target.docker_daemon_id,
                    },
                    &prepared.task_id,
                    &service.reference,
                    &target.platform,
                    &service.image_ids,
                    &credentials,
                    cancel.clone(),
                )
            })();
            if let Err(error) = result {
                let _ = release_store::settle(
                    &prepared.task_id,
                    Phase::Loading,
                    Outcome::Unknown,
                    "批次镜像仓库拉取结果待核实",
                );
                return Err(error);
            }
        }
        release_store::advance(&prepared.task_id, Phase::Loading, Phase::Loaded)?;
        release_store::advance(&prepared.task_id, Phase::Loaded, Phase::AwaitingDeployment)?;
        return Ok(services);
    }
    let records = release_store::uploaded_archives(&prepared.task_id)?;
    if records.len() != plan.images.len() || plan.images.is_empty() {
        return Err("批次远端归档记录不完整，禁止加载。".into());
    }
    let mut archives = Vec::new();
    for ((service, reference), record) in plan.images.iter().zip(records) {
        let (bytes, sha256, image_ids, saved_reference) =
            crate::batch_store::archive_record(batch_id, service, &target.platform)?;
        if record.service != *service
            || record.state != "uploaded"
            || record.bytes != bytes
            || record.sha256 != sha256
            || saved_reference != *reference
        {
            return Err("批次远端归档与共享镜像计划不同，禁止加载。".into());
        }
        archives.push((
            service.clone(),
            reference.clone(),
            ssh::UploadedArchive {
                path: format!("{}/image.tar", record.directory),
                directory: record.directory,
                bytes,
                sha256,
                image_ids,
            },
        ));
    }
    if cancel.load(Ordering::Relaxed) {
        return Err("批次镜像加载已取消。".into());
    }
    release_store::advance(&prepared.task_id, Phase::Uploaded, Phase::Loading)?;
    let identity = ssh::ReleaseDockerIdentity {
        endpoint: &target.docker_endpoint,
        daemon_id: &target.docker_daemon_id,
    };
    for (service, reference, archive) in &archives {
        on_event(&format!("远端加载镜像：{service}"));
        let result = (|| {
            release_store::record_image_ids(&prepared.task_id, service, &archive.image_ids)?;
            ssh::load_uploaded_archive(
                target.host.clone(),
                identity,
                archive,
                reference,
                &target.platform,
                cancel.clone(),
            )?;
            release_store::advance_uploaded_archive(
                &prepared.task_id,
                service,
                "uploaded",
                "loaded",
            )
        })();
        if let Err(error) = result {
            let _ = release_store::settle(
                &prepared.task_id,
                Phase::Loading,
                Outcome::Unknown,
                "批次远端镜像加载结果待核实",
            );
            return Err(error);
        }
    }
    release_store::advance(&prepared.task_id, Phase::Loading, Phase::Loaded)?;
    for (service, _, archive) in &archives {
        if let Err(error) =
            ssh::cleanup_uploaded_archive(target.host.clone(), archive).and_then(|_| {
                release_store::advance_uploaded_archive(
                    &prepared.task_id,
                    service,
                    "loaded",
                    "cleaned",
                )
            })
        {
            let _ = release_store::settle(
                &prepared.task_id,
                Phase::Loaded,
                Outcome::Unknown,
                "批次远端临时归档清理待核实",
            );
            return Err(error);
        }
    }
    release_store::advance(&prepared.task_id, Phase::Loaded, Phase::AwaitingDeployment)?;
    Ok(archives
        .into_iter()
        .map(|(service, reference, archive)| StackService {
            service,
            reference,
            image_ids: archive.image_ids,
        })
        .collect())
}

#[allow(dead_code)] // The staged path remains bound to the prepared task in this process.
pub struct StagedBatchHost {
    task_id: String,
    batch_id: String,
    host_id: i64,
    project_id: i64,
    staged: ssh::StagedDeployment,
    built: Vec<StackService>,
    services: Vec<StackService>,
}

#[allow(dead_code)] // Validates the full stack but does not start any containers.
pub fn stage_batch_host_stack(
    prepared: &PreparedBatchHost,
    batch_id: &str,
    preflight: &BatchPreflight,
    host_index: usize,
    cancel: Arc<AtomicBool>,
    mut on_event: impl FnMut(&str),
) -> Result<StagedBatchHost, String> {
    let target = preflight
        .targets
        .get(host_index)
        .ok_or("批次目标序号无效。")?;
    let task = release_store::get_task(&prepared.task_id)?;
    let batch = crate::batch_store::get(batch_id)?;
    if task.phase != "awaiting_deployment"
        || task.outcome != "running"
        || !matches!(batch.state.as_str(), "running" | "paused" | "unknown")
        || task.host.id != target.host_id
        || task.docker_daemon_id.as_deref() != Some(target.docker_daemon_id.as_str())
        || batch.targets.get(host_index).is_none_or(|host| {
            host.release_task_id.as_deref() != Some(&prepared.task_id)
                || host.state != "running"
                || host.host != target.host
        })
    {
        return Err("批次目标整栈暂存前身份或阶段已变化。".into());
    }
    let snapshot = release_materials::load(&prepared.task_id)?;
    if snapshot.env_file
        != *preflight
            .env_files
            .get(host_index)
            .ok_or("环境快照缺失。")?
        || snapshot.compose_files.len() != preflight.compose_files.len()
        || snapshot
            .compose_files
            .iter()
            .zip(&preflight.compose_files)
            .any(|(stored, frozen)| stored.path != frozen.path || stored.bytes != frozen.bytes)
    {
        return Err("任务版本材料与批次确认时不同，禁止部署。".into());
    }
    let images = release_store::historical_images(&prepared.task_id)?;
    let plan = preflight
        .plan
        .hosts
        .get(host_index)
        .ok_or("批次目标镜像计划缺失。")?;
    if images.len() != plan.images.len()
        || images.iter().zip(&plan.images).any(|(image, expected)| {
            image.service != expected.0
                || image.reference != expected.1
                || image.platform != target.platform
        })
    {
        return Err("任务镜像与批次确认时不同，禁止部署。".into());
    }
    let expected = images
        .into_iter()
        .map(|image| StackService {
            service: image.service,
            reference: image.reference,
            image_ids: image.image_ids,
        })
        .collect::<Vec<_>>();
    if cancel.load(Ordering::Relaxed) {
        return Err("整栈暂存已取消。".into());
    }
    release_store::advance(&prepared.task_id, Phase::AwaitingDeployment, Phase::Staging)?;
    on_event("上传 Compose 和环境配置");
    let staged = match ssh::stage_deployment(
        target.host.clone(),
        batch.project_id,
        &prepared.task_id,
        &snapshot.compose_files,
        &snapshot.env_file,
        cancel.clone(),
    ) {
        Ok(staged) => staged,
        Err(error) => {
            let _ = release_store::settle(
                &prepared.task_id,
                Phase::Staging,
                Outcome::Unknown,
                "批次整栈部署文件待核实",
            );
            return Err(error);
        }
    };
    release_store::advance(&prepared.task_id, Phase::Staging, Phase::Staged)?;
    release_store::advance(&prepared.task_id, Phase::Staged, Phase::Validating)?;
    let identity = ssh::ReleaseDockerIdentity {
        endpoint: &target.docker_endpoint,
        daemon_id: &target.docker_daemon_id,
    };
    on_event("校验所有 Compose 服务");
    let services = match ssh::validate_staged_stack(
        target.host.clone(),
        identity,
        &staged,
        batch.project_id,
        &expected,
        cancel,
    ) {
        Ok(services) => services,
        Err(error) => {
            let _ = release_store::settle(
                &prepared.task_id,
                Phase::Validating,
                Outcome::Unknown,
                "批次远端整栈配置待核实",
            );
            return Err(error);
        }
    };
    if let Err(error) = release_store::record_services(&prepared.task_id, &services) {
        let _ = release_store::settle(
            &prepared.task_id,
            Phase::Validating,
            Outcome::Failed,
            "批次整栈服务计划未保存",
        );
        return Err(error);
    }
    release_store::advance(&prepared.task_id, Phase::Validating, Phase::Validated)?;
    Ok(StagedBatchHost {
        task_id: prepared.task_id.clone(),
        batch_id: batch_id.to_owned(),
        host_id: target.host_id,
        project_id: batch.project_id,
        staged,
        built: expected,
        services,
    })
}

#[allow(dead_code)] // One host only; caller confirms healthy completion before moving onward.
pub fn deploy_batch_host_stack(
    prepared: &PreparedBatchHost,
    batch_id: &str,
    preflight: &BatchPreflight,
    host_index: usize,
    staged: &StagedBatchHost,
    cancel: Arc<AtomicBool>,
    mut on_event: impl FnMut(&str),
) -> Result<String, String> {
    let target = preflight
        .targets
        .get(host_index)
        .ok_or("批次目标序号无效。")?;
    let batch = crate::batch_store::get(batch_id)?;
    let task = release_store::get_task(&prepared.task_id)?;
    if task.phase != "validated"
        || task.outcome != "running"
        || !matches!(batch.state.as_str(), "running" | "paused" | "unknown")
        || staged.task_id != prepared.task_id
        || staged.batch_id != batch_id
        || staged.host_id != target.host_id
        || staged.project_id != batch.project_id
        || task.host.id != target.host_id
        || task.docker_daemon_id.as_deref() != Some(target.docker_daemon_id.as_str())
        || batch.targets.get(host_index).is_none_or(|host| {
            host.release_task_id.as_deref() != Some(&prepared.task_id)
                || host.state != "running"
                || host.host != target.host
        })
    {
        return Err("批次目标启动前身份或阶段已变化。".into());
    }
    let recorded = release_store::planned_services(&prepared.task_id)?;
    let images = release_store::historical_images(&prepared.task_id)?;
    if images.len() != staged.built.len()
        || images.iter().zip(&staged.built).any(|(image, built)| {
            image.service != built.service
                || image.reference != built.reference
                || image.image_ids != built.image_ids
        })
    {
        return Err("自有镜像计划与启动确认不同。".into());
    }
    if recorded.len() != staged.services.len()
        || recorded.iter().any(|service| {
            !staged.services.iter().any(|expected| {
                expected.service == service.service
                    && expected.reference == service.reference
                    && expected.image_ids == service.image_ids
            })
        })
    {
        return Err("整栈服务计划与启动确认不同。".into());
    }
    if cancel.load(Ordering::Relaxed) {
        return Err("批次启动已取消。".into());
    }
    let identity = ssh::ReleaseDockerIdentity {
        endpoint: &target.docker_endpoint,
        daemon_id: &target.docker_daemon_id,
    };
    release_store::advance(&prepared.task_id, Phase::Validated, Phase::Deploying)?;
    on_event("启动全部 Compose 服务");
    if let Err(error) = ssh::start_staged_stack(
        target.host.clone(),
        identity,
        &staged.staged,
        batch.project_id,
        &staged.built,
        cancel.clone(),
    ) {
        let _ = ssh::check_staged_stack(
            target.host.clone(),
            identity,
            &staged.staged,
            batch.project_id,
            &staged.built,
            &staged.services,
            Arc::new(AtomicBool::new(false)),
        );
        let _ = release_store::settle(
            &prepared.task_id,
            Phase::Deploying,
            Outcome::Unknown,
            "批次整栈启动结果待核实，禁止自动重试",
        );
        return Err(error);
    }
    release_store::advance(&prepared.task_id, Phase::Deploying, Phase::Checking)?;
    on_event("核对全部 Compose 服务健康状态");
    let health = match ssh::check_staged_stack(
        target.host.clone(),
        identity,
        &staged.staged,
        batch.project_id,
        &staged.built,
        &staged.services,
        cancel,
    ) {
        Ok(value) => value,
        Err(error) => {
            let _ = release_store::settle(
                &prepared.task_id,
                Phase::Checking,
                Outcome::Unknown,
                "批次整栈健康状态待核实",
            );
            return Err(error);
        }
    };
    let result = match health {
        DeploymentState::Healthy => HealthResult::Healthy,
        DeploymentState::RunningUnverified => HealthResult::RunningUnverified,
        DeploymentState::Failed => HealthResult::Failed,
    };
    release_store::settle_checked(
        &prepared.task_id,
        result,
        match result {
            HealthResult::Healthy => "全部服务健康；仍需人工确认完成",
            HealthResult::RunningUnverified => "服务未全部配置健康检查，批次结果待核实",
            HealthResult::Failed => "至少一个服务未健康；批次已暂停",
        },
    )?;
    Ok(prepared.task_id.clone())
}

#[allow(dead_code)] // The UI will present confirmation before invoking the next target.
pub enum BatchProgress {
    AwaitingConfirmation { batch_id: String, task_id: String },
    Complete { batch_id: String },
}

#[allow(dead_code)] // Failure always identifies a created batch for history/reconciliation.
pub struct BatchRunError {
    pub batch_id: Option<String>,
    pub message: String,
}

enum BatchNext {
    Waiting(usize),
    AwaitingConfirmation(String),
}

fn next_batch_target(
    targets: &[crate::batch_store::BatchTargetRecord],
    mut is_awaiting: impl FnMut(&str) -> Result<bool, String>,
) -> Result<BatchNext, String> {
    let index = targets
        .iter()
        .position(|target| target.state == "waiting")
        .unwrap_or(targets.len());
    for prior in &targets[..index] {
        if matches!(prior.state.as_str(), "healthy" | "skipped") {
            continue;
        }
        if prior.state == "running" {
            let task_id = prior
                .release_task_id
                .as_deref()
                .ok_or("进行中的批次目标缺少任务记录。")?;
            if is_awaiting(task_id)? {
                return Ok(BatchNext::AwaitingConfirmation(task_id.into()));
            }
        }
        return Err("上一台主机任务仍在执行或待核实，禁止开始下一台。".into());
    }
    if index == targets.len() {
        return Err("批次没有可开始的等待目标；请核对已开始任务。".into());
    }
    Ok(BatchNext::Waiting(index))
}

#[allow(dead_code)] // Disabled in the GUI until isolated multi-host acceptance and recovery UI exist.
pub fn run_batch_sequential(
    project_id: i64,
    confirmed: &BatchPreflight,
    cancel: Arc<AtomicBool>,
    mut on_event: impl FnMut(&str),
) -> Result<BatchProgress, BatchRunError> {
    revalidate_batch(project_id, confirmed, cancel.clone()).map_err(|message| BatchRunError {
        batch_id: None,
        message,
    })?;
    let batch_id =
        crate::batch_store::create(project_id, confirmed).map_err(|message| BatchRunError {
            batch_id: None,
            message,
        })?;
    crate::batch_store::begin_build(&batch_id).map_err(|message| BatchRunError {
        batch_id: Some(batch_id.clone()),
        message,
    })?;
    if let Err(message) = build_shared_images(
        project_id,
        &batch_id,
        confirmed,
        cancel.clone(),
        &mut on_event,
    ) {
        let detail = if message.contains(UNKNOWN_REGISTRY_PUSH)
            || crate::batch_store::get(&batch_id).is_ok_and(|batch| batch.state == "unknown")
        {
            message
        } else {
            match crate::batch_store::terminate_unstarted(&batch_id) {
                Ok(()) => message,
                Err(reason) => format!("{message}；批次终止待核实：{reason}"),
            }
        };
        return Err(BatchRunError {
            batch_id: Some(batch_id),
            message: detail,
        });
    }
    continue_batch_sequential(&batch_id, None, confirmed, cancel, on_event)
}

pub fn run_batch_parallel(
    project_id: i64,
    confirmed: &BatchPreflight,
    parallelism: usize,
    cancel: Arc<AtomicBool>,
    mut on_event: impl FnMut(&str),
) -> Result<BatchProgress, BatchRunError> {
    revalidate_batch(project_id, confirmed, cancel.clone()).map_err(|message| BatchRunError {
        batch_id: None,
        message,
    })?;
    let batch_id = crate::batch_store::create_with_parallel(project_id, confirmed, parallelism)
        .map_err(|message| BatchRunError {
            batch_id: None,
            message,
        })?;
    crate::batch_store::begin_build(&batch_id).map_err(|message| BatchRunError {
        batch_id: Some(batch_id.clone()),
        message,
    })?;
    if let Err(message) = build_shared_images(
        project_id,
        &batch_id,
        confirmed,
        cancel.clone(),
        &mut on_event,
    ) {
        let detail = if message.contains(UNKNOWN_REGISTRY_PUSH)
            || crate::batch_store::get(&batch_id).is_ok_and(|batch| batch.state == "unknown")
        {
            message
        } else {
            match crate::batch_store::terminate_unstarted(&batch_id) {
                Ok(()) => message,
                Err(reason) => format!("{message}；批次终止待核实：{reason}"),
            }
        };
        return Err(BatchRunError {
            batch_id: Some(batch_id),
            message: detail,
        });
    }
    continue_batch_parallel(&batch_id, None, confirmed, cancel, on_event)
}

pub fn continue_batch_parallel(
    batch_id: &str,
    expected_host_id: Option<i64>,
    confirmed: &BatchPreflight,
    cancel: Arc<AtomicBool>,
    mut on_event: impl FnMut(&str),
) -> Result<BatchProgress, BatchRunError> {
    let result = (|| -> Result<BatchProgress, String> {
        let batch = crate::batch_store::get(batch_id)?;
        if batch.parallelism <= 1
            || batch.parallelism > batch.targets.len()
            || batch.config_sha256 != confirmed.config_sha256
            || batch.targets.len() != confirmed.targets.len()
            || batch.targets.len() != confirmed.env_files.len()
            || batch.images.len() != confirmed.plan.builds.len()
            || batch
                .targets
                .iter()
                .zip(&confirmed.targets)
                .zip(&confirmed.env_files)
                .any(|((saved, target), env)| {
                    saved.host != target.host
                        || saved.platform != target.platform
                        || saved.docker_endpoint != target.docker_endpoint
                        || saved.docker_daemon_id != target.docker_daemon_id
                        || saved.env_sha256.as_deref() != Some(env_digest(env).as_str())
                })
            || confirmed.plan.builds.iter().any(|expected| {
                !batch.images.iter().any(|saved| {
                    saved.service == expected.service
                        && saved.platform == expected.platform
                        && saved.reference == expected.reference
                })
            })
        {
            return Err("批次并行度或目标/镜像快照已变化。".into());
        }
        if batch.state == "complete" {
            return Ok(BatchProgress::Complete {
                batch_id: batch_id.into(),
            });
        }
        if !matches!(batch.state.as_str(), "ready" | "running") {
            return Err("批次已暂停或待核实，不会自动继续。".into());
        }
        let index = batch
            .targets
            .iter()
            .position(|target| target.state == "waiting");
        let Some(index) = index else {
            if let Some(task_id) = batch
                .targets
                .iter()
                .filter_map(|target| target.release_task_id.as_deref())
                .find(|id| {
                    release_store::get_task(id).is_ok_and(|task| {
                        task.phase == "checked"
                            && task.outcome == "blocked"
                            && task.health_result.as_deref() == Some("healthy")
                    })
                })
            {
                return Ok(BatchProgress::AwaitingConfirmation {
                    batch_id: batch_id.into(),
                    task_id: task_id.into(),
                });
            }
            return Err("批次没有可开始的等待目标；请核对已开始任务。".into());
        };
        let wave_start = index / batch.parallelism * batch.parallelism;
        let wave_end = (wave_start + batch.parallelism).min(batch.targets.len());
        if batch.targets[index..wave_end]
            .iter()
            .any(|target| target.state != "waiting" || target.release_task_id.is_some())
        {
            return Err("当前波次仍有已开始目标，禁止启动新目标。".into());
        }
        if expected_host_id.is_some_and(|id| batch.targets[index].host.id != id) {
            return Err("人工确认的下一波主机已变化。".into());
        }
        for prior in &batch.targets[..index] {
            let task_id = prior
                .release_task_id
                .as_deref()
                .ok_or("上一波目标缺少任务记录。")?;
            let task = release_store::get_task(task_id)?;
            let resolved = match prior.state.as_str() {
                "healthy" => {
                    task.phase == "checked"
                        && task.outcome == "complete"
                        && task.health_result.as_deref() == Some("healthy")
                }
                "skipped" => task.outcome == "failed",
                "running"
                    if task.phase == "checked"
                        && task.outcome == "blocked"
                        && task.health_result.as_deref() == Some("healthy") =>
                {
                    return Ok(BatchProgress::AwaitingConfirmation {
                        batch_id: batch_id.into(),
                        task_id: task_id.into(),
                    });
                }
                _ => false,
            };
            if !resolved
                || task.project_id != batch.project_id
                || task.host.id != prior.host.id
                || task.docker_daemon_id.as_deref() != Some(prior.docker_daemon_id.as_str())
            {
                return Err("上一波目标尚未全部健康并确认，禁止继续。".into());
            }
        }
        let guard = Arc::new(release_materials::acquire_guard(batch.project_id)?);
        let current = crate::batch_store::get(batch_id)?;
        if current.state != batch.state
            || current.parallelism != batch.parallelism
            || current
                .targets
                .iter()
                .zip(&batch.targets)
                .any(|(now, before)| {
                    now.state != before.state || now.release_task_id != before.release_task_id
                })
        {
            return Err("批次状态在取得项目锁期间已变化。".into());
        }
        let mut prepared = Vec::new();
        for host_index in index..wave_end {
            match prepare_batch_host(
                batch.project_id,
                batch_id,
                confirmed,
                host_index,
                guard.clone(),
                cancel.clone(),
            ) {
                Ok(host) => prepared.push((host_index, host)),
                Err(error) => {
                    for (_, host) in &prepared {
                        let _ = release_store::settle(
                            &host.task_id,
                            Phase::Built,
                            Outcome::Failed,
                            "同波次本地准备未完成，未开始远端传输",
                        );
                    }
                    let _ = crate::batch_store::pause_waiting(batch_id, host_index);
                    return Err(error);
                }
            }
        }
        let (events, receiver) = std::sync::mpsc::channel::<String>();
        let results = std::thread::scope(|scope| {
            let mut handles = Vec::new();
            for (host_index, host) in prepared {
                let events = events.clone();
                let cancel = cancel.clone();
                handles.push(scope.spawn(move || {
                    let mut report = |event: &str| {
                        let _ = events.send(event.to_owned());
                    };
                    let result = (|| {
                        upload_batch_host_archives(
                            &host,
                            batch_id,
                            confirmed,
                            host_index,
                            cancel.clone(),
                            &mut report,
                        )?;
                        load_batch_host_archives(
                            &host,
                            batch_id,
                            confirmed,
                            host_index,
                            cancel.clone(),
                            &mut report,
                        )?;
                        let staged = stage_batch_host_stack(
                            &host,
                            batch_id,
                            confirmed,
                            host_index,
                            cancel.clone(),
                            &mut report,
                        )?;
                        deploy_batch_host_stack(
                            &host,
                            batch_id,
                            confirmed,
                            host_index,
                            &staged,
                            cancel,
                            &mut report,
                        )
                    })();
                    (host_index, result)
                }));
            }
            drop(events);
            for event in receiver {
                on_event(&event);
            }
            handles
                .into_iter()
                .map(|handle| handle.join())
                .collect::<Vec<_>>()
        });
        let mut awaiting = None;
        let mut failures = Vec::new();
        for result in results {
            let (index, result) = result.map_err(|_| "批次工作线程意外退出；请重启后核实任务。")?;
            match result {
                Ok(task_id) => {
                    let task = release_store::get_task(&task_id)?;
                    if task.phase == "checked"
                        && task.outcome == "blocked"
                        && task.health_result.as_deref() == Some("healthy")
                    {
                        awaiting.get_or_insert(task_id);
                    } else {
                        failures.push(format!("{}: 结果待核实", batch.targets[index].host.name));
                    }
                }
                Err(error) => failures.push(format!("{}: {error}", batch.targets[index].host.name)),
            }
        }
        if !failures.is_empty() {
            return Err(format!(
                "当前波次已启动目标均已停止执行；{}",
                failures.join("；")
            ));
        }
        Ok(BatchProgress::AwaitingConfirmation {
            batch_id: batch_id.into(),
            task_id: awaiting.ok_or("当前波次没有可确认的健康目标。")?,
        })
    })();
    result.map_err(|message| BatchRunError {
        batch_id: Some(batch_id.into()),
        message,
    })
}

#[allow(dead_code)] // Must be called only after human confirmation moves prior task to complete.
pub fn continue_batch_sequential(
    batch_id: &str,
    expected_host_id: Option<i64>,
    confirmed: &BatchPreflight,
    cancel: Arc<AtomicBool>,
    mut on_event: impl FnMut(&str),
) -> Result<BatchProgress, BatchRunError> {
    let result = (|| {
        let batch = crate::batch_store::get(batch_id)?;
        if batch.config_sha256 != confirmed.config_sha256
            || batch.targets.len() != confirmed.targets.len()
            || batch.targets.len() != confirmed.env_files.len()
            || batch
                .targets
                .iter()
                .zip(&confirmed.env_files)
                .any(|(saved, env)| saved.env_sha256.as_deref() != Some(env_digest(env).as_str()))
            || batch.images.len() != confirmed.plan.builds.len()
            || batch
                .targets
                .iter()
                .zip(&confirmed.targets)
                .any(|(saved, target)| {
                    saved.host != target.host
                        || saved.platform != target.platform
                        || saved.docker_endpoint != target.docker_endpoint
                        || saved.docker_daemon_id != target.docker_daemon_id
                })
            || confirmed.plan.builds.iter().any(|expected| {
                !batch.images.iter().any(|saved| {
                    saved.service == expected.service
                        && saved.platform == expected.platform
                        && saved.reference == expected.reference
                })
            })
        {
            return Err("批次记录与用户确认的目标/镜像快照不同。".into());
        }
        if batch.state == "complete" {
            return Ok(BatchProgress::Complete {
                batch_id: batch_id.into(),
            });
        }
        if !matches!(batch.state.as_str(), "ready" | "running") {
            return Err("批次已暂停或待核实，不会自动继续。".into());
        }
        let index = match next_batch_target(&batch.targets, |id| {
            let task = release_store::get_task(id)?;
            Ok(task.phase == "checked"
                && task.outcome == "blocked"
                && task.health_result.as_deref() == Some("healthy"))
        })? {
            BatchNext::Waiting(index) => index,
            BatchNext::AwaitingConfirmation(task_id) => {
                return Ok(BatchProgress::AwaitingConfirmation {
                    batch_id: batch_id.into(),
                    task_id,
                });
            }
        };
        if expected_host_id.is_some_and(|id| batch.targets[index].host.id != id) {
            return Err("人工确认的下一台主机已变化。".into());
        }
        let guard = Arc::new(release_materials::acquire_guard(batch.project_id)?);
        let current = crate::batch_store::get(batch_id)?;
        if current.state != batch.state
            || current.targets.len() != batch.targets.len()
            || current.targets.get(index).is_none_or(|target| {
                target.state != "waiting"
                    || target.release_task_id.is_some()
                    || target.host.id != batch.targets[index].host.id
            })
            || current.targets[..index]
                .iter()
                .zip(&batch.targets[..index])
                .any(|(now, prior)| {
                    now.state != prior.state || now.release_task_id != prior.release_task_id
                })
            || current.targets[index + 1..]
                .iter()
                .any(|target| target.state != "waiting" || target.release_task_id.is_some())
        {
            return Err("批次目标顺序或上一台结果已变化。".into());
        }
        for prior in &current.targets[..index] {
            let task_id = prior
                .release_task_id
                .as_deref()
                .ok_or("已开始主机缺少关联任务。")?;
            let task = release_store::get_task(task_id)?;
            let resolved = match prior.state.as_str() {
                "healthy" => {
                    task.phase == "checked"
                        && task.outcome == "complete"
                        && task.health_result.as_deref() == Some("healthy")
                }
                "skipped" => task.outcome == "failed",
                _ => false,
            };
            if !resolved
                || task.project_id != batch.project_id
                || task.host.id != prior.host.id
                || task.docker_daemon_id.as_deref() != Some(prior.docker_daemon_id.as_str())
            {
                return Err("上一台主机的关联任务尚未明确完成，禁止继续。".into());
            }
        }
        let prepared = match prepare_batch_host(
            batch.project_id,
            batch_id,
            confirmed,
            index,
            guard,
            cancel.clone(),
        ) {
            Ok(value) => value,
            Err(error) => {
                return Err(match crate::batch_store::pause_waiting(batch_id, index) {
                    Ok(()) => error,
                    Err(reason) => format!("{error}；批次暂停待核实：{reason}"),
                });
            }
        };
        upload_batch_host_archives(
            &prepared,
            batch_id,
            confirmed,
            index,
            cancel.clone(),
            &mut on_event,
        )?;
        load_batch_host_archives(
            &prepared,
            batch_id,
            confirmed,
            index,
            cancel.clone(),
            &mut on_event,
        )?;
        let staged = stage_batch_host_stack(
            &prepared,
            batch_id,
            confirmed,
            index,
            cancel.clone(),
            &mut on_event,
        )?;
        let task_id = deploy_batch_host_stack(
            &prepared,
            batch_id,
            confirmed,
            index,
            &staged,
            cancel,
            &mut on_event,
        )?;
        let task = release_store::get_task(&task_id)?;
        if task.phase != "checked"
            || task.outcome != "blocked"
            || task.health_result.as_deref() != Some("healthy")
        {
            return Err("批次目标未确认全部健康；已暂停后续目标。".into());
        }
        Ok(BatchProgress::AwaitingConfirmation {
            batch_id: batch_id.into(),
            task_id,
        })
    })();
    result.map_err(|message| BatchRunError {
        batch_id: Some(batch_id.into()),
        message,
    })
}

pub fn retry_failed_batch_sequential(
    batch_id: &str,
    expected_host_id: i64,
    expected_task_id: &str,
    cancel: Arc<AtomicBool>,
    on_event: impl FnMut(&str),
) -> Result<BatchProgress, BatchRunError> {
    let parallelism = crate::batch_store::get(batch_id)
        .map_err(|message| BatchRunError {
            batch_id: Some(batch_id.into()),
            message,
        })?
        .parallelism;
    if parallelism > 1 {
        return retry_failed_batch_parallel(
            batch_id,
            expected_host_id,
            expected_task_id,
            cancel,
            on_event,
        );
    }
    let prepared = (|| -> Result<BatchPreflight, String> {
        let batch = crate::batch_store::get(batch_id)?;
        let index = batch
            .targets
            .iter()
            .position(|target| target.host.id == expected_host_id)
            .ok_or("失败目标已不在批次中。")?;
        let target = &batch.targets[index];
        if batch.state != "paused"
            || target.state != "failed"
            || target.release_task_id.as_deref() != Some(expected_task_id)
            || batch.targets[..index]
                .iter()
                .any(|prior| !matches!(prior.state.as_str(), "healthy" | "skipped"))
            || batch.targets[index + 1..]
                .iter()
                .any(|later| later.state != "waiting" || later.release_task_id.is_some())
        {
            return Err("批次失败目标或执行顺序已变化。".into());
        }
        let confirmed = reconfirm_batch(batch_id, cancel.clone())?;
        let _guard = release_materials::acquire_guard(batch.project_id)?;
        revalidate_batch_inputs(
            batch.project_id,
            &confirmed,
            cancel.clone(),
            false,
            Some(index),
        )?;
        let task = release_store::get_task(expected_task_id)?;
        if task.outcome != "failed"
            || task.project_id != batch.project_id
            || task.host.id != target.host.id
            || task.docker_daemon_id.as_deref() != Some(target.docker_daemon_id.as_str())
            || !ssh::observe_release_project(&task)?.containers.is_empty()
        {
            return Err("失败任务尚未关闭，或远端项目仍有容器。".into());
        }
        crate::batch_store::retry_failed(batch_id, expected_host_id, expected_task_id)?;
        Ok(confirmed)
    })()
    .map_err(|message| BatchRunError {
        batch_id: Some(batch_id.into()),
        message,
    })?;
    continue_batch_sequential(
        batch_id,
        Some(expected_host_id),
        &prepared,
        cancel,
        on_event,
    )
}

fn retry_failed_batch_parallel(
    batch_id: &str,
    expected_host_id: i64,
    expected_task_id: &str,
    cancel: Arc<AtomicBool>,
    mut on_event: impl FnMut(&str),
) -> Result<BatchProgress, BatchRunError> {
    let result = (|| -> Result<BatchProgress, String> {
        let batch = crate::batch_store::get(batch_id)?;
        let index = batch
            .targets
            .iter()
            .position(|target| target.host.id == expected_host_id)
            .ok_or("失败目标已不在批次中。")?;
        let wave_start = index / batch.parallelism * batch.parallelism;
        let wave_end = (wave_start + batch.parallelism).min(batch.targets.len());
        let target = &batch.targets[index];
        if batch.state != "paused"
            || target.state != "failed"
            || target.release_task_id.as_deref() != Some(expected_task_id)
            || batch.targets[..wave_start]
                .iter()
                .any(|prior| !matches!(prior.state.as_str(), "healthy" | "skipped"))
            || batch.targets[wave_start..wave_end]
                .iter()
                .enumerate()
                .any(|(offset, other)| {
                    offset + wave_start != index
                        && matches!(other.state.as_str(), "running" | "unknown")
                })
            || batch.targets[wave_end..]
                .iter()
                .any(|later| later.state != "waiting" || later.release_task_id.is_some())
        {
            return Err("批次失败目标或波次状态已变化。".into());
        }
        let confirmed = reconfirm_batch(batch_id, cancel.clone())?;
        let guard = Arc::new(release_materials::acquire_guard(batch.project_id)?);
        revalidate_batch_inputs(
            batch.project_id,
            &confirmed,
            cancel.clone(),
            false,
            Some(index),
        )?;
        let task = release_store::get_task(expected_task_id)?;
        if task.outcome != "failed"
            || task.project_id != batch.project_id
            || task.host.id != target.host.id
            || task.docker_daemon_id.as_deref() != Some(target.docker_daemon_id.as_str())
            || !ssh::observe_release_project(&task)?.containers.is_empty()
        {
            return Err("失败任务尚未关闭，或远端项目仍有容器。".into());
        }
        crate::batch_store::retry_failed(batch_id, expected_host_id, expected_task_id)?;
        let prepared = match prepare_batch_host(
            batch.project_id,
            batch_id,
            &confirmed,
            index,
            guard,
            cancel.clone(),
        ) {
            Ok(host) => host,
            Err(error) => {
                let _ = crate::batch_store::pause_waiting(batch_id, index);
                return Err(error);
            }
        };
        upload_batch_host_archives(
            &prepared,
            batch_id,
            &confirmed,
            index,
            cancel.clone(),
            &mut on_event,
        )?;
        load_batch_host_archives(
            &prepared,
            batch_id,
            &confirmed,
            index,
            cancel.clone(),
            &mut on_event,
        )?;
        let staged = stage_batch_host_stack(
            &prepared,
            batch_id,
            &confirmed,
            index,
            cancel.clone(),
            &mut on_event,
        )?;
        let task_id = deploy_batch_host_stack(
            &prepared,
            batch_id,
            &confirmed,
            index,
            &staged,
            cancel,
            &mut on_event,
        )?;
        let task = release_store::get_task(&task_id)?;
        if task.phase != "checked"
            || task.outcome != "blocked"
            || task.health_result.as_deref() != Some("healthy")
        {
            return Err("重试目标未确认全部健康；批次已暂停。".into());
        }
        Ok(BatchProgress::AwaitingConfirmation {
            batch_id: batch_id.into(),
            task_id,
        })
    })();
    result.map_err(|message| BatchRunError {
        batch_id: Some(batch_id.into()),
        message,
    })
}

fn resumable_unknown_target_index(
    batch: &crate::batch_store::BatchRecord,
    expected_task_id: &str,
) -> Result<usize, String> {
    let index = batch
        .targets
        .iter()
        .position(|target| target.release_task_id.as_deref() == Some(expected_task_id))
        .ok_or("人工确认的目标任务已变化，禁止续传。")?;
    if batch.targets[index].state != "unknown" || batch.parallelism == 0 {
        return Err("批次没有对应的待核实上传目标。".into());
    }
    let wave = index / batch.parallelism;
    for (ordinal, target) in batch.targets.iter().enumerate() {
        let target_wave = ordinal / batch.parallelism;
        if (target_wave < wave && !matches!(target.state.as_str(), "healthy" | "skipped"))
            || (target_wave > wave
                && (target.state != "waiting" || target.release_task_id.is_some()))
            || (target_wave == wave
                && ordinal != index
                && !matches!(
                    target.state.as_str(),
                    "unknown" | "healthy" | "skipped" | "failed"
                )
                && !(target.state == "waiting" && target.release_task_id.is_none())
                && !(target.state == "running"
                    && target.task_outcome.as_deref() == Some("blocked")))
        {
            return Err("批次顺序存在其他未核实目标，不能续传。".into());
        }
    }
    Ok(index)
}

pub fn resume_unknown_batch_upload(
    batch_id: &str,
    expected_task_id: &str,
    cancel: Arc<AtomicBool>,
    mut on_event: impl FnMut(&str),
) -> Result<BatchProgress, BatchRunError> {
    let result = (|| {
        let confirmed = reconfirm_batch(batch_id, cancel.clone())?;
        let batch = crate::batch_store::get(batch_id)?;
        if batch.state != "unknown" {
            return Err("仅结果不明的批次可人工续传。".into());
        }
        let index = resumable_unknown_target_index(&batch, expected_task_id)?;
        let target = &confirmed.targets[index];
        let saved = &batch.targets[index];
        let task_id = saved
            .release_task_id
            .as_deref()
            .ok_or("待续传目标没有任务记录。")?;
        if task_id != expected_task_id {
            return Err("人工确认的目标任务已变化，禁止续传。".into());
        }
        let task = release_store::get_task(task_id)?;
        if task.phase != "uploading"
            || task.outcome != "unknown"
            || task.project_id != batch.project_id
            || task.host.name != target.host.name
            || !ssh::same_connection_target(&task.host, &target.host)
            || task.docker_endpoint.as_deref() != Some(target.docker_endpoint.as_str())
            || task.docker_daemon_id.as_deref() != Some(target.docker_daemon_id.as_str())
        {
            return Err("待续传任务阶段或可信主机身份已变化。".into());
        }
        let guard = release_materials::acquire_guard(batch.project_id)?;
        revalidate_batch_inputs(
            batch.project_id,
            &confirmed,
            cancel.clone(),
            false,
            Some(index),
        )?;
        let frozen = release_materials::load(task_id)?;
        if frozen.env_file != confirmed.env_files[index]
            || frozen.compose_files.len() != confirmed.compose_files.len()
            || frozen
                .compose_files
                .iter()
                .zip(&confirmed.compose_files)
                .any(|(saved, expected)| {
                    saved.path != expected.path || saved.bytes != expected.bytes
                })
        {
            return Err("任务版本材料与重新确认的批次不同。".into());
        }
        let inspection = release::inspect_unknown_upload(task_id)?;
        if !inspection.containers.is_empty() {
            return Err("远端已有该项目容器，不能按纯上传阶段续传。".into());
        }
        let host_plan = &confirmed.plan.hosts[index];
        let inventory = ssh::inventory(target.host_id)?;
        if host_plan.images.iter().any(|(_, reference)| {
            inventory
                .images
                .iter()
                .any(|image| image.reference == *reference)
        }) {
            return Err("远端已有本批次镜像 tag，禁止按上传阶段续传。".into());
        }
        let records = release_store::uploaded_archives(task_id)?;
        if records.len() > host_plan.images.len()
            || records.iter().any(|record| {
                !matches!(record.state.as_str(), "reserved" | "uploaded")
                    || !host_plan
                        .images
                        .iter()
                        .any(|(service, _)| service == &record.service)
            })
        {
            return Err("远端归档意图与批次镜像矩阵不一致。".into());
        }
        for (service, reference) in &host_plan.images {
            if cancel.load(Ordering::Relaxed) {
                return Err("批次续传已取消，仍保持待核实。".into());
            }
            let (bytes, digest, image_ids, saved_reference) =
                crate::batch_store::archive_record(batch_id, service, &target.platform)?;
            if &saved_reference != reference {
                return Err("共享镜像引用与批次记录不符。".into());
            }
            let archive = crate::release_archive::copy_verified(task_id, service, image_ids)?;
            if archive.bytes != bytes
                || archive.sha256 != digest
                || crate::build::archive_image_ids(&archive.path, reference, &target.platform)?
                    != archive.image_ids
            {
                return Err("目标版本归档身份与共享镜像不同。".into());
            }
            on_event(&format!("核实并续传镜像：{service}"));
            let Some(record) = records.iter().find(|item| item.service == *service) else {
                let directory = format!("/tmp/devingpanel.{}", uuid::Uuid::new_v4().simple());
                release_store::reserve_unknown_uploaded_archive(
                    task_id, service, &directory, bytes, &digest,
                )?;
                let remote = ssh::upload_unknown_archive_at(
                    target.host.clone(),
                    ssh::ReleaseDockerIdentity {
                        endpoint: &target.docker_endpoint,
                        daemon_id: &target.docker_daemon_id,
                    },
                    &archive.path,
                    &directory,
                    cancel.clone(),
                    |sent, total| on_event(&format!("镜像续传 {sent}/{total} 字节")),
                )?;
                release_store::confirm_uploaded_archive(task_id, service, &remote)?;
                continue;
            };
            if record.bytes != bytes || record.sha256 != digest {
                return Err("远端归档意图与本机归档摘要不同。".into());
            }
            let observed = inspection
                .archives
                .iter()
                .find(|item| item.service == *service)
                .ok_or("远端归档只读核实记录缺失。")?;
            if record.state == "uploaded"
                && (observed.actual_bytes != Some(bytes) || observed.digest_matches != Some(true))
            {
                return Err("已确认上传的远端归档发生变化，禁止续传。".into());
            }
            let remote = if observed.actual_bytes.is_none() {
                if record.state != "reserved" {
                    return Err("已确认上传的远端归档缺失，禁止自动重建。".into());
                }
                ssh::upload_unknown_archive_at(
                    target.host.clone(),
                    ssh::ReleaseDockerIdentity {
                        endpoint: &target.docker_endpoint,
                        daemon_id: &target.docker_daemon_id,
                    },
                    &archive.path,
                    &record.directory,
                    cancel.clone(),
                    |sent, total| on_event(&format!("镜像续传 {sent}/{total} 字节")),
                )?
            } else {
                ssh::resume_uploaded_archive(
                    target.host.clone(),
                    ssh::ReleaseDockerIdentity {
                        endpoint: &target.docker_endpoint,
                        daemon_id: &target.docker_daemon_id,
                    },
                    record,
                    &archive.path,
                    cancel.clone(),
                    |sent, total| on_event(&format!("镜像续传 {sent}/{total} 字节")),
                )?
            };
            if record.state == "reserved" {
                release_store::confirm_uploaded_archive(task_id, service, &remote)?;
            }
        }
        release_store::finish_unknown_upload(task_id)?;
        let prepared = PreparedBatchHost {
            task_id: task_id.into(),
            _guard: Arc::new(guard),
        };
        let services = load_batch_host_archives(
            &prepared,
            batch_id,
            &confirmed,
            index,
            cancel.clone(),
            &mut on_event,
        )?;
        let staged = stage_batch_host_stack(
            &prepared,
            batch_id,
            &confirmed,
            index,
            cancel.clone(),
            &mut on_event,
        )?;
        let completed = deploy_batch_host_stack(
            &prepared,
            batch_id,
            &confirmed,
            index,
            &staged,
            cancel,
            &mut on_event,
        )?;
        let task = release_store::get_task(&completed)?;
        if services.is_empty()
            || task.phase != "checked"
            || task.outcome != "blocked"
            || task.health_result.as_deref() != Some("healthy")
        {
            return Err("续传后的主机未确认全部健康，后续目标保持阻断。".into());
        }
        Ok(BatchProgress::AwaitingConfirmation {
            batch_id: batch_id.into(),
            task_id: completed,
        })
    })();
    result.map_err(|message| {
        let recovery = crate::batch_store::get(batch_id).ok().and_then(|batch| {
            if !matches!(batch.state.as_str(), "running" | "unknown") {
                return None;
            }
            batch
                .targets
                .iter()
                .find(|target| {
                    target.state == "running"
                        && target.release_task_id.as_deref() == Some(expected_task_id)
                })
                .and_then(|target| target.release_task_id.as_deref())
                .and_then(|id| {
                    release_store::get_task(id)
                        .ok()
                        .map(|task| (id.to_owned(), task))
                })
        });
        let settle_error = recovery.and_then(|(id, task)| {
            if task.outcome != "running" {
                return None;
            }
            let phase = match task.phase.as_str() {
                "uploaded" => Phase::Uploaded,
                "loading" => Phase::Loading,
                "loaded" => Phase::Loaded,
                "awaiting_deployment" => Phase::AwaitingDeployment,
                "staging" => Phase::Staging,
                "staged" => Phase::Staged,
                "validating" => Phase::Validating,
                "validated" => Phase::Validated,
                "deploying" => Phase::Deploying,
                "checking" => Phase::Checking,
                _ => return Some("续传任务阶段无法安全标记待核实。".to_owned()),
            };
            release_store::settle(
                &id,
                phase,
                Outcome::Unknown,
                "批次续传后中断，远端结果待核实",
            )
            .err()
        });
        BatchRunError {
            batch_id: Some(batch_id.into()),
            message: match settle_error {
                Some(reason) => format!("{message}；任务状态需人工核对：{reason}"),
                None => message,
            },
        }
    })
}

#[allow(dead_code)] // The batch executor must call this before creating its first task.
pub fn revalidate_batch(
    project_id: i64,
    preflight: &BatchPreflight,
    cancel: Arc<AtomicBool>,
) -> Result<(), String> {
    revalidate_batch_inputs(project_id, preflight, cancel, true, None)
}

fn revalidate_batch_inputs(
    project_id: i64,
    preflight: &BatchPreflight,
    cancel: Arc<AtomicBool>,
    require_unused_tags: bool,
    remote_index: Option<usize>,
) -> Result<(), String> {
    let project = project_store::effective(project_id)?;
    if cancel.load(Ordering::Relaxed)
        || release::source_digest(&project)? != preflight.config_sha256
        || project.host_ids
            != preflight
                .targets
                .iter()
                .map(|target| target.host_id)
                .collect::<Vec<_>>()
    {
        return Err("批次配置或目标顺序已变化，请重新预检。".into());
    }
    let (hosts, _) = host_store::list()?;
    if !same_batch_hosts(&preflight.targets, &hosts) {
        return Err("批次主机资料已变化，请重新预检。".into());
    }
    if require_unused_tags {
        let supported = docker::build_platforms()?;
        if preflight
            .targets
            .iter()
            .any(|target| !supported.contains(&target.platform))
        {
            return Err("本机 default 构建器已不支持目标平台，请重新预检。".into());
        }
        let images = preflight
            .plan
            .builds
            .iter()
            .map(|build| release::PlannedImage {
                platform: build.platform.clone(),
                reference: build.reference.clone(),
            })
            .collect::<Vec<_>>();
        release::check_planned_tags(&images)?;
    }
    if preflight.env_files.len() != preflight.targets.len() {
        return Err("批次环境快照数量已变化。".into());
    }
    for (index, (target, frozen_env)) in preflight
        .targets
        .iter()
        .zip(&preflight.env_files)
        .enumerate()
    {
        if cancel.load(Ordering::Relaxed) {
            return Err("批次执行前核实已取消。".into());
        }
        let host_plan = preflight
            .plan
            .hosts
            .iter()
            .find(|host| host.host_id == target.host_id)
            .ok_or("批次目标镜像计划缺失。")?;
        let current_env = batch_env_file(
            &env_store::deployment_env_file(project_id, target.host_id)?,
            &host_plan.images,
        )?;
        if &current_env != frozen_env {
            return Err("批次环境变量在预检后变化，请重新预检。".into());
        }
        if remote_index.is_some_and(|selected| selected != index) {
            continue;
        }
        let platform = release::probe_targets(&project, &[target.host_id], &hosts, cancel.clone())?
            [0]
        .1
        .clone();
        let (endpoint, daemon_id) = ssh::docker_identity_for_host(target.host.clone())?;
        if platform != target.platform
            || endpoint != target.docker_endpoint
            || daemon_id != target.docker_daemon_id
        {
            return Err("批次目标架构或 Docker daemon 身份已变化，请重新预检。".into());
        }
    }
    Ok(())
}

pub fn run(
    project_id: i64,
    host_id: i64,
    base_tag: &str,
    expected_digest: Option<&str>,
    cancel: Arc<AtomicBool>,
    mut on_event: impl FnMut(&str),
) -> Result<String, String> {
    let project = project_store::effective(project_id)?;
    release::check_project_config(&project, &project.host_ids)?;
    if !project.host_ids.contains(&host_id) {
        return Err("目标主机不属于 YAML 配置的发布目标。".into());
    }
    env_store::check_deployment_environment(project_id, &[host_id])?;
    let manifest = project_config::load(Path::new(&project.path))?;
    let (hosts, _) = host_store::list()?;
    let host = hosts
        .iter()
        .find(|host| host.id == host_id)
        .cloned()
        .ok_or("目标主机不存在。")?;
    let digest = release::source_digest(&project)?;
    if expected_digest.is_some_and(|expected| expected != digest) {
        return Err("项目配置与发布确认时不一致，请重新确认。".into());
    }
    let targets = release::probe_targets(&project, &[host_id], &hosts, cancel.clone())?;
    let platform = &targets[0].1;
    if !docker::build_platforms()?.contains(platform) {
        return Err("本机 default 构建器不支持目标平台。".into());
    }
    let planned = release::plan_manifest_images(&manifest, base_tag, &targets)?;
    let registry_credentials = registry::enabled_credentials()?;
    if let Some(credentials) = &registry_credentials
        && planned.iter().any(|image| {
            !image
                .reference
                .starts_with(&format!("{}/", credentials.config.address))
        })
    {
        return Err("镜像仓库模式要求所有构建镜像名以 Registry 地址开头。".into());
    }
    if planned.len() != manifest.builds.len() {
        return Err("多镜像构建计划不完整。".into());
    }
    for image in &planned {
        if docker::image_reference_exists(&image.reference)? {
            return Err("本地镜像 tag 已存在，请更换发布标识。".into());
        }
    }
    let files = release::deployment_files(&project)?;
    let mut env_file = env_store::deployment_env_file(project_id, host_id)?;
    if env_file
        .split(|byte| *byte == b'\n')
        .any(|line| line.starts_with(b"DEVINGPANEL_IMAGE_"))
    {
        return Err("环境变量与构建镜像变量冲突。".into());
    }
    for (spec, image) in manifest.builds.iter().zip(&planned) {
        let variable = project_config::image_variable(&spec.service)?;
        env_file.extend_from_slice(format!("{variable}=\"{}\"\n", image.reference).as_bytes());
    }
    let materials = release_materials::prepare(&files, &env_file)?;
    if cancel.load(Ordering::Relaxed)
        || release::source_digest(&project_store::effective(project_id)?)? != digest
    {
        return Err("项目配置在预检期间变化或预检已取消。".into());
    }
    let (docker_endpoint, docker_daemon_id) = ssh::docker_identity_for_host(host.clone())?;
    let docker_identity = ssh::ReleaseDockerIdentity {
        endpoint: &docker_endpoint,
        daemon_id: &docker_daemon_id,
    };
    let id = uuid::Uuid::new_v4().to_string();
    let first = &planned[0];
    registry::create_release_intent(
        &Intent {
            id: id.clone(),
            project_id,
            project_name: project.name.clone(),
            host: host.clone(),
            docker_endpoint: docker_endpoint.clone(),
            docker_daemon_id: docker_daemon_id.clone(),
            image_reference: first.reference.clone(),
            platform: first.platform.clone(),
            images: manifest
                .builds
                .iter()
                .zip(&planned)
                .map(|(spec, image)| {
                    (
                        spec.service.clone(),
                        image.reference.clone(),
                        image.platform.clone(),
                    )
                })
                .collect(),
            config_sha256: digest.clone(),
        },
        &registry_credentials,
    )?;
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
    let mut built = Vec::new();
    for (spec, image) in manifest.builds.iter().zip(&planned) {
        on_event(&format!("构建镜像：{}", spec.service));
        let result = build::build_image(
            &build_project(&project, spec),
            image,
            cancel.clone(),
            |_| {}, // Build output is not a trusted progress stage and may contain secrets.
        );
        match result {
            Ok(value) => built.push(value),
            Err(error) => {
                let _ = release_store::settle(
                    &id,
                    Phase::Building,
                    Outcome::Unknown,
                    "多镜像构建结果待核实",
                );
                return Err(error.message);
            }
        }
    }
    release_store::advance(&id, Phase::Building, Phase::Built)?;
    if project_store::effective(project_id)
        .and_then(|current| release::source_digest(&current))
        .as_deref()
        != Ok(digest.as_str())
    {
        release_store::settle(&id, Phase::Built, Outcome::Failed, "项目文件在构建期间变化")?;
        return Err("项目文件在构建期间变化，未开始上传。".into());
    }
    release_store::advance(&id, Phase::Built, Phase::Uploading)?;
    let expected: Vec<StackService> = if let Some(credentials) = &registry_credentials {
        let mut images = Vec::new();
        for (spec, image) in manifest.builds.iter().zip(&built) {
            on_event(&format!("推送镜像：{}", spec.service));
            let result = (|| {
                let archive = build::save_image(image, cancel.clone())?;
                crate::release_archive::retain(&id, &spec.service, &archive)?;
                registry::push_with_credentials(credentials, &image.reference, &cancel)?;
                Ok::<_, String>(archive.image_ids.clone())
            })();
            match result {
                Ok(image_ids) => images.push(StackService {
                    service: spec.service.clone(),
                    reference: image.reference.clone(),
                    image_ids,
                }),
                Err(error) => {
                    let _ = release_store::settle(
                        &id,
                        Phase::Uploading,
                        Outcome::Unknown,
                        "镜像仓库推送结果待核实",
                    );
                    return Err(error);
                }
            }
        }
        release_store::advance(&id, Phase::Uploading, Phase::Uploaded)?;
        release_store::advance(&id, Phase::Uploaded, Phase::Loading)?;
        for (service, image) in images.iter().zip(&built) {
            if let Err(error) =
                release_store::record_image_ids(&id, &service.service, &service.image_ids)
            {
                let _ = release_store::settle(
                    &id,
                    Phase::Loading,
                    Outcome::Unknown,
                    "镜像仓库镜像 ID 保存结果待核实",
                );
                return Err(error);
            }
            on_event(&format!("远端拉取镜像：{}", service.service));
            if let Err(error) = ssh::pull_registry_image(
                host.clone(),
                docker_identity,
                &id,
                &image.reference,
                &image.platform,
                &service.image_ids,
                credentials,
                cancel.clone(),
            ) {
                let _ = release_store::settle(
                    &id,
                    Phase::Loading,
                    Outcome::Unknown,
                    "镜像仓库拉取结果待核实",
                );
                return Err(error);
            }
        }
        release_store::advance(&id, Phase::Loading, Phase::Loaded)?;
        images
    } else {
        let mut uploaded = Vec::new();
        for (spec, image) in manifest.builds.iter().zip(&built) {
            on_event(&format!("传输镜像：{}", spec.service));
            match release::transfer_built_image(
                &id,
                &spec.service,
                host.clone(),
                image,
                cancel.clone(),
                |sent, total| {
                    on_event(&format!("镜像传输 {sent}/{total} 字节"));
                },
            ) {
                Ok(value) => uploaded.push(value),
                Err(error) => {
                    let cleanup = cleanup_known_archives(&uploaded, None, |archive| {
                        ssh::cleanup_uploaded_archive(host.clone(), archive)
                    });
                    let _ = release_store::settle(
                        &id,
                        Phase::Uploading,
                        Outcome::Unknown,
                        "多镜像传输结果待核实",
                    );
                    return Err(match cleanup {
                        Ok(()) => error,
                        Err(warning) => format!("{error}；{warning}"),
                    });
                }
            }
        }
        release_store::advance(&id, Phase::Uploading, Phase::Uploaded)?;
        release_store::advance(&id, Phase::Uploaded, Phase::Loading)?;
        for (index, ((spec, image), archive)) in manifest
            .builds
            .iter()
            .zip(&built)
            .zip(&uploaded)
            .enumerate()
        {
            on_event(&format!("远端加载镜像：{}", spec.service));
            if let Err(error) =
                release_store::record_image_ids(&id, &spec.service, &archive.image_ids)
            {
                let cleanup = cleanup_known_archives(&uploaded, None, |archive| {
                    ssh::cleanup_uploaded_archive(host.clone(), archive)
                });
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
            if let Err(error) = ssh::load_uploaded_archive(
                host.clone(),
                docker_identity,
                archive,
                &image.reference,
                &image.platform,
                cancel.clone(),
            ) {
                let cleanup = cleanup_known_archives(&uploaded, Some(index), |archive| {
                    ssh::cleanup_uploaded_archive(host.clone(), archive)
                });
                let _ = release_store::settle(
                    &id,
                    Phase::Loading,
                    Outcome::Unknown,
                    "多镜像加载结果待核实",
                );
                return Err(match cleanup {
                    Ok(()) => error,
                    Err(warning) => format!("{error}；{warning}"),
                });
            }
        }
        release_store::advance(&id, Phase::Loading, Phase::Loaded)?;
        if let Err(error) = cleanup_known_archives(&uploaded, None, |archive| {
            ssh::cleanup_uploaded_archive(host.clone(), archive)
        }) {
            let _ =
                release_store::settle(&id, Phase::Loaded, Outcome::Unknown, "临时归档清理待核实");
            return Err(error);
        }
        manifest
            .builds
            .iter()
            .zip(&built)
            .zip(&uploaded)
            .map(|((spec, image), archive)| StackService {
                service: spec.service.clone(),
                reference: image.reference.clone(),
                image_ids: archive.image_ids.clone(),
            })
            .collect()
    };
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
        Ok(value) => value,
        Err(error) => {
            let _ =
                release_store::settle(&id, Phase::Staging, Outcome::Unknown, "整栈部署文件待核实");
            return Err(error);
        }
    };
    release_store::advance(&id, Phase::Staging, Phase::Staged)?;
    release_store::advance(&id, Phase::Staged, Phase::Validating)?;
    on_event("校验所有 Compose 服务");
    let services = match ssh::validate_staged_stack(
        host.clone(),
        docker_identity,
        &staged,
        project_id,
        &expected,
        cancel.clone(),
    ) {
        Ok(value) => value,
        Err(error) => {
            let _ = release_store::settle(
                &id,
                Phase::Validating,
                Outcome::Unknown,
                "远端整栈配置待核实",
            );
            return Err(error);
        }
    };
    if let Err(error) = release_store::record_services(&id, &services) {
        let _ = release_store::settle(
            &id,
            Phase::Validating,
            Outcome::Failed,
            "整栈服务计划未保存",
        );
        return Err(error);
    }
    release_store::advance(&id, Phase::Validating, Phase::Validated)?;
    release_store::advance(&id, Phase::Validated, Phase::Deploying)?;
    on_event("启动全部 Compose 服务");
    if let Err(error) = ssh::start_staged_stack(
        host.clone(),
        docker_identity,
        &staged,
        project_id,
        &expected,
        cancel.clone(),
    ) {
        let _ = ssh::check_staged_stack(
            host,
            docker_identity,
            &staged,
            project_id,
            &expected,
            &services,
            Arc::new(AtomicBool::new(false)),
        );
        let _ = release_store::settle(
            &id,
            Phase::Deploying,
            Outcome::Unknown,
            "整栈启动结果待核实，禁止自动重试",
        );
        return Err(error);
    }
    release_store::advance(&id, Phase::Deploying, Phase::Checking)?;
    on_event("核对全部服务健康状态");
    let health = match ssh::check_staged_stack(
        host,
        docker_identity,
        &staged,
        project_id,
        &expected,
        &services,
        cancel,
    ) {
        Ok(value) => value,
        Err(error) => {
            let _ = release_store::settle(&id, Phase::Checking, Outcome::Unknown, "整栈状态待核实");
            return Err(error);
        }
    };
    let (result, detail) = match health {
        DeploymentState::Healthy => (HealthResult::Healthy, "全部服务健康；回滚执行尚未实现"),
        DeploymentState::RunningUnverified => (
            HealthResult::RunningUnverified,
            "至少一个服务缺少健康检查，需人工核实",
        ),
        DeploymentState::Failed => (HealthResult::Failed, "至少一个服务未健康，需人工核实"),
    };
    release_store::settle_checked(&id, result, detail)?;
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::{
        BatchBuild, BatchHost, BatchNext, BatchPlan, BatchPreflight, BatchTargetIdentity,
        batch_env_file, batch_plan_from, build_shared_images_local, cleanup_known_archives,
        ensure_distinct_daemons, env_digest, next_batch_target, plan_batch_from_manifest,
        preflight_batch, same_batch_hosts, spec_for_batch_build, verify_batch_stack_config,
        verify_resolved_batch_images,
    };
    use crate::batch_store::{BatchRecord, BatchTargetRecord};
    use crate::host_store::Host;
    use crate::project_config::{self, BuildSpec};
    use crate::project_store::Project;
    use crate::release::PlannedImage;
    use std::sync::{Arc, atomic::AtomicBool};

    struct CutOnceSshProxy {
        port: u16,
        enabled: Arc<AtomicBool>,
        cut: Arc<AtomicBool>,
        offline: Arc<AtomicBool>,
        stop: Arc<AtomicBool>,
        worker: Option<std::thread::JoinHandle<()>>,
    }

    impl CutOnceSshProxy {
        fn new(target: &'static str) -> Self {
            use std::io::{Read, Write};
            use std::net::{Shutdown, TcpListener, TcpStream};
            use std::sync::atomic::Ordering;

            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let port = listener.local_addr().unwrap().port();
            let enabled = Arc::new(AtomicBool::new(false));
            let cut = Arc::new(AtomicBool::new(false));
            let offline = Arc::new(AtomicBool::new(false));
            let stop = Arc::new(AtomicBool::new(false));
            let running = stop.clone();
            let fault = enabled.clone();
            let fired = cut.clone();
            let unavailable = offline.clone();
            let worker = std::thread::spawn(move || {
                while !running.load(Ordering::Relaxed) {
                    let (client, _) = match listener.accept() {
                        Ok(pair) => pair,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(std::time::Duration::from_millis(20));
                            continue;
                        }
                        Err(_) => break,
                    };
                    if client.set_nonblocking(false).is_err() {
                        continue;
                    }
                    if unavailable.load(Ordering::Relaxed) {
                        let _ = client.shutdown(Shutdown::Both);
                        continue;
                    }
                    let fault = fault.clone();
                    let fired = fired.clone();
                    let unavailable = unavailable.clone();
                    std::thread::spawn(move || {
                        let Ok(server) = TcpStream::connect(target) else {
                            return;
                        };
                        let mut from_client = client.try_clone().unwrap();
                        let mut to_server = server.try_clone().unwrap();
                        let mut from_server = server.try_clone().unwrap();
                        let mut to_client = client.try_clone().unwrap();
                        let downstream = std::thread::spawn(move || {
                            let _ = std::io::copy(&mut from_server, &mut to_client);
                        });
                        let mut buffer = [0u8; 8192];
                        let mut sent = 0usize;
                        loop {
                            match from_client.read(&mut buffer) {
                                Ok(0) => break,
                                Ok(size) => {
                                    if to_server.write_all(&buffer[..size]).is_err() {
                                        break;
                                    }
                                    sent += size;
                                    if sent >= 256 * 1024
                                        && fault.load(Ordering::Relaxed)
                                        && !fired.swap(true, Ordering::Relaxed)
                                    {
                                        unavailable.store(true, Ordering::Relaxed);
                                        let _ = client.shutdown(Shutdown::Both);
                                        let _ = server.shutdown(Shutdown::Both);
                                        break;
                                    }
                                }
                                Err(_) => break,
                            }
                        }
                        let _ = client.shutdown(Shutdown::Both);
                        let _ = server.shutdown(Shutdown::Both);
                        let _ = downstream.join();
                    });
                }
            });
            Self {
                port,
                enabled,
                cut,
                offline,
                stop,
                worker: Some(worker),
            }
        }
    }

    impl Drop for CutOnceSshProxy {
        fn drop(&mut self) {
            use std::sync::atomic::Ordering;
            self.stop.store(true, Ordering::Relaxed);
            if let Some(worker) = self.worker.take() {
                let _ = worker.join();
            }
        }
    }

    #[test]
    #[ignore = "requires a fresh isolated APPDATA under target/isolated-tests and local Compose CLI"]
    fn isolated_batch_preflight_missing_environment_has_no_release_intent() {
        let root = std::path::PathBuf::from(std::env::var_os("APPDATA").unwrap())
            .canonicalize()
            .unwrap();
        let allowed = std::env::current_dir()
            .unwrap()
            .join("target/isolated-tests")
            .canonicalize()
            .unwrap();
        assert_eq!(root.parent(), Some(allowed.as_path()));
        assert!(
            root.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("batch-preflight-")
        );
        assert!(std::fs::read_dir(&root).unwrap().next().is_none());
        let conn = crate::database::connect().unwrap();
        conn.execute_batch("INSERT INTO hosts (id, name, address, port, username, auth, group_name, credential_id, trusted_fingerprint)
            VALUES (11, 'vps-smoke', 'invalid.example', 22, 'root', 'password', 'test', 'missing-one', 'SHA256:test'),
                   (12, 'vps-second', 'invalid.example', 22, 'root', 'password', 'test', 'missing-two', 'SHA256:test');")
            .unwrap();
        let project_dir = root.join("fixture");
        std::fs::create_dir(&project_dir).unwrap();
        let source = std::env::current_dir()
            .unwrap()
            .join("tests/fixtures/stack-v2-success");
        for file in [
            "compose.yaml",
            "Dockerfile.web",
            "Dockerfile.worker",
            "probe-web.txt",
            "probe-worker.txt",
        ] {
            std::fs::copy(source.join(file), project_dir.join(file)).unwrap();
        }
        let yaml = std::fs::read_to_string(source.join("devingpanel.yaml")).unwrap();
        std::fs::write(project_dir.join("devingpanel.yaml"),
            yaml.replace("    - group: test\n      host: vps-smoke",
                "    - group: test\n      host: vps-smoke\n    - group: test\n      host: vps-second"))
            .unwrap();
        let project = crate::project_store::create_from_manifest(&project_dir).unwrap();
        assert_eq!(project.host_ids, [11, 12]);
        conn.execute(
            "INSERT INTO project_env (project_id, host_id, ordinal, name, credential_id)
            VALUES (?1, 12, 0, 'MISSING_SECRET', 'missing-isolated-secret')",
            [project.id],
        )
        .unwrap();
        let count = |table: &str| -> i64 {
            conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .unwrap()
        };
        let before = [
            count("release_tasks"),
            count("release_batches"),
            count("release_batch_targets"),
        ];
        let error = preflight_batch(project.id, "isolated", Arc::new(AtomicBool::new(false)))
            .err()
            .expect("missing environment must fail preflight");
        assert!(
            error.contains("凭据") || error.contains("环境变量"),
            "{error}"
        );
        assert_eq!(
            before,
            [
                count("release_tasks"),
                count("release_batches"),
                count("release_batch_targets")
            ]
        );
        assert!(!root.join("devingPanel").join("batch-archives").exists());
    }

    #[test]
    #[ignore = "builds uniquely tagged local images; requires a fresh isolated APPDATA and Docker daemon"]
    fn isolated_batch_local_failures_stop_before_transfer() {
        let root = std::path::PathBuf::from(std::env::var_os("APPDATA").unwrap())
            .canonicalize()
            .unwrap();
        let allowed = std::env::current_dir()
            .unwrap()
            .join("target/isolated-tests")
            .canonicalize()
            .unwrap();
        assert_eq!(root.parent(), Some(allowed.as_path()));
        assert!(
            root.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("batch-health-")
        );
        assert!(std::fs::read_dir(&root).unwrap().next().is_none());
        let source = std::env::current_dir()
            .unwrap()
            .join("tests/fixtures/stack-v2-success");
        let directory = root.join("fixture");
        std::fs::create_dir(&directory).unwrap();
        for file in [
            "compose.yaml",
            "devingpanel.yaml",
            "Dockerfile.web",
            "probe-web.txt",
            "probe-worker.txt",
        ] {
            std::fs::copy(source.join(file), directory.join(file)).unwrap();
        }
        let dockerfile = std::fs::read_to_string(source.join("Dockerfile.worker")).unwrap();
        std::fs::write(
            directory.join("Dockerfile.worker"),
            dockerfile
                .lines()
                .filter(|line| !line.starts_with("HEALTHCHECK "))
                .collect::<Vec<_>>()
                .join("\n")
                + "\n",
        )
        .unwrap();
        let manifest = project_config::load(&directory).unwrap();
        let project = Project {
            id: 910001,
            name: manifest.name.clone(),
            path: directory.to_string_lossy().into_owned(),
            dockerfile: manifest.dockerfile.clone(),
            image: manifest.image.clone(),
            tag_rule: manifest.tag_rule.clone(),
            platform: manifest.platform.clone(),
            service: manifest.service.clone(),
            compose_files: manifest.compose_files.clone(),
            missing_compose_files: vec![],
            build_args: vec![],
            host_ids: vec![11, 12],
            manifest_mode: true,
            config_error: None,
        };
        let suffix = uuid::Uuid::new_v4().simple().to_string();
        let builds = manifest
            .builds
            .iter()
            .map(|spec| BatchBuild {
                service: spec.service.clone(),
                platform: "linux/amd64".into(),
                reference: format!("{}:{suffix}-amd64", spec.image),
            })
            .collect::<Vec<_>>();
        let host = |host_id| BatchHost {
            host_id,
            platform: "linux/amd64".into(),
            images: builds
                .iter()
                .map(|build| (build.service.clone(), build.reference.clone()))
                .collect(),
        };
        let hosts = vec![host(11), host(12)];
        let plan = BatchPlan { builds, hosts };
        let env = batch_env_file(b"", &plan.hosts[0].images).unwrap();
        let preflight = BatchPreflight {
            plan,
            config_sha256: "unused-before-health-failure".into(),
            builds: manifest.builds,
            compose_files: crate::release::deployment_files(&project).unwrap(),
            env_files: vec![env.clone(), env],
            targets: vec![],
        };
        let containers = || {
            let output = std::process::Command::new("docker")
                .args(["container", "ls", "-a", "--format", "{{.ID}}"])
                .output()
                .unwrap();
            assert!(output.status.success());
            output.stdout
        };
        let before = containers();
        let error = build_shared_images_local(
            &project,
            "isolated-health",
            &preflight,
            Arc::new(AtomicBool::new(false)),
            &mut |_| {},
        )
        .err();
        for build in &preflight.plan.builds {
            let status = std::process::Command::new("docker")
                .args(["image", "rm", &build.reference])
                .status()
                .unwrap();
            assert!(status.success(), "could not remove isolated test image");
        }
        assert!(error.unwrap().contains("缺少有效 HEALTHCHECK"));
        std::fs::write(
            directory.join("Dockerfile.worker"),
            "FROM scratch\nUNKNOWN_INSTRUCTION fail\n",
        )
        .unwrap();
        let build_error = build_shared_images_local(
            &project,
            "isolated-build-failure",
            &preflight,
            Arc::new(AtomicBool::new(false)),
            &mut |_| {},
        )
        .err();
        assert!(
            build_error.is_some(),
            "invalid Dockerfile did not fail the build"
        );
        let web = &preflight.plan.builds[0].reference;
        let removed = std::process::Command::new("docker")
            .args(["image", "rm", web])
            .status()
            .unwrap();
        assert!(removed.success(), "could not remove isolated test image");
        let worker = &preflight.plan.builds[1].reference;
        let absent = std::process::Command::new("docker")
            .args(["image", "inspect", worker])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(!absent.success(), "failed build left a tagged worker image");
        assert_eq!(containers(), before);
        assert!(!root.join("devingPanel").join("batch-archives").exists());
        assert!(
            std::fs::read_dir(&root).unwrap().all(|entry| {
                !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with("devingpanel-image-")
            }),
            "local temporary archive survived the failed build"
        );
        let conn = crate::database::connect().unwrap();
        for table in [
            "release_batches",
            "release_tasks",
            "release_uploaded_archives",
        ] {
            let count: i64 = conn
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(count, 0, "{table} acquired a remote intent");
        }
    }

    #[test]
    #[ignore = "one-shot full coordinator build failure against two trusted LAN VPS hosts; requires an isolated copied APPDATA"]
    fn isolated_lan_batch_build_failure_stops_before_transfer() {
        let appdata = std::path::PathBuf::from(std::env::var_os("APPDATA").unwrap())
            .canonicalize()
            .unwrap();
        let root = appdata.parent().unwrap();
        assert!(
            root.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("batch-vps-build-failure-")
        );
        assert_eq!(
            root.parent().unwrap(),
            std::env::current_dir()
                .unwrap()
                .join("target/isolated-tests")
                .canonicalize()
                .unwrap()
        );
        let project_dir = root.join("project");
        std::fs::create_dir(&project_dir).unwrap();
        let source = std::env::current_dir()
            .unwrap()
            .join("tests/fixtures/stack-v2-success");
        for file in [
            "compose.yaml",
            "Dockerfile.web",
            "probe-web.txt",
            "probe-worker.txt",
        ] {
            std::fs::copy(source.join(file), project_dir.join(file)).unwrap();
        }
        std::fs::write(
            project_dir.join("Dockerfile.worker"),
            "FROM scratch\nUNKNOWN_INSTRUCTION fail\n",
        )
        .unwrap();
        let yaml = std::fs::read_to_string(source.join("devingpanel.yaml")).unwrap();
        std::fs::write(
            project_dir.join("devingpanel.yaml"),
            yaml.replace(
                "name: stack-v2-success",
                &format!("name: stack-v2-build-fail-{}", uuid::Uuid::new_v4().simple()),
            )
            .replace(
                "    - group: test\n      host: vps-smoke",
                "    - group: test\n      host: vps-smoke\n    - group: test\n      host: vps-second",
            ),
        )
        .unwrap();
        let project = crate::project_store::create_from_manifest(&project_dir).unwrap();
        assert_eq!(project.host_ids, [1, 2]);
        let hosts = crate::host_store::list().unwrap().0;
        let remote = || {
            project
                .host_ids
                .iter()
                .map(|id| {
                    let inventory = crate::ssh::inventory(*id).unwrap();
                    let mut containers = inventory
                        .containers
                        .iter()
                        .map(|item| item.id.clone())
                        .collect::<Vec<_>>();
                    let mut images = inventory
                        .images
                        .iter()
                        .map(|item| item.reference.clone())
                        .collect::<Vec<_>>();
                    containers.sort();
                    images.sort();
                    (containers, images)
                })
                .collect::<Vec<_>>()
        };
        for (id, address, daemon) in [
            (1, "192.0.2.129", "00000000-0000-4000-8000-000000000008"),
            (2, "192.0.2.130", "00000000-0000-4000-8000-000000000011"),
        ] {
            let host = hosts.iter().find(|host| host.id == id).unwrap();
            assert_eq!(host.address, address);
            assert_eq!(
                crate::ssh::docker_identity_for_host(host.clone())
                    .unwrap()
                    .1,
                daemon
            );
        }
        let before_remote = remote();
        let conn = crate::database::connect().unwrap();
        let count = |table: &str| {
            conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap()
        };
        let before_db = [count("release_tasks"), count("release_uploaded_archives")];
        let tag = format!("lanbuildfail{}", uuid::Uuid::new_v4().simple());
        let preflight =
            preflight_batch(project.id, &tag, Arc::new(AtomicBool::new(false))).unwrap();
        assert_eq!(preflight.targets.len(), 2);
        assert_ne!(
            preflight.targets[0].docker_daemon_id,
            preflight.targets[1].docker_daemon_id
        );
        let error = super::run_batch_sequential(
            project.id,
            &preflight,
            Arc::new(AtomicBool::new(false)),
            |_| {},
        )
        .err()
        .expect("invalid worker Dockerfile must stop the batch");
        assert!(
            error.message.contains("本地 Docker 构建失败"),
            "{}",
            error.message
        );
        let batch_id = error
            .batch_id
            .expect("batch intent must exist before build");
        let batch = crate::batch_store::get(&batch_id).unwrap();
        assert_eq!(batch.state, "terminated");
        assert!(
            batch
                .targets
                .iter()
                .all(|target| { target.state == "waiting" && target.release_task_id.is_none() })
        );
        assert_eq!(
            [count("release_tasks"), count("release_uploaded_archives")],
            before_db
        );
        assert!(
            !appdata
                .join("devingPanel")
                .join("batch-archives")
                .join(&batch_id)
                .exists()
        );
        assert_eq!(remote(), before_remote);
        for build in &preflight.plan.builds {
            let status = std::process::Command::new("docker")
                .args(["image", "inspect", &build.reference])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .unwrap();
            if build.service == "web" {
                assert!(status.success());
                assert!(
                    std::process::Command::new("docker")
                        .args(["image", "rm", &build.reference])
                        .status()
                        .unwrap()
                        .success()
                );
            } else {
                assert!(!status.success());
            }
        }

        let health_dir = root.join("health-project");
        std::fs::create_dir(&health_dir).unwrap();
        for file in [
            "compose.yaml",
            "Dockerfile.web",
            "probe-web.txt",
            "probe-worker.txt",
        ] {
            std::fs::copy(source.join(file), health_dir.join(file)).unwrap();
        }
        let worker = std::fs::read_to_string(source.join("Dockerfile.worker")).unwrap();
        std::fs::write(
            health_dir.join("Dockerfile.worker"),
            worker
                .lines()
                .filter(|line| !line.starts_with("HEALTHCHECK "))
                .collect::<Vec<_>>()
                .join("\n")
                + "\n",
        )
        .unwrap();
        std::fs::write(
            health_dir.join("devingpanel.yaml"),
            yaml.replace(
                "name: stack-v2-success",
                &format!("name: stack-v2-health-fail-{}", uuid::Uuid::new_v4().simple()),
            )
            .replace(
                "    - group: test\n      host: vps-smoke",
                "    - group: test\n      host: vps-smoke\n    - group: test\n      host: vps-second",
            ),
        )
        .unwrap();
        let health_project = crate::project_store::create_from_manifest(&health_dir).unwrap();
        assert_eq!(health_project.host_ids, [1, 2]);
        let health_tag = format!("lanhealthfail{}", uuid::Uuid::new_v4().simple());
        let health_preflight = preflight_batch(
            health_project.id,
            &health_tag,
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
        let health_error = super::run_batch_sequential(
            health_project.id,
            &health_preflight,
            Arc::new(AtomicBool::new(false)),
            |_| {},
        )
        .err()
        .expect("missing worker healthcheck must stop the batch");
        assert!(
            health_error.message.contains("缺少有效 HEALTHCHECK"),
            "{}",
            health_error.message
        );
        let health_batch_id = health_error.batch_id.expect("batch intent must exist");
        let health_batch = crate::batch_store::get(&health_batch_id).unwrap();
        assert_eq!(health_batch.state, "terminated");
        assert!(
            health_batch
                .targets
                .iter()
                .all(|target| { target.state == "waiting" && target.release_task_id.is_none() })
        );
        assert_eq!(
            [count("release_tasks"), count("release_uploaded_archives")],
            before_db
        );
        assert!(
            !appdata
                .join("devingPanel")
                .join("batch-archives")
                .join(&health_batch_id)
                .exists()
        );
        assert_eq!(remote(), before_remote);
        for build in &health_preflight.plan.builds {
            assert!(
                std::process::Command::new("docker")
                    .args(["image", "rm", &build.reference])
                    .status()
                    .unwrap()
                    .success(),
                "could not remove isolated health-failure image"
            );
        }
        eprintln!(
            "LAN local failures stopped before upload: build={}/{} health={}/{}",
            project.id, batch_id, health_project.id, health_batch_id
        );
    }

    #[test]
    #[ignore = "builds and removes four uniquely tagged local amd64/arm64 images; requires an isolated APPDATA and Docker Buildx"]
    fn isolated_mixed_arch_matrix_builds_and_exports_each_image_once() {
        let root = std::path::PathBuf::from(std::env::var_os("APPDATA").unwrap())
            .canonicalize()
            .unwrap();
        assert_eq!(
            root.parent().unwrap(),
            std::env::current_dir()
                .unwrap()
                .join("target/isolated-tests")
                .canonicalize()
                .unwrap()
        );
        assert!(
            root.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("batch-mixed-")
        );
        assert!(std::fs::read_dir(&root).unwrap().next().is_none());
        for service in ["web", "worker"] {
            std::fs::write(root.join(format!("Dockerfile.{service}")),
                format!("FROM scratch\nCOPY {service}.txt /probe.txt\nHEALTHCHECK CMD [\"/probe.txt\"]\n"))
                .unwrap();
            std::fs::write(root.join(format!("{service}.txt")), service).unwrap();
        }
        let builds = ["web", "worker"].map(|service| BuildSpec {
            service: service.into(),
            dockerfile: format!("Dockerfile.{service}"),
            image: format!("devingpanel-mixed-{service}"),
            platform: "auto".into(),
            args: vec![],
        });
        let suffix = uuid::Uuid::new_v4().simple().to_string();
        let targets: [(i64, String); 2] = [(11, "linux/amd64".into()), (12, "linux/arm64".into())];
        let planned = builds
            .iter()
            .flat_map(|spec| {
                targets
                    .iter()
                    .map(|(_, platform)| crate::release::PlannedImage {
                        platform: platform.clone(),
                        reference: format!(
                            "{}:{suffix}-{}",
                            spec.image,
                            platform.split('/').nth(1).unwrap()
                        ),
                    })
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        let plan = batch_plan_from(&builds, &[11, 12], &targets, &planned).unwrap();
        assert_eq!(plan.builds.len(), 4);
        assert_eq!(
            plan.hosts
                .iter()
                .map(|host| host.images.len())
                .collect::<Vec<_>>(),
            [2, 2]
        );
        let containers = || {
            let output = std::process::Command::new("docker")
                .args(["container", "ls", "-a", "--format", "{{.ID}}"])
                .output()
                .unwrap();
            assert!(output.status.success());
            output.stdout
        };
        let before = containers();
        let project = Project {
            id: 0,
            name: "isolated-mixed".into(),
            path: root.to_string_lossy().into_owned(),
            dockerfile: String::new(),
            image: String::new(),
            tag_rule: "{timestamp}".into(),
            platform: "auto".into(),
            service: String::new(),
            compose_files: vec![],
            missing_compose_files: vec![],
            build_args: vec![],
            host_ids: vec![],
            manifest_mode: false,
            config_error: None,
        };
        let result = (|| -> Result<(), String> {
            let mut seen = std::collections::HashSet::new();
            for build in &plan.builds {
                assert!(seen.insert((build.service.clone(), build.platform.clone())));
                let spec = spec_for_batch_build(&builds, build)?;
                let image = crate::release::PlannedImage {
                    platform: build.platform.clone(),
                    reference: build.reference.clone(),
                };
                let built = crate::build::build_image(
                    &super::build_project(&project, spec),
                    &image,
                    Arc::new(AtomicBool::new(false)),
                    |_| {},
                )
                .map_err(|err| err.message)?;
                if !built.has_healthcheck {
                    return Err("built image missing healthcheck".into());
                }
                let archive = crate::build::save_image(&built, Arc::new(AtomicBool::new(false)))?;
                if archive.bytes == 0
                    || crate::build::archive_image_ids(
                        &archive.path,
                        &build.reference,
                        &build.platform,
                    )? != archive.image_ids
                {
                    return Err("saved archive platform identity mismatch".into());
                }
            }
            assert_eq!(seen.len(), 4);
            Ok(())
        })();
        let endpoint = crate::docker::local_engine_endpoint().unwrap();
        for build in &plan.builds {
            let mut inspect = crate::docker::command_for_endpoint(&endpoint);
            if inspect
                .args(["image", "inspect", &build.reference])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .unwrap()
                .success()
            {
                let mut remove = crate::docker::command_for_endpoint(&endpoint);
                assert!(
                    remove
                        .args(["image", "rm", &build.reference])
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .status()
                        .unwrap()
                        .success()
                );
            }
        }
        assert!(
            result.is_ok(),
            "mixed-platform build/export failed: {result:?}"
        );
        assert_eq!(containers(), before);
    }

    #[test]
    #[ignore = "builds four mixed-platform images into a fresh isolated SQLite batch and retained archives"]
    fn isolated_mixed_arch_batch_reaches_ready_with_verified_shared_archives() {
        let root = std::path::PathBuf::from(std::env::var_os("APPDATA").unwrap())
            .canonicalize()
            .unwrap();
        assert_eq!(
            root.parent().unwrap(),
            std::env::current_dir()
                .unwrap()
                .join("target/isolated-tests")
                .canonicalize()
                .unwrap()
        );
        assert!(
            root.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("batch-ready-")
        );
        assert!(std::fs::read_dir(&root).unwrap().next().is_none());
        let source = root.join("project");
        std::fs::create_dir(&source).unwrap();
        for service in ["web", "worker"] {
            std::fs::write(source.join(format!("Dockerfile.{service}")),
                format!("FROM scratch\nCOPY {service}.txt /probe.txt\nHEALTHCHECK CMD [\"/probe.txt\"]\n"))
                .unwrap();
            std::fs::write(source.join(format!("{service}.txt")), service).unwrap();
        }
        std::fs::write(source.join("compose.yaml"),
            "services:\n  web:\n    image: ${DEVINGPANEL_IMAGE_WEB}\n  worker:\n    image: ${DEVINGPANEL_IMAGE_WORKER}\n").unwrap();
        std::fs::write(source.join("devingpanel.yaml"), concat!(
            "version: 2\nname: mixed-ready\ntag: '{timestamp}'\nbuilds:\n",
            "  - service: web\n    dockerfile: Dockerfile.web\n    image: devingpanel-mixed-web\n    platform: auto\n    args: {}\n",
            "  - service: worker\n    dockerfile: Dockerfile.worker\n    image: devingpanel-mixed-worker\n    platform: auto\n    args: {}\n",
            "deploy:\n  compose:\n    files: [compose.yaml]\n  targets:\n",
            "    - group: test\n      host: fake-amd64\n    - group: test\n      host: fake-arm64\n",
        )).unwrap();
        let conn = crate::database::connect().unwrap();
        conn.execute_batch("INSERT INTO hosts (id,name,address,port,username,auth,group_name,credential_id,trusted_fingerprint)
            VALUES (11,'fake-amd64','invalid.example',22,'root','password','test','fake-11','SHA256:fake'),
                   (12,'fake-arm64','invalid.example',22,'root','password','test','fake-12','SHA256:fake');").unwrap();
        let project = crate::project_store::create_from_manifest(&source).unwrap();
        assert_eq!(project.host_ids, [11, 12]);
        let manifest = project_config::load(&source).unwrap();
        let targets = [(11, "linux/amd64".into()), (12, "linux/arm64".into())];
        let tag = format!("mixed{}", uuid::Uuid::new_v4().simple());
        let (hosts, _) = crate::host_store::list().unwrap();
        let plan = plan_batch_from_manifest(&project, &manifest, &hosts, &tag, &targets).unwrap();
        let env_files = plan
            .hosts
            .iter()
            .map(|host| batch_env_file(b"", &host.images).unwrap())
            .collect::<Vec<_>>();
        let digest = crate::release::source_digest(&project).unwrap();
        let snapshot = BatchPreflight {
            plan,
            config_sha256: digest.clone(),
            builds: manifest.builds,
            compose_files: crate::release::deployment_files(&project).unwrap(),
            env_files,
            targets: targets
                .iter()
                .map(|(id, platform)| BatchTargetIdentity {
                    host_id: *id,
                    host: hosts.iter().find(|host| host.id == *id).unwrap().clone(),
                    platform: platform.clone(),
                    docker_endpoint: "unix:///var/run/docker.sock".into(),
                    docker_daemon_id: format!("FAKE:{id}"),
                })
                .collect(),
        };
        let batch_id = uuid::Uuid::new_v4().to_string();
        conn.execute(
            "INSERT INTO release_batches (id,project_id,config_sha256,state)
            VALUES (?1,?2,?3,'building')",
            rusqlite::params![batch_id, project.id, digest],
        )
        .unwrap();
        for build in &snapshot.plan.builds {
            conn.execute(
                "INSERT INTO release_batch_images (batch_id,service,platform,reference)
                VALUES (?1,?2,?3,?4)",
                rusqlite::params![batch_id, build.service, build.platform, build.reference],
            )
            .unwrap();
        }
        for (ordinal, target) in snapshot.targets.iter().enumerate() {
            conn.execute("INSERT INTO release_batch_targets (batch_id,ordinal,host_id,host_name,host_group,
                host_address,host_port,host_username,host_auth,host_fingerprint,platform,docker_endpoint,
                docker_daemon_id,env_sha256,state) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,'waiting')",
                rusqlite::params![batch_id, ordinal as i64, target.host_id, target.host.name,
                    target.host.group, target.host.address, target.host.port, target.host.username,
                    target.host.auth, target.host.trusted_fingerprint, target.platform,
                    target.docker_endpoint, target.docker_daemon_id,
                    super::env_digest(&snapshot.env_files[ordinal])]).unwrap();
        }
        let containers = || {
            let output = std::process::Command::new("docker")
                .args(["container", "ls", "-a", "--format", "{{.ID}}"])
                .output()
                .unwrap();
            assert!(output.status.success());
            output.stdout
        };
        let before = containers();
        let mut events = Vec::new();
        let result = build_shared_images_local(
            &project,
            &batch_id,
            &snapshot,
            Arc::new(AtomicBool::new(false)),
            &mut |event| events.push(event.to_owned()),
        );
        let endpoint = crate::docker::local_engine_endpoint().unwrap();
        for build in &snapshot.plan.builds {
            let mut inspect = crate::docker::command_for_endpoint(&endpoint);
            if inspect
                .args(["image", "inspect", &build.reference])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .unwrap()
                .success()
            {
                let mut remove = crate::docker::command_for_endpoint(&endpoint);
                assert!(
                    remove
                        .args(["image", "rm", &build.reference])
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .status()
                        .unwrap()
                        .success()
                );
            }
        }
        let built = result.unwrap();
        assert_eq!(built.len(), 4);
        assert_eq!(
            events
                .iter()
                .filter(|event| event.starts_with("构建镜像："))
                .count(),
            4
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| event.starts_with("核对构建后 Compose"))
                .count(),
            2
        );
        let saved = crate::batch_store::get(&batch_id).unwrap();
        assert_eq!(saved.state, "ready");
        assert_eq!(saved.images.len(), 4);
        assert!(
            saved
                .targets
                .iter()
                .all(|target| target.state == "waiting" && target.release_task_id.is_none())
        );
        for host in &snapshot.plan.hosts {
            for (service, reference) in &host.images {
                let archive = crate::release_archive::copy_verified_shared(
                    &batch_id,
                    service,
                    &host.platform,
                )
                .unwrap();
                assert_eq!(
                    crate::build::archive_image_ids(&archive.path, reference, &host.platform)
                        .unwrap(),
                    archive.image_ids
                );
            }
        }
        assert_eq!(containers(), before);
        assert!(
            crate::release_store::recent(Some(project.id))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    #[ignore = "read-only environment, source, and platform checks against two LAN VPS hosts; requires fresh isolated APPDATA and DEVINGPANEL_TEST_LAN_PASSWORD"]
    fn isolated_lan_batch_preflight_rejects_drift_without_side_effects() {
        let appdata = std::path::PathBuf::from(std::env::var_os("APPDATA").unwrap())
            .canonicalize()
            .unwrap();
        let root = appdata.parent().unwrap();
        assert!(
            root.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("lan-vps-preflight-")
        );
        assert_eq!(
            root.parent().unwrap(),
            std::env::current_dir()
                .unwrap()
                .join("target/isolated-tests")
                .canonicalize()
                .unwrap()
        );
        assert!(std::fs::read_dir(&appdata).unwrap().next().is_none());
        let password = std::env::var("DEVINGPANEL_TEST_LAN_PASSWORD")
            .expect("isolated LAN test password is required");

        struct Cleanup {
            project_id: Option<i64>,
            host_ids: Vec<i64>,
        }
        impl Cleanup {
            fn finish(&mut self) -> Result<(), String> {
                if let Some(id) = self.project_id {
                    crate::project_store::delete(id)?;
                    self.project_id = None;
                }
                while let Some(&id) = self.host_ids.last() {
                    crate::host_store::delete(id)?;
                    self.host_ids.pop();
                }
                Ok(())
            }
        }
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = self.finish();
            }
        }
        let mut cleanup = Cleanup {
            project_id: None,
            host_ids: Vec::new(),
        };
        let fingerprint = "SHA256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
        for (name, address) in [("vps-smoke", "192.0.2.129"), ("vps-second", "192.0.2.130")] {
            let host = crate::host_store::create(crate::host_store::NewHost {
                name: name.into(),
                address: address.into(),
                port: "22".into(),
                username: "root".into(),
                group: "test".into(),
                auth: "password".into(),
                credential: password.clone(),
            })
            .unwrap();
            cleanup.host_ids.push(host.id);
            crate::host_store::trust_fingerprint(host.id, address, 22, None, fingerprint).unwrap();
        }
        drop(password);

        let project_dir = root.join("project");
        std::fs::create_dir(&project_dir).unwrap();
        let source = std::env::current_dir()
            .unwrap()
            .join("tests/fixtures/stack-v2-success");
        for file in [
            "compose.yaml",
            "Dockerfile.web",
            "Dockerfile.worker",
            "probe-web.txt",
            "probe-worker.txt",
        ] {
            std::fs::copy(source.join(file), project_dir.join(file)).unwrap();
        }
        let yaml = std::fs::read_to_string(source.join("devingpanel.yaml")).unwrap();
        std::fs::write(
            project_dir.join("devingpanel.yaml"),
            yaml.replace(
                "    - group: test\n      host: vps-smoke",
                "    - group: test\n      host: vps-smoke\n    - group: test\n      host: vps-second",
            ),
        )
        .unwrap();
        let project = crate::project_store::create_from_manifest(&project_dir).unwrap();
        cleanup.project_id = Some(project.id);
        assert_eq!(project.host_ids.len(), 2);

        let conn = crate::database::connect().unwrap();
        let counts = || {
            [
                "release_tasks",
                "release_batches",
                "release_uploaded_archives",
            ]
            .map(|table| {
                conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                    row.get::<_, i64>(0)
                })
                .unwrap()
            })
        };
        let local_images = || {
            let output = std::process::Command::new("docker")
                .args(["image", "ls", "--format", "{{.ID}}"])
                .output()
                .unwrap();
            assert!(output.status.success());
            let mut ids = String::from_utf8(output.stdout)
                .unwrap()
                .lines()
                .map(str::to_owned)
                .collect::<Vec<_>>();
            ids.sort();
            ids
        };
        let remote = || {
            project
                .host_ids
                .iter()
                .map(|id| {
                    let inventory = crate::ssh::inventory(*id).unwrap();
                    let mut containers = inventory
                        .containers
                        .iter()
                        .map(|container| container.id.clone())
                        .collect::<Vec<_>>();
                    let mut images = inventory
                        .images
                        .iter()
                        .map(|image| image.reference.clone())
                        .collect::<Vec<_>>();
                    containers.sort();
                    images.sort();
                    (containers, images)
                })
                .collect::<Vec<_>>()
        };
        let before_db = counts();
        let before_local = local_images();
        let before_remote = remote();
        let daemons = project
            .host_ids
            .iter()
            .map(|id| {
                let host = crate::host_store::list()
                    .unwrap()
                    .0
                    .into_iter()
                    .find(|host| host.id == *id)
                    .unwrap();
                crate::ssh::docker_identity_for_host(host).unwrap().1
            })
            .collect::<Vec<_>>();
        assert_ne!(daemons[0], daemons[1]);

        let tag = format!("lanpreflight{}", uuid::Uuid::new_v4().simple());
        let mut preflight =
            preflight_batch(project.id, &tag, Arc::new(AtomicBool::new(false))).unwrap();
        assert_eq!(preflight.targets.len(), 2);
        assert_ne!(
            preflight.targets[0].docker_daemon_id,
            preflight.targets[1].docker_daemon_id
        );
        assert_eq!(
            preflight
                .targets
                .iter()
                .map(|target| target.docker_daemon_id.as_str())
                .collect::<Vec<_>>(),
            daemons.iter().map(String::as_str).collect::<Vec<_>>()
        );
        let frozen_platform =
            std::mem::replace(&mut preflight.targets[1].platform, "linux/arm64".into());
        let error =
            super::revalidate_batch(project.id, &preflight, Arc::new(AtomicBool::new(false)))
                .unwrap_err();
        assert!(
            error.contains("目标架构或 Docker daemon 身份已变化"),
            "{error}"
        );
        preflight.targets[1].platform = frozen_platform;
        assert_eq!(counts(), before_db);
        assert_eq!(local_images(), before_local);
        assert_eq!(remote(), before_remote);
        let second = project.host_ids[1];
        crate::env_store::save(
            project.id,
            Some(second),
            vec![crate::env_store::EnvInput {
                name: "PREFLIGHT_DRIFT".into(),
                value: Some("changed".into()),
                credential_id: None,
                sensitive: false,
            }],
        )
        .unwrap();
        let error =
            super::revalidate_batch(project.id, &preflight, Arc::new(AtomicBool::new(false)))
                .unwrap_err();
        assert!(error.contains("环境变量在预检后变化"), "{error}");
        assert_eq!(counts(), before_db);
        assert_eq!(local_images(), before_local);
        assert_eq!(remote(), before_remote);
        crate::env_store::save(project.id, Some(second), vec![]).unwrap();
        super::revalidate_batch(project.id, &preflight, Arc::new(AtomicBool::new(false))).unwrap();
        let dockerfile = project_dir.join("Dockerfile.worker");
        let mut bytes = std::fs::read(&dockerfile).unwrap();
        bytes.extend_from_slice(b"\n# isolated drift injection\n");
        std::fs::write(&dockerfile, bytes).unwrap();
        assert!(
            super::revalidate_batch(project.id, &preflight, Arc::new(AtomicBool::new(false)))
                .is_err()
        );
        assert_eq!(counts(), before_db);
        assert_eq!(local_images(), before_local);
        assert_eq!(remote(), before_remote);
        std::fs::copy(source.join("Dockerfile.worker"), &dockerfile).unwrap();
        let manifest_path = project_dir.join("devingpanel.yaml");
        let manifest = std::fs::read_to_string(&manifest_path).unwrap();
        assert_eq!(manifest.matches("platform: linux/amd64").count(), 2);
        std::fs::write(
            &manifest_path,
            manifest.replace("platform: linux/amd64", "platform: linux/arm64"),
        )
        .unwrap();
        let error = preflight_batch(project.id, &tag, Arc::new(AtomicBool::new(false)))
            .err()
            .expect("target architecture mismatch must stop preflight");
        assert!(error.contains("与项目目标平台"), "{error}");
        assert_eq!(counts(), before_db);
        assert_eq!(local_images(), before_local);
        assert_eq!(remote(), before_remote);
        std::fs::write(manifest_path, manifest).unwrap();
        assert!(!appdata.join("devingPanel").join("batch-archives").exists());
        cleanup.finish().unwrap();
    }

    #[test]
    #[ignore = "one-shot manual confirmation and continuation of an audited isolated LAN batch"]
    fn isolated_lan_batch_confirms_first_then_runs_second() {
        let appdata = std::path::PathBuf::from(std::env::var_os("APPDATA").unwrap())
            .canonicalize()
            .unwrap();
        let root = appdata.parent().unwrap();
        assert!(
            root.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("ui-lan-batch-")
        );
        assert_eq!(
            root.parent().unwrap(),
            std::env::current_dir()
                .unwrap()
                .join("target/isolated-tests")
                .canonicalize()
                .unwrap()
        );
        let id = std::env::var("DEVINGPANEL_TEST_LAN_BATCH_ID").expect("audited batch ID required");
        let batch = crate::batch_store::get(&id).unwrap();
        assert_eq!(batch.project_id, 1856624456);
        assert_eq!(batch.state, "running");
        assert_eq!(batch.targets.len(), 2);
        assert_eq!(batch.targets[0].state, "running");
        assert_eq!(batch.targets[1].state, "waiting");
        assert!(batch.targets[1].release_task_id.is_none());
        assert_eq!(batch.targets[0].host.address, "192.0.2.129");
        assert_eq!(batch.targets[1].host.address, "192.0.2.130");
        assert_eq!(
            batch.targets[0].docker_daemon_id,
            "00000000-0000-4000-8000-000000000008"
        );
        assert_eq!(
            batch.targets[1].docker_daemon_id,
            "00000000-0000-4000-8000-000000000011"
        );
        for target in &batch.targets {
            assert_eq!(
                crate::ssh::docker_identity_for_host(target.host.clone())
                    .unwrap()
                    .1,
                target.docker_daemon_id
            );
        }
        let project = crate::project_store::effective(batch.project_id).unwrap();
        assert!(
            std::path::Path::new(&project.path)
                .canonicalize()
                .unwrap()
                .starts_with(root)
        );
        let first_id = batch.targets[0].release_task_id.as_ref().unwrap();
        let first = crate::release_store::get_task(first_id).unwrap();
        assert_eq!(
            (
                first.phase.as_str(),
                first.outcome.as_str(),
                first.health_result.as_deref()
            ),
            ("checked", "blocked", Some("healthy"))
        );
        let observed = crate::ssh::observe_release_project(&first).unwrap();
        assert_eq!(observed.containers.len(), 4);
        assert!(observed.containers.iter().all(|container| {
            container.status == "running" && container.health.as_deref() == Some("healthy")
        }));
        let mut first_before = crate::ssh::inventory(batch.targets[0].host.id)
            .unwrap()
            .containers
            .iter()
            .map(|container| container.id.clone())
            .collect::<Vec<_>>();
        first_before.sort();
        assert_eq!(first_before.len(), 40);
        let mut second_before = crate::ssh::inventory(batch.targets[1].host.id)
            .unwrap()
            .containers
            .iter()
            .map(|container| container.id.clone())
            .collect::<Vec<_>>();
        second_before.sort();
        assert_eq!(second_before.len(), 28);
        let mut second_probe = first.clone();
        second_probe.host = batch.targets[1].host.clone();
        second_probe.docker_daemon_id = Some(batch.targets[1].docker_daemon_id.clone());
        second_probe.docker_endpoint = Some(batch.targets[1].docker_endpoint.clone());
        crate::ssh::assert_empty_project_namespace(&second_probe).unwrap();

        assert_eq!(
            crate::release::confirm_healthy_and_complete(first_id).unwrap(),
            4
        );
        let batch = crate::batch_store::get(&id).unwrap();
        assert_eq!(batch.state, "running");
        assert_eq!(batch.targets[0].state, "healthy");
        assert_eq!(batch.targets[1].state, "waiting");
        let conn = crate::database::connect().unwrap();
        let pointers = || {
            conn.prepare(
                "SELECT docker_daemon_id, task_id FROM project_current_version
                 WHERE project_id = ?1 ORDER BY docker_daemon_id",
            )
            .unwrap()
            .query_map([batch.project_id], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
        };
        assert_eq!(pointers().len(), 1);
        assert_eq!(pointers()[0].1, *first_id);

        let cancel = Arc::new(AtomicBool::new(false));
        let confirmed = super::reconfirm_batch(&id, cancel.clone()).unwrap();
        let result = super::continue_batch_sequential(
            &id,
            Some(batch.targets[1].host.id),
            &confirmed,
            cancel,
            |_| {},
        )
        .unwrap_or_else(|error| panic!("batch {}: {}", id, error.message));
        let second_id = match result {
            super::BatchProgress::AwaitingConfirmation { task_id, .. } => task_id,
            super::BatchProgress::Complete { .. } => panic!("second host skipped confirmation"),
        };
        let batch = crate::batch_store::get(&id).unwrap();
        assert_eq!(batch.targets[0].state, "healthy");
        assert_eq!(batch.targets[1].state, "running");
        let second = crate::release_store::get_task(&second_id).unwrap();
        assert_eq!(
            (
                second.phase.as_str(),
                second.outcome.as_str(),
                second.health_result.as_deref()
            ),
            ("checked", "blocked", Some("healthy"))
        );
        let mut first_after = crate::ssh::inventory(batch.targets[0].host.id)
            .unwrap()
            .containers
            .iter()
            .map(|container| container.id.clone())
            .collect::<Vec<_>>();
        first_after.sort();
        assert_eq!(first_after, first_before);
        let second_after = crate::ssh::inventory(batch.targets[1].host.id)
            .unwrap()
            .containers
            .iter()
            .map(|container| container.id.clone())
            .collect::<Vec<_>>();
        assert_eq!(second_after.len(), second_before.len() + 4);
        assert!(second_before.iter().all(|id| second_after.contains(id)));
        assert_eq!(pointers().len(), 1);
        assert_eq!(
            crate::release::confirm_healthy_and_complete(&second_id).unwrap(),
            4
        );
        let batch = crate::batch_store::get(&id).unwrap();
        assert_eq!(batch.state, "complete");
        assert!(batch.targets.iter().all(|target| target.state == "healthy"));
        assert_eq!(pointers().len(), 2);
        assert_eq!(
            crate::release_store::uploaded_archives(&second_id)
                .unwrap()
                .iter()
                .filter(|archive| archive.state == "cleaned")
                .count(),
            2
        );
        eprintln!(
            "LAN batch complete: project={}, batch={}, tasks={},{}",
            project.id, id, first_id, second_id
        );
    }

    #[test]
    #[ignore = "one-shot parallel success, health failure or upload cut on two trusted LAN VPS hosts; requires fresh parallel-lan APPDATA and test password"]
    fn isolated_lan_parallel_wave_confirms_both_hosts() {
        let fail_second =
            std::env::var("DEVINGPANEL_TEST_PARALLEL_FAIL_SECOND").as_deref() == Ok("1");
        let cut_second =
            std::env::var("DEVINGPANEL_TEST_PARALLEL_CUT_SECOND").as_deref() == Ok("1");
        assert!(!(fail_second && cut_second));
        let proxy = cut_second.then(|| CutOnceSshProxy::new("192.0.2.130:22"));
        let appdata = std::path::PathBuf::from(std::env::var_os("APPDATA").unwrap())
            .canonicalize()
            .unwrap();
        let root = appdata.parent().unwrap();
        assert!(
            root.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("parallel-lan-")
        );
        assert_eq!(
            root.parent().unwrap(),
            std::env::current_dir()
                .unwrap()
                .join("target/isolated-tests")
                .canonicalize()
                .unwrap()
        );
        assert!(std::fs::read_dir(&appdata).unwrap().next().is_none());
        let password = std::env::var("DEVINGPANEL_TEST_LAN_PASSWORD")
            .expect("isolated LAN test password is required");
        let fingerprint = "SHA256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
        let mut host_ids = Vec::new();
        for (name, address, port) in [
            ("vps-smoke", "192.0.2.129", 22),
            (
                "vps-second",
                if cut_second {
                    "127.0.0.1"
                } else {
                    "192.0.2.130"
                },
                proxy.as_ref().map_or(22, |proxy| proxy.port),
            ),
        ] {
            let host = crate::host_store::create(crate::host_store::NewHost {
                name: name.into(),
                address: address.into(),
                port: port.to_string(),
                username: "root".into(),
                group: "test".into(),
                auth: "password".into(),
                credential: password.clone(),
            })
            .unwrap();
            crate::host_store::trust_fingerprint(host.id, address, port, None, fingerprint)
                .unwrap();
            host_ids.push(host.id);
        }
        drop(password);
        let source = std::env::current_dir()
            .unwrap()
            .join("tests/fixtures/stack-v2-success");
        let project_dir = root.join("project");
        std::fs::create_dir(&project_dir).unwrap();
        for file in [
            "compose.yaml",
            "Dockerfile.web",
            "Dockerfile.worker",
            "probe-web.txt",
            "probe-worker.txt",
        ] {
            let source_file = if fail_second && file == "compose.yaml" {
                std::env::current_dir()
                    .unwrap()
                    .join("tests/fixtures/stack-v2-host-health/compose.yaml")
            } else {
                source.join(file)
            };
            std::fs::copy(source_file, project_dir.join(file)).unwrap();
        }
        let yaml = std::fs::read_to_string(source.join("devingpanel.yaml")).unwrap();
        std::fs::write(project_dir.join("devingpanel.yaml"), yaml
            .replace("name: stack-v2-success",
                &format!("name: stack-v2-parallel-{}", uuid::Uuid::new_v4().simple()))
            .replace("    - group: test\n      host: vps-smoke",
                "    - group: test\n      host: vps-smoke\n    - group: test\n      host: vps-second"),
        ).unwrap();
        let reservation_id = chrono::Utc::now().timestamp_millis();
        crate::database::connect()
            .unwrap()
            .execute(
                "INSERT INTO projects (id, name, path, dockerfile, image)
             VALUES (?1, 'parallel-id-reservation', '.', 'Dockerfile', 'reservation')",
                [reservation_id],
            )
            .unwrap();
        let project = crate::project_store::create_from_manifest(&project_dir).unwrap();
        assert!(project.id > 1_000_000_000);
        assert_eq!(project.host_ids, host_ids);
        if fail_second {
            for (host_id, command) in [
                (host_ids[0], "test -s /probe-worker.txt"),
                (host_ids[1], "false"),
            ] {
                let (_, warning) = crate::env_store::save(
                    project.id,
                    Some(host_id),
                    vec![crate::env_store::EnvInput {
                        name: "WORKER_HEALTH_COMMAND".into(),
                        value: Some(command.into()),
                        credential_id: None,
                        sensitive: false,
                    }],
                )
                .unwrap();
                assert!(warning.is_none());
            }
        }
        let tag = format!("parallel{}", uuid::Uuid::new_v4().simple());
        let cancel = Arc::new(AtomicBool::new(false));
        let plan = preflight_batch(project.id, &tag, cancel.clone()).unwrap();
        assert_eq!(plan.targets.len(), 2);
        assert_ne!(
            plan.targets[0].docker_daemon_id,
            plan.targets[1].docker_daemon_id
        );
        assert_eq!(
            plan.targets[0].docker_daemon_id,
            "00000000-0000-4000-8000-000000000008"
        );
        assert_eq!(
            plan.targets[1].docker_daemon_id,
            "00000000-0000-4000-8000-000000000011"
        );
        let mut baselines = Vec::new();
        for target in &plan.targets {
            let probe = crate::release_store::TaskSnapshot {
                id: "namespace-probe".into(),
                project_id: project.id,
                project_name: project.name.clone(),
                host: target.host.clone(),
                docker_endpoint: Some(target.docker_endpoint.clone()),
                docker_daemon_id: Some(target.docker_daemon_id.clone()),
                phase: "prepared".into(),
                outcome: "running".into(),
                health_result: None,
                detail: String::new(),
                started_at: String::new(),
                execution_finished_at: None,
                image_reference: plan.plan.builds[0].reference.clone(),
            };
            crate::ssh::assert_empty_project_namespace(&probe).unwrap();
            let inventory = crate::ssh::inventory(target.host.id).unwrap();
            for image in ["redis:7-alpine", "postgres:16-alpine"] {
                assert!(inventory.images.iter().any(|item| item.reference == image));
            }
            let mut ids = inventory
                .containers
                .iter()
                .map(|item| item.id.clone())
                .collect::<Vec<_>>();
            ids.sort();
            baselines.push(ids);
        }
        if let Some(proxy) = &proxy {
            proxy
                .enabled
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }
        let result = super::run_batch_parallel(project.id, &plan, 2, cancel, |_| {});
        if let Some(proxy) = &proxy {
            let error = result
                .err()
                .expect("second host upload must become unknown");
            let batch_id = error.batch_id.unwrap();
            assert!(proxy.cut.load(std::sync::atomic::Ordering::Relaxed));
            let batch = crate::batch_store::get(&batch_id).unwrap();
            assert_eq!(batch.state, "unknown");
            assert_eq!(batch.parallelism, 2);
            assert_eq!(batch.targets[0].state, "running");
            assert_eq!(batch.targets[1].state, "unknown");
            let first =
                crate::release_store::get_task(batch.targets[0].release_task_id.as_ref().unwrap())
                    .unwrap();
            let second =
                crate::release_store::get_task(batch.targets[1].release_task_id.as_ref().unwrap())
                    .unwrap();
            assert_eq!(
                (
                    first.phase.as_str(),
                    first.outcome.as_str(),
                    first.health_result.as_deref()
                ),
                ("checked", "blocked", Some("healthy"))
            );
            assert_eq!(
                (second.phase.as_str(), second.outcome.as_str()),
                ("uploading", "unknown")
            );
            let conn = crate::database::connect().unwrap();
            let pointers: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM project_current_version WHERE project_id = ?1",
                    [project.id],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(pointers, 0);
            assert!(proxy.offline.load(std::sync::atomic::Ordering::Relaxed));
            proxy
                .offline
                .store(false, std::sync::atomic::Ordering::Relaxed);
            for (index, target) in batch.targets.iter().enumerate() {
                let mut ids = crate::ssh::inventory(target.host.id)
                    .unwrap()
                    .containers
                    .iter()
                    .map(|item| item.id.clone())
                    .collect::<Vec<_>>();
                ids.sort();
                assert_eq!(
                    ids.len(),
                    baselines[index].len() + if index == 0 { 4 } else { 0 }
                );
                assert!(baselines[index].iter().all(|id| ids.contains(id)));
            }
            eprintln!(
                "parallel LAN upload cut retained healthy sibling: project={}, batch={}",
                project.id, batch_id
            );
            return;
        }
        let batch_id = if fail_second {
            result
                .err()
                .expect("second host must fail health")
                .batch_id
                .unwrap()
        } else {
            match result.unwrap_or_else(|error| {
                panic!("parallel batch {:?}: {}", error.batch_id, error.message)
            }) {
                super::BatchProgress::AwaitingConfirmation { batch_id, .. } => batch_id,
                super::BatchProgress::Complete { .. } => panic!("wave skipped health confirmation"),
            }
        };
        let batch = crate::batch_store::get(&batch_id).unwrap();
        assert_eq!(batch.parallelism, 2);
        assert_eq!(batch.state, if fail_second { "paused" } else { "running" });
        for (index, target) in batch.targets.iter().enumerate() {
            assert_eq!(
                target.state,
                if fail_second && index == 1 {
                    "failed"
                } else {
                    "running"
                }
            );
            let task_id = target.release_task_id.as_ref().unwrap();
            let task = crate::release_store::get_task(task_id).unwrap();
            assert_eq!(
                (
                    task.phase.as_str(),
                    task.outcome.as_str(),
                    task.health_result.as_deref()
                ),
                (
                    "checked",
                    "blocked",
                    Some(if fail_second && index == 1 {
                        "failed"
                    } else {
                        "healthy"
                    })
                )
            );
            let mut after = crate::ssh::inventory(target.host.id)
                .unwrap()
                .containers
                .iter()
                .map(|item| item.id.clone())
                .collect::<Vec<_>>();
            after.sort();
            assert_eq!(after.len(), baselines[index].len() + 4);
            assert!(baselines[index].iter().all(|id| after.contains(id)));
        }
        let conn = crate::database::connect().unwrap();
        let pointers: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM project_current_version WHERE project_id = ?1",
                [project.id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(pointers, 0);
        if fail_second {
            assert_eq!(
                crate::release::confirm_healthy_and_complete(
                    batch.targets[0].release_task_id.as_ref().unwrap()
                )
                .unwrap(),
                4
            );
            let paused = crate::batch_store::get(&batch_id).unwrap();
            assert_eq!(paused.state, "paused");
            assert_eq!(paused.targets[0].state, "healthy");
            assert_eq!(paused.targets[1].state, "failed");
            let pointers: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM project_current_version WHERE project_id = ?1",
                    [project.id],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(pointers, 1);
            eprintln!(
                "parallel LAN health failure retained: project={}, batch={}",
                project.id, batch_id
            );
            return;
        }
        for target in &batch.targets {
            assert_eq!(
                crate::release::confirm_healthy_and_complete(
                    target.release_task_id.as_ref().unwrap()
                )
                .unwrap(),
                4
            );
        }
        let finished = crate::batch_store::get(&batch_id).unwrap();
        assert_eq!(finished.state, "complete");
        assert!(
            finished
                .targets
                .iter()
                .all(|target| target.state == "healthy")
        );
        eprintln!(
            "parallel LAN batch complete: project={}, batch={}",
            project.id, batch_id
        );
    }

    #[test]
    #[ignore = "one-shot amd64 plus QEMU arm64 release; requires fresh mixed-qemu APPDATA, LAN password and ARM64 private key path"]
    fn isolated_mixed_qemu_parallel_release() {
        let appdata = std::path::PathBuf::from(std::env::var_os("APPDATA").unwrap())
            .canonicalize()
            .unwrap();
        let root = appdata.parent().unwrap();
        assert!(
            root.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("mixed-qemu-")
        );
        assert_eq!(
            root.parent().unwrap(),
            std::env::current_dir()
                .unwrap()
                .join("target/isolated-tests")
                .canonicalize()
                .unwrap()
        );
        assert!(std::fs::read_dir(&appdata).unwrap().next().is_none());
        let password = std::env::var("DEVINGPANEL_TEST_LAN_PASSWORD").unwrap();
        let key_path = std::env::var("DEVINGPANEL_TEST_ARM64_KEY_PATH").unwrap();
        let private_key = std::fs::read_to_string(key_path).unwrap();
        let hosts = [
            (
                "vps-smoke",
                "192.0.2.129",
                22,
                "root",
                "password",
                password.as_str(),
                "SHA256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            ),
            (
                "qemu-arm64",
                "192.0.2.131",
                2223,
                "tester",
                "key",
                private_key.as_str(),
                "SHA256:BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB",
            ),
        ];
        let mut host_ids = Vec::new();
        for (name, address, port, username, auth, credential, fingerprint) in hosts {
            let host = crate::host_store::create(crate::host_store::NewHost {
                name: name.into(),
                address: address.into(),
                port: port.to_string(),
                username: username.into(),
                group: "test".into(),
                auth: auth.into(),
                credential: credential.into(),
            })
            .unwrap();
            crate::host_store::trust_fingerprint(host.id, address, port, None, fingerprint)
                .unwrap();
            host_ids.push(host.id);
        }
        drop(password);
        drop(private_key);

        let source = std::env::current_dir()
            .unwrap()
            .join("tests/fixtures/stack-v2-success");
        let project_dir = root.join("project");
        std::fs::create_dir(&project_dir).unwrap();
        for file in [
            "compose.yaml",
            "Dockerfile.web",
            "Dockerfile.worker",
            "probe-web.txt",
            "probe-worker.txt",
        ] {
            std::fs::copy(source.join(file), project_dir.join(file)).unwrap();
        }
        let yaml = std::fs::read_to_string(source.join("devingpanel.yaml")).unwrap();
        let yaml = yaml
            .replace("platform: linux/amd64", "platform: auto")
            .replace("name: stack-v2-success", &format!("name: stack-v2-mixed-{}", uuid::Uuid::new_v4().simple()))
            .replace("    - group: test\n      host: vps-smoke", "    - group: test\n      host: vps-smoke\n    - group: test\n      host: qemu-arm64");
        std::fs::write(project_dir.join("devingpanel.yaml"), yaml).unwrap();
        let reservation_id = chrono::Utc::now().timestamp_millis();
        crate::database::connect()
            .unwrap()
            .execute(
                "INSERT INTO projects (id, name, path, dockerfile, image) VALUES (?1, 'mixed-id-reservation', '.', 'Dockerfile', 'reservation')",
                [reservation_id],
            )
            .unwrap();
        let project = crate::project_store::create_from_manifest(&project_dir).unwrap();
        assert!(project.id > 1_000_000_000);
        assert_eq!(project.host_ids, host_ids);
        let tag = format!("mixed{}", uuid::Uuid::new_v4().simple());
        let cancel = Arc::new(AtomicBool::new(false));
        let plan = preflight_batch(project.id, &tag, cancel.clone()).unwrap();
        assert_eq!(plan.plan.builds.len(), 4);
        assert_eq!(
            plan.targets
                .iter()
                .map(|target| target.platform.as_str())
                .collect::<Vec<_>>(),
            ["linux/amd64", "linux/arm64"]
        );
        assert_ne!(
            plan.targets[0].docker_daemon_id,
            plan.targets[1].docker_daemon_id
        );
        let mut baselines = Vec::new();
        for target in &plan.targets {
            let probe = crate::release_store::TaskSnapshot {
                id: "namespace-probe".into(),
                project_id: project.id,
                project_name: project.name.clone(),
                host: target.host.clone(),
                docker_endpoint: Some(target.docker_endpoint.clone()),
                docker_daemon_id: Some(target.docker_daemon_id.clone()),
                phase: "prepared".into(),
                outcome: "running".into(),
                health_result: None,
                detail: String::new(),
                started_at: String::new(),
                execution_finished_at: None,
                image_reference: plan.plan.builds[0].reference.clone(),
            };
            crate::ssh::assert_empty_project_namespace(&probe).unwrap();
            let inventory = crate::ssh::inventory(target.host.id).unwrap();
            assert!(
                ["redis:7-alpine", "postgres:16-alpine"]
                    .iter()
                    .all(|image| inventory.images.iter().any(|item| item.reference == *image))
            );
            baselines.push(
                inventory
                    .containers
                    .iter()
                    .map(|item| item.id.clone())
                    .collect::<Vec<_>>(),
            );
        }
        let progress = super::run_batch_parallel(project.id, &plan, 2, cancel, |_| {})
            .unwrap_or_else(|error| panic!("mixed batch {:?}: {}", error.batch_id, error.message));
        let batch_id = match progress {
            super::BatchProgress::AwaitingConfirmation { batch_id, .. } => batch_id,
            super::BatchProgress::Complete { .. } => panic!("health confirmation was skipped"),
        };
        let batch = crate::batch_store::get(&batch_id).unwrap();
        assert_eq!(batch.images.len(), 4);
        assert!(batch.images.iter().all(|image| {
            image.archive_bytes.unwrap() > 0
                && image
                    .archive_sha256
                    .as_ref()
                    .is_some_and(|hash| hash.len() == 64)
                && image.image_ids.as_ref().is_some_and(|ids| !ids.is_empty())
        }));
        let conn = crate::database::connect().unwrap();
        let pointers: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM project_current_version WHERE project_id = ?1",
                [project.id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(pointers, 0);
        for (index, target) in batch.targets.iter().enumerate() {
            let task =
                crate::release_store::get_task(target.release_task_id.as_ref().unwrap()).unwrap();
            assert_eq!(
                (
                    task.phase.as_str(),
                    task.outcome.as_str(),
                    task.health_result.as_deref()
                ),
                ("checked", "blocked", Some("healthy"))
            );
            let observed = crate::ssh::observe_release_project(&task).unwrap();
            assert_eq!(observed.containers.len(), 4);
            assert!(
                observed
                    .containers
                    .iter()
                    .all(|item| item.status == "running"
                        && item.health.as_deref() == Some("healthy"))
            );
            let inventory = crate::ssh::inventory(target.host.id).unwrap();
            let after = inventory
                .containers
                .iter()
                .map(|item| item.id.as_str())
                .collect::<Vec<_>>();
            assert_eq!(after.len(), baselines[index].len() + 4);
            assert!(
                baselines[index]
                    .iter()
                    .all(|id| after.contains(&id.as_str()))
            );
            assert_eq!(
                crate::release::confirm_healthy_and_complete(&task.id).unwrap(),
                4
            );
        }
        let finished = crate::batch_store::get(&batch_id).unwrap();
        assert_eq!(finished.state, "complete");
        assert!(
            finished
                .targets
                .iter()
                .all(|target| target.state == "healthy")
        );
        eprintln!(
            "QEMU mixed batch complete: project={}, batch={}",
            project.id, batch_id
        );
    }

    #[test]
    #[ignore = "one-shot three-host LAN release; requires fresh parallel-lan APPDATA and test password"]
    fn isolated_lan_parallel_three_host_waves() {
        let appdata = std::path::PathBuf::from(std::env::var_os("APPDATA").unwrap())
            .canonicalize()
            .unwrap();
        let root = appdata.parent().unwrap();
        assert!(
            root.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("parallel-lan-")
        );
        assert_eq!(
            root.parent().unwrap(),
            std::env::current_dir()
                .unwrap()
                .join("target/isolated-tests")
                .canonicalize()
                .unwrap()
        );
        assert!(std::fs::read_dir(&appdata).unwrap().next().is_none());
        let password = std::env::var("DEVINGPANEL_TEST_LAN_PASSWORD").unwrap();
        let fingerprint = "SHA256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
        let mut host_ids = Vec::new();
        for (name, address) in [
            ("vps-smoke", "192.0.2.129"),
            ("vps-third", "192.0.2.131"),
            ("vps-second", "192.0.2.130"),
        ] {
            let host = crate::host_store::create(crate::host_store::NewHost {
                name: name.into(),
                address: address.into(),
                port: "22".into(),
                username: "root".into(),
                group: "test".into(),
                auth: "password".into(),
                credential: password.clone(),
            })
            .unwrap();
            crate::host_store::trust_fingerprint(host.id, address, 22, None, fingerprint).unwrap();
            host_ids.push(host.id);
        }
        drop(password);
        let source = std::env::current_dir()
            .unwrap()
            .join("tests/fixtures/stack-v2-success");
        let project_dir = root.join("project");
        std::fs::create_dir(&project_dir).unwrap();
        for file in [
            "compose.yaml",
            "Dockerfile.web",
            "Dockerfile.worker",
            "probe-web.txt",
            "probe-worker.txt",
        ] {
            std::fs::copy(source.join(file), project_dir.join(file)).unwrap();
        }
        let yaml = std::fs::read_to_string(source.join("devingpanel.yaml")).unwrap();
        std::fs::write(
            project_dir.join("devingpanel.yaml"),
            yaml.replace(
                "name: stack-v2-success",
                &format!("name: stack-v2-three-{}", uuid::Uuid::new_v4().simple()),
            )
            .replace(
                "    - group: test\n      host: vps-smoke",
                "    - group: test\n      host: vps-smoke\n    - group: test\n      host: vps-third\n    - group: test\n      host: vps-second",
            ),
        )
        .unwrap();
        let reservation_id = chrono::Utc::now().timestamp_millis();
        crate::database::connect()
            .unwrap()
            .execute(
                "INSERT INTO projects (id, name, path, dockerfile, image)
                 VALUES (?1, 'three-host-id-reservation', '.', 'Dockerfile', 'reservation')",
                [reservation_id],
            )
            .unwrap();
        let project = crate::project_store::create_from_manifest(&project_dir).unwrap();
        assert!(project.id > 1_000_000_000);
        assert_eq!(project.host_ids, host_ids);
        let tag = format!("three{}", uuid::Uuid::new_v4().simple());
        let cancel = Arc::new(AtomicBool::new(false));
        let plan = preflight_batch(project.id, &tag, cancel.clone()).unwrap();
        assert_eq!(plan.targets.len(), 3);
        let daemons = plan
            .targets
            .iter()
            .map(|target| target.docker_daemon_id.as_str())
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(daemons.len(), 3);
        let mut baselines = Vec::new();
        for target in &plan.targets {
            let probe = crate::release_store::TaskSnapshot {
                id: "namespace-probe".into(),
                project_id: project.id,
                project_name: project.name.clone(),
                host: target.host.clone(),
                docker_endpoint: Some(target.docker_endpoint.clone()),
                docker_daemon_id: Some(target.docker_daemon_id.clone()),
                phase: "prepared".into(),
                outcome: "running".into(),
                health_result: None,
                detail: String::new(),
                started_at: String::new(),
                execution_finished_at: None,
                image_reference: plan.plan.builds[0].reference.clone(),
            };
            crate::ssh::assert_empty_project_namespace(&probe).unwrap();
            let inventory = crate::ssh::inventory(target.host.id).unwrap();
            for image in ["redis:7-alpine", "postgres:16-alpine"] {
                assert!(inventory.images.iter().any(|item| item.reference == image));
            }
            let mut ids = inventory
                .containers
                .iter()
                .map(|item| item.id.clone())
                .collect::<Vec<_>>();
            ids.sort();
            baselines.push(ids);
        }
        if std::env::var("DEVINGPANEL_TEST_PRETRANSFER_FAIL_SECOND").as_deref() == Ok("1") {
            let batch_id = crate::batch_store::create_with_parallel(project.id, &plan, 2).unwrap();
            crate::batch_store::begin_build(&batch_id).unwrap();
            super::build_shared_images(project.id, &batch_id, &plan, cancel.clone(), |_| {})
                .unwrap();
            let guard = Arc::new(crate::release_materials::acquire_guard(project.id).unwrap());
            let first = super::prepare_batch_host(
                project.id,
                &batch_id,
                &plan,
                0,
                guard.clone(),
                cancel.clone(),
            )
            .unwrap();
            let failed =
                super::prepare_batch_host(project.id, &batch_id, &plan, 1, guard, cancel.clone())
                    .unwrap();
            let failed_task = failed.task_id.clone();
            assert!(
                crate::ssh::observe_release_project(
                    &crate::release_store::get_task(&failed_task).unwrap()
                )
                .unwrap()
                .containers
                .is_empty()
            );
            super::upload_batch_host_archives(&first, &batch_id, &plan, 0, cancel.clone(), |_| {})
                .unwrap();
            super::load_batch_host_archives(&first, &batch_id, &plan, 0, cancel.clone(), |_| {})
                .unwrap();
            let staged =
                super::stage_batch_host_stack(&first, &batch_id, &plan, 0, cancel.clone(), |_| {})
                    .unwrap();
            let first_task = super::deploy_batch_host_stack(
                &first,
                &batch_id,
                &plan,
                0,
                &staged,
                cancel,
                |_| {},
            )
            .unwrap();
            drop(first);
            drop(failed);
            crate::batch_store::close_pretransfer(&failed_task).unwrap();
            let paused = crate::batch_store::get(&batch_id).unwrap();
            assert_eq!(paused.state, "paused");
            assert_eq!(paused.targets[0].state, "running");
            assert_eq!(paused.targets[1].state, "failed");
            assert_eq!(paused.targets[2].state, "waiting");
            assert!(paused.targets[2].release_task_id.is_none());
            let task = crate::release_store::get_task(&first_task).unwrap();
            assert_eq!(
                (
                    task.phase.as_str(),
                    task.outcome.as_str(),
                    task.health_result.as_deref()
                ),
                ("checked", "blocked", Some("healthy"))
            );
            assert_eq!(
                crate::release::confirm_healthy_and_complete(&first_task).unwrap(),
                4
            );
            let paused = crate::batch_store::get(&batch_id).unwrap();
            assert_eq!(paused.targets[0].state, "healthy");
            for (index, target) in paused.targets.iter().enumerate() {
                let mut after = crate::ssh::inventory(target.host.id)
                    .unwrap()
                    .containers
                    .into_iter()
                    .map(|item| item.id)
                    .collect::<Vec<_>>();
                after.sort();
                assert_eq!(
                    after.len(),
                    baselines[index].len() + if index == 0 { 4 } else { 0 }
                );
                assert!(baselines[index].iter().all(|id| after.contains(id)));
            }
            eprintln!(
                "three-host pretransfer failure ready for GUI skip: project={}, batch={}, failed={}",
                project.id, batch_id, failed_task
            );
            return;
        }
        let batch_id =
            match super::run_batch_parallel(project.id, &plan, 2, cancel.clone(), |stage| {
                eprintln!("first wave: {stage}")
            })
            .unwrap_or_else(|error| panic!("first wave {:?}: {}", error.batch_id, error.message))
            {
                super::BatchProgress::AwaitingConfirmation { batch_id, .. } => batch_id,
                super::BatchProgress::Complete { .. } => panic!("first wave skipped confirmation"),
            };
        let first_wave = crate::batch_store::get(&batch_id).unwrap();
        assert_eq!(first_wave.state, "running");
        assert_eq!(first_wave.parallelism, 2);
        assert_eq!(first_wave.targets[2].state, "waiting");
        assert!(first_wave.targets[2].release_task_id.is_none());
        assert!(matches!(
            super::continue_batch_parallel(
                &batch_id,
                Some(host_ids[2]),
                &plan,
                cancel.clone(),
                |_| {}
            )
            .unwrap_or_else(|error| panic!("blocked wave: {}", error.message)),
            super::BatchProgress::AwaitingConfirmation { .. }
        ));
        for target in &first_wave.targets[..2] {
            let task =
                crate::release_store::get_task(target.release_task_id.as_ref().unwrap()).unwrap();
            assert_eq!(
                (
                    task.phase.as_str(),
                    task.outcome.as_str(),
                    task.health_result.as_deref()
                ),
                ("checked", "blocked", Some("healthy"))
            );
            assert_eq!(
                crate::release::confirm_healthy_and_complete(&task.id).unwrap(),
                4
            );
        }
        let ready = crate::batch_store::get(&batch_id).unwrap();
        assert_eq!(ready.targets[2].state, "waiting");
        assert!(ready.targets[2].release_task_id.is_none());
        if std::env::var("DEVINGPANEL_TEST_STOP_AFTER_FIRST_WAVE").as_deref() == Ok("1") {
            eprintln!(
                "three-host first wave confirmed: project={}, batch={}",
                project.id, batch_id
            );
            return;
        }
        let confirmed = super::reconfirm_batch(&batch_id, cancel.clone()).unwrap();
        match super::continue_batch_parallel(
            &batch_id,
            Some(host_ids[2]),
            &confirmed,
            cancel,
            |stage| eprintln!("second wave: {stage}"),
        )
        .unwrap_or_else(|error| panic!("second wave {:?}: {}", error.batch_id, error.message))
        {
            super::BatchProgress::AwaitingConfirmation { .. } => {}
            super::BatchProgress::Complete { .. } => panic!("second wave skipped confirmation"),
        }
        let second_wave = crate::batch_store::get(&batch_id).unwrap();
        let third = &second_wave.targets[2];
        assert_eq!(third.state, "running");
        let third_task =
            crate::release_store::get_task(third.release_task_id.as_ref().unwrap()).unwrap();
        assert_eq!(
            (
                third_task.phase.as_str(),
                third_task.outcome.as_str(),
                third_task.health_result.as_deref()
            ),
            ("checked", "blocked", Some("healthy"))
        );
        assert_eq!(
            crate::release::confirm_healthy_and_complete(&third_task.id).unwrap(),
            4
        );
        let finished = crate::batch_store::get(&batch_id).unwrap();
        assert_eq!(finished.state, "complete");
        assert!(
            finished
                .targets
                .iter()
                .all(|target| target.state == "healthy")
        );
        let conn = crate::database::connect().unwrap();
        for (index, target) in finished.targets.iter().enumerate() {
            let task_id = target.release_task_id.as_ref().unwrap();
            let task = crate::release_store::get_task(task_id).unwrap();
            assert_eq!(
                (
                    task.phase.as_str(),
                    task.outcome.as_str(),
                    task.health_result.as_deref()
                ),
                ("checked", "complete", Some("healthy"))
            );
            let pointer: String = conn.query_row(
                "SELECT task_id FROM project_current_version WHERE project_id = ?1 AND docker_daemon_id = ?2",
                rusqlite::params![project.id, target.docker_daemon_id],
                |row| row.get(0),
            ).unwrap();
            assert_eq!(pointer, *task_id);
            let mut after = crate::ssh::inventory(target.host.id)
                .unwrap()
                .containers
                .into_iter()
                .map(|item| item.id)
                .collect::<Vec<_>>();
            after.sort();
            assert_eq!(after.len(), baselines[index].len() + 4);
            assert!(baselines[index].iter().all(|id| after.contains(id)));
        }
        eprintln!(
            "three-host waves complete: project={}, batch={}",
            project.id, batch_id
        );
    }

    #[test]
    #[ignore = "finish an explicitly identified healthy first wave in an isolated three-host LAN batch"]
    fn isolated_lan_parallel_finish_existing_three_host_wave() {
        let appdata = std::path::PathBuf::from(std::env::var_os("APPDATA").unwrap())
            .canonicalize()
            .unwrap();
        let root = appdata.parent().unwrap();
        assert!(
            root.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("parallel-lan-three-")
        );
        assert_eq!(
            root.parent().unwrap(),
            std::env::current_dir()
                .unwrap()
                .join("target/isolated-tests")
                .canonicalize()
                .unwrap()
        );
        let batch_id = std::env::var("DEVINGPANEL_TEST_EXISTING_BATCH").unwrap();
        let batch = crate::batch_store::get(&batch_id).unwrap();
        assert!(batch.project_id > 1_000_000_000);
        assert_eq!(batch.state, "running");
        assert_eq!(batch.parallelism, 2);
        assert_eq!(batch.targets.len(), 3);
        assert_eq!(batch.targets[2].state, "waiting");
        assert!(batch.targets[2].release_task_id.is_none());
        let mut baselines = Vec::new();
        for (index, target) in batch.targets.iter().enumerate() {
            let inventory = crate::ssh::inventory(target.host.id).unwrap();
            let project_containers = inventory
                .containers
                .iter()
                .filter(|container| container.name.contains(&batch.project_id.to_string()))
                .count();
            assert_eq!(project_containers, if index < 2 { 4 } else { 0 });
            let mut ids = inventory
                .containers
                .into_iter()
                .map(|item| item.id)
                .collect::<Vec<_>>();
            ids.sort();
            baselines.push(ids);
        }
        let conn = crate::database::connect().unwrap();
        let pointers: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM project_current_version WHERE project_id = ?1",
                [batch.project_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(pointers, 0);
        for target in &batch.targets[..2] {
            let task =
                crate::release_store::get_task(target.release_task_id.as_ref().unwrap()).unwrap();
            assert_eq!(
                (
                    task.phase.as_str(),
                    task.outcome.as_str(),
                    task.health_result.as_deref()
                ),
                ("checked", "blocked", Some("healthy"))
            );
            assert_eq!(
                crate::release::confirm_healthy_and_complete(&task.id).unwrap(),
                4
            );
        }
        let ready = crate::batch_store::get(&batch_id).unwrap();
        assert_eq!(ready.targets[2].state, "waiting");
        assert!(ready.targets[2].release_task_id.is_none());
        let cancel = Arc::new(AtomicBool::new(false));
        let confirmed = super::reconfirm_batch(&batch_id, cancel.clone()).unwrap();
        match super::continue_batch_parallel(
            &batch_id,
            Some(ready.targets[2].host.id),
            &confirmed,
            cancel,
            |stage| {
                if !stage.starts_with("镜像传输 ") {
                    eprintln!("second wave: {stage}");
                }
            },
        )
        .unwrap_or_else(|error| panic!("second wave {:?}: {}", error.batch_id, error.message))
        {
            super::BatchProgress::AwaitingConfirmation { .. } => {}
            super::BatchProgress::Complete { .. } => panic!("second wave skipped confirmation"),
        }
        let second_wave = crate::batch_store::get(&batch_id).unwrap();
        let third = &second_wave.targets[2];
        assert_eq!(third.state, "running");
        let third_task =
            crate::release_store::get_task(third.release_task_id.as_ref().unwrap()).unwrap();
        assert_eq!(
            (
                third_task.phase.as_str(),
                third_task.outcome.as_str(),
                third_task.health_result.as_deref()
            ),
            ("checked", "blocked", Some("healthy"))
        );
        assert_eq!(
            crate::release::confirm_healthy_and_complete(&third_task.id).unwrap(),
            4
        );
        let finished = crate::batch_store::get(&batch_id).unwrap();
        assert_eq!(finished.state, "complete");
        for (index, target) in finished.targets.iter().enumerate() {
            assert_eq!(target.state, "healthy");
            let task_id = target.release_task_id.as_ref().unwrap();
            let task = crate::release_store::get_task(task_id).unwrap();
            assert_eq!(
                (
                    task.phase.as_str(),
                    task.outcome.as_str(),
                    task.health_result.as_deref()
                ),
                ("checked", "complete", Some("healthy"))
            );
            let pointer: String = conn
                .query_row(
                    "SELECT task_id FROM project_current_version WHERE project_id = ?1 AND docker_daemon_id = ?2",
                    rusqlite::params![batch.project_id, target.docker_daemon_id],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(pointer, *task_id);
            let mut after = crate::ssh::inventory(target.host.id)
                .unwrap()
                .containers
                .into_iter()
                .map(|item| item.id)
                .collect::<Vec<_>>();
            after.sort();
            assert_eq!(
                after.len(),
                baselines[index].len() + if index == 2 { 4 } else { 0 }
            );
            assert!(baselines[index].iter().all(|id| after.contains(id)));
        }
        eprintln!(
            "three-host waves complete: project={}, batch={}",
            batch.project_id, batch_id
        );
    }

    #[test]
    #[ignore = "confirm an explicitly identified already-healthy isolated parallel LAN batch; no deploy"]
    fn isolated_lan_parallel_confirm_existing_healthy() {
        let appdata = std::path::PathBuf::from(std::env::var_os("APPDATA").unwrap())
            .canonicalize()
            .unwrap();
        let root = appdata.parent().unwrap();
        assert!(
            root.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("parallel-lan-")
        );
        assert_eq!(
            root.parent().unwrap(),
            std::env::current_dir()
                .unwrap()
                .join("target/isolated-tests")
                .canonicalize()
                .unwrap()
        );
        let batches = crate::batch_store::recent(None).unwrap();
        assert_eq!(batches.len(), 1);
        let batch = &batches[0];
        let batch_id = batch.id.clone();
        assert_eq!(batch.parallelism, 2);
        assert_eq!(batch.state, "running");
        assert_eq!(batch.targets.len(), 2);
        let conn = crate::database::connect().unwrap();
        let pointers: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM project_current_version WHERE project_id = ?1",
                [batch.project_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(pointers, 0);
        let addresses = ["192.0.2.129", "192.0.2.130"];
        let daemons = [
            "00000000-0000-4000-8000-000000000008",
            "00000000-0000-4000-8000-000000000011",
        ];
        for (index, target) in batch.targets.iter().enumerate() {
            assert_eq!(target.host.address, addresses[index]);
            assert_eq!(target.docker_daemon_id, daemons[index]);
            assert_eq!(
                crate::ssh::docker_identity_for_host(target.host.clone())
                    .unwrap()
                    .1,
                daemons[index]
            );
            assert_eq!(target.state, "running");
            let task =
                crate::release_store::get_task(target.release_task_id.as_ref().unwrap()).unwrap();
            assert_eq!(
                (
                    task.phase.as_str(),
                    task.outcome.as_str(),
                    task.health_result.as_deref()
                ),
                ("checked", "blocked", Some("healthy"))
            );
            let observed = crate::ssh::observe_release_project(&task).unwrap();
            assert_eq!(observed.containers.len(), 4);
            assert!(observed.containers.iter().all(|item| {
                item.status == "running" && item.health.as_deref() == Some("healthy")
            }));
        }
        for target in &batch.targets {
            assert_eq!(
                crate::release::confirm_healthy_and_complete(
                    target.release_task_id.as_ref().unwrap()
                )
                .unwrap(),
                4
            );
        }
        let complete = crate::batch_store::get(&batch_id).unwrap();
        assert_eq!(complete.state, "complete");
        assert!(
            complete
                .targets
                .iter()
                .all(|target| target.state == "healthy")
        );
        eprintln!(
            "confirmed existing parallel LAN batch: project={}, batch={}",
            batch.project_id, batch_id
        );
    }

    #[test]
    #[ignore = "one-shot parallel process crash on two trusted LAN VPS hosts; requires fresh parallel-crash APPDATA and test password"]
    fn isolated_lan_parallel_crash_recovers_both_unknown() {
        use std::io::{BufRead as _, Write as _};

        let appdata = std::path::PathBuf::from(std::env::var_os("APPDATA").unwrap())
            .canonicalize()
            .unwrap();
        let root = appdata.parent().unwrap();
        assert!(
            root.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("parallel-crash-")
        );
        assert_eq!(
            root.parent().unwrap(),
            std::env::current_dir()
                .unwrap()
                .join("target/isolated-tests")
                .canonicalize()
                .unwrap()
        );
        if std::env::var("DEVINGPANEL_TEST_PARALLEL_CRASH_CHILD").as_deref() == Ok("1") {
            let project_id: i64 = std::env::var("DEVINGPANEL_TEST_PARALLEL_PROJECT")
                .unwrap()
                .parse()
                .unwrap();
            let tag = std::env::var("DEVINGPANEL_TEST_PARALLEL_TAG").unwrap();
            let plan = preflight_batch(project_id, &tag, Arc::new(AtomicBool::new(false))).unwrap();
            let _ = super::run_batch_parallel(
                project_id,
                &plan,
                2,
                Arc::new(AtomicBool::new(false)),
                |event| {
                    if event.starts_with("传输镜像：") {
                        println!("PARALLEL_UPLOAD_STARTED");
                        std::io::stdout().flush().unwrap();
                        loop {
                            std::thread::park();
                        }
                    }
                },
            );
            panic!("child must be terminated while both remote tasks are active");
        }

        assert!(std::fs::read_dir(&appdata).unwrap().next().is_none());
        let password = std::env::var("DEVINGPANEL_TEST_LAN_PASSWORD").unwrap();
        let fingerprint = "SHA256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
        let mut hosts = Vec::new();
        for (name, address) in [("vps-smoke", "192.0.2.129"), ("vps-second", "192.0.2.130")] {
            let host = crate::host_store::create(crate::host_store::NewHost {
                name: name.into(),
                address: address.into(),
                port: "22".into(),
                username: "root".into(),
                group: "test".into(),
                auth: "password".into(),
                credential: password.clone(),
            })
            .unwrap();
            crate::host_store::trust_fingerprint(host.id, address, 22, None, fingerprint).unwrap();
            hosts.push(host.id);
        }
        drop(password);
        let source = std::env::current_dir()
            .unwrap()
            .join("tests/fixtures/stack-v2-success");
        let project_dir = root.join("project");
        std::fs::create_dir(&project_dir).unwrap();
        for file in [
            "compose.yaml",
            "Dockerfile.web",
            "Dockerfile.worker",
            "probe-web.txt",
            "probe-worker.txt",
        ] {
            std::fs::copy(source.join(file), project_dir.join(file)).unwrap();
        }
        let yaml = std::fs::read_to_string(source.join("devingpanel.yaml")).unwrap();
        std::fs::write(
            project_dir.join("devingpanel.yaml"),
            yaml.replace(
                "name: stack-v2-success",
                &format!("name: stack-v2-parallel-crash-{}", uuid::Uuid::new_v4().simple()),
            ).replace(
                "    - group: test\n      host: vps-smoke",
                "    - group: test\n      host: vps-smoke\n    - group: test\n      host: vps-second",
            ),
        ).unwrap();
        let reservation_id = chrono::Utc::now().timestamp_millis();
        crate::database::connect()
            .unwrap()
            .execute(
                "INSERT INTO projects (id, name, path, dockerfile, image)
             VALUES (?1, 'parallel-crash-id-reservation', '.', 'Dockerfile', 'reservation')",
                [reservation_id],
            )
            .unwrap();
        let project = crate::project_store::create_from_manifest(&project_dir).unwrap();
        assert!(project.id > 1_000_000_000);
        assert_eq!(project.host_ids, hosts);
        let tag = format!("parallelcrash{}", uuid::Uuid::new_v4().simple());
        let plan = preflight_batch(project.id, &tag, Arc::new(AtomicBool::new(false))).unwrap();
        assert_eq!(plan.targets.len(), 2);
        assert_eq!(
            plan.targets[0].docker_daemon_id,
            "00000000-0000-4000-8000-000000000008"
        );
        assert_eq!(
            plan.targets[1].docker_daemon_id,
            "00000000-0000-4000-8000-000000000011"
        );
        let mut baselines = Vec::new();
        for target in &plan.targets {
            let probe = crate::release_store::TaskSnapshot {
                id: "namespace-probe".into(),
                project_id: project.id,
                project_name: project.name.clone(),
                host: target.host.clone(),
                docker_endpoint: Some(target.docker_endpoint.clone()),
                docker_daemon_id: Some(target.docker_daemon_id.clone()),
                phase: "prepared".into(),
                outcome: "running".into(),
                health_result: None,
                detail: String::new(),
                started_at: String::new(),
                execution_finished_at: None,
                image_reference: plan.plan.builds[0].reference.clone(),
            };
            crate::ssh::assert_empty_project_namespace(&probe).unwrap();
            let inventory = crate::ssh::inventory(target.host.id).unwrap();
            for image in ["redis:7-alpine", "postgres:16-alpine"] {
                assert!(inventory.images.iter().any(|item| item.reference == image));
            }
            let mut ids = inventory
                .containers
                .iter()
                .map(|item| item.id.clone())
                .collect::<Vec<_>>();
            ids.sort();
            baselines.push(ids);
        }
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("stack_release::tests::isolated_lan_parallel_crash_recovers_both_unknown")
            .args(["--ignored", "--exact", "--nocapture"])
            .env("DEVINGPANEL_TEST_PARALLEL_CRASH_CHILD", "1")
            .env("DEVINGPANEL_TEST_PARALLEL_PROJECT", project.id.to_string())
            .env("DEVINGPANEL_TEST_PARALLEL_TAG", &tag)
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        struct KillOnDrop(std::process::Child);
        impl Drop for KillOnDrop {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let mut child = KillOnDrop(child);
        let stdout = child.0.stdout.take().unwrap();
        let (started, received) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let reached_upload = std::io::BufReader::new(stdout)
                .lines()
                .map_while(Result::ok)
                .any(|line| line.contains("PARALLEL_UPLOAD_STARTED"));
            let _ = started.send(reached_upload);
        });
        assert_eq!(
            received.recv_timeout(std::time::Duration::from_secs(120)),
            Ok(true),
            "parallel child did not reach remote upload"
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(90);
        let batch = loop {
            let batches = crate::batch_store::recent(Some(project.id)).unwrap();
            if let Some(batch) = batches.into_iter().next() {
                let both_remote = batch.targets.len() == 2
                    && batch.targets.iter().all(|target| {
                        target.state == "running"
                            && target.release_task_id.as_deref().is_some_and(|id| {
                                crate::release_store::get_task(id).is_ok_and(|task| {
                                    task.outcome == "running" && task.phase == "uploading"
                                })
                            })
                    });
                if both_remote {
                    break batch;
                }
            }
            assert!(
                std::time::Instant::now() < deadline,
                "both targets did not reach remote execution before crash"
            );
            std::thread::sleep(std::time::Duration::from_millis(200));
        };
        assert_eq!(batch.parallelism, 2);
        assert!(crate::release_materials::acquire_guard(project.id).is_err());
        child.0.kill().unwrap();
        child.0.wait().unwrap();
        assert_eq!(
            crate::release_materials::recover_interrupted_batches().unwrap(),
            1
        );
        assert_eq!(
            crate::release_materials::recover_interrupted_batches().unwrap(),
            0
        );
        let recovered = crate::batch_store::get(&batch.id).unwrap();
        assert_eq!(recovered.state, "unknown");
        for target in &recovered.targets {
            assert_eq!(target.state, "unknown");
            let task =
                crate::release_store::get_task(target.release_task_id.as_ref().unwrap()).unwrap();
            assert_eq!(task.outcome, "unknown");
        }
        let conn = crate::database::connect().unwrap();
        let pointers: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM project_current_version WHERE project_id = ?1",
                [project.id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(pointers, 0);
        for (index, target) in recovered.targets.iter().enumerate() {
            let inventory = crate::ssh::inventory(target.host.id).unwrap();
            let mut ids = inventory
                .containers
                .iter()
                .map(|item| item.id.clone())
                .collect::<Vec<_>>();
            ids.sort();
            assert_eq!(ids, baselines[index]);
        }
        if std::env::var("DEVINGPANEL_TEST_PARALLEL_RESUME_AFTER_CRASH").as_deref() == Ok("1") {
            for ordinal in (0..recovered.targets.len()).rev() {
                let task_id = recovered.targets[ordinal].release_task_id.as_ref().unwrap();
                let progress = super::resume_unknown_batch_upload(
                    &batch.id,
                    task_id,
                    Arc::new(AtomicBool::new(false)),
                    |_| {},
                )
                .unwrap_or_else(|error| {
                    panic!("parallel resume {:?}: {}", error.batch_id, error.message)
                });
                assert!(matches!(
                    progress,
                    super::BatchProgress::AwaitingConfirmation { task_id: resumed, .. }
                        if resumed == *task_id
                ));
                let current = crate::batch_store::get(&batch.id).unwrap();
                assert_eq!(
                    current.state,
                    if ordinal == 0 { "running" } else { "unknown" }
                );
                assert_eq!(current.targets[ordinal].state, "running");
                assert_eq!(
                    crate::release::confirm_healthy_and_complete(task_id).unwrap(),
                    4
                );
                let current = crate::batch_store::get(&batch.id).unwrap();
                assert_eq!(
                    current.state,
                    if ordinal == 0 { "complete" } else { "unknown" }
                );
            }
            let complete = crate::batch_store::get(&batch.id).unwrap();
            assert!(
                complete
                    .targets
                    .iter()
                    .all(|target| target.state == "healthy")
            );
            let pointers: i64 = crate::database::connect()
                .unwrap()
                .query_row(
                    "SELECT COUNT(*) FROM project_current_version WHERE project_id = ?1",
                    [project.id],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(pointers, 2);
            for (target, before) in complete.targets.iter().zip(&baselines) {
                let inventory = crate::ssh::inventory(target.host.id).unwrap();
                let ids = inventory
                    .containers
                    .iter()
                    .map(|item| item.id.as_str())
                    .collect::<Vec<_>>();
                assert_eq!(ids.len(), before.len() + 4);
                assert!(before.iter().all(|id| ids.contains(&id.as_str())));
            }
            eprintln!(
                "parallel LAN crash and independent resumes complete: project={}, batch={}",
                project.id, batch.id
            );
        } else {
            eprintln!(
                "parallel LAN crash recovered unknown: project={}, batch={}",
                project.id, batch.id
            );
        }
    }

    fn audit_completed_lan_batch(batch_id: &str, previous_id: &str) {
        let batch = crate::batch_store::get(batch_id).unwrap();
        let previous = crate::batch_store::get(previous_id).unwrap();
        assert_eq!(batch.state, "complete");
        assert_eq!(previous.state, "complete");
        assert_eq!(batch.targets.len(), 2);
        assert_eq!(previous.targets.len(), 2);
        let conn = crate::database::connect().unwrap();
        for (target, old) in batch.targets.iter().zip(&previous.targets) {
            assert_eq!(target.state, "healthy");
            assert_eq!(old.state, "healthy");
            assert_eq!(target.host.id, old.host.id);
            let task =
                crate::release_store::get_task(target.release_task_id.as_ref().unwrap()).unwrap();
            assert_eq!(
                (
                    task.phase.as_str(),
                    task.outcome.as_str(),
                    task.health_result.as_deref()
                ),
                ("checked", "complete", Some("healthy"))
            );
            assert_eq!(
                task.docker_daemon_id.as_deref(),
                Some(target.docker_daemon_id.as_str())
            );
            let pointer: String = conn
                .query_row(
                    "SELECT task_id FROM project_current_version
                     WHERE project_id = ?1 AND docker_daemon_id = ?2",
                    rusqlite::params![batch.project_id, target.docker_daemon_id],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(pointer, task.id);
            let archives = crate::release_store::uploaded_archives(&task.id).unwrap();
            assert_eq!(archives.len(), 2);
            assert!(archives.iter().all(|archive| archive.state == "cleaned"));
            let observed = crate::ssh::observe_release_project(&task).unwrap();
            assert_eq!(observed.containers.len(), 4);
            assert!(observed.containers.iter().all(|item| {
                item.status == "running" && item.health.as_deref() == Some("healthy")
            }));
            let old_task =
                crate::release_store::get_task(old.release_task_id.as_ref().unwrap()).unwrap();
            let old_observed = crate::ssh::observe_release_project(&old_task).unwrap();
            assert_eq!(old_observed.containers.len(), 4);
            assert!(old_observed.containers.iter().all(|item| {
                item.status == "running" && item.health.as_deref() == Some("healthy")
            }));
            let inventory = crate::ssh::inventory(target.host.id).unwrap();
            assert_eq!(inventory.containers.len(), 8);
            for project_id in [batch.project_id, previous.project_id] {
                let prefix = format!("dp{project_id}-");
                assert_eq!(
                    inventory
                        .containers
                        .iter()
                        .filter(|container| container.name.starts_with(&prefix))
                        .count(),
                    4
                );
            }
        }
        let local = std::process::Command::new("docker")
            .args(["ps", "-aq"])
            .output()
            .unwrap();
        assert!(local.status.success());
        assert_eq!(String::from_utf8(local.stdout).unwrap().lines().count(), 2);
    }

    #[test]
    #[ignore = "read-only audit of a completed isolated LAN fault-injection batch"]
    fn isolated_lan_completed_unknown_batch_audit() {
        let appdata = std::path::PathBuf::from(std::env::var_os("APPDATA").unwrap())
            .canonicalize()
            .unwrap();
        let root = appdata.parent().unwrap();
        assert!(
            root.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("ui-lan-batch-")
        );
        assert_eq!(
            root.parent().unwrap(),
            std::env::current_dir()
                .unwrap()
                .join("target/isolated-tests")
                .canonicalize()
                .unwrap()
        );
        let batch_id =
            std::env::var("DEVINGPANEL_TEST_LAN_BATCH_ID").expect("audited batch ID required");
        audit_completed_lan_batch(&batch_id, "00000000-0000-4000-8000-00000000001e");
    }

    #[test]
    #[ignore = "one-shot isolated LAN batch; injects a client-side health-check disconnect and explicitly reconciles before continuing"]
    fn isolated_lan_batch_unknown_health_requires_reconciliation() {
        let appdata = std::path::PathBuf::from(std::env::var_os("APPDATA").unwrap())
            .canonicalize()
            .unwrap();
        let root = appdata.parent().unwrap();
        assert!(
            root.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("ui-lan-batch-")
        );
        assert_eq!(
            root.parent().unwrap(),
            std::env::current_dir()
                .unwrap()
                .join("target/isolated-tests")
                .canonicalize()
                .unwrap()
        );
        let hosts = crate::host_store::list().unwrap().0;
        assert_eq!(hosts.len(), 2);
        let project_dir = root.join(format!("unknown-project-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir(&project_dir).unwrap();
        let source = std::env::current_dir()
            .unwrap()
            .join("tests/fixtures/stack-v2-success");
        for file in [
            "compose.yaml",
            "Dockerfile.web",
            "Dockerfile.worker",
            "probe-web.txt",
            "probe-worker.txt",
        ] {
            std::fs::copy(source.join(file), project_dir.join(file)).unwrap();
        }
        let yaml = std::fs::read_to_string(source.join("devingpanel.yaml")).unwrap();
        std::fs::write(
            project_dir.join("devingpanel.yaml"),
            yaml.replace(
                "name: stack-v2-success",
                &format!("name: stack-v2-lan-unknown-{}", uuid::Uuid::new_v4().simple()),
            )
            .replace(
                "    - group: test\n      host: vps-smoke",
                "    - group: test\n      host: vps-smoke\n    - group: test\n      host: vps-second",
            ),
        )
        .unwrap();
        let project = crate::project_store::create_from_manifest(&project_dir).unwrap();
        assert!(project.id > 1_000_000_000);
        assert_eq!(project.host_ids, [1, 2]);
        let mut baseline = Vec::new();
        for (id, address) in [(1, "192.0.2.129"), (2, "192.0.2.130")] {
            let host = hosts.iter().find(|host| host.id == id).unwrap();
            assert_eq!(host.address, address);
            let mut ids = crate::ssh::inventory(id)
                .unwrap()
                .containers
                .iter()
                .map(|container| container.id.clone())
                .collect::<Vec<_>>();
            ids.sort();
            assert_eq!(ids.len(), 4);
            baseline.push(ids);
        }
        let tag = format!("lanunknown{}", uuid::Uuid::new_v4().simple());
        let preflight =
            preflight_batch(project.id, &tag, Arc::new(AtomicBool::new(false))).unwrap();
        assert_eq!(preflight.targets.len(), 2);
        for (target, daemon_id) in preflight.targets.iter().zip([
            "00000000-0000-4000-8000-000000000008",
            "00000000-0000-4000-8000-000000000011",
        ]) {
            assert_eq!(target.docker_daemon_id, daemon_id);
            let probe = crate::release_store::TaskSnapshot {
                id: "namespace-probe".into(),
                project_id: project.id,
                project_name: project.name.clone(),
                host: target.host.clone(),
                docker_endpoint: Some(target.docker_endpoint.clone()),
                docker_daemon_id: Some(target.docker_daemon_id.clone()),
                phase: "prepared".into(),
                outcome: "running".into(),
                health_result: None,
                detail: String::new(),
                started_at: String::new(),
                execution_finished_at: None,
                image_reference: preflight.plan.builds[0].reference.clone(),
            };
            crate::ssh::assert_empty_project_namespace(&probe).unwrap();
            assert!(
                crate::ssh::observe_release_project(&probe)
                    .unwrap()
                    .containers
                    .is_empty()
            );
        }
        let mut injected = false;
        let error = super::run_batch_sequential(
            project.id,
            &preflight,
            Arc::new(AtomicBool::new(false)),
            |event| {
                if event == "核对全部 Compose 服务健康状态" && !injected {
                    crate::ssh::inject_next_stack_check_disconnect(project.id);
                    injected = true;
                }
            },
        )
        .err()
        .expect("injected health check must leave an unknown batch");
        assert!(injected);
        assert!(error.message.contains("测试注入"), "{}", error.message);
        let batch_id = error.batch_id.expect("created batch ID required");
        let batch = crate::batch_store::get(&batch_id).unwrap();
        assert_eq!(batch.state, "unknown");
        assert_eq!(batch.targets[0].state, "unknown");
        assert_eq!(batch.targets[1].state, "waiting");
        assert!(batch.targets[1].release_task_id.is_none());
        let first_id = batch.targets[0].release_task_id.as_ref().unwrap();
        let first = crate::release_store::get_task(first_id).unwrap();
        assert_eq!(
            (first.phase.as_str(), first.outcome.as_str()),
            ("checking", "unknown")
        );
        assert!(crate::release::confirm_healthy_and_complete(first_id).is_err());
        let conn = crate::database::connect().unwrap();
        let pointers = || {
            conn.query_row(
                "SELECT COUNT(*) FROM project_current_version WHERE project_id = ?1",
                [project.id],
                |row| row.get::<_, i64>(0),
            )
            .unwrap()
        };
        assert_eq!(pointers(), 0);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
        loop {
            let observed = crate::ssh::observe_release_project(&first).unwrap();
            if observed.containers.len() == 4
                && observed.containers.iter().all(|item| {
                    item.status == "running" && item.health.as_deref() == Some("healthy")
                })
            {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "first host did not become healthy after injected disconnect"
            );
            std::thread::sleep(std::time::Duration::from_secs(3));
        }
        assert_eq!(
            crate::release::reconcile_interrupted_health(first_id).unwrap(),
            4
        );
        let audit: i64 = conn
            .query_row(
                "SELECT observed_containers FROM release_task_health_reconciliations
                 WHERE task_id = ?1",
                [first_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(audit, 4);
        let batch = crate::batch_store::get(&batch_id).unwrap();
        assert_eq!(batch.state, "running");
        assert_eq!(batch.targets[0].state, "running");
        assert_eq!(batch.targets[1].state, "waiting");
        assert_eq!(pointers(), 0);
        assert_eq!(
            crate::release::confirm_healthy_and_complete(first_id).unwrap(),
            4
        );
        assert_eq!(pointers(), 1);
        let mut first_after = crate::ssh::inventory(1)
            .unwrap()
            .containers
            .iter()
            .map(|container| container.id.clone())
            .collect::<Vec<_>>();
        first_after.sort();
        assert_eq!(first_after.len(), baseline[0].len() + 4);
        assert!(baseline[0].iter().all(|id| first_after.contains(id)));
        let mut second_before = crate::ssh::inventory(2)
            .unwrap()
            .containers
            .iter()
            .map(|container| container.id.clone())
            .collect::<Vec<_>>();
        second_before.sort();
        assert_eq!(second_before, baseline[1]);

        let cancel = Arc::new(AtomicBool::new(false));
        let confirmed = super::reconfirm_batch(&batch_id, cancel.clone()).unwrap();
        let progress =
            super::continue_batch_sequential(&batch_id, Some(2), &confirmed, cancel, |_| {})
                .unwrap_or_else(|error| panic!("batch {}: {}", batch_id, error.message));
        let second_id = match progress {
            super::BatchProgress::AwaitingConfirmation { task_id, .. } => task_id,
            super::BatchProgress::Complete { .. } => panic!("second host skipped confirmation"),
        };
        let batch = crate::batch_store::get(&batch_id).unwrap();
        assert_eq!(batch.targets[0].state, "healthy");
        assert_eq!(batch.targets[1].state, "running");
        let second = crate::release_store::get_task(&second_id).unwrap();
        assert_eq!(
            (
                second.phase.as_str(),
                second.outcome.as_str(),
                second.health_result.as_deref()
            ),
            ("checked", "blocked", Some("healthy"))
        );
        assert_eq!(pointers(), 1);
        let mut first_final = crate::ssh::inventory(1)
            .unwrap()
            .containers
            .iter()
            .map(|container| container.id.clone())
            .collect::<Vec<_>>();
        first_final.sort();
        assert_eq!(first_final, first_after);
        let mut second_after = crate::ssh::inventory(2)
            .unwrap()
            .containers
            .iter()
            .map(|container| container.id.clone())
            .collect::<Vec<_>>();
        second_after.sort();
        assert_eq!(second_after.len(), baseline[1].len() + 4);
        assert!(baseline[1].iter().all(|id| second_after.contains(id)));
        assert_eq!(
            crate::release::confirm_healthy_and_complete(&second_id).unwrap(),
            4
        );
        let batch = crate::batch_store::get(&batch_id).unwrap();
        assert_eq!(batch.state, "complete");
        assert!(batch.targets.iter().all(|target| target.state == "healthy"));
        assert_eq!(pointers(), 2);
        audit_completed_lan_batch(&batch_id, "00000000-0000-4000-8000-00000000001e");
        eprintln!(
            "LAN unknown-health batch reconciled and complete: project={}, batch={}, tasks={},{}",
            project.id, batch_id, first_id, second_id
        );
    }

    #[test]
    #[ignore = "child process intentionally terminated during isolated LAN remote execution"]
    fn isolated_lan_crash_remote_child() {
        use std::io::Write as _;
        let appdata = std::path::PathBuf::from(std::env::var_os("APPDATA").unwrap())
            .canonicalize()
            .unwrap();
        assert!(
            appdata
                .parent()
                .unwrap()
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("ui-lan-crash-")
        );
        let project_id: i64 = std::env::var("DEVINGPANEL_TEST_LAN_CRASH_PROJECT")
            .unwrap()
            .parse()
            .unwrap();
        assert!(project_id > 1_000_000_000);
        let project = crate::project_store::effective(project_id).unwrap();
        assert!(
            std::path::Path::new(&project.path)
                .canonicalize()
                .unwrap()
                .starts_with(appdata.parent().unwrap())
        );
        let tag = std::env::var("DEVINGPANEL_TEST_LAN_CRASH_TAG").unwrap();
        let crash_phase =
            std::env::var("DEVINGPANEL_TEST_LAN_CRASH_PHASE").unwrap_or_else(|_| "upload".into());
        assert!(matches!(crash_phase.as_str(), "upload" | "deploying"));
        let snapshot = preflight_batch(project_id, &tag, Arc::new(AtomicBool::new(false))).unwrap();
        let _ = super::run_batch_sequential(
            project_id,
            &snapshot,
            Arc::new(AtomicBool::new(false)),
            |event| {
                if crash_phase == "deploying" && event == "启动全部 Compose 服务" {
                    println!("START_INFLIGHT");
                    std::io::stdout().flush().unwrap();
                    loop {
                        std::thread::park();
                    }
                }
                let sent = event
                    .strip_prefix("镜像传输 ")
                    .and_then(|value| value.split_once('/'))
                    .and_then(|(sent, _)| sent.parse::<u64>().ok());
                if crash_phase == "upload" && sent.is_some_and(|sent| sent >= 512 * 1024) {
                    println!("UPLOAD_INFLIGHT");
                    std::io::stdout().flush().unwrap();
                    loop {
                        std::thread::park();
                    }
                }
            },
        );
        panic!("child must be terminated while upload and project lock are active");
    }

    #[test]
    #[ignore = "second process checks the live isolated LAN project lock"]
    fn isolated_lan_crash_lock_probe_child() {
        let appdata = std::path::PathBuf::from(std::env::var_os("APPDATA").unwrap())
            .canonicalize()
            .unwrap();
        assert!(
            appdata
                .parent()
                .unwrap()
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("ui-lan-crash-")
        );
        let project_id: i64 = std::env::var("DEVINGPANEL_TEST_LAN_CRASH_PROJECT")
            .unwrap()
            .parse()
            .unwrap();
        assert!(project_id > 1_000_000_000);
        let should_be_locked = std::env::var("DEVINGPANEL_TEST_EXPECT_LOCKED").unwrap() == "yes";
        assert_eq!(
            crate::release_materials::acquire_guard(project_id).is_err(),
            should_be_locked
        );
    }

    #[test]
    #[ignore = "one-shot actual process crash during isolated LAN batch upload"]
    fn isolated_lan_batch_crash_during_upload_recovers_unknown() {
        use std::io::BufRead as _;
        let appdata = std::path::PathBuf::from(std::env::var_os("APPDATA").unwrap())
            .canonicalize()
            .unwrap();
        let root = appdata.parent().unwrap();
        assert!(
            root.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("ui-lan-crash-")
        );
        assert_eq!(
            root.parent().unwrap(),
            std::env::current_dir()
                .unwrap()
                .join("target/isolated-tests")
                .canonicalize()
                .unwrap()
        );
        let project_dir = root.join(format!("project-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir(&project_dir).unwrap();
        let source = std::env::current_dir()
            .unwrap()
            .join("tests/fixtures/stack-v2-success");
        for file in [
            "compose.yaml",
            "Dockerfile.web",
            "Dockerfile.worker",
            "probe-web.txt",
            "probe-worker.txt",
        ] {
            std::fs::copy(source.join(file), project_dir.join(file)).unwrap();
        }
        let yaml = std::fs::read_to_string(source.join("devingpanel.yaml")).unwrap();
        std::fs::write(
            project_dir.join("devingpanel.yaml"),
            yaml.replace(
                "name: stack-v2-success",
                &format!("name: stack-v2-lan-crash-{}", uuid::Uuid::new_v4().simple()),
            )
            .replace(
                "    - group: test\n      host: vps-smoke",
                "    - group: test\n      host: vps-smoke\n    - group: test\n      host: vps-second",
            ),
        )
        .unwrap();
        let project = crate::project_store::create_from_manifest(&project_dir).unwrap();
        assert!(project.id > 1_000_000_000);
        assert_eq!(project.host_ids, [1, 2]);
        let mut baseline = Vec::new();
        for (host_id, address, daemon) in [
            (1, "192.0.2.129", "00000000-0000-4000-8000-000000000008"),
            (2, "192.0.2.130", "00000000-0000-4000-8000-000000000011"),
        ] {
            let host = crate::host_store::list()
                .unwrap()
                .0
                .into_iter()
                .find(|host| host.id == host_id)
                .unwrap();
            assert_eq!(host.address, address);
            assert_eq!(
                crate::ssh::docker_identity_for_host(host).unwrap().1,
                daemon
            );
            let mut ids = crate::ssh::inventory(host_id)
                .unwrap()
                .containers
                .iter()
                .map(|container| container.id.clone())
                .collect::<Vec<_>>();
            ids.sort();
            baseline.push(ids);
        }
        let tag = format!("lancrash{}", uuid::Uuid::new_v4().simple());
        let snapshot = preflight_batch(project.id, &tag, Arc::new(AtomicBool::new(false))).unwrap();
        assert_eq!(snapshot.targets.len(), 2);
        for (target, daemon) in snapshot.targets.iter().zip([
            "00000000-0000-4000-8000-000000000008",
            "00000000-0000-4000-8000-000000000011",
        ]) {
            assert_eq!(target.docker_daemon_id, daemon);
            let probe = crate::release_store::TaskSnapshot {
                id: "namespace-probe".into(),
                project_id: project.id,
                project_name: project.name.clone(),
                host: target.host.clone(),
                docker_endpoint: Some(target.docker_endpoint.clone()),
                docker_daemon_id: Some(target.docker_daemon_id.clone()),
                phase: "prepared".into(),
                outcome: "running".into(),
                health_result: None,
                detail: String::new(),
                started_at: String::new(),
                execution_finished_at: None,
                image_reference: snapshot.plan.builds[0].reference.clone(),
            };
            crate::ssh::assert_empty_project_namespace(&probe).unwrap();
        }
        struct ChildGuard(std::process::Child);
        impl Drop for ChildGuard {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--ignored",
                "--exact",
                "stack_release::tests::isolated_lan_crash_remote_child",
                "--nocapture",
            ])
            .env("APPDATA", &appdata)
            .env("DEVINGPANEL_TEST_LAN_CRASH_PROJECT", project.id.to_string())
            .env("DEVINGPANEL_TEST_LAN_CRASH_TAG", &tag)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt as _;
            command.creation_flags(0x0800_0000);
        }
        let mut child = ChildGuard(command.spawn().unwrap());
        let stdout = child.0.stdout.take().unwrap();
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if line.contains("UPLOAD_INFLIGHT") {
                    let _ = sender.send(());
                    break;
                }
            }
        });
        receiver
            .recv_timeout(std::time::Duration::from_secs(1800))
            .expect("child did not reach an active SFTP write; inspect isolated database");
        assert!(child.0.try_wait().unwrap().is_none());
        let probe = |locked: bool| {
            let mut command = std::process::Command::new(std::env::current_exe().unwrap());
            command
                .args([
                    "--ignored",
                    "--exact",
                    "stack_release::tests::isolated_lan_crash_lock_probe_child",
                ])
                .env("APPDATA", &appdata)
                .env("DEVINGPANEL_TEST_LAN_CRASH_PROJECT", project.id.to_string())
                .env(
                    "DEVINGPANEL_TEST_EXPECT_LOCKED",
                    if locked { "yes" } else { "no" },
                )
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt as _;
                command.creation_flags(0x0800_0000);
            }
            assert!(
                command.status().unwrap().success(),
                "second client lock probe failed"
            );
        };
        probe(true);
        let batches = crate::batch_store::recent(Some(project.id)).unwrap();
        assert_eq!(batches.len(), 1);
        let batch = &batches[0];
        assert_eq!(batch.state, "running");
        assert_eq!(batch.targets[0].state, "running");
        assert_eq!(batch.targets[1].state, "waiting");
        assert!(batch.targets[1].release_task_id.is_none());
        let task_id = batch.targets[0].release_task_id.as_ref().unwrap();
        let task = crate::release_store::get_task(task_id).unwrap();
        assert_eq!(
            (task.phase.as_str(), task.outcome.as_str()),
            ("uploading", "running")
        );
        let records = crate::release_store::uploaded_archives(task_id).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].state, "reserved");
        let observed = crate::ssh::inspect_uploaded_archives(&task, &records).unwrap();
        assert_eq!(observed.len(), 1);
        assert_eq!(observed[0].directory, records[0].directory);
        let before_kill = observed[0].actual_bytes.unwrap();
        assert!(before_kill > 0 && before_kill < records[0].bytes);
        assert_eq!(
            crate::release_materials::recover_interrupted_batches().unwrap(),
            0
        );
        assert!(child.0.try_wait().unwrap().is_none());
        child.0.kill().unwrap();
        assert!(!child.0.wait().unwrap().success());
        probe(false);
        assert_eq!(
            crate::release_materials::recover_interrupted_batches().unwrap(),
            1
        );
        assert_eq!(
            crate::release_materials::recover_interrupted_batches().unwrap(),
            0
        );
        let batch = crate::batch_store::get(&batch.id).unwrap();
        assert_eq!(batch.state, "unknown");
        assert_eq!(batch.targets[0].state, "unknown");
        assert_eq!(batch.targets[1].state, "waiting");
        assert!(batch.targets[1].release_task_id.is_none());
        let task = crate::release_store::get_task(task_id).unwrap();
        assert_eq!(
            (task.phase.as_str(), task.outcome.as_str()),
            ("uploading", "unknown")
        );
        let inspection = crate::release::inspect_unknown_upload(task_id).unwrap();
        assert!(inspection.containers.is_empty());
        assert_eq!(inspection.archives.len(), 1);
        let after_kill = inspection.archives[0].actual_bytes.unwrap();
        assert!(after_kill >= before_kill && after_kill < records[0].bytes);
        assert_eq!(inspection.archives[0].directory, records[0].directory);
        assert!(crate::batch_store::skip_failed(&batch.id, 1).is_err());
        assert!(crate::batch_store::terminate_resolved(&batch.id).is_err());
        for (host_id, before) in [(1, &baseline[0]), (2, &baseline[1])] {
            let mut after = crate::ssh::inventory(host_id)
                .unwrap()
                .containers
                .iter()
                .map(|container| container.id.clone())
                .collect::<Vec<_>>();
            after.sort();
            assert_eq!(&after, before);
        }
        eprintln!(
            "LAN upload process crash recovered unknown: project={}, batch={}, task={}, remote={}/{}",
            project.id, batch.id, task_id, after_kill, records[0].bytes
        );
    }

    #[test]
    #[ignore = "one-shot LAN client crash after remote materials are ready and before Compose starts"]
    fn isolated_lan_batch_crash_at_stack_start_recovers_unknown() {
        use std::io::BufRead as _;
        let appdata = std::path::PathBuf::from(std::env::var_os("APPDATA").unwrap())
            .canonicalize()
            .unwrap();
        let root = appdata.parent().unwrap();
        assert!(
            root.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("ui-lan-crash-start-")
        );
        assert_eq!(
            root.parent().unwrap(),
            std::env::current_dir()
                .unwrap()
                .join("target/isolated-tests")
                .canonicalize()
                .unwrap()
        );
        let project_dir = root.join(format!("project-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir(&project_dir).unwrap();
        let source = std::env::current_dir()
            .unwrap()
            .join("tests/fixtures/stack-v2-success");
        for file in [
            "compose.yaml",
            "Dockerfile.web",
            "Dockerfile.worker",
            "probe-web.txt",
            "probe-worker.txt",
        ] {
            std::fs::copy(source.join(file), project_dir.join(file)).unwrap();
        }
        let yaml = std::fs::read_to_string(source.join("devingpanel.yaml")).unwrap();
        std::fs::write(
            project_dir.join("devingpanel.yaml"),
            yaml.replace(
                "name: stack-v2-success",
                &format!("name: stack-v2-crash-start-{}", uuid::Uuid::new_v4().simple()),
            )
            .replace(
                "    - group: test\n      host: vps-smoke",
                "    - group: test\n      host: vps-smoke\n    - group: test\n      host: vps-second",
            ),
        )
        .unwrap();
        let project = crate::project_store::create_from_manifest(&project_dir).unwrap();
        assert!(project.id > 1_000_000_000);
        assert_eq!(project.host_ids, [1, 2]);
        let tag = format!("lancrashstart{}", uuid::Uuid::new_v4().simple());
        let snapshot = preflight_batch(project.id, &tag, Arc::new(AtomicBool::new(false))).unwrap();
        let mut baselines = Vec::new();
        for (target, (address, daemon)) in snapshot.targets.iter().zip([
            ("192.0.2.129", "00000000-0000-4000-8000-000000000008"),
            ("192.0.2.130", "00000000-0000-4000-8000-000000000011"),
        ]) {
            assert_eq!(target.host.address, address);
            assert_eq!(target.docker_daemon_id, daemon);
            let probe = crate::release_store::TaskSnapshot {
                id: "namespace-probe".into(),
                project_id: project.id,
                project_name: project.name.clone(),
                host: target.host.clone(),
                docker_endpoint: Some(target.docker_endpoint.clone()),
                docker_daemon_id: Some(target.docker_daemon_id.clone()),
                phase: "prepared".into(),
                outcome: "running".into(),
                health_result: None,
                detail: String::new(),
                started_at: String::new(),
                execution_finished_at: None,
                image_reference: snapshot.plan.builds[0].reference.clone(),
            };
            crate::ssh::assert_empty_project_namespace(&probe).unwrap();
            let inventory = crate::ssh::inventory(target.host.id).unwrap();
            assert!(
                inventory
                    .images
                    .iter()
                    .any(|image| image.reference == "redis:7-alpine")
            );
            assert!(
                inventory
                    .images
                    .iter()
                    .any(|image| image.reference == "postgres:16-alpine")
            );
            let mut ids = inventory
                .containers
                .iter()
                .map(|container| container.id.clone())
                .collect::<Vec<_>>();
            ids.sort();
            baselines.push(ids);
        }
        struct ChildGuard(std::process::Child);
        impl Drop for ChildGuard {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--ignored",
                "--exact",
                "stack_release::tests::isolated_lan_crash_remote_child",
                "--nocapture",
            ])
            .env("APPDATA", &appdata)
            .env("DEVINGPANEL_TEST_LAN_CRASH_PROJECT", project.id.to_string())
            .env("DEVINGPANEL_TEST_LAN_CRASH_TAG", &tag)
            .env("DEVINGPANEL_TEST_LAN_CRASH_PHASE", "deploying")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt as _;
            command.creation_flags(0x0800_0000);
        }
        let mut child = ChildGuard(command.spawn().unwrap());
        let stdout = child.0.stdout.take().unwrap();
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if line.contains("START_INFLIGHT") {
                    let _ = sender.send(());
                    break;
                }
            }
        });
        receiver
            .recv_timeout(std::time::Duration::from_secs(1800))
            .expect("child did not reach the Compose start boundary");
        assert!(child.0.try_wait().unwrap().is_none());
        let batch = crate::batch_store::recent(Some(project.id))
            .unwrap()
            .remove(0);
        assert_eq!(batch.state, "running");
        assert_eq!(batch.targets[0].state, "running");
        assert_eq!(batch.targets[1].state, "waiting");
        assert!(batch.targets[1].release_task_id.is_none());
        let task_id = batch.targets[0].release_task_id.as_ref().unwrap();
        let task = crate::release_store::get_task(task_id).unwrap();
        assert_eq!(
            (task.phase.as_str(), task.outcome.as_str()),
            ("deploying", "running")
        );
        assert_eq!(
            crate::release_materials::recover_interrupted_batches().unwrap(),
            0
        );
        child.0.kill().unwrap();
        assert!(!child.0.wait().unwrap().success());
        assert_eq!(
            crate::release_materials::recover_interrupted_batches().unwrap(),
            1
        );
        assert_eq!(
            crate::release_materials::recover_interrupted_batches().unwrap(),
            0
        );
        let batch = crate::batch_store::get(&batch.id).unwrap();
        assert_eq!(batch.state, "unknown");
        assert_eq!(batch.targets[0].state, "unknown");
        assert_eq!(batch.targets[1].state, "waiting");
        let task = crate::release_store::get_task(task_id).unwrap();
        assert_eq!(
            (task.phase.as_str(), task.outcome.as_str()),
            ("deploying", "unknown")
        );
        assert!(crate::ssh::verify_start_completed(&task).is_err());
        assert!(crate::release::reconcile_interrupted_health(task_id).is_err());
        assert!(crate::batch_store::terminate_resolved(&batch.id).is_err());
        assert!(crate::batch_store::skip_failed(&batch.id, 1).is_err());
        for (target, before) in batch.targets.iter().zip(&baselines) {
            let mut after = crate::ssh::inventory(target.host.id)
                .unwrap()
                .containers
                .iter()
                .map(|container| container.id.clone())
                .collect::<Vec<_>>();
            after.sort();
            assert_eq!(&after, before);
        }
        let conn = crate::database::connect().unwrap();
        let pointers: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM project_current_version WHERE project_id = ?1",
                [project.id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(pointers, 0);
        eprintln!(
            "LAN Compose start-boundary crash stayed unknown: project={}, batch={}, task={}",
            project.id, batch.id, task_id
        );
    }

    #[test]
    #[ignore = "one-shot LAN batch; cancels a real partial SFTP write and leaves the unknown task for explicit audit"]
    fn isolated_lan_batch_partial_upload_retains_reserved_tar() {
        let appdata = std::path::PathBuf::from(std::env::var_os("APPDATA").unwrap())
            .canonicalize()
            .unwrap();
        let root = appdata.parent().unwrap();
        assert!(
            root.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("ui-lan-batch-")
        );
        assert_eq!(
            root.parent().unwrap(),
            std::env::current_dir()
                .unwrap()
                .join("target/isolated-tests")
                .canonicalize()
                .unwrap()
        );
        let project_dir = root.join(format!("upload-project-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir(&project_dir).unwrap();
        let source = std::env::current_dir()
            .unwrap()
            .join("tests/fixtures/stack-v2-success");
        for file in [
            "compose.yaml",
            "Dockerfile.web",
            "Dockerfile.worker",
            "probe-web.txt",
            "probe-worker.txt",
        ] {
            std::fs::copy(source.join(file), project_dir.join(file)).unwrap();
        }
        let yaml = std::fs::read_to_string(source.join("devingpanel.yaml")).unwrap();
        std::fs::write(
            project_dir.join("devingpanel.yaml"),
            yaml.replace(
                "name: stack-v2-success",
                &format!("name: stack-v2-lan-upload-{}", uuid::Uuid::new_v4().simple()),
            )
            .replace(
                "    - group: test\n      host: vps-smoke",
                "    - group: test\n      host: vps-smoke\n    - group: test\n      host: vps-second",
            ),
        )
        .unwrap();
        let project = crate::project_store::create_from_manifest(&project_dir).unwrap();
        assert_eq!(project.id, 1856624456);
        assert_eq!(project.host_ids, [1, 2]);
        let mut before = Vec::new();
        for (id, address, daemon, count) in [
            (1, "192.0.2.129", "00000000-0000-4000-8000-000000000008", 36),
            (2, "192.0.2.130", "00000000-0000-4000-8000-000000000011", 28),
        ] {
            let host = crate::host_store::list()
                .unwrap()
                .0
                .into_iter()
                .find(|host| host.id == id)
                .unwrap();
            assert_eq!(host.address, address);
            assert_eq!(
                crate::ssh::docker_identity_for_host(host).unwrap().1,
                daemon
            );
            let inventory = crate::ssh::inventory(id).unwrap();
            let mut containers = inventory
                .containers
                .iter()
                .map(|container| container.id.clone())
                .collect::<Vec<_>>();
            let mut images = inventory
                .images
                .iter()
                .map(|image| image.reference.clone())
                .collect::<Vec<_>>();
            containers.sort();
            images.sort();
            assert_eq!(containers.len(), count);
            assert!(images.contains(&"redis:7-alpine".to_owned()));
            assert!(images.contains(&"postgres:16-alpine".to_owned()));
            before.push((containers, images));
        }
        let local_containers = || {
            let output = std::process::Command::new("docker")
                .args(["ps", "-aq"])
                .output()
                .unwrap();
            assert!(output.status.success());
            let mut ids = String::from_utf8(output.stdout)
                .unwrap()
                .lines()
                .map(str::to_owned)
                .collect::<Vec<_>>();
            ids.sort();
            ids
        };
        let local_before = local_containers();
        assert_eq!(local_before.len(), 2);
        let tag = format!("lanpartial{}", uuid::Uuid::new_v4().simple());
        let preflight =
            preflight_batch(project.id, &tag, Arc::new(AtomicBool::new(false))).unwrap();
        assert_eq!(preflight.targets.len(), 2);
        for (target, daemon_id) in preflight.targets.iter().zip([
            "00000000-0000-4000-8000-000000000008",
            "00000000-0000-4000-8000-000000000011",
        ]) {
            assert_eq!(target.docker_daemon_id, daemon_id);
            let probe = crate::release_store::TaskSnapshot {
                id: "namespace-probe".into(),
                project_id: project.id,
                project_name: project.name.clone(),
                host: target.host.clone(),
                docker_endpoint: Some(target.docker_endpoint.clone()),
                docker_daemon_id: Some(target.docker_daemon_id.clone()),
                phase: "prepared".into(),
                outcome: "running".into(),
                health_result: None,
                detail: String::new(),
                started_at: String::new(),
                execution_finished_at: None,
                image_reference: preflight.plan.builds[0].reference.clone(),
            };
            crate::ssh::assert_empty_project_namespace(&probe).unwrap();
        }
        let cancel = Arc::new(AtomicBool::new(false));
        let mut interrupted = false;
        let error = super::run_batch_sequential(project.id, &preflight, cancel.clone(), |event| {
            let sent = event
                .strip_prefix("镜像传输 ")
                .and_then(|value| value.split_once('/'))
                .and_then(|(sent, _)| sent.parse::<u64>().ok());
            if sent.is_some_and(|sent| sent >= 128 * 1024) && !interrupted {
                interrupted = true;
                cancel.store(true, std::sync::atomic::Ordering::Relaxed);
            }
        })
        .err()
        .expect("partial upload cancellation must pause the batch");
        assert!(interrupted);
        let batch_id = error.batch_id.expect("created batch ID required");
        let batch = crate::batch_store::get(&batch_id).unwrap();
        assert_eq!(batch.state, "unknown");
        assert_eq!(batch.targets[0].state, "unknown");
        assert_eq!(batch.targets[1].state, "waiting");
        assert!(batch.targets[1].release_task_id.is_none());
        let task_id = batch.targets[0].release_task_id.as_ref().unwrap();
        let task = crate::release_store::get_task(task_id).unwrap();
        assert_eq!(
            (task.phase.as_str(), task.outcome.as_str()),
            ("uploading", "unknown")
        );
        let records = crate::release_store::uploaded_archives(task_id).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].state, "reserved");
        let inspection = crate::release::inspect_unknown_upload(task_id).unwrap();
        assert!(inspection.containers.is_empty());
        assert_eq!(inspection.archives.len(), 1);
        assert_eq!(inspection.archives[0].directory, records[0].directory);
        let actual = inspection.archives[0]
            .actual_bytes
            .expect("partial remote tar must be retained");
        assert!(actual > 0 && actual < records[0].bytes);
        assert_eq!(local_containers(), local_before);
        for (id, (containers, images)) in [(1, &before[0]), (2, &before[1])] {
            let inventory = crate::ssh::inventory(id).unwrap();
            let mut after_containers = inventory
                .containers
                .iter()
                .map(|container| container.id.clone())
                .collect::<Vec<_>>();
            let mut after_images = inventory
                .images
                .iter()
                .map(|image| image.reference.clone())
                .collect::<Vec<_>>();
            after_containers.sort();
            after_images.sort();
            assert_eq!(&after_containers, containers);
            assert_eq!(&after_images, images);
        }
        let conn = crate::database::connect().unwrap();
        let pointers: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM project_current_version WHERE project_id = ?1",
                [project.id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(pointers, 0);
        eprintln!(
            "LAN partial upload retained: project={}, batch={}, task={}, service={}, bytes={}/{}",
            project.id, batch_id, task_id, records[0].service, actual, records[0].bytes
        );
    }

    #[test]
    #[ignore = "one-shot explicit resume of an audited partial LAN batch upload; never retries an unverified task"]
    fn isolated_lan_batch_resumes_verified_partial_upload() {
        let appdata = std::path::PathBuf::from(std::env::var_os("APPDATA").unwrap())
            .canonicalize()
            .unwrap();
        let root = appdata.parent().unwrap();
        assert!(
            root.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("ui-lan-batch-")
        );
        assert_eq!(
            root.parent().unwrap(),
            std::env::current_dir()
                .unwrap()
                .join("target/isolated-tests")
                .canonicalize()
                .unwrap()
        );
        let batch_id =
            std::env::var("DEVINGPANEL_TEST_LAN_BATCH_ID").expect("audited batch ID required");
        let batch = crate::batch_store::get(&batch_id).unwrap();
        assert!(batch.project_id > 1_000_000_000);
        assert_eq!(batch.state, "unknown");
        assert_eq!(batch.targets.len(), 2);
        assert_eq!(batch.targets[0].state, "unknown");
        assert_eq!(batch.targets[1].state, "waiting");
        assert!(batch.targets[1].release_task_id.is_none());
        assert_eq!(batch.targets[0].host.address, "192.0.2.129");
        assert_eq!(batch.targets[1].host.address, "192.0.2.130");
        assert_eq!(
            batch.targets[0].docker_daemon_id,
            "00000000-0000-4000-8000-000000000008"
        );
        assert_eq!(
            batch.targets[1].docker_daemon_id,
            "00000000-0000-4000-8000-000000000011"
        );
        let project = crate::project_store::effective(batch.project_id).unwrap();
        assert!(
            std::path::Path::new(&project.path)
                .canonicalize()
                .unwrap()
                .starts_with(root)
        );
        let first_id = batch.targets[0].release_task_id.as_ref().unwrap();
        let first = crate::release_store::get_task(first_id).unwrap();
        assert_eq!(
            (first.phase.as_str(), first.outcome.as_str()),
            ("uploading", "unknown")
        );
        let records = crate::release_store::uploaded_archives(first_id).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].state, "reserved");
        let inspection = crate::release::inspect_unknown_upload(first_id).unwrap();
        assert!(inspection.containers.is_empty());
        assert_eq!(inspection.archives.len(), 1);
        assert_eq!(inspection.archives[0].directory, records[0].directory);
        let offset = inspection.archives[0]
            .actual_bytes
            .expect("retained partial tar required");
        assert!(offset > 0 && offset < records[0].bytes);
        crate::ssh::assert_empty_project_namespace(&first).unwrap();
        let mut baseline = Vec::new();
        for target in &batch.targets {
            let mut ids = crate::ssh::inventory(target.host.id)
                .unwrap()
                .containers
                .iter()
                .map(|container| container.id.clone())
                .collect::<Vec<_>>();
            ids.sort();
            assert_eq!(ids.len(), 8);
            baseline.push(ids);
        }
        let conn = crate::database::connect().unwrap();
        let pointers = || {
            conn.query_row(
                "SELECT COUNT(*) FROM project_current_version WHERE project_id = ?1",
                [project.id],
                |row| row.get::<_, i64>(0),
            )
            .unwrap()
        };
        assert_eq!(pointers(), 0);

        let progress = super::resume_unknown_batch_upload(
            &batch_id,
            first_id,
            Arc::new(AtomicBool::new(false)),
            |_| {},
        )
        .unwrap_or_else(|error| panic!("batch {}: {}", batch_id, error.message));
        assert!(matches!(
            progress,
            super::BatchProgress::AwaitingConfirmation { task_id, .. } if task_id == *first_id
        ));
        let batch = crate::batch_store::get(&batch_id).unwrap();
        assert_eq!(batch.state, "running");
        assert_eq!(batch.targets[0].state, "running");
        assert_eq!(batch.targets[1].state, "waiting");
        assert!(batch.targets[1].release_task_id.is_none());
        let first = crate::release_store::get_task(first_id).unwrap();
        assert_eq!(
            (
                first.phase.as_str(),
                first.outcome.as_str(),
                first.health_result.as_deref()
            ),
            ("checked", "blocked", Some("healthy"))
        );
        assert_eq!(pointers(), 0);
        let mut first_after = crate::ssh::inventory(1)
            .unwrap()
            .containers
            .iter()
            .map(|container| container.id.clone())
            .collect::<Vec<_>>();
        first_after.sort();
        assert_eq!(first_after.len(), baseline[0].len() + 4);
        assert!(baseline[0].iter().all(|id| first_after.contains(id)));
        let mut second_before = crate::ssh::inventory(2)
            .unwrap()
            .containers
            .iter()
            .map(|container| container.id.clone())
            .collect::<Vec<_>>();
        second_before.sort();
        assert_eq!(second_before, baseline[1]);
        assert_eq!(
            crate::release::confirm_healthy_and_complete(first_id).unwrap(),
            4
        );
        assert_eq!(pointers(), 1);

        let cancel = Arc::new(AtomicBool::new(false));
        let confirmed = super::reconfirm_batch(&batch_id, cancel.clone()).unwrap();
        let progress =
            super::continue_batch_sequential(&batch_id, Some(2), &confirmed, cancel, |_| {})
                .unwrap_or_else(|error| panic!("batch {}: {}", batch_id, error.message));
        let second_id = match progress {
            super::BatchProgress::AwaitingConfirmation { task_id, .. } => task_id,
            super::BatchProgress::Complete { .. } => panic!("second host skipped confirmation"),
        };
        let batch = crate::batch_store::get(&batch_id).unwrap();
        assert_eq!(batch.targets[0].state, "healthy");
        assert_eq!(batch.targets[1].state, "running");
        let second = crate::release_store::get_task(&second_id).unwrap();
        assert_eq!(
            (
                second.phase.as_str(),
                second.outcome.as_str(),
                second.health_result.as_deref()
            ),
            ("checked", "blocked", Some("healthy"))
        );
        let mut first_final = crate::ssh::inventory(1)
            .unwrap()
            .containers
            .iter()
            .map(|container| container.id.clone())
            .collect::<Vec<_>>();
        first_final.sort();
        assert_eq!(first_final, first_after);
        let mut second_after = crate::ssh::inventory(2)
            .unwrap()
            .containers
            .iter()
            .map(|container| container.id.clone())
            .collect::<Vec<_>>();
        second_after.sort();
        assert_eq!(second_after.len(), baseline[1].len() + 4);
        assert!(baseline[1].iter().all(|id| second_after.contains(id)));
        assert_eq!(pointers(), 1);
        assert_eq!(
            crate::release::confirm_healthy_and_complete(&second_id).unwrap(),
            4
        );
        let batch = crate::batch_store::get(&batch_id).unwrap();
        assert_eq!(batch.state, "complete");
        assert!(batch.targets.iter().all(|target| target.state == "healthy"));
        assert_eq!(pointers(), 2);
        for task_id in [first_id.as_str(), second_id.as_str()] {
            let archives = crate::release_store::uploaded_archives(task_id).unwrap();
            assert_eq!(archives.len(), 2);
            assert!(archives.iter().all(|archive| archive.state == "cleaned"));
        }
        eprintln!(
            "LAN batch resumed from remote offset {}: project={}, batch={}, tasks={},{}",
            offset, project.id, batch_id, first_id, second_id
        );
    }

    #[test]
    #[ignore = "one-shot isolated LAN batch; loses the client acknowledgement after real Compose startup"]
    fn isolated_lan_batch_start_ack_loss_blocks_next_host() {
        let appdata = std::path::PathBuf::from(std::env::var_os("APPDATA").unwrap())
            .canonicalize()
            .unwrap();
        let root = appdata.parent().unwrap();
        assert!(
            root.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("ui-lan-batch-")
        );
        assert_eq!(
            root.parent().unwrap(),
            std::env::current_dir()
                .unwrap()
                .join("target/isolated-tests")
                .canonicalize()
                .unwrap()
        );
        let project_dir = root.join(format!("start-project-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir(&project_dir).unwrap();
        let source = std::env::current_dir()
            .unwrap()
            .join("tests/fixtures/stack-v2-success");
        for file in [
            "compose.yaml",
            "Dockerfile.web",
            "Dockerfile.worker",
            "probe-web.txt",
            "probe-worker.txt",
        ] {
            std::fs::copy(source.join(file), project_dir.join(file)).unwrap();
        }
        let yaml = std::fs::read_to_string(source.join("devingpanel.yaml")).unwrap();
        std::fs::write(
            project_dir.join("devingpanel.yaml"),
            yaml.replace(
                "name: stack-v2-success",
                &format!("name: stack-v2-lan-start-{}", uuid::Uuid::new_v4().simple()),
            )
            .replace(
                "    - group: test\n      host: vps-smoke",
                "    - group: test\n      host: vps-smoke\n    - group: test\n      host: vps-second",
            ),
        )
        .unwrap();
        let project = crate::project_store::create_from_manifest(&project_dir).unwrap();
        assert!(project.id > 1_000_000_000);
        assert_eq!(project.host_ids, [1, 2]);
        let mut baseline = Vec::new();
        for (id, address) in [(1, "192.0.2.129"), (2, "192.0.2.130")] {
            let host = crate::host_store::list()
                .unwrap()
                .0
                .into_iter()
                .find(|host| host.id == id)
                .unwrap();
            assert_eq!(host.address, address);
            let mut ids = crate::ssh::inventory(id)
                .unwrap()
                .containers
                .iter()
                .map(|container| container.id.clone())
                .collect::<Vec<_>>();
            ids.sort();
            assert_eq!(ids.len(), 16);
            baseline.push(ids);
        }
        let tag = format!("lanstart{}", uuid::Uuid::new_v4().simple());
        let preflight =
            preflight_batch(project.id, &tag, Arc::new(AtomicBool::new(false))).unwrap();
        assert_eq!(preflight.targets.len(), 2);
        for (target, daemon_id) in preflight.targets.iter().zip([
            "00000000-0000-4000-8000-000000000008",
            "00000000-0000-4000-8000-000000000011",
        ]) {
            assert_eq!(target.docker_daemon_id, daemon_id);
            let probe = crate::release_store::TaskSnapshot {
                id: "namespace-probe".into(),
                project_id: project.id,
                project_name: project.name.clone(),
                host: target.host.clone(),
                docker_endpoint: Some(target.docker_endpoint.clone()),
                docker_daemon_id: Some(target.docker_daemon_id.clone()),
                phase: "prepared".into(),
                outcome: "running".into(),
                health_result: None,
                detail: String::new(),
                started_at: String::new(),
                execution_finished_at: None,
                image_reference: preflight.plan.builds[0].reference.clone(),
            };
            crate::ssh::assert_empty_project_namespace(&probe).unwrap();
        }
        let mut injected = false;
        let error = super::run_batch_sequential(
            project.id,
            &preflight,
            Arc::new(AtomicBool::new(false)),
            |event| {
                if event == "启动全部 Compose 服务" && !injected {
                    crate::ssh::inject_next_stack_start_ack_loss(project.id);
                    injected = true;
                }
            },
        )
        .err()
        .expect("lost start acknowledgement must block the batch");
        assert!(injected);
        assert!(error.message.contains("测试注入"), "{}", error.message);
        let batch_id = error.batch_id.expect("created batch ID required");
        let batch = crate::batch_store::get(&batch_id).unwrap();
        assert_eq!(batch.state, "unknown");
        assert_eq!(batch.targets[0].state, "unknown");
        assert_eq!(batch.targets[1].state, "waiting");
        assert!(batch.targets[1].release_task_id.is_none());
        let task_id = batch.targets[0].release_task_id.as_ref().unwrap();
        let task = crate::release_store::get_task(task_id).unwrap();
        assert_eq!(
            (task.phase.as_str(), task.outcome.as_str()),
            ("deploying", "unknown")
        );
        crate::ssh::verify_start_completed(&task).unwrap();
        assert!(crate::release::confirm_healthy_and_complete(task_id).is_err());
        assert!(crate::batch_store::terminate_resolved(&batch_id).is_err());
        assert!(crate::batch_store::skip_failed(&batch_id, 1).is_err());
        let records = crate::release_store::uploaded_archives(task_id).unwrap();
        assert_eq!(records.len(), 2);
        assert!(records.iter().all(|record| record.state == "cleaned"));
        let conn = crate::database::connect().unwrap();
        let pointers: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM project_current_version WHERE project_id = ?1",
                [project.id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(pointers, 0);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
        loop {
            let observed = crate::ssh::observe_release_project(&task).unwrap();
            if observed.containers.len() == 4
                && observed.containers.iter().all(|item| {
                    item.status == "running" && item.health.as_deref() == Some("healthy")
                })
            {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "remote stack did not become healthy after lost acknowledgement"
            );
            std::thread::sleep(std::time::Duration::from_secs(3));
        }
        for (id, before) in [(1, &baseline[0]), (2, &baseline[1])] {
            let mut after = crate::ssh::inventory(id)
                .unwrap()
                .containers
                .iter()
                .map(|container| container.id.clone())
                .collect::<Vec<_>>();
            after.sort();
            assert_eq!(after.len(), before.len() + if id == 1 { 4 } else { 0 });
            assert!(before.iter().all(|old| after.contains(old)));
        }
        assert_eq!(crate::batch_store::get(&batch_id).unwrap().state, "unknown");
        eprintln!(
            "LAN start acknowledgement lost: project={}, batch={}, task={}",
            project.id, batch_id, task_id
        );
    }

    #[test]
    #[ignore = "one-shot LAN batch with a completed Compose marker for subsequent GUI recovery"]
    fn isolated_lan_gui_marker_recovery_setup() {
        let appdata = std::path::PathBuf::from(std::env::var_os("APPDATA").unwrap())
            .canonicalize()
            .unwrap();
        let root = appdata.parent().unwrap();
        assert!(
            root.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("ui-lan-recovery-")
        );
        assert_eq!(
            root.parent().unwrap(),
            std::env::current_dir()
                .unwrap()
                .join("target/isolated-tests")
                .canonicalize()
                .unwrap()
        );
        let project_dir = root.join(format!("project-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir(&project_dir).unwrap();
        let source = std::env::current_dir()
            .unwrap()
            .join("tests/fixtures/stack-v2-success");
        for file in [
            "compose.yaml",
            "Dockerfile.web",
            "Dockerfile.worker",
            "probe-web.txt",
            "probe-worker.txt",
        ] {
            std::fs::copy(source.join(file), project_dir.join(file)).unwrap();
        }
        let yaml = std::fs::read_to_string(source.join("devingpanel.yaml")).unwrap();
        std::fs::write(
            project_dir.join("devingpanel.yaml"),
            yaml.replace(
                "name: stack-v2-success",
                &format!("name: stack-v2-lan-gui-recovery-{}", uuid::Uuid::new_v4().simple()),
            )
            .replace(
                "    - group: test\n      host: vps-smoke",
                "    - group: test\n      host: vps-smoke\n    - group: test\n      host: vps-second",
            ),
        )
        .unwrap();
        let project = crate::project_store::create_from_manifest(&project_dir).unwrap();
        assert!(project.id > 1_000_000_000);
        assert_eq!(project.host_ids, [1, 2]);
        let mut baseline = Vec::new();
        for (host_id, address, daemon, count) in [
            (1, "192.0.2.129", "00000000-0000-4000-8000-000000000008", 32),
            (2, "192.0.2.130", "00000000-0000-4000-8000-000000000011", 24),
        ] {
            let host = crate::host_store::list()
                .unwrap()
                .0
                .into_iter()
                .find(|host| host.id == host_id)
                .unwrap();
            assert_eq!(host.address, address);
            assert_eq!(
                crate::ssh::docker_identity_for_host(host).unwrap().1,
                daemon
            );
            let mut ids = crate::ssh::inventory(host_id)
                .unwrap()
                .containers
                .iter()
                .map(|container| container.id.clone())
                .collect::<Vec<_>>();
            ids.sort();
            assert_eq!(ids.len(), count);
            baseline.push(ids);
        }
        let tag = format!("languiresume{}", uuid::Uuid::new_v4().simple());
        let snapshot = preflight_batch(project.id, &tag, Arc::new(AtomicBool::new(false))).unwrap();
        assert_eq!(snapshot.targets.len(), 2);
        for (target, daemon) in snapshot.targets.iter().zip([
            "00000000-0000-4000-8000-000000000008",
            "00000000-0000-4000-8000-000000000011",
        ]) {
            assert_eq!(target.docker_daemon_id, daemon);
            let probe = crate::release_store::TaskSnapshot {
                id: "namespace-probe".into(),
                project_id: project.id,
                project_name: project.name.clone(),
                host: target.host.clone(),
                docker_endpoint: Some(target.docker_endpoint.clone()),
                docker_daemon_id: Some(target.docker_daemon_id.clone()),
                phase: "prepared".into(),
                outcome: "running".into(),
                health_result: None,
                detail: String::new(),
                started_at: String::new(),
                execution_finished_at: None,
                image_reference: snapshot.plan.builds[0].reference.clone(),
            };
            crate::ssh::assert_empty_project_namespace(&probe).unwrap();
        }
        let mut injected = false;
        let error = super::run_batch_sequential(
            project.id,
            &snapshot,
            Arc::new(AtomicBool::new(false)),
            |event| {
                if event == "启动全部 Compose 服务" && !injected {
                    crate::ssh::inject_next_stack_start_ack_loss(project.id);
                    injected = true;
                }
            },
        )
        .err()
        .expect("lost start acknowledgement must block the batch");
        assert!(injected);
        assert!(error.message.contains("测试注入"), "{}", error.message);
        let batch_id = error.batch_id.unwrap();
        let batch = crate::batch_store::get(&batch_id).unwrap();
        assert_eq!(batch.state, "unknown");
        assert_eq!(batch.targets[0].state, "unknown");
        assert_eq!(batch.targets[1].state, "waiting");
        assert!(batch.targets[1].release_task_id.is_none());
        let task_id = batch.targets[0].release_task_id.as_ref().unwrap();
        let task = crate::release_store::get_task(task_id).unwrap();
        assert_eq!(
            (task.phase.as_str(), task.outcome.as_str()),
            ("deploying", "unknown")
        );
        crate::ssh::verify_start_completed(&task).unwrap();
        assert!(crate::release::confirm_healthy_and_complete(task_id).is_err());
        let observed = crate::ssh::observe_release_project(&task).unwrap();
        assert_eq!(observed.containers.len(), 4);
        assert!(observed.containers.iter().all(|container| {
            container.status == "running" && container.health.as_deref() == Some("healthy")
        }));
        let conn = crate::database::connect().unwrap();
        let pointers: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM project_current_version WHERE project_id = ?1",
                [project.id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(pointers, 0);
        for (host_id, before, added) in [(1, &baseline[0], 4), (2, &baseline[1], 0)] {
            let mut after = crate::ssh::inventory(host_id)
                .unwrap()
                .containers
                .iter()
                .map(|container| container.id.clone())
                .collect::<Vec<_>>();
            after.sort();
            assert_eq!(after.len(), before.len() + added);
            assert!(before.iter().all(|id| after.contains(id)));
        }
        eprintln!(
            "LAN GUI marker recovery setup: project={}, batch={}, task={}",
            project.id, batch_id, task_id
        );
    }

    #[test]
    #[ignore = "read-only audit of a LAN second-host GUI continuation with unknown startup"]
    fn isolated_lan_gui_second_unknown_audit() {
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
        let batch = crate::batch_store::get("00000000-0000-4000-8000-000000000006").unwrap();
        assert_eq!(batch.project_id, 1856624454);
        assert_eq!(batch.state, "unknown");
        assert_eq!(batch.targets[0].state, "healthy");
        assert_eq!(batch.targets[1].state, "unknown");
        let first =
            crate::release_store::get_task(batch.targets[0].release_task_id.as_ref().unwrap())
                .unwrap();
        let second =
            crate::release_store::get_task(batch.targets[1].release_task_id.as_ref().unwrap())
                .unwrap();
        assert_eq!(
            (first.phase.as_str(), first.outcome.as_str()),
            ("checked", "complete")
        );
        assert_eq!(
            (second.phase.as_str(), second.outcome.as_str()),
            ("deploying", "unknown")
        );
        eprintln!("second task detail: {}", second.detail);
        for (target, address, daemon) in [
            (
                &batch.targets[0],
                "192.0.2.129",
                "00000000-0000-4000-8000-000000000008",
            ),
            (
                &batch.targets[1],
                "192.0.2.130",
                "00000000-0000-4000-8000-000000000011",
            ),
        ] {
            assert_eq!(target.host.address, address);
            assert_eq!(target.docker_daemon_id, daemon);
            let identity = crate::ssh::docker_identity_for_host(target.host.clone()).unwrap();
            assert_eq!(identity.1, daemon);
            let inventory = crate::ssh::inventory(target.host.id).unwrap();
            eprintln!("host {address}: containers={}", inventory.containers.len());
        }
        eprintln!(
            "second startup marker: {:?}",
            crate::ssh::verify_start_completed(&second)
        );
        let observed = crate::ssh::observe_release_project(&second).unwrap();
        eprintln!(
            "second project containers: {:?}",
            observed
                .containers
                .iter()
                .map(|c| (&c.service, &c.status, &c.health))
                .collect::<Vec<_>>()
        );
        let conn = crate::database::connect().unwrap();
        let pointers = conn
            .prepare(
                "SELECT docker_daemon_id, task_id FROM project_current_version
             WHERE project_id = ?1 ORDER BY docker_daemon_id",
            )
            .unwrap()
            .query_map([batch.project_id], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(
            pointers,
            [("00000000-0000-4000-8000-000000000008".into(), first.id)]
        );
    }

    #[test]
    #[ignore = "one-shot LAN batch; Compose succeeds but the remote completion step fails, leaving an unresolved start"]
    fn isolated_lan_batch_missing_start_marker_stays_unknown() {
        let appdata = std::path::PathBuf::from(std::env::var_os("APPDATA").unwrap())
            .canonicalize()
            .unwrap();
        let root = appdata.parent().unwrap();
        assert!(
            root.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("ui-lan-batch-")
        );
        assert_eq!(
            root.parent().unwrap(),
            std::env::current_dir()
                .unwrap()
                .join("target/isolated-tests")
                .canonicalize()
                .unwrap()
        );
        let project_dir = root.join(format!("missing-marker-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir(&project_dir).unwrap();
        let source = std::env::current_dir()
            .unwrap()
            .join("tests/fixtures/stack-v2-success");
        for file in [
            "compose.yaml",
            "Dockerfile.web",
            "Dockerfile.worker",
            "probe-web.txt",
            "probe-worker.txt",
        ] {
            std::fs::copy(source.join(file), project_dir.join(file)).unwrap();
        }
        let yaml = std::fs::read_to_string(source.join("devingpanel.yaml")).unwrap();
        std::fs::write(
            project_dir.join("devingpanel.yaml"),
            yaml.replace(
                "name: stack-v2-success",
                &format!("name: stack-v2-lan-marker-{}", uuid::Uuid::new_v4().simple()),
            )
            .replace(
                "    - group: test\n      host: vps-smoke",
                "    - group: test\n      host: vps-smoke\n    - group: test\n      host: vps-second",
            ),
        )
        .unwrap();
        let project = crate::project_store::create_from_manifest(&project_dir).unwrap();
        assert!(project.id > 1_000_000_000);
        assert_eq!(project.host_ids, [1, 2]);
        let mut baseline = Vec::new();
        for (id, address) in [(1, "192.0.2.129"), (2, "192.0.2.130")] {
            let host = crate::host_store::list()
                .unwrap()
                .0
                .into_iter()
                .find(|host| host.id == id)
                .unwrap();
            assert_eq!(host.address, address);
            let mut ids = crate::ssh::inventory(id)
                .unwrap()
                .containers
                .iter()
                .map(|container| container.id.clone())
                .collect::<Vec<_>>();
            ids.sort();
            assert_eq!(ids.len(), 20);
            baseline.push(ids);
        }
        let tag = format!("lanmarker{}", uuid::Uuid::new_v4().simple());
        let preflight =
            preflight_batch(project.id, &tag, Arc::new(AtomicBool::new(false))).unwrap();
        assert_eq!(preflight.targets.len(), 2);
        for (target, daemon_id) in preflight.targets.iter().zip([
            "00000000-0000-4000-8000-000000000008",
            "00000000-0000-4000-8000-000000000011",
        ]) {
            assert_eq!(target.docker_daemon_id, daemon_id);
            let probe = crate::release_store::TaskSnapshot {
                id: "namespace-probe".into(),
                project_id: project.id,
                project_name: project.name.clone(),
                host: target.host.clone(),
                docker_endpoint: Some(target.docker_endpoint.clone()),
                docker_daemon_id: Some(target.docker_daemon_id.clone()),
                phase: "prepared".into(),
                outcome: "running".into(),
                health_result: None,
                detail: String::new(),
                started_at: String::new(),
                execution_finished_at: None,
                image_reference: preflight.plan.builds[0].reference.clone(),
            };
            crate::ssh::assert_empty_project_namespace(&probe).unwrap();
        }
        let mut injected = false;
        let error = super::run_batch_sequential(
            project.id,
            &preflight,
            Arc::new(AtomicBool::new(false)),
            |event| {
                if event == "启动全部 Compose 服务" && !injected {
                    crate::ssh::inject_next_stack_completion_failure(project.id);
                    injected = true;
                }
            },
        )
        .err()
        .expect("missing completion marker must pause the batch");
        assert!(injected);
        let batch_id = error.batch_id.expect("created batch ID required");
        let batch = crate::batch_store::get(&batch_id).unwrap();
        assert_eq!(batch.state, "unknown");
        assert_eq!(batch.targets[0].state, "unknown");
        assert_eq!(batch.targets[1].state, "waiting");
        assert!(batch.targets[1].release_task_id.is_none());
        let task_id = batch.targets[0].release_task_id.as_ref().unwrap();
        let task = crate::release_store::get_task(task_id).unwrap();
        assert_eq!(
            (task.phase.as_str(), task.outcome.as_str()),
            ("deploying", "unknown")
        );
        let marker_error = crate::ssh::verify_start_completed(&task).unwrap_err();
        assert!(marker_error.contains("标记"), "{marker_error}");
        let reconcile_error = crate::release::reconcile_interrupted_health(task_id).unwrap_err();
        assert!(reconcile_error.contains("标记"), "{reconcile_error}");
        assert!(crate::release::confirm_healthy_and_complete(task_id).is_err());
        let conn = crate::database::connect().unwrap();
        let audits: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM release_task_health_reconciliations WHERE task_id = ?1",
                [task_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(audits, 0);
        let pointers: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM project_current_version WHERE project_id = ?1",
                [project.id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(pointers, 0);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
        loop {
            let observed = crate::ssh::observe_release_project(&task).unwrap();
            if observed.containers.len() == 4
                && observed.containers.iter().all(|item| {
                    item.status == "running" && item.health.as_deref() == Some("healthy")
                })
            {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "remote stack did not become healthy after completion failure"
            );
            std::thread::sleep(std::time::Duration::from_secs(3));
        }
        for (id, before) in [(1, &baseline[0]), (2, &baseline[1])] {
            let mut after = crate::ssh::inventory(id)
                .unwrap()
                .containers
                .iter()
                .map(|container| container.id.clone())
                .collect::<Vec<_>>();
            after.sort();
            assert_eq!(after.len(), before.len() + if id == 1 { 4 } else { 0 });
            assert!(before.iter().all(|old| after.contains(old)));
        }
        assert_eq!(crate::batch_store::get(&batch_id).unwrap().state, "unknown");
        eprintln!(
            "LAN missing marker stays unknown: project={}, batch={}, task={}",
            project.id, batch_id, task_id
        );
    }

    #[test]
    #[ignore = "one-shot LAN transport cut through a local SSH proxy and explicit batch resume"]
    fn isolated_lan_proxy_disconnects_upload_then_resumes() {
        use std::sync::atomic::Ordering;

        let appdata = std::path::PathBuf::from(std::env::var_os("APPDATA").unwrap())
            .canonicalize()
            .unwrap();
        let root = appdata.parent().unwrap();
        assert!(
            root.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("ui-lan-netcut-")
        );
        assert_eq!(
            root.parent().unwrap(),
            std::env::current_dir()
                .unwrap()
                .join("target/isolated-tests")
                .canonicalize()
                .unwrap()
        );
        let proxy = CutOnceSshProxy::new("192.0.2.130:22");
        let conn = crate::database::connect().unwrap();
        assert_eq!(
            conn.execute(
                "UPDATE hosts SET address = '127.0.0.1', port = ?1
                 WHERE id = 2 AND address = '192.0.2.130' AND port = 22",
                [proxy.port],
            )
            .unwrap(),
            1
        );
        let source = std::env::current_dir()
            .unwrap()
            .join("tests/fixtures/stack-v2-success");
        let project_dir = root.join("netcut-project");
        std::fs::create_dir(&project_dir).unwrap();
        for file in [
            "compose.yaml",
            "Dockerfile.web",
            "Dockerfile.worker",
            "probe-web.txt",
            "probe-worker.txt",
        ] {
            std::fs::copy(source.join(file), project_dir.join(file)).unwrap();
        }
        let yaml = std::fs::read_to_string(source.join("devingpanel.yaml")).unwrap();
        std::fs::write(
            project_dir.join("devingpanel.yaml"),
            yaml.replace(
                "name: stack-v2-success",
                &format!("name: stack-v2-netcut-{}", uuid::Uuid::new_v4().simple()),
            )
            .replace(
                "    - group: test\n      host: vps-smoke",
                "    - group: test\n      host: vps-smoke\n    - group: test\n      host: vps-second",
            ),
        )
        .unwrap();
        let project = crate::project_store::create_from_manifest(&project_dir).unwrap();
        assert!(project.id > 1_000_000_000);
        assert_eq!(project.host_ids, [1, 2]);
        let tag = format!("lannetcut{}", uuid::Uuid::new_v4().simple());
        let plan = preflight_batch(project.id, &tag, Arc::new(AtomicBool::new(false))).unwrap();
        assert_eq!(plan.targets[0].host.address, "192.0.2.129");
        assert_eq!(plan.targets[1].host.address, "127.0.0.1");
        assert_eq!(plan.targets[1].host.port, proxy.port);
        assert_eq!(
            plan.targets[0].docker_daemon_id,
            "00000000-0000-4000-8000-000000000008"
        );
        assert_eq!(
            plan.targets[1].docker_daemon_id,
            "00000000-0000-4000-8000-000000000011"
        );
        let mut baselines = Vec::new();
        for target in &plan.targets {
            let probe = crate::release_store::TaskSnapshot {
                id: "namespace-probe".into(),
                project_id: project.id,
                project_name: project.name.clone(),
                host: target.host.clone(),
                docker_endpoint: Some(target.docker_endpoint.clone()),
                docker_daemon_id: Some(target.docker_daemon_id.clone()),
                phase: "prepared".into(),
                outcome: "running".into(),
                health_result: None,
                detail: String::new(),
                started_at: String::new(),
                execution_finished_at: None,
                image_reference: plan.plan.builds[0].reference.clone(),
            };
            crate::ssh::assert_empty_project_namespace(&probe).unwrap();
            assert!(
                crate::ssh::observe_release_project(&probe)
                    .unwrap()
                    .containers
                    .is_empty()
            );
            let inventory = crate::ssh::inventory(target.host.id).unwrap();
            assert!(
                inventory
                    .images
                    .iter()
                    .any(|image| image.reference == "redis:7-alpine")
            );
            assert!(
                inventory
                    .images
                    .iter()
                    .any(|image| image.reference == "postgres:16-alpine")
            );
            let mut ids = inventory
                .containers
                .iter()
                .map(|container| container.id.clone())
                .collect::<Vec<_>>();
            ids.sort();
            baselines.push(ids);
        }
        let first = super::run_batch_sequential(
            project.id,
            &plan,
            Arc::new(AtomicBool::new(false)),
            |_| {},
        )
        .unwrap_or_else(|error| panic!("first host: {}", error.message));
        let (batch_id, first_task) = match first {
            super::BatchProgress::AwaitingConfirmation { batch_id, task_id } => (batch_id, task_id),
            super::BatchProgress::Complete { .. } => panic!("first host skipped confirmation"),
        };
        assert_eq!(
            crate::release::confirm_healthy_and_complete(&first_task).unwrap(),
            4
        );
        let confirmed =
            super::reconfirm_batch(&batch_id, Arc::new(AtomicBool::new(false))).unwrap();
        proxy.enabled.store(true, Ordering::Relaxed);
        let error = super::continue_batch_sequential(
            &batch_id,
            Some(2),
            &confirmed,
            Arc::new(AtomicBool::new(false)),
            |_| {},
        )
        .err()
        .expect("the proxy must interrupt one upload connection");
        assert_eq!(error.batch_id.as_deref(), Some(batch_id.as_str()));
        assert!(proxy.cut.load(Ordering::Relaxed));
        let interrupted = crate::batch_store::get(&batch_id).unwrap();
        assert_eq!(interrupted.state, "unknown");
        assert_eq!(interrupted.targets[0].state, "healthy");
        assert_eq!(interrupted.targets[1].state, "unknown");
        let second_task = interrupted.targets[1].release_task_id.as_ref().unwrap();
        let task = crate::release_store::get_task(second_task).unwrap();
        assert_eq!(
            (task.phase.as_str(), task.outcome.as_str()),
            ("uploading", "unknown")
        );
        assert!(proxy.offline.load(Ordering::Relaxed));
        assert!(crate::release::inspect_unknown_upload(second_task).is_err());
        let still_unknown = crate::batch_store::get(&batch_id).unwrap();
        assert_eq!(still_unknown.state, "unknown");
        assert_eq!(still_unknown.targets[0].state, "healthy");
        assert_eq!(
            still_unknown.targets[1].release_task_id.as_deref(),
            Some(second_task.as_str())
        );
        proxy.offline.store(false, Ordering::Relaxed);
        let inspection = crate::release::inspect_unknown_upload(second_task).unwrap();
        assert!(inspection.containers.is_empty());
        assert_eq!(inspection.archives.len(), 1);
        let offset = inspection.archives[0]
            .actual_bytes
            .expect("partial remote tar must be retained");
        assert!(offset > 0 && offset < inspection.archives[0].expected_bytes);
        let resumed = super::resume_unknown_batch_upload(
            &batch_id,
            second_task,
            Arc::new(AtomicBool::new(false)),
            |_| {},
        )
        .unwrap_or_else(|error| panic!("resume batch {batch_id}: {}", error.message));
        assert!(matches!(
            resumed,
            super::BatchProgress::AwaitingConfirmation { task_id, .. } if task_id == *second_task
        ));
        assert_eq!(
            crate::release::confirm_healthy_and_complete(second_task).unwrap(),
            4
        );
        let complete = crate::batch_store::get(&batch_id).unwrap();
        assert_eq!(complete.state, "complete");
        assert!(
            complete
                .targets
                .iter()
                .all(|target| target.state == "healthy")
        );
        let pointers: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM project_current_version WHERE project_id = ?1",
                [project.id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(pointers, 2);
        for (target, before) in complete.targets.iter().zip(&baselines) {
            let mut after = crate::ssh::inventory(target.host.id)
                .unwrap()
                .containers
                .iter()
                .map(|container| container.id.clone())
                .collect::<Vec<_>>();
            after.sort();
            assert_eq!(after.len(), before.len() + 4);
            assert!(before.iter().all(|id| after.contains(id)));
        }
        eprintln!(
            "LAN proxy cut/resume complete: project={}, batch={}, task={}, offset={}",
            project.id, batch_id, second_task, offset
        );
    }

    #[test]
    #[ignore = "read-only LAN residue refusal using an isolated copy of a completed retry batch"]
    fn isolated_lan_retry_rejects_existing_project_containers() {
        let appdata = std::path::PathBuf::from(std::env::var_os("APPDATA").unwrap())
            .canonicalize()
            .unwrap();
        let root = appdata.parent().unwrap();
        assert!(
            root.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("ui-lan-retry-residue-")
        );
        assert_eq!(
            root.parent().unwrap(),
            std::env::current_dir()
                .unwrap()
                .join("target/isolated-tests")
                .canonicalize()
                .unwrap()
        );
        let batch_id = "00000000-0000-4000-8000-000000000010";
        let before = crate::batch_store::get(batch_id).unwrap();
        assert_eq!(before.project_id, 1856624458);
        assert_eq!(before.state, "complete");
        assert!(
            before
                .targets
                .iter()
                .all(|target| target.state == "healthy")
        );
        assert_eq!(before.targets[1].host.address, "192.0.2.130");
        assert_eq!(
            before.targets[1].docker_daemon_id,
            "00000000-0000-4000-8000-000000000011"
        );
        let current_id = before.targets[1].release_task_id.as_ref().unwrap();
        let current = crate::release_store::get_task(current_id).unwrap();
        assert_eq!(current.outcome, "complete");
        let observed = crate::ssh::observe_release_project(&current).unwrap();
        assert_eq!(observed.containers.len(), 4);
        let mut baseline = crate::ssh::inventory(current.host.id)
            .unwrap()
            .containers
            .iter()
            .map(|container| container.id.clone())
            .collect::<Vec<_>>();
        baseline.sort();
        let conn = crate::database::connect().unwrap();
        conn.execute(
            "UPDATE release_tasks SET outcome = 'failed', health_result = 'failed'
             WHERE id = ?1 AND phase = 'checked' AND outcome = 'complete'",
            [current_id],
        )
        .unwrap();
        conn.execute(
            "UPDATE release_batch_targets SET state = 'failed'
             WHERE batch_id = ?1 AND ordinal = 1 AND state = 'healthy'",
            [batch_id],
        )
        .unwrap();
        conn.execute(
            "UPDATE release_batches SET state = 'paused'
             WHERE id = ?1 AND state = 'complete'",
            [batch_id],
        )
        .unwrap();
        let tasks_before: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM release_tasks WHERE project_id = ?1",
                [before.project_id],
                |row| row.get(0),
            )
            .unwrap();
        let attempts_before = before.targets[1].attempt_task_ids.len();
        let error = super::retry_failed_batch_sequential(
            batch_id,
            current.host.id,
            current_id,
            Arc::new(AtomicBool::new(false)),
            |_| {},
        )
        .err()
        .expect("remote project containers must block retry");
        assert!(error.message.contains("容器"));
        let after = crate::batch_store::get(batch_id).unwrap();
        assert_eq!(after.state, "paused");
        assert_eq!(after.targets[1].state, "failed");
        assert_eq!(
            after.targets[1].release_task_id.as_deref(),
            Some(current_id.as_str())
        );
        assert_eq!(after.targets[1].attempt_task_ids.len(), attempts_before);
        let tasks_after: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM release_tasks WHERE project_id = ?1",
                [before.project_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(tasks_after, tasks_before);
        let mut remote_after = crate::ssh::inventory(current.host.id)
            .unwrap()
            .containers
            .iter()
            .map(|container| container.id.clone())
            .collect::<Vec<_>>();
        remote_after.sort();
        assert_eq!(remote_after, baseline);
    }

    #[test]
    #[ignore = "one-shot LAN batch retry; requires an isolated copy of the audited LAN APPDATA"]
    fn isolated_lan_batch_retries_closed_pretransfer_attempt() {
        let appdata = std::path::PathBuf::from(std::env::var_os("APPDATA").unwrap())
            .canonicalize()
            .unwrap();
        let root = appdata.parent().unwrap();
        assert!(
            root.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("ui-lan-retry-")
        );
        assert_eq!(
            root.parent().unwrap(),
            std::env::current_dir()
                .unwrap()
                .join("target/isolated-tests")
                .canonicalize()
                .unwrap()
        );
        let source = std::env::current_dir()
            .unwrap()
            .join("tests/fixtures/stack-v2-success");
        let project_dir = root.join("retry-project");
        std::fs::create_dir(&project_dir).unwrap();
        for file in [
            "compose.yaml",
            "Dockerfile.web",
            "Dockerfile.worker",
            "probe-web.txt",
            "probe-worker.txt",
        ] {
            std::fs::copy(source.join(file), project_dir.join(file)).unwrap();
        }
        let yaml = std::fs::read_to_string(source.join("devingpanel.yaml")).unwrap();
        std::fs::write(
            project_dir.join("devingpanel.yaml"),
            yaml.replace(
                "name: stack-v2-success",
                &format!("name: stack-v2-retry-{}", uuid::Uuid::new_v4().simple()),
            )
            .replace(
                "    - group: test\n      host: vps-smoke",
                "    - group: test\n      host: vps-smoke\n    - group: test\n      host: vps-second",
            ),
        )
        .unwrap();
        let project = crate::project_store::create_from_manifest(&project_dir).unwrap();
        assert!(project.id > 1_000_000_000);
        assert_eq!(project.host_ids, [1, 2]);
        let tag = format!("lanretry{}", uuid::Uuid::new_v4().simple());
        let plan = preflight_batch(project.id, &tag, Arc::new(AtomicBool::new(false))).unwrap();
        let mut baselines = Vec::new();
        for (target, (address, daemon)) in plan.targets.iter().zip([
            ("192.0.2.129", "00000000-0000-4000-8000-000000000008"),
            ("192.0.2.130", "00000000-0000-4000-8000-000000000011"),
        ]) {
            assert_eq!(target.host.address, address);
            assert_eq!(target.docker_daemon_id, daemon);
            let probe = crate::release_store::TaskSnapshot {
                id: "namespace-probe".into(),
                project_id: project.id,
                project_name: project.name.clone(),
                host: target.host.clone(),
                docker_endpoint: Some(target.docker_endpoint.clone()),
                docker_daemon_id: Some(target.docker_daemon_id.clone()),
                phase: "prepared".into(),
                outcome: "running".into(),
                health_result: None,
                detail: String::new(),
                started_at: String::new(),
                execution_finished_at: None,
                image_reference: plan.plan.builds[0].reference.clone(),
            };
            crate::ssh::assert_empty_project_namespace(&probe).unwrap();
            assert!(
                crate::ssh::observe_release_project(&probe)
                    .unwrap()
                    .containers
                    .is_empty()
            );
            let inventory = crate::ssh::inventory(target.host.id).unwrap();
            assert!(
                inventory
                    .images
                    .iter()
                    .any(|image| image.reference == "redis:7-alpine")
            );
            assert!(
                inventory
                    .images
                    .iter()
                    .any(|image| image.reference == "postgres:16-alpine")
            );
            let mut ids = inventory
                .containers
                .iter()
                .map(|container| container.id.clone())
                .collect::<Vec<_>>();
            ids.sort();
            baselines.push(ids);
        }
        let first = super::run_batch_sequential(
            project.id,
            &plan,
            Arc::new(AtomicBool::new(false)),
            |_| {},
        )
        .unwrap_or_else(|error| panic!("first host: {}", error.message));
        let (batch_id, first_task) = match first {
            super::BatchProgress::AwaitingConfirmation { batch_id, task_id } => (batch_id, task_id),
            super::BatchProgress::Complete { .. } => panic!("first host skipped confirmation"),
        };
        assert_eq!(
            crate::release::confirm_healthy_and_complete(&first_task).unwrap(),
            4
        );
        let confirmed =
            super::reconfirm_batch(&batch_id, Arc::new(AtomicBool::new(false))).unwrap();
        let guard = Arc::new(crate::release_materials::acquire_guard(project.id).unwrap());
        let prepared = super::prepare_batch_host(
            project.id,
            &batch_id,
            &confirmed,
            1,
            guard,
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
        let failed_task = prepared.task_id.clone();
        drop(prepared);
        crate::batch_store::close_pretransfer(&failed_task).unwrap();
        let paused = crate::batch_store::get(&batch_id).unwrap();
        assert_eq!(paused.state, "paused");
        assert_eq!(paused.targets[1].state, "failed");
        assert_eq!(
            paused.targets[1].attempt_task_ids.as_slice(),
            std::slice::from_ref(&failed_task)
        );
        assert!(
            crate::ssh::observe_release_project(
                &crate::release_store::get_task(&failed_task).unwrap()
            )
            .unwrap()
            .containers
            .is_empty()
        );
        let retried = super::retry_failed_batch_sequential(
            &batch_id,
            2,
            &failed_task,
            Arc::new(AtomicBool::new(false)),
            |_| {},
        )
        .unwrap_or_else(|error| panic!("retry batch {batch_id}: {}", error.message));
        let new_task = match retried {
            super::BatchProgress::AwaitingConfirmation { task_id, .. } => task_id,
            super::BatchProgress::Complete { .. } => panic!("retry skipped confirmation"),
        };
        assert_ne!(new_task, failed_task);
        assert_eq!(
            crate::release::confirm_healthy_and_complete(&new_task).unwrap(),
            4
        );
        let completed = crate::batch_store::get(&batch_id).unwrap();
        assert_eq!(completed.state, "complete");
        assert!(
            completed
                .targets
                .iter()
                .all(|target| target.state == "healthy")
        );
        assert_eq!(
            completed.targets[1].attempt_task_ids,
            [failed_task.clone(), new_task.clone()]
        );
        assert_eq!(
            completed.targets[1].release_task_id.as_deref(),
            Some(new_task.as_str())
        );
        assert_eq!(
            crate::release_store::get_task(&failed_task)
                .unwrap()
                .outcome,
            "failed"
        );
        let conn = crate::database::connect().unwrap();
        let pointers: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM project_current_version WHERE project_id = ?1",
                [project.id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(pointers, 2);
        for (target, before) in completed.targets.iter().zip(&baselines) {
            let mut after = crate::ssh::inventory(target.host.id)
                .unwrap()
                .containers
                .iter()
                .map(|container| container.id.clone())
                .collect::<Vec<_>>();
            after.sort();
            assert_eq!(after.len(), before.len() + 4);
            assert!(before.iter().all(|id| after.contains(id)));
        }
        eprintln!(
            "LAN retry complete: project={}, batch={}, old={}, new={}",
            project.id, batch_id, failed_task, new_task
        );
    }

    #[test]
    #[ignore = "one-shot LAN first-host success followed by second-host environment drift"]
    fn isolated_lan_batch_second_host_drift_after_first_success() {
        let appdata = std::path::PathBuf::from(std::env::var_os("APPDATA").unwrap())
            .canonicalize()
            .unwrap();
        let root = appdata.parent().unwrap();
        assert!(
            root.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("ui-lan-drift-after-first-")
        );
        assert_eq!(
            root.parent().unwrap(),
            std::env::current_dir()
                .unwrap()
                .join("target/isolated-tests")
                .canonicalize()
                .unwrap()
        );
        let source = std::env::current_dir()
            .unwrap()
            .join("tests/fixtures/stack-v2-success");
        let project_dir = root.join("drift-project");
        std::fs::create_dir(&project_dir).unwrap();
        for file in [
            "compose.yaml",
            "Dockerfile.web",
            "Dockerfile.worker",
            "probe-web.txt",
            "probe-worker.txt",
        ] {
            std::fs::copy(source.join(file), project_dir.join(file)).unwrap();
        }
        let yaml = std::fs::read_to_string(source.join("devingpanel.yaml")).unwrap();
        std::fs::write(
            project_dir.join("devingpanel.yaml"),
            yaml.replace(
                "name: stack-v2-success",
                &format!("name: stack-v2-drift-{}", uuid::Uuid::new_v4().simple()),
            )
            .replace(
                "    - group: test\n      host: vps-smoke",
                "    - group: test\n      host: vps-smoke\n    - group: test\n      host: vps-second",
            ),
        )
        .unwrap();
        let project = crate::project_store::create_from_manifest(&project_dir).unwrap();
        assert!(project.id > 1_000_000_000);
        assert_eq!(project.host_ids, [1, 2]);
        let tag = format!("landrift{}", uuid::Uuid::new_v4().simple());
        let plan = preflight_batch(project.id, &tag, Arc::new(AtomicBool::new(false))).unwrap();
        let mut baselines = Vec::new();
        for (target, (address, daemon)) in plan.targets.iter().zip([
            ("192.0.2.129", "00000000-0000-4000-8000-000000000008"),
            ("192.0.2.130", "00000000-0000-4000-8000-000000000011"),
        ]) {
            assert_eq!(target.host.address, address);
            assert_eq!(target.docker_daemon_id, daemon);
            let probe = crate::release_store::TaskSnapshot {
                id: "namespace-probe".into(),
                project_id: project.id,
                project_name: project.name.clone(),
                host: target.host.clone(),
                docker_endpoint: Some(target.docker_endpoint.clone()),
                docker_daemon_id: Some(target.docker_daemon_id.clone()),
                phase: "prepared".into(),
                outcome: "running".into(),
                health_result: None,
                detail: String::new(),
                started_at: String::new(),
                execution_finished_at: None,
                image_reference: plan.plan.builds[0].reference.clone(),
            };
            crate::ssh::assert_empty_project_namespace(&probe).unwrap();
            let inventory = crate::ssh::inventory(target.host.id).unwrap();
            assert!(
                inventory
                    .images
                    .iter()
                    .any(|image| image.reference == "redis:7-alpine")
            );
            assert!(
                inventory
                    .images
                    .iter()
                    .any(|image| image.reference == "postgres:16-alpine")
            );
            let mut ids = inventory
                .containers
                .iter()
                .map(|container| container.id.clone())
                .collect::<Vec<_>>();
            ids.sort();
            baselines.push(ids);
        }
        let first = super::run_batch_sequential(
            project.id,
            &plan,
            Arc::new(AtomicBool::new(false)),
            |_| {},
        )
        .unwrap_or_else(|error| panic!("first host: {}", error.message));
        let (batch_id, first_task) = match first {
            super::BatchProgress::AwaitingConfirmation { batch_id, task_id } => (batch_id, task_id),
            super::BatchProgress::Complete { .. } => panic!("first host skipped confirmation"),
        };
        assert_eq!(
            crate::release::confirm_healthy_and_complete(&first_task).unwrap(),
            4
        );
        let (_, warning) = crate::env_store::save(
            project.id,
            Some(2),
            vec![crate::env_store::EnvInput {
                name: "DRIFT_PROBE".into(),
                value: Some("changed-after-first".into()),
                credential_id: None,
                sensitive: false,
            }],
        )
        .unwrap();
        assert!(warning.is_none());
        assert!(
            super::reconfirm_batch(&batch_id, Arc::new(AtomicBool::new(false)))
                .err()
                .expect("environment drift must block reconfirmation")
                .contains("冻结环境已变化")
        );
        let batch = crate::batch_store::get(&batch_id).unwrap();
        assert_eq!(batch.state, "running");
        assert_eq!(batch.targets[0].state, "healthy");
        assert_eq!(batch.targets[1].state, "waiting");
        assert!(batch.targets[1].release_task_id.is_none());
        let conn = crate::database::connect().unwrap();
        let pointer: String = conn
            .query_row(
                "SELECT task_id FROM project_current_version WHERE project_id = ?1",
                [project.id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(pointer, first_task);
        for (target, before) in batch.targets.iter().zip(&baselines) {
            let mut after = crate::ssh::inventory(target.host.id)
                .unwrap()
                .containers
                .iter()
                .map(|container| container.id.clone())
                .collect::<Vec<_>>();
            after.sort();
            assert_eq!(
                after.len(),
                before.len() + if target.host.id == 1 { 4 } else { 0 }
            );
            assert!(before.iter().all(|id| after.contains(id)));
        }
        crate::batch_store::terminate_resolved(&batch_id).unwrap();
        assert_eq!(
            crate::batch_store::get(&batch_id).unwrap().state,
            "terminated"
        );
        eprintln!(
            "LAN second-host drift blocked: project={}, batch={}, first={}",
            project.id, batch_id, first_task
        );
    }

    #[test]
    #[ignore = "one-shot LAN batch; first host healthy, second host worker becomes unhealthy and pauses without rollback"]
    fn isolated_lan_batch_second_host_health_failure_pauses() {
        let appdata = std::path::PathBuf::from(std::env::var_os("APPDATA").unwrap())
            .canonicalize()
            .unwrap();
        let root = appdata.parent().unwrap();
        assert!(
            root.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("ui-lan-batch-")
        );
        assert_eq!(
            root.parent().unwrap(),
            std::env::current_dir()
                .unwrap()
                .join("target/isolated-tests")
                .canonicalize()
                .unwrap()
        );
        let project_dir = root.join(format!("health-project-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir(&project_dir).unwrap();
        let source = std::env::current_dir()
            .unwrap()
            .join("tests/fixtures/stack-v2-success");
        let health_compose = std::env::current_dir()
            .unwrap()
            .join("tests/fixtures/stack-v2-host-health/compose.yaml");
        std::fs::copy(health_compose, project_dir.join("compose.yaml")).unwrap();
        for file in [
            "Dockerfile.web",
            "Dockerfile.worker",
            "probe-web.txt",
            "probe-worker.txt",
        ] {
            std::fs::copy(source.join(file), project_dir.join(file)).unwrap();
        }
        let yaml = std::fs::read_to_string(source.join("devingpanel.yaml")).unwrap();
        std::fs::write(
            project_dir.join("devingpanel.yaml"),
            yaml.replace(
                "name: stack-v2-success",
                &format!("name: stack-v2-lan-health-{}", uuid::Uuid::new_v4().simple()),
            )
            .replace(
                "    - group: test\n      host: vps-smoke",
                "    - group: test\n      host: vps-smoke\n    - group: test\n      host: vps-second",
            ),
        )
        .unwrap();
        let project = crate::project_store::create_from_manifest(&project_dir).unwrap();
        assert!(project.id > 1_000_000_000);
        assert_eq!(project.host_ids, [1, 2]);
        for (host_id, command) in [(1, "test -s /probe-worker.txt"), (2, "false")] {
            let (_, warning) = crate::env_store::save(
                project.id,
                Some(host_id),
                vec![crate::env_store::EnvInput {
                    name: "WORKER_HEALTH_COMMAND".into(),
                    value: Some(command.into()),
                    credential_id: None,
                    sensitive: false,
                }],
            )
            .unwrap();
            assert!(warning.is_none());
        }
        let mut baseline = Vec::new();
        for (id, address) in [(1, "192.0.2.129"), (2, "192.0.2.130")] {
            let host = crate::host_store::list()
                .unwrap()
                .0
                .into_iter()
                .find(|host| host.id == id)
                .unwrap();
            assert_eq!(host.address, address);
            let mut ids = crate::ssh::inventory(id)
                .unwrap()
                .containers
                .iter()
                .map(|container| container.id.clone())
                .collect::<Vec<_>>();
            ids.sort();
            baseline.push(ids);
        }
        let local_containers = || {
            let output = std::process::Command::new("docker")
                .args(["ps", "-aq"])
                .output()
                .unwrap();
            assert!(output.status.success());
            let mut ids = String::from_utf8(output.stdout)
                .unwrap()
                .lines()
                .map(str::to_owned)
                .collect::<Vec<_>>();
            ids.sort();
            ids
        };
        let local_before = local_containers();
        assert_eq!(local_before.len(), 2);
        let tag = format!("lanhealth{}", uuid::Uuid::new_v4().simple());
        let preflight =
            preflight_batch(project.id, &tag, Arc::new(AtomicBool::new(false))).unwrap();
        assert_eq!(preflight.targets.len(), 2);
        assert_ne!(preflight.env_files[0], preflight.env_files[1]);
        for (target, daemon_id) in preflight.targets.iter().zip([
            "00000000-0000-4000-8000-000000000008",
            "00000000-0000-4000-8000-000000000011",
        ]) {
            assert_eq!(target.docker_daemon_id, daemon_id);
            let probe = crate::release_store::TaskSnapshot {
                id: "namespace-probe".into(),
                project_id: project.id,
                project_name: project.name.clone(),
                host: target.host.clone(),
                docker_endpoint: Some(target.docker_endpoint.clone()),
                docker_daemon_id: Some(target.docker_daemon_id.clone()),
                phase: "prepared".into(),
                outcome: "running".into(),
                health_result: None,
                detail: String::new(),
                started_at: String::new(),
                execution_finished_at: None,
                image_reference: preflight.plan.builds[0].reference.clone(),
            };
            crate::ssh::assert_empty_project_namespace(&probe).unwrap();
        }
        let progress = super::run_batch_sequential(
            project.id,
            &preflight,
            Arc::new(AtomicBool::new(false)),
            |_| {},
        )
        .unwrap_or_else(|error| panic!("first host: {}", error.message));
        let (batch_id, first_id) = match progress {
            super::BatchProgress::AwaitingConfirmation { batch_id, task_id } => (batch_id, task_id),
            super::BatchProgress::Complete { .. } => panic!("first host skipped confirmation"),
        };
        let batch = crate::batch_store::get(&batch_id).unwrap();
        assert_eq!(batch.state, "running");
        assert_eq!(batch.targets[0].state, "running");
        assert_eq!(batch.targets[1].state, "waiting");
        assert!(batch.targets[1].release_task_id.is_none());
        let first = crate::release_store::get_task(&first_id).unwrap();
        assert_eq!(
            (
                first.phase.as_str(),
                first.outcome.as_str(),
                first.health_result.as_deref()
            ),
            ("checked", "blocked", Some("healthy"))
        );
        let mut first_before_second = crate::ssh::inventory(1)
            .unwrap()
            .containers
            .iter()
            .map(|container| container.id.clone())
            .collect::<Vec<_>>();
        first_before_second.sort();
        assert_eq!(first_before_second.len(), baseline[0].len() + 4);
        assert!(
            baseline[0]
                .iter()
                .all(|id| first_before_second.contains(id))
        );
        assert_eq!(
            crate::release::confirm_healthy_and_complete(&first_id).unwrap(),
            4
        );
        let conn = crate::database::connect().unwrap();
        let pointers = || {
            conn.prepare(
                "SELECT docker_daemon_id, task_id FROM project_current_version
                 WHERE project_id = ?1 ORDER BY docker_daemon_id",
            )
            .unwrap()
            .query_map([project.id], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
        };
        assert_eq!(
            pointers(),
            vec![(
                "00000000-0000-4000-8000-000000000008".into(),
                first_id.clone()
            )]
        );
        let cancel = Arc::new(AtomicBool::new(false));
        let confirmed = super::reconfirm_batch(&batch_id, cancel.clone()).unwrap();
        let error =
            super::continue_batch_sequential(&batch_id, Some(2), &confirmed, cancel, |_| {})
                .err()
                .expect("unhealthy worker must pause the batch");
        assert_eq!(error.batch_id.as_deref(), Some(batch_id.as_str()));
        let batch = crate::batch_store::get(&batch_id).unwrap();
        assert_eq!(batch.state, "paused");
        assert_eq!(batch.targets[0].state, "healthy");
        assert_eq!(batch.targets[1].state, "failed");
        let second_id = batch.targets[1].release_task_id.as_ref().unwrap();
        let second = crate::release_store::get_task(second_id).unwrap();
        assert_eq!(
            (
                second.phase.as_str(),
                second.outcome.as_str(),
                second.health_result.as_deref()
            ),
            ("checked", "blocked", Some("failed"))
        );
        assert!(crate::batch_store::skip_failed(&batch_id, 2).is_err());
        assert!(crate::batch_store::terminate_resolved(&batch_id).is_err());
        assert!(crate::release::confirm_healthy_and_complete(second_id).is_err());
        let observed = crate::ssh::observe_release_project(&second).unwrap();
        assert_eq!(observed.containers.len(), 4);
        let worker = observed
            .containers
            .iter()
            .find(|container| container.service == "worker")
            .unwrap();
        assert_eq!(worker.health.as_deref(), Some("unhealthy"));
        let mut first_after_second = crate::ssh::inventory(1)
            .unwrap()
            .containers
            .iter()
            .map(|container| container.id.clone())
            .collect::<Vec<_>>();
        first_after_second.sort();
        assert_eq!(first_after_second, first_before_second);
        let mut second_after = crate::ssh::inventory(2)
            .unwrap()
            .containers
            .iter()
            .map(|container| container.id.clone())
            .collect::<Vec<_>>();
        second_after.sort();
        assert_eq!(second_after.len(), baseline[1].len() + 4);
        assert!(baseline[1].iter().all(|id| second_after.contains(id)));
        assert_eq!(
            pointers(),
            vec![(
                "00000000-0000-4000-8000-000000000008".into(),
                first_id.clone()
            )]
        );
        assert_eq!(local_containers(), local_before);
        eprintln!(
            "LAN health failure paused: project={}, batch={}, tasks={},{}",
            project.id, batch_id, first_id, second_id
        );
    }

    #[test]
    #[ignore = "one-shot LAN first-host health failure for GUI resolution; requires fresh isolated APPDATA"]
    fn isolated_lan_batch_first_host_health_failure_for_gui() {
        let appdata = std::path::PathBuf::from(std::env::var_os("APPDATA").unwrap())
            .canonicalize()
            .unwrap();
        let root = appdata.parent().unwrap();
        assert!(
            root.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("ui-lan-first-fail-")
        );
        assert_eq!(
            root.parent().unwrap(),
            std::env::current_dir()
                .unwrap()
                .join("target/isolated-tests")
                .canonicalize()
                .unwrap()
        );
        let source = std::env::current_dir()
            .unwrap()
            .join("tests/fixtures/stack-v2-success");
        let project_dir = root.join("first-failure-project");
        std::fs::create_dir(&project_dir).unwrap();
        std::fs::copy(
            "tests/fixtures/stack-v2-host-health/compose.yaml",
            project_dir.join("compose.yaml"),
        )
        .unwrap();
        for file in [
            "Dockerfile.web",
            "Dockerfile.worker",
            "probe-web.txt",
            "probe-worker.txt",
        ] {
            std::fs::copy(source.join(file), project_dir.join(file)).unwrap();
        }
        let yaml = std::fs::read_to_string(source.join("devingpanel.yaml")).unwrap();
        std::fs::write(
            project_dir.join("devingpanel.yaml"),
            yaml.replace(
                "name: stack-v2-success",
                &format!("name: stack-v2-first-fail-{}", uuid::Uuid::new_v4().simple()),
            )
            .replace(
                "    - group: test\n      host: vps-smoke",
                "    - group: test\n      host: vps-second\n    - group: test\n      host: vps-smoke",
            ),
        )
        .unwrap();
        let project = crate::project_store::create_from_manifest(&project_dir).unwrap();
        assert!(project.id > 1_000_000_000);
        assert_eq!(project.host_ids, [2, 1]);
        for (host_id, command) in [(2, "false"), (1, "test -s /probe-worker.txt")] {
            let (_, warning) = crate::env_store::save(
                project.id,
                Some(host_id),
                vec![crate::env_store::EnvInput {
                    name: "WORKER_HEALTH_COMMAND".into(),
                    value: Some(command.into()),
                    credential_id: None,
                    sensitive: false,
                }],
            )
            .unwrap();
            assert!(warning.is_none());
        }
        let tag = format!("lanfirstfail{}", uuid::Uuid::new_v4().simple());
        let plan = preflight_batch(project.id, &tag, Arc::new(AtomicBool::new(false))).unwrap();
        assert_eq!(
            plan.targets
                .iter()
                .map(|target| target.host.id)
                .collect::<Vec<_>>(),
            [2, 1]
        );
        let mut baseline = Vec::new();
        for (target, (address, daemon)) in plan.targets.iter().zip([
            ("192.0.2.130", "00000000-0000-4000-8000-000000000011"),
            ("192.0.2.129", "00000000-0000-4000-8000-000000000008"),
        ]) {
            assert_eq!(target.host.address, address);
            assert_eq!(target.docker_daemon_id, daemon);
            let probe = crate::release_store::TaskSnapshot {
                id: "namespace-probe".into(),
                project_id: project.id,
                project_name: project.name.clone(),
                host: target.host.clone(),
                docker_endpoint: Some(target.docker_endpoint.clone()),
                docker_daemon_id: Some(target.docker_daemon_id.clone()),
                phase: "prepared".into(),
                outcome: "running".into(),
                health_result: None,
                detail: String::new(),
                started_at: String::new(),
                execution_finished_at: None,
                image_reference: plan.plan.builds[0].reference.clone(),
            };
            crate::ssh::assert_empty_project_namespace(&probe).unwrap();
            let inventory = crate::ssh::inventory(target.host.id).unwrap();
            assert!(
                inventory
                    .images
                    .iter()
                    .any(|image| image.reference == "redis:7-alpine")
            );
            assert!(
                inventory
                    .images
                    .iter()
                    .any(|image| image.reference == "postgres:16-alpine")
            );
            let mut ids = inventory
                .containers
                .iter()
                .map(|container| container.id.clone())
                .collect::<Vec<_>>();
            ids.sort();
            baseline.push(ids);
        }
        let error = super::run_batch_sequential(
            project.id,
            &plan,
            Arc::new(AtomicBool::new(false)),
            |_| {},
        )
        .err()
        .expect("unhealthy first host must pause the batch");
        let batch_id = error.batch_id.expect("failed batch ID must be retained");
        let batch = crate::batch_store::get(&batch_id).unwrap();
        assert_eq!(batch.state, "paused");
        assert_eq!(batch.targets[0].state, "failed");
        assert_eq!(batch.targets[1].state, "waiting");
        assert!(batch.targets[1].release_task_id.is_none());
        let task_id = batch.targets[0].release_task_id.as_ref().unwrap();
        let task = crate::release_store::get_task(task_id).unwrap();
        assert_eq!(
            (
                task.phase.as_str(),
                task.outcome.as_str(),
                task.health_result.as_deref()
            ),
            ("checked", "blocked", Some("failed"))
        );
        let observed = crate::ssh::observe_release_project(&task).unwrap();
        assert_eq!(observed.containers.len(), 4);
        assert_eq!(
            observed
                .containers
                .iter()
                .find(|item| item.service == "worker")
                .unwrap()
                .health
                .as_deref(),
            Some("unhealthy")
        );
        let conn = crate::database::connect().unwrap();
        let pointers: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM project_current_version WHERE project_id = ?1",
                [project.id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(pointers, 0);
        for (target, before) in batch.targets.iter().zip(&baseline) {
            let mut after = crate::ssh::inventory(target.host.id)
                .unwrap()
                .containers
                .iter()
                .map(|container| container.id.clone())
                .collect::<Vec<_>>();
            after.sort();
            assert_eq!(
                after.len(),
                before.len() + if target.host.id == 2 { 4 } else { 0 }
            );
            assert!(before.iter().all(|id| after.contains(id)));
        }
        eprintln!(
            "LAN first-host health failure: project={}, batch={}, task={}",
            project.id, batch_id, task_id
        );
    }

    #[test]
    #[ignore = "one-shot failed-task closure and skip after separately audited targeted container cleanup"]
    fn isolated_lan_batch_resolves_failed_second_host_after_absence() {
        let appdata = std::path::PathBuf::from(std::env::var_os("APPDATA").unwrap())
            .canonicalize()
            .unwrap();
        let root = appdata.parent().unwrap();
        assert!(
            root.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("ui-lan-batch-")
        );
        assert_eq!(
            root.parent().unwrap(),
            std::env::current_dir()
                .unwrap()
                .join("target/isolated-tests")
                .canonicalize()
                .unwrap()
        );
        let batch_id =
            std::env::var("DEVINGPANEL_TEST_LAN_BATCH_ID").expect("audited batch ID required");
        let batch = crate::batch_store::get(&batch_id).unwrap();
        assert_eq!(batch.project_id, 1856624452);
        assert_eq!(batch.state, "paused");
        assert_eq!(batch.targets.len(), 2);
        assert_eq!(batch.targets[0].state, "healthy");
        assert_eq!(batch.targets[1].state, "failed");
        assert_eq!(batch.targets[1].host.address, "192.0.2.130");
        assert_eq!(
            batch.targets[1].docker_daemon_id,
            "00000000-0000-4000-8000-000000000011"
        );
        let project = crate::project_store::effective(batch.project_id).unwrap();
        assert!(
            std::path::Path::new(&project.path)
                .canonicalize()
                .unwrap()
                .starts_with(root)
        );
        let first_id = batch.targets[0].release_task_id.as_ref().unwrap();
        let second_id = batch.targets[1].release_task_id.as_ref().unwrap();
        let second = crate::release_store::get_task(second_id).unwrap();
        assert_eq!(
            (
                second.phase.as_str(),
                second.outcome.as_str(),
                second.health_result.as_deref()
            ),
            ("checked", "blocked", Some("failed"))
        );
        assert!(crate::batch_store::skip_failed(&batch_id, 2).is_err());
        assert!(crate::batch_store::terminate_resolved(&batch_id).is_err());
        let host = batch.targets[1].host.clone();
        let (_, daemon_id) = crate::ssh::docker_identity_for_host(host.clone()).unwrap();
        assert_eq!(daemon_id, batch.targets[1].docker_daemon_id);
        assert!(
            crate::ssh::observe_release_project(&second)
                .unwrap()
                .containers
                .is_empty()
        );
        let inventory = crate::ssh::inventory(host.id).unwrap();
        assert_eq!(inventory.containers.len(), 20);
        assert!(
            inventory
                .containers
                .iter()
                .all(|container| !container.name.starts_with(&format!("dp{}-", project.id)))
        );
        crate::release::confirm_absent_and_close(second_id).unwrap();
        let failed = crate::release_store::get_task(second_id).unwrap();
        assert_eq!(
            (
                failed.phase.as_str(),
                failed.outcome.as_str(),
                failed.health_result.as_deref()
            ),
            ("checked", "failed", Some("failed"))
        );
        assert_eq!(crate::batch_store::get(&batch_id).unwrap().state, "paused");
        crate::batch_store::skip_failed(&batch_id, host.id).unwrap();
        let batch = crate::batch_store::get(&batch_id).unwrap();
        assert_eq!(batch.state, "complete");
        assert_eq!(batch.targets[0].state, "healthy");
        assert_eq!(batch.targets[1].state, "skipped");
        let conn = crate::database::connect().unwrap();
        let pointers = conn
            .prepare(
                "SELECT docker_daemon_id, task_id FROM project_current_version
                 WHERE project_id = ?1",
            )
            .unwrap()
            .query_map([project.id], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(
            pointers,
            [(
                "00000000-0000-4000-8000-000000000008".into(),
                first_id.clone()
            )]
        );
        eprintln!(
            "LAN unhealthy target resolved and skipped: project={}, batch={}, task={}",
            project.id, batch_id, second_id
        );
    }

    #[test]
    #[ignore = "read-only check that the pre-marker LAN release cannot prove Compose command completion"]
    fn isolated_lan_old_start_has_no_completion_marker() {
        let appdata = std::path::PathBuf::from(std::env::var_os("APPDATA").unwrap())
            .canonicalize()
            .unwrap();
        let root = appdata.parent().unwrap();
        assert!(
            root.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("ui-lan-batch-")
        );
        assert_eq!(
            root.parent().unwrap(),
            std::env::current_dir()
                .unwrap()
                .join("target/isolated-tests")
                .canonicalize()
                .unwrap()
        );
        let task = crate::release_store::get_task("00000000-0000-4000-8000-000000000026").unwrap();
        assert_eq!(task.project_id, 1856624449);
        assert_eq!(task.host.address, "192.0.2.129");
        assert_eq!(
            task.docker_daemon_id.as_deref(),
            Some("00000000-0000-4000-8000-000000000008")
        );
        let error = crate::ssh::verify_start_completed(&task).unwrap_err();
        assert!(error.contains("标记"), "{error}");
    }

    #[test]
    #[ignore = "one-shot explicit read-only recovery of an audited LAN batch with lost Compose start acknowledgement"]
    fn isolated_lan_batch_reconciles_unknown_start_then_continues() {
        let appdata = std::path::PathBuf::from(std::env::var_os("APPDATA").unwrap())
            .canonicalize()
            .unwrap();
        let root = appdata.parent().unwrap();
        assert!(
            root.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("ui-lan-batch-")
        );
        assert_eq!(
            root.parent().unwrap(),
            std::env::current_dir()
                .unwrap()
                .join("target/isolated-tests")
                .canonicalize()
                .unwrap()
        );
        let batch_id =
            std::env::var("DEVINGPANEL_TEST_LAN_BATCH_ID").expect("audited batch ID required");
        let batch = crate::batch_store::get(&batch_id).unwrap();
        assert_eq!(batch.project_id, 1856624450);
        assert_eq!(batch.state, "unknown");
        assert_eq!(batch.targets.len(), 2);
        assert_eq!(batch.targets[0].state, "unknown");
        assert_eq!(batch.targets[1].state, "waiting");
        assert!(batch.targets[1].release_task_id.is_none());
        assert_eq!(batch.targets[0].host.address, "192.0.2.129");
        assert_eq!(batch.targets[1].host.address, "192.0.2.130");
        assert_eq!(
            batch.targets[0].docker_daemon_id,
            "00000000-0000-4000-8000-000000000008"
        );
        assert_eq!(
            batch.targets[1].docker_daemon_id,
            "00000000-0000-4000-8000-000000000011"
        );
        let project = crate::project_store::effective(batch.project_id).unwrap();
        assert!(
            std::path::Path::new(&project.path)
                .canonicalize()
                .unwrap()
                .starts_with(root)
        );
        let first_id = batch.targets[0].release_task_id.as_ref().unwrap();
        let first = crate::release_store::get_task(first_id).unwrap();
        assert_eq!(
            (first.phase.as_str(), first.outcome.as_str()),
            ("deploying", "unknown")
        );
        crate::ssh::verify_start_completed(&first).unwrap();
        let observed = crate::ssh::observe_release_project(&first).unwrap();
        assert_eq!(observed.containers.len(), 4);
        assert!(
            observed.containers.iter().all(|item| {
                item.status == "running" && item.health.as_deref() == Some("healthy")
            })
        );
        let mut baseline = Vec::new();
        for (id, count) in [(1, 20), (2, 16)] {
            let mut ids = crate::ssh::inventory(id)
                .unwrap()
                .containers
                .iter()
                .map(|container| container.id.clone())
                .collect::<Vec<_>>();
            ids.sort();
            assert_eq!(ids.len(), count);
            baseline.push(ids);
        }
        let conn = crate::database::connect().unwrap();
        let pointers = || {
            conn.prepare(
                "SELECT docker_daemon_id, task_id FROM project_current_version
                 WHERE project_id = ?1 ORDER BY docker_daemon_id",
            )
            .unwrap()
            .query_map([project.id], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
        };
        assert!(pointers().is_empty());
        assert!(crate::release::confirm_healthy_and_complete(first_id).is_err());
        assert_eq!(
            crate::release::reconcile_interrupted_health(first_id).unwrap(),
            4
        );
        let audit: (i64, String) = conn
            .query_row(
                "SELECT observed_containers, source_phase
                 FROM release_task_health_reconciliations WHERE task_id = ?1",
                [first_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(audit, (4, "deploying".into()));
        let batch = crate::batch_store::get(&batch_id).unwrap();
        assert_eq!(batch.state, "running");
        assert_eq!(batch.targets[0].state, "running");
        assert_eq!(batch.targets[1].state, "waiting");
        assert!(pointers().is_empty());
        let recovered = crate::release_store::get_task(first_id).unwrap();
        assert_eq!(
            (
                recovered.phase.as_str(),
                recovered.outcome.as_str(),
                recovered.health_result.as_deref()
            ),
            ("checked", "blocked", Some("healthy"))
        );
        assert_eq!(
            crate::release::confirm_healthy_and_complete(first_id).unwrap(),
            4
        );
        assert_eq!(pointers().len(), 1);
        assert_eq!(pointers()[0].1, *first_id);
        let batch = crate::batch_store::get(&batch_id).unwrap();
        assert_eq!(batch.targets[0].state, "healthy");
        assert_eq!(batch.targets[1].state, "waiting");
        assert!(batch.targets[1].release_task_id.is_none());

        let cancel = Arc::new(AtomicBool::new(false));
        let confirmed = super::reconfirm_batch(&batch_id, cancel.clone()).unwrap();
        let progress =
            super::continue_batch_sequential(&batch_id, Some(2), &confirmed, cancel, |_| {})
                .unwrap_or_else(|error| panic!("batch {}: {}", batch_id, error.message));
        let second_id = match progress {
            super::BatchProgress::AwaitingConfirmation { task_id, .. } => task_id,
            super::BatchProgress::Complete { .. } => panic!("second host skipped confirmation"),
        };
        let batch = crate::batch_store::get(&batch_id).unwrap();
        assert_eq!(batch.targets[0].state, "healthy");
        assert_eq!(batch.targets[1].state, "running");
        let second = crate::release_store::get_task(&second_id).unwrap();
        assert_eq!(
            (
                second.phase.as_str(),
                second.outcome.as_str(),
                second.health_result.as_deref()
            ),
            ("checked", "blocked", Some("healthy"))
        );
        assert_eq!(pointers().len(), 1);
        let mut first_after = crate::ssh::inventory(1)
            .unwrap()
            .containers
            .iter()
            .map(|container| container.id.clone())
            .collect::<Vec<_>>();
        first_after.sort();
        assert_eq!(first_after, baseline[0]);
        let mut second_after = crate::ssh::inventory(2)
            .unwrap()
            .containers
            .iter()
            .map(|container| container.id.clone())
            .collect::<Vec<_>>();
        second_after.sort();
        assert_eq!(second_after.len(), baseline[1].len() + 4);
        assert!(baseline[1].iter().all(|id| second_after.contains(id)));
        assert_eq!(
            crate::release::confirm_healthy_and_complete(&second_id).unwrap(),
            4
        );
        let batch = crate::batch_store::get(&batch_id).unwrap();
        assert_eq!(batch.state, "complete");
        assert!(batch.targets.iter().all(|target| target.state == "healthy"));
        assert_eq!(pointers().len(), 2);
        assert_eq!(pointers()[0].1, *first_id);
        assert_eq!(pointers()[1].1, second_id);
        eprintln!(
            "LAN start-unknown batch reconciled and complete: project={}, batch={}, tasks={},{}",
            project.id, batch_id, first_id, second_id
        );
    }

    #[test]
    #[ignore = "cleans only an audited empty LAN GUI attempt and its test credentials"]
    fn isolated_lan_cleanup_empty_gui_attempt() {
        let appdata = std::path::PathBuf::from(std::env::var_os("APPDATA").unwrap())
            .canonicalize()
            .unwrap();
        let root = appdata.parent().unwrap();
        assert!(
            root.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("ui-lan-batch-")
        );
        assert_eq!(
            root.parent().unwrap(),
            std::env::current_dir()
                .unwrap()
                .join("target/isolated-tests")
                .canonicalize()
                .unwrap()
        );
        let expected: i64 = std::env::var("DEVINGPANEL_TEST_LAN_CLEANUP_PROJECT_ID")
            .unwrap()
            .parse()
            .unwrap();
        assert!(expected > 1_000_000_000);
        let conn = crate::database::connect().unwrap();
        for table in ["release_tasks", "release_batches", "rollback_tasks"] {
            let count: i64 = conn
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(count, 0, "{table} is not empty; do not clean this attempt");
        }
        let project = crate::project_store::effective(expected).unwrap();
        assert_eq!(project.name, format!("stack-v2-lan-{expected}"));
        assert!(
            std::path::Path::new(&project.path)
                .canonicalize()
                .unwrap()
                .starts_with(root)
        );
        assert_eq!(project.host_ids, [1, 2]);
        let hosts = crate::host_store::list().unwrap().0;
        assert_eq!(hosts.len(), 2);
        assert_eq!(hosts[0].address, "192.0.2.130");
        assert_eq!(hosts[1].address, "192.0.2.129");
        let reservation: (String, String, i64) = conn
            .query_row(
                "SELECT name, path, manifest_mode FROM projects WHERE id = ?1",
                [expected - 1],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            reservation,
            ("isolated-id-reservation".into(), ".".into(), 0)
        );
        crate::project_store::delete(expected).unwrap();
        crate::project_store::delete(expected - 1).unwrap();
        for host in hosts {
            crate::host_store::delete(host.id).unwrap();
        }
        for table in ["hosts", "projects", "pending_credentials"] {
            let count: i64 = conn
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(count, 0);
        }
    }

    #[test]
    #[ignore = "read-only preflight against two trusted VPS hosts; requires an isolated copied APPDATA and project"]
    fn isolated_vps_batch_drift_has_no_database_or_docker_side_effects() {
        let appdata = std::path::PathBuf::from(std::env::var_os("APPDATA").unwrap())
            .canonicalize()
            .unwrap();
        let root = appdata.parent().unwrap();
        assert!(
            root.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("batch-vps-preflight-")
        );
        assert_eq!(
            root.parent().unwrap(),
            std::env::current_dir()
                .unwrap()
                .join("target/isolated-tests")
                .canonicalize()
                .unwrap()
        );
        let project = crate::project_store::effective(900002).unwrap();
        assert_eq!(project.host_ids.len(), 2);
        assert!(
            std::path::Path::new(&project.path)
                .canonicalize()
                .unwrap()
                .starts_with(root)
        );
        let conn = crate::database::connect().unwrap();
        let counts = || {
            [
                "release_tasks",
                "release_batches",
                "release_uploaded_archives",
            ]
            .map(|table| {
                conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                    row.get::<_, i64>(0)
                })
                .unwrap()
            })
        };
        let before_db = counts();
        let pointers = || {
            conn.prepare(
                "SELECT docker_daemon_id, task_id FROM project_current_version
                WHERE project_id = 900002 ORDER BY docker_daemon_id",
            )
            .unwrap()
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
        };
        let before_pointers = pointers();
        assert_eq!(before_pointers.len(), 2);
        let local_images = || {
            let output = std::process::Command::new("docker")
                .args(["image", "ls", "--format", "{{.ID}}"])
                .output()
                .unwrap();
            assert!(output.status.success());
            let mut ids = String::from_utf8(output.stdout)
                .unwrap()
                .lines()
                .map(str::to_owned)
                .collect::<Vec<_>>();
            ids.sort();
            ids
        };
        let before_local = local_images();
        let remote = || {
            project
                .host_ids
                .iter()
                .map(|host_id| {
                    let inventory = crate::ssh::inventory(*host_id).unwrap();
                    let mut containers = inventory
                        .containers
                        .iter()
                        .map(|item| item.id.clone())
                        .collect::<Vec<_>>();
                    let mut images = inventory
                        .images
                        .iter()
                        .map(|item| item.reference.clone())
                        .collect::<Vec<_>>();
                    containers.sort();
                    images.sort();
                    (containers, images)
                })
                .collect::<Vec<_>>()
        };
        let before_remote = remote();
        assert_eq!(before_remote.len(), 2);
        eprintln!(
            "trusted VPS baseline: targets=2, containers={:?}, images={:?}",
            before_remote
                .iter()
                .map(|(containers, _)| containers.len())
                .collect::<Vec<_>>(),
            before_remote
                .iter()
                .map(|(_, images)| images.len())
                .collect::<Vec<_>>()
        );
        let tag = format!("preflight{}", uuid::Uuid::new_v4().simple());
        let preflight = preflight_batch(900002, &tag, Arc::new(AtomicBool::new(false))).unwrap();
        let dockerfile = std::path::Path::new(&project.path).join("Dockerfile.worker");
        let mut source = std::fs::read(&dockerfile).unwrap();
        source.extend_from_slice(b"\n# isolated drift injection\n");
        std::fs::write(&dockerfile, source).unwrap();
        assert!(
            super::revalidate_batch(900002, &preflight, Arc::new(AtomicBool::new(false))).is_err()
        );
        assert_eq!(counts(), before_db);
        assert_eq!(pointers(), before_pointers);
        assert_eq!(local_images(), before_local);
        assert_eq!(remote(), before_remote);
    }

    #[test]
    #[ignore = "read-only same-daemon alias check against a trusted VPS; requires an isolated copied APPDATA and project"]
    fn isolated_vps_same_daemon_alias_stops_before_batch_creation() {
        let appdata = std::path::PathBuf::from(std::env::var_os("APPDATA").unwrap())
            .canonicalize()
            .unwrap();
        let root = appdata.parent().unwrap();
        assert!(
            root.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("batch-vps-alias-")
        );
        assert_eq!(
            root.parent().unwrap(),
            std::env::current_dir()
                .unwrap()
                .join("target/isolated-tests")
                .canonicalize()
                .unwrap()
        );
        let project = crate::project_store::effective(900002).unwrap();
        assert_eq!(project.host_ids, [2, 1]);
        assert!(
            std::path::Path::new(&project.path)
                .canonicalize()
                .unwrap()
                .starts_with(root)
        );
        let conn = crate::database::connect().unwrap();
        let first = crate::host_store::connection_material(1).unwrap();
        assert_eq!(first.host.auth, "password");
        struct TestCredential(String);
        impl Drop for TestCredential {
            fn drop(&mut self) {
                if let Ok(entry) = keyring::Entry::new("devingPanel:ssh", &self.0) {
                    let _ = entry.delete_credential();
                }
            }
        }
        let credential = TestCredential(uuid::Uuid::new_v4().to_string());
        keyring::Entry::new("devingPanel:ssh", &credential.0)
            .unwrap()
            .set_password(&first.credential)
            .unwrap();
        conn.execute(
            "UPDATE hosts SET address=?1, port=?2, username=?3, auth=?4,
            trusted_fingerprint=?5, credential_id=?6 WHERE id=2",
            rusqlite::params![
                first.host.address,
                first.host.port,
                first.host.username,
                first.host.auth,
                first.host.trusted_fingerprint,
                &credential.0
            ],
        )
        .unwrap();
        let count = |table: &str| -> i64 {
            conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .unwrap()
        };
        let before_db = [
            count("release_tasks"),
            count("release_batches"),
            count("release_uploaded_archives"),
        ];
        let pointers = || {
            conn.prepare(
                "SELECT docker_daemon_id, task_id FROM project_current_version
            WHERE project_id=900002 ORDER BY docker_daemon_id",
            )
            .unwrap()
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
        };
        let before_pointers = pointers();
        let inventory = || {
            let snapshot = crate::ssh::inventory(1).unwrap();
            let mut containers = snapshot
                .containers
                .iter()
                .map(|item| item.id.clone())
                .collect::<Vec<_>>();
            let mut images = snapshot
                .images
                .iter()
                .map(|item| item.reference.clone())
                .collect::<Vec<_>>();
            containers.sort();
            images.sort();
            (containers, images)
        };
        let before_remote = inventory();
        let error = preflight_batch(
            900002,
            &format!("alias{}", uuid::Uuid::new_v4().simple()),
            Arc::new(AtomicBool::new(false)),
        )
        .err()
        .unwrap();
        assert!(error.contains("同一 Docker daemon"), "{error}");
        assert_eq!(inventory(), before_remote);
        assert_eq!(
            before_db,
            [
                count("release_tasks"),
                count("release_batches"),
                count("release_uploaded_archives")
            ]
        );
        assert_eq!(pointers(), before_pointers);
        assert!(!appdata.join("devingPanel").join("batch-archives").exists());
    }

    #[test]
    fn sequential_batch_waits_for_confirmation_before_next_target() {
        let target = |ordinal, state: &str, task_id: Option<&str>| BatchTargetRecord {
            ordinal,
            host: Host {
                id: ordinal as i64 + 1,
                name: format!("host-{ordinal}"),
                address: "127.0.0.1".into(),
                port: 22,
                username: "root".into(),
                group: "test".into(),
                auth: "password".into(),
                trusted_fingerprint: None,
            },
            platform: "linux/amd64".into(),
            docker_endpoint: "unix:///var/run/docker.sock".into(),
            docker_daemon_id: format!("DAEMON:{ordinal}"),
            env_sha256: Some(env_digest(b"").to_string()),
            state: state.into(),
            release_task_id: task_id.map(str::to_owned),
            task_outcome: None,
            attempt_task_ids: task_id.map(|id| vec![id.into()]).unwrap_or_default(),
        };
        let rows = [
            target(0, "running", Some("task")),
            target(1, "waiting", None),
        ];
        assert!(matches!(next_batch_target(&rows, |_| Ok(true)).unwrap(),
            BatchNext::AwaitingConfirmation(id) if id == "task"));
        assert!(next_batch_target(&rows, |_| Ok(false)).is_err());
        let rows = [
            target(0, "healthy", Some("task")),
            target(1, "waiting", None),
        ];
        assert!(matches!(
            next_batch_target(&rows, |_| panic!("confirmed task need not be read")).unwrap(),
            BatchNext::Waiting(1)
        ));
    }

    #[test]
    fn parallel_unknown_upload_targets_can_resume_independently() {
        let target = |ordinal, state: &str, outcome: Option<&str>| BatchTargetRecord {
            ordinal,
            host: Host {
                id: ordinal as i64 + 1,
                name: format!("host-{ordinal}"),
                address: "127.0.0.1".into(),
                port: 22,
                username: "root".into(),
                group: "test".into(),
                auth: "password".into(),
                trusted_fingerprint: None,
            },
            platform: "linux/amd64".into(),
            docker_endpoint: "unix:///var/run/docker.sock".into(),
            docker_daemon_id: format!("DAEMON:{ordinal}"),
            env_sha256: None,
            state: state.into(),
            release_task_id: (state != "waiting").then(|| format!("task-{ordinal}")),
            task_outcome: outcome.map(str::to_owned),
            attempt_task_ids: vec![],
        };
        let mut batch = BatchRecord {
            id: "batch".into(),
            project_id: 1,
            config_sha256: "digest".into(),
            parallelism: 2,
            registry_mode: false,
            state: "unknown".into(),
            created_at: String::new(),
            images: vec![],
            targets: vec![
                target(0, "unknown", Some("unknown")),
                target(1, "unknown", Some("unknown")),
                target(2, "waiting", None),
            ],
        };
        assert_eq!(
            super::resumable_unknown_target_index(&batch, "task-0"),
            Ok(0)
        );
        assert_eq!(
            super::resumable_unknown_target_index(&batch, "task-1"),
            Ok(1)
        );
        assert!(super::resumable_unknown_target_index(&batch, "other").is_err());
        batch.targets[0] = target(0, "running", Some("blocked"));
        assert_eq!(
            super::resumable_unknown_target_index(&batch, "task-1"),
            Ok(1)
        );
        batch.targets[0] = target(0, "running", Some("running"));
        assert!(super::resumable_unknown_target_index(&batch, "task-1").is_err());
        batch.targets[0] = target(0, "healthy", Some("complete"));
        batch.targets[2] = target(2, "unknown", Some("unknown"));
        assert!(super::resumable_unknown_target_index(&batch, "task-1").is_err());
        batch.targets[1] = target(1, "skipped", Some("failed"));
        assert_eq!(
            super::resumable_unknown_target_index(&batch, "task-2"),
            Ok(2)
        );
        batch.targets[1] = target(1, "unknown", Some("unknown"));
        assert!(super::resumable_unknown_target_index(&batch, "task-2").is_err());
    }

    #[test]
    fn batch_matrix_builds_once_per_service_and_architecture_then_maps_ordered_hosts() {
        let builds = ["web", "worker"].map(|service| BuildSpec {
            service: service.into(),
            dockerfile: format!("Dockerfile.{service}"),
            image: format!("app-{service}"),
            platform: "auto".into(),
            args: vec![],
        });
        let planned = builds
            .iter()
            .flat_map(|build| {
                ["linux/amd64", "linux/arm64"].map(|platform| PlannedImage {
                    platform: platform.into(),
                    reference: format!(
                        "{}:v1-{}",
                        build.image,
                        platform.split('/').nth(1).unwrap()
                    ),
                })
            })
            .collect::<Vec<_>>();
        let targets = [
            (7, "linux/arm64".into()),
            (3, "linux/amd64".into()),
            (9, "linux/arm64".into()),
        ];
        let plan = batch_plan_from(&builds, &[7, 3, 9], &targets, &planned).unwrap();
        assert_eq!(plan.builds.len(), 4);
        assert_eq!(
            plan.hosts
                .iter()
                .map(|host| host.host_id)
                .collect::<Vec<_>>(),
            [7, 3, 9]
        );
        assert_eq!(plan.hosts[0].images.len(), 2);
        assert_eq!(plan.hosts[0].images, plan.hosts[2].images);
        assert_ne!(plan.hosts[0].images, plan.hosts[1].images);
        assert_eq!(plan.hosts[0].images[0].0, "web");
        assert_eq!(plan.hosts[0].platform, "linux/arm64");
        assert!(batch_plan_from(&builds, &[3, 7, 9], &targets, &planned).is_err());
        assert!(batch_plan_from(&builds, &[7, 3, 9], &targets, &planned[..3]).is_err());
        assert!(
            batch_plan_from(
                &builds,
                &[7, 3, 7],
                &[
                    (7, "linux/arm64".into()),
                    (3, "linux/amd64".into()),
                    (7, "linux/arm64".into())
                ],
                &planned
            )
            .is_err()
        );
        let same_arch = [(7, "linux/amd64".into()), (3, "linux/amd64".into())];
        let same_images = planned
            .iter()
            .filter(|image| image.platform == "linux/amd64")
            .cloned()
            .collect::<Vec<_>>();
        let plan = batch_plan_from(&builds, &[7, 3], &same_arch, &same_images).unwrap();
        assert_eq!(plan.builds.len(), 2);
        assert_eq!(plan.hosts[0].images, plan.hosts[1].images);
        for build in &plan.builds {
            assert_eq!(
                spec_for_batch_build(&builds, build).unwrap().service,
                build.service
            );
        }
        let mut changed = plan.builds[0].clone();
        changed.reference = "wrong:v1-amd64".into();
        assert!(spec_for_batch_build(&builds, &changed).is_err());
    }

    #[test]
    fn batch_plan_rejects_same_count_yaml_target_drift_and_uses_one_manifest_snapshot() {
        let source = "version: 2\nname: app\ntag: '{timestamp}'\nbuilds:\n  - service: web\n    dockerfile: Dockerfile\n    image: app-web\n    platform: auto\n    args: {}\ndeploy:\n  compose:\n    files: [compose.yaml]\n  targets:\n    - group: test\n      host: one\n    - group: test\n      host: two\n";
        let manifest = project_config::parse(source).unwrap();
        let hosts = ["one", "two", "three"].map(|name| Host {
            id: match name {
                "one" => 1,
                "two" => 2,
                _ => 3,
            },
            name: name.into(),
            address: "127.0.0.1".into(),
            port: 22,
            username: "root".into(),
            group: "test".into(),
            auth: "password".into(),
            trusted_fingerprint: None,
        });
        let project = Project {
            id: 1,
            name: "app".into(),
            path: String::new(),
            dockerfile: "Dockerfile".into(),
            image: "app-web".into(),
            tag_rule: "{timestamp}".into(),
            platform: "auto".into(),
            service: "web".into(),
            compose_files: vec!["compose.yaml".into()],
            missing_compose_files: vec![],
            build_args: vec![],
            host_ids: vec![1, 2],
            manifest_mode: true,
            config_error: None,
        };
        let targets = [(1, "linux/amd64".into()), (2, "linux/amd64".into())];
        let plan = plan_batch_from_manifest(&project, &manifest, &hosts, "v1", &targets).unwrap();
        assert_eq!(plan.builds[0].reference, "app-web:v1-amd64");

        for changed in [
            source.replace("host: one", "host: three"),
            source.replace(
                "host: one\n    - group: test\n      host: two",
                "host: two\n    - group: test\n      host: one",
            ),
        ] {
            let changed = project_config::parse(&changed).unwrap();
            assert!(plan_batch_from_manifest(&project, &changed, &hosts, "v1", &targets).is_err());
        }

        let changed = project_config::parse(&source.replace("app-web", "new-web")).unwrap();
        let plan = plan_batch_from_manifest(&project, &changed, &hosts, "v1", &targets).unwrap();
        assert_eq!(plan.builds[0].service, "web");
        assert_eq!(plan.builds[0].reference, "new-web:v1-amd64");
        assert_eq!(plan.hosts[1].images[0].1, "new-web:v1-amd64");
    }

    #[test]
    fn batch_preflight_rejects_two_host_aliases_for_one_daemon() {
        let target = |host_id, docker_daemon_id: &str| BatchTargetIdentity {
            host_id,
            host: Host {
                id: host_id,
                name: format!("host-{host_id}"),
                address: "127.0.0.1".into(),
                port: 22,
                username: "root".into(),
                group: "test".into(),
                auth: "password".into(),
                trusted_fingerprint: Some("SHA256:verified".into()),
            },
            platform: "linux/amd64".into(),
            docker_endpoint: "unix:///var/run/docker.sock".into(),
            docker_daemon_id: docker_daemon_id.into(),
        };
        assert!(
            ensure_distinct_daemons(&[target(1, "DAEMON:ONE"), target(2, "DAEMON:ONE"),]).is_err()
        );
        assert!(
            ensure_distinct_daemons(&[target(1, "DAEMON:ONE"), target(2, "DAEMON:TWO"),]).is_ok()
        );
        let targets = [target(1, "DAEMON:ONE"), target(2, "DAEMON:TWO")];
        let mut hosts = targets
            .iter()
            .map(|target| target.host.clone())
            .collect::<Vec<_>>();
        assert!(same_batch_hosts(&targets, &hosts));
        hosts[1].trusted_fingerprint = Some("SHA256:changed".into());
        assert!(!same_batch_hosts(&targets, &hosts));
    }

    #[test]
    fn batch_preflight_freezes_per_host_image_variables_without_exposing_environment() {
        let images = [
            ("web".into(), "app-web:v1-amd64".into()),
            ("worker".into(), "app-worker:v1-amd64".into()),
        ];
        let file = batch_env_file(b"TOKEN=private\n", &images).unwrap();
        assert_eq!(file, b"TOKEN=private\nDEVINGPANEL_IMAGE_WEB=\"app-web:v1-amd64\"\nDEVINGPANEL_IMAGE_WORKER=\"app-worker:v1-amd64\"\n");
        assert!(batch_env_file(b"DEVINGPANEL_IMAGE_WEB=overridden\n", &images).is_err());
        assert!(
            batch_env_file(b"TOKEN=private", &images)
                .unwrap()
                .starts_with(b"TOKEN=private\nDEVINGPANEL_IMAGE_WEB=")
        );
    }

    #[test]
    fn batch_stack_requires_resolved_images_and_effective_healthchecks() {
        let host = BatchHost {
            host_id: 1,
            platform: "linux/amd64".into(),
            images: vec![("web".into(), "app:v1-amd64".into())],
        };
        let config = serde_json::json!({"services": {
            "web": {"image": "app:v1-amd64"},
            "redis": {"image": "redis:7", "healthcheck": {"test": ["CMD", "true"]}}
        }});
        let health = [("web".into(), "app:v1-amd64".into(), true)];
        assert!(verify_resolved_batch_images(&config, &host).is_ok());
        assert!(verify_batch_stack_config(&config, &host, &health).is_ok());
        assert!(verify_batch_stack_config(&config, &host, &[]).is_err());
        let mut changed = config.clone();
        changed["services"]["web"]["image"] = "app:wrong".into();
        assert!(verify_resolved_batch_images(&changed, &host).is_err());
        assert!(verify_batch_stack_config(&changed, &host, &health).is_err());
        let mut disabled = config.clone();
        disabled["services"]["web"]["healthcheck"] = serde_json::json!({"disable": true});
        assert!(verify_batch_stack_config(&disabled, &host, &health).is_err());
        let mut missing = config.clone();
        missing["services"]["redis"]
            .as_object_mut()
            .unwrap()
            .remove("healthcheck");
        assert!(verify_batch_stack_config(&missing, &host, &health).is_err());
        let mut compose_override = config.clone();
        compose_override["services"]["web"]["healthcheck"] =
            serde_json::json!({"test": ["CMD", "true"]});
        let no_image_health = [("web".into(), "app:v1-amd64".into(), false)];
        assert!(verify_batch_stack_config(&compose_override, &host, &no_image_health).is_ok());
        let wrong_build = [("web".into(), "app:other".into(), true)];
        assert!(verify_batch_stack_config(&compose_override, &host, &wrong_build).is_err());
    }

    #[test]
    fn partial_batch_cleans_known_archives_except_uncertain_load() {
        let archives = [0, 1, 2];
        let mut cleaned = Vec::new();
        assert!(
            cleanup_known_archives(&archives, Some(1), |id| {
                cleaned.push(*id);
                if *id == 0 {
                    Err("connection lost".into())
                } else {
                    Ok(())
                }
            })
            .is_err()
        );
        assert_eq!(cleaned, [0, 2]);
    }
}
