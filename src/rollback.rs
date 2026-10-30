use crate::release_archive;
use crate::release_materials;
use crate::release_store;
use crate::rollback_store::{self, Phase};
use crate::ssh::{self, DeploymentState, ReleaseDockerIdentity, StackService};
use std::sync::{Arc, atomic::AtomicBool};

fn expected_services(
    confirmed: &[release_store::ConfirmedService],
    resolved: &[StackService],
) -> Result<Vec<StackService>, String> {
    if confirmed.is_empty() || confirmed.len() != resolved.len() {
        return Err("目标版本的服务集合与 Compose 配置不符。".into());
    }
    resolved
        .iter()
        .map(|service| {
            let matches: Vec<_> = confirmed
                .iter()
                .filter(|item| item.service == service.service)
                .collect();
            if matches.len() != 1 || matches[0].reference != service.reference {
                return Err("目标版本服务或镜像引用已变化。".into());
            }
            Ok(StackService {
                service: service.service.clone(),
                reference: service.reference.clone(),
                image_ids: vec![matches[0].image_id.clone()],
            })
        })
        .collect()
}

fn known_source_containers(
    source: &release_store::TaskSnapshot,
    target: &[release_store::ConfirmedService],
    observed: &[ssh::ObservedContainer],
) -> Result<(), String> {
    if observed.is_empty() {
        return Err("远端没有该项目的容器，回滚前状态不明确。".into());
    }
    let planned = if source.outcome == "complete" {
        let services = release_store::confirmed_services(&source.id)?;
        if services.len() != observed.len()
            || services.iter().any(|expected| {
                !observed.iter().any(|item| {
                    item.service == expected.service
                        && item.config_image == expected.reference
                        && item.image_id == expected.image_id
                })
            })
        {
            return Err("当前容器不属于源已完成版本，不能直接回滚。".into());
        }
        services
            .into_iter()
            .map(|item| (item.service, item.reference))
            .collect::<Vec<_>>()
    } else {
        release_store::planned_services(&source.id)?
            .into_iter()
            .map(|item| (item.service, item.reference))
            .collect::<Vec<_>>()
    };
    if observed.iter().any(|item| {
        !planned
            .iter()
            .any(|(name, reference)| name == &item.service && reference == &item.config_image)
            && !target.iter().any(|expected| {
                expected.service == item.service && expected.reference == item.config_image
            })
    }) {
        return Err("项目包含未记录的容器服务或镜像，不允许移除孤儿服务。".into());
    }
    Ok(())
}

struct PreparedRollback {
    source: release_store::TaskSnapshot,
    snapshot: release_materials::RestoredMaterials,
    built: Vec<release_store::HistoricalImage>,
    confirmed: Vec<release_store::ConfirmedService>,
    current: ssh::ReleaseObservation,
    remote: Vec<(String, ssh::HistoricalImageState)>,
}

#[derive(Clone)]
pub struct RollbackPreview {
    pub project_id: i64,
    pub source_id: String,
    pub target_id: String,
    pub project_name: String,
    pub host_name: String,
    pub host_endpoint: String,
    pub source_services: Vec<String>,
    pub observed: Vec<ssh::ObservedContainer>,
    pub target_services: Vec<String>,
    pub missing_images: Vec<String>,
}

fn prepare(intent: &rollback_store::Intent) -> Result<PreparedRollback, String> {
    let source = release_store::get_task(&intent.source_task_id)?;
    let target = release_store::get_task(&intent.target_task_id)?;
    if source.project_id != intent.project_id || target.project_id != intent.project_id {
        return Err("回滚任务项目已变化。".into());
    }
    let snapshot = release_materials::load(&target.id)?;
    let built = release_store::historical_images(&target.id)?;
    let confirmed = release_store::confirmed_services(&target.id)?;
    let current = ssh::observe_release_project(&source)?;
    known_source_containers(&source, &confirmed, &current.containers)?;
    for image in &built {
        release_archive::verify(&target.id, &image.service)?;
    }
    let remote = ssh::historical_image_states(&source, &built)?;
    if remote.len() != built.len()
        || remote
            .iter()
            .any(|(_, state)| *state == ssh::HistoricalImageState::Changed)
    {
        return Err("目标版本镜像 tag 已被其他镜像占用，禁止覆盖。".into());
    }
    Ok(PreparedRollback {
        source,
        snapshot,
        built,
        confirmed,
        current,
        remote,
    })
}

pub fn preview(source_id: &str) -> Result<RollbackPreview, String> {
    let intent = rollback_store::candidate(source_id)?;
    let ready = prepare(&intent)?;
    Ok(RollbackPreview {
        project_id: intent.project_id,
        source_id: intent.source_task_id,
        target_id: intent.target_task_id,
        project_name: ready.source.project_name,
        host_name: ready.source.host.name,
        host_endpoint: format!("{}:{}", ready.source.host.address, ready.source.host.port),
        source_services: ready
            .current
            .containers
            .iter()
            .map(|service| format!("{}: {}", service.service, service.config_image))
            .collect(),
        observed: ready.current.containers,
        target_services: ready
            .confirmed
            .iter()
            .map(|service| format!("{}: {}", service.service, service.reference))
            .collect(),
        missing_images: ready
            .remote
            .iter()
            .filter(|(_, state)| *state == ssh::HistoricalImageState::Missing)
            .map(|(service, _)| service.clone())
            .collect(),
    })
}

fn confirmed_preview_unchanged(
    observed: &[ssh::ObservedContainer],
    remote: &[(String, ssh::HistoricalImageState)],
    expected_containers: Option<&[ssh::ObservedContainer]>,
    expected_missing: Option<&[String]>,
) -> bool {
    expected_containers.is_none_or(|expected| {
        expected.len() == observed.len()
            && expected.iter().all(|original| observed.contains(original))
    }) && expected_missing.is_none_or(|expected| {
        expected.len()
            == remote
                .iter()
                .filter(|(_, state)| *state == ssh::HistoricalImageState::Missing)
                .count()
            && expected.iter().all(|service| {
                remote.iter().any(|(name, state)| {
                    name == service && *state == ssh::HistoricalImageState::Missing
                })
            })
    })
}

#[allow(dead_code)] // Internal test entry point; the GUI binds the confirmed target instead.
pub fn run(
    source_id: &str,
    cancel: Arc<AtomicBool>,
    on_event: impl FnMut(&str),
) -> Result<String, String> {
    run_confirmed(source_id, None, None, None, cancel, on_event)
}

pub fn run_confirmed(
    source_id: &str,
    confirmed_target_id: Option<&str>,
    confirmed_containers: Option<&[ssh::ObservedContainer]>,
    confirmed_missing_images: Option<&[String]>,
    cancel: Arc<AtomicBool>,
    mut on_event: impl FnMut(&str),
) -> Result<String, String> {
    let intent = rollback_store::candidate(source_id)?;
    if confirmed_target_id.is_some_and(|target| target != intent.target_task_id) {
        return Err("回滚目标在确认期间变化，请重新预检。".into());
    }
    let _guard = release_materials::acquire_guard(intent.project_id)?;
    let PreparedRollback {
        source,
        snapshot,
        built,
        confirmed,
        current,
        remote,
    } = prepare(&intent)?;
    if !confirmed_preview_unchanged(
        &current.containers,
        &remote,
        confirmed_containers,
        confirmed_missing_images,
    ) {
        return Err("回滚源容器或目标镜像在确认期间变化，请重新预检。".into());
    }
    rollback_store::create(&intent)?;
    let identity = ReleaseDockerIdentity {
        endpoint: source
            .docker_endpoint
            .as_deref()
            .ok_or("源任务缺少 Docker endpoint。")?,
        daemon_id: source
            .docker_daemon_id
            .as_deref()
            .ok_or("源任务缺少 Docker daemon。")?,
    };
    rollback_store::advance(&intent.id, Phase::Prepared, Phase::Restoring)?;
    for image in &built {
        if !remote.iter().any(|(service, state)| {
            service == &image.service && *state == ssh::HistoricalImageState::Missing
        }) {
            continue;
        }
        on_event(&format!("恢复历史镜像：{}", image.service));
        let restore = (|| {
            let archive = release_archive::copy_verified(
                &intent.target_task_id,
                &image.service,
                image.image_ids.clone(),
            )?;
            let directory = format!("/tmp/devingpanel.{}", uuid::Uuid::new_v4().simple());
            rollback_store::reserve_uploaded_archive(
                &intent.id,
                &image.service,
                &directory,
                archive.bytes,
                &archive.sha256,
            )?;
            let mut uploaded = ssh::upload_archive_at(
                source.host.clone(),
                &archive.path,
                &directory,
                cancel.clone(),
                |_, _| {},
            )?;
            if uploaded.bytes != archive.bytes || uploaded.sha256 != archive.sha256 {
                if ssh::cleanup_uploaded_archive(source.host.clone(), &uploaded).is_ok() {
                    let _ = rollback_store::advance_uploaded_archive(
                        &intent.id,
                        &image.service,
                        "reserved",
                        "cleaned",
                    );
                }
                return Err("历史镜像传输校验失败。".into());
            }
            if let Err(error) =
                rollback_store::confirm_uploaded_archive(&intent.id, &image.service, &uploaded)
            {
                let cleanup = ssh::cleanup_uploaded_archive(source.host.clone(), &uploaded);
                if cleanup.is_ok() {
                    let _ = rollback_store::advance_uploaded_archive(
                        &intent.id,
                        &image.service,
                        "reserved",
                        "cleaned",
                    );
                }
                return Err(if cleanup.is_ok() {
                    error
                } else {
                    format!("{error}；远端临时目录 {} 清理待核实", uploaded.directory)
                });
            }
            uploaded.image_ids = archive.image_ids.clone();
            ssh::load_uploaded_archive(
                source.host.clone(),
                identity,
                &uploaded,
                &image.reference,
                &image.platform,
                cancel.clone(),
            )?;
            rollback_store::advance_uploaded_archive(
                &intent.id,
                &image.service,
                "uploaded",
                "loaded",
            )?;
            ssh::cleanup_uploaded_archive(source.host.clone(), &uploaded)?;
            rollback_store::advance_uploaded_archive(
                &intent.id,
                &image.service,
                "loaded",
                "cleaned",
            )?;
            Ok::<_, String>(())
        })();
        if let Err(error) = restore {
            let _ = rollback_store::settle(
                &intent.id,
                Phase::Restoring,
                true,
                "历史镜像恢复结果待核实",
            );
            return Err(error);
        }
    }
    rollback_store::advance(&intent.id, Phase::Restoring, Phase::Restored)?;
    let built: Vec<StackService> = built
        .into_iter()
        .map(|image| StackService {
            service: image.service,
            reference: image.reference,
            image_ids: image.image_ids,
        })
        .collect();
    rollback_store::advance(&intent.id, Phase::Restored, Phase::Staging)?;
    on_event("上传历史 Compose 和环境快照");
    let staged = match ssh::stage_deployment(
        source.host.clone(),
        intent.project_id,
        &intent.id,
        &snapshot.compose_files,
        &snapshot.env_file,
        cancel.clone(),
    ) {
        Ok(staged) => staged,
        Err(error) => {
            let _ =
                rollback_store::settle(&intent.id, Phase::Staging, true, "历史材料暂存结果待核实");
            return Err(error);
        }
    };
    rollback_store::advance(&intent.id, Phase::Staging, Phase::Staged)?;
    rollback_store::advance(&intent.id, Phase::Staged, Phase::Validating)?;
    on_event("核对目标版本全部服务配置");
    let resolved = match ssh::validate_staged_stack(
        source.host.clone(),
        identity,
        &staged,
        intent.project_id,
        &built,
        cancel.clone(),
    ) {
        Ok(services) => services,
        Err(error) => {
            let _ = rollback_store::settle(
                &intent.id,
                Phase::Validating,
                true,
                "目标版本配置核实待处理",
            );
            return Err(error);
        }
    };
    let expected = match expected_services(&confirmed, &resolved) {
        Ok(value) => value,
        Err(error) => {
            let _ = rollback_store::settle(
                &intent.id,
                Phase::Validating,
                false,
                "目标版本服务集合不匹配",
            );
            return Err(error);
        }
    };
    rollback_store::advance(&intent.id, Phase::Validating, Phase::Validated)?;
    let refreshed = match ssh::observe_release_project(&source) {
        Ok(observation) => observation,
        Err(error) => {
            let _ =
                rollback_store::settle(&intent.id, Phase::Validated, true, "回滚前容器状态待核实");
            return Err(error);
        }
    };
    if let Err(error) = known_source_containers(&source, &confirmed, &refreshed.containers) {
        let _ = rollback_store::settle(
            &intent.id,
            Phase::Validated,
            false,
            "回滚前容器集合发生变化",
        );
        return Err(error);
    }
    if current.containers.len() != refreshed.containers.len()
        || current.containers.iter().any(|original| {
            !refreshed.containers.iter().any(|latest| {
                latest.service == original.service
                    && latest.config_image == original.config_image
                    && latest.image_id == original.image_id
            })
        })
    {
        let _ = rollback_store::settle(
            &intent.id,
            Phase::Validated,
            false,
            "回滚前镜像集合发生变化",
        );
        return Err("回滚预检后远端容器镜像集合已变化，未执行 Compose。".into());
    }
    rollback_store::advance(&intent.id, Phase::Validated, Phase::Deploying)?;
    on_event("恢复上一健康版本");
    if let Err(error) = ssh::start_staged_rollback(
        source.host.clone(),
        identity,
        &staged,
        intent.project_id,
        &built,
        &expected,
        cancel.clone(),
    ) {
        let _ = rollback_store::settle(
            &intent.id,
            Phase::Deploying,
            true,
            "回滚启动结果待核实，不自动重试",
        );
        return Err(error);
    }
    rollback_store::advance(&intent.id, Phase::Deploying, Phase::Checking)?;
    on_event("核对恢复后的全部服务健康状态");
    let health = match ssh::check_staged_stack(
        source.host.clone(),
        identity,
        &staged,
        intent.project_id,
        &built,
        &expected,
        cancel,
    ) {
        Ok(value) => value,
        Err(error) => {
            let _ = rollback_store::settle(&intent.id, Phase::Checking, true, "回滚健康结果待核实");
            return Err(error);
        }
    };
    let observation = if health == DeploymentState::Healthy {
        match ssh::observe_release_project(&source) {
            Ok(value) => value.containers,
            Err(error) => {
                let _ = rollback_store::settle(
                    &intent.id,
                    Phase::Checking,
                    true,
                    "回滚后容器复核待确认",
                );
                return Err(error);
            }
        }
    } else {
        Vec::new()
    };
    if let Err(error) =
        rollback_store::complete(&intent.id, health == DeploymentState::Healthy, &observation)
    {
        let _ = rollback_store::settle(&intent.id, Phase::Checking, true, "回滚完成记录待核实");
        return Err(error);
    }
    if health != DeploymentState::Healthy {
        return Err("目标版本未全部健康，回滚记录保持阻断。".into());
    }
    Ok(intent.id)
}

pub fn reconcile(id: &str) -> Result<usize, String> {
    let task = rollback_store::get(id)?;
    let _guard = release_materials::acquire_guard(task.project_id)?;
    if !matches!(
        (task.phase.as_str(), task.outcome.as_str()),
        ("checking", "unknown" | "running") | ("checked", "blocked")
    ) {
        return Err("该阶段不能只读核实回滚完成。".into());
    }
    let source = release_store::get_task(&task.source_task_id)?;
    let target = release_store::get_task(&task.target_task_id)?;
    if source.project_id != task.project_id || target.project_id != task.project_id {
        return Err("回滚任务关联项目已变化。".into());
    }
    let snapshot = release_materials::load(&target.id)?;
    let mut staged_identity = source.clone();
    staged_identity.id = task.id.clone();
    ssh::verify_staged_materials(&staged_identity, &snapshot)?;
    let built = release_store::historical_images(&target.id)?;
    for image in &built {
        release_archive::verify(&target.id, &image.service)?;
    }
    let remote = ssh::historical_image_states(&source, &built)?;
    if remote.len() != built.len()
        || remote
            .iter()
            .any(|(_, state)| *state != ssh::HistoricalImageState::Matching)
    {
        return Err("回滚目标的远端自有镜像缺失或已变化。".into());
    }
    let observed = ssh::observe_release_project(&source)?.containers;
    rollback_store::reconcile(&task, &observed)?;
    Ok(observed.len())
}

#[cfg(test)]
mod tests {
    use super::{confirmed_preview_unchanged, expected_services};
    use crate::release_store::ConfirmedService;
    use crate::ssh::{HistoricalImageState, ObservedContainer, StackService};

    #[test]
    fn rollback_confirmation_rejects_container_or_missing_image_drift() {
        let original = ObservedContainer {
            service: "web".into(),
            status: "running".into(),
            health: Some("healthy".into()),
            image_id: "sha256:old".into(),
            config_image: "app:new".into(),
        };
        let remote = vec![("web".into(), HistoricalImageState::Missing)];
        let missing = vec!["web".into()];
        assert!(confirmed_preview_unchanged(
            std::slice::from_ref(&original),
            &remote,
            Some(std::slice::from_ref(&original)),
            Some(&missing)
        ));
        let changed = ObservedContainer {
            image_id: "sha256:changed".into(),
            ..original.clone()
        };
        assert!(!confirmed_preview_unchanged(
            &[changed],
            &remote,
            Some(&[original]),
            Some(&missing)
        ));
        assert!(!confirmed_preview_unchanged(
            &[],
            &[("web".into(), HistoricalImageState::Matching)],
            None,
            Some(&missing)
        ));
    }

    #[test]
    fn rollback_requires_exact_confirmed_service_set() {
        let confirmed = [ConfirmedService {
            service: "web".into(),
            reference: "app:old".into(),
            image_id: format!("sha256:{}", "a".repeat(64)),
        }];
        let resolved = [StackService {
            service: "web".into(),
            reference: "app:old".into(),
            image_ids: vec![],
        }];
        assert_eq!(
            expected_services(&confirmed, &resolved).unwrap()[0].image_ids,
            [confirmed[0].image_id.clone()]
        );
        assert!(expected_services(&confirmed, &[]).is_err());
        assert!(
            expected_services(
                &confirmed,
                &[StackService {
                    reference: "app:new".into(),
                    ..resolved[0].clone()
                }]
            )
            .is_err()
        );
    }
}
