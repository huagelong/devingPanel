use crate::build::{self, BuiltImage, SavedArchive};
use crate::compose;
use crate::docker;
use crate::host_store::Host;
use crate::project_store::{self, NewProject, Project};
use crate::ssh::{self, TestResult};
use std::collections::BTreeSet;
use std::collections::HashSet;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

pub fn check_project_config(project: &Project, host_ids: &[i64]) -> Result<(), String> {
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
    finish_transfer(&archive, uploaded, |uploaded| {
        ssh::cleanup_uploaded_archive(host, uploaded)
    })
}

#[cfg(test)]
mod tests {
    use super::{
        check_project_config, checked_platform, finish_transfer, plan_images, verify_transfer,
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
        }
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
        };
        let mut uploaded = crate::ssh::UploadedArchive {
            directory: "/tmp/devingpanel.0123456789abcdef0123456789abcdef".into(),
            path: "/tmp/devingpanel.0123456789abcdef0123456789abcdef/image.tar".into(),
            bytes: 100,
            sha256: "a".repeat(64),
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
