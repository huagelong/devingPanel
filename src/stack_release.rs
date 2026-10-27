use crate::build;
use crate::docker;
use crate::env_store;
use crate::host_store;
use crate::project_config::{self, BuildSpec};
use crate::project_store::{self, Project};
use crate::release;
use crate::release_materials;
use crate::release_store::{self, HealthResult, Intent, Outcome, Phase};
use crate::ssh::{self, DeploymentState, StackService};
use std::path::Path;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

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

pub fn run(
    project_id: i64,
    host_id: i64,
    base_tag: &str,
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
    let targets = release::probe_targets(&project, &[host_id], &hosts, cancel.clone())?;
    let platform = &targets[0].1;
    if !docker::build_platforms()?.contains(platform) {
        return Err("本机 default 构建器不支持目标平台。".into());
    }
    let planned = release::plan_images(&project, base_tag, &targets)?;
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
    release_store::create(&Intent {
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
    let mut built = Vec::new();
    for (spec, image) in manifest.builds.iter().zip(&planned) {
        on_event(&format!("构建镜像：{}", spec.service));
        let result = build::build_image(
            &build_project(&project, spec),
            image,
            cancel.clone(),
            |line| on_event(line),
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
        if let Err(error) = release_store::record_image_ids(&id, &spec.service, &archive.image_ids)
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
        let _ = release_store::settle(&id, Phase::Loaded, Outcome::Unknown, "临时归档清理待核实");
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
        Ok(value) => value,
        Err(error) => {
            let _ =
                release_store::settle(&id, Phase::Staging, Outcome::Unknown, "整栈部署文件待核实");
            return Err(error);
        }
    };
    release_store::advance(&id, Phase::Staging, Phase::Staged)?;
    release_store::advance(&id, Phase::Staged, Phase::Validating)?;
    let expected: Vec<StackService> = manifest
        .builds
        .iter()
        .zip(&planned)
        .zip(&uploaded)
        .map(|((spec, image), archive)| StackService {
            service: spec.service.clone(),
            reference: image.reference.clone(),
            image_ids: archive.image_ids.clone(),
        })
        .collect();
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
    use super::cleanup_known_archives;

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
