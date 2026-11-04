use crate::build::SavedArchive;
use crate::preferences;
use crate::release_store;
use crate::release_store::{RetentionClass, RetentionPlan};
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

fn archive_path(root: &Path, task_id: &str, service: &str) -> Result<PathBuf, String> {
    uuid::Uuid::parse_str(task_id).map_err(|_| "发布任务 ID 无效。")?;
    if service.is_empty() || service.len() > 128 {
        return Err("归档服务名无效。".into());
    }
    let key = format!("{:x}", Sha256::digest(service.as_bytes()));
    Ok(root.join(task_id).join(format!("{key}.tar")))
}

fn checked_existing_archive_path(
    root: &Path,
    task_id: &str,
    service: &str,
) -> Result<PathBuf, String> {
    let path = archive_path(root, task_id, service)?;
    let dir = path.parent().ok_or("本机归档目录无效。")?;
    for (item, directory) in [(root, true), (dir, true), (path.as_path(), false)] {
        let meta = fs::symlink_metadata(item).map_err(|_| "本机归档路径不可用。")?;
        if meta.file_type().is_symlink()
            || (directory && !meta.is_dir())
            || (!directory && !meta.is_file())
        {
            return Err("本机归档路径不可信。".into());
        }
    }
    let parent = dir.canonicalize().map_err(|_| "无法核对归档目录。")?;
    let canonical = path.canonicalize().map_err(|_| "无法核对镜像归档。")?;
    if parent.parent()
        != Some(
            root.canonicalize()
                .map_err(|_| "无法核对归档根目录。")?
                .as_path(),
        )
        || canonical.parent() != Some(parent.as_path())
    {
        return Err("本机归档路径越出托管目录。".into());
    }
    Ok(path)
}

fn archive_root() -> Result<PathBuf, String> {
    Ok(preferences::data_dir()
        .ok_or("无法确定本机配置目录。")?
        .join("release-archives"))
}

fn batch_archive_root() -> Result<PathBuf, String> {
    Ok(preferences::data_dir()
        .ok_or("无法确定本机配置目录。")?
        .join("batch-archives"))
}

fn batch_archive_key(service: &str, platform: &str) -> Result<String, String> {
    if service.is_empty()
        || service.len() > 100
        || !matches!(platform, "linux/amd64" | "linux/arm64")
    {
        return Err("批次归档服务或架构无效。".into());
    }
    Ok(format!("{service}:{platform}"))
}

fn retain_in(
    root: &Path,
    task_id: &str,
    service: &str,
    archive: &SavedArchive,
    record: impl FnOnce(u64, &str) -> Result<(), String>,
) -> Result<(), String> {
    let target = archive_path(root, task_id, service)?;
    let directory = target.parent().expect("archive has a task directory");
    let mut builder = fs::DirBuilder::new();
    builder.recursive(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    for path in [root, directory] {
        match builder.create(path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(_) => return Err("无法创建本机发布归档目录。".into()),
        }
        let metadata = fs::symlink_metadata(path).map_err(|_| "无法核对本机归档目录。")?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err("本机归档目录不可信。".into());
        }
    }
    let root_path = root.canonicalize().map_err(|_| "无法核对归档根目录。")?;
    let directory_path = directory
        .canonicalize()
        .map_err(|_| "无法核对归档任务目录。")?;
    let expected_parent = root
        .parent()
        .ok_or("归档根目录无父目录。")?
        .canonicalize()
        .map_err(|_| "无法核对归档父目录。")?;
    if root_path.parent() != Some(expected_parent.as_path())
        || directory_path.parent() != Some(root_path.as_path())
    {
        return Err("归档任务目录越出托管根目录。".into());
    }
    let partial = directory.join(format!("{}.partial", uuid::Uuid::new_v4().simple()));
    let result = (|| {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut output = options
            .open(&partial)
            .map_err(|_| "无法创建本机镜像归档。")?;
        let mut input = File::open(&archive.path).map_err(|_| "无法读取待保留的镜像归档。")?;
        let mut digest = Sha256::new();
        let mut bytes = 0u64;
        let mut buffer = [0u8; 64 * 1024];
        loop {
            let count = input.read(&mut buffer).map_err(|_| "无法读取镜像归档。")?;
            if count == 0 {
                break;
            }
            output
                .write_all(&buffer[..count])
                .map_err(|_| "本机归档写入失败。")?;
            digest.update(&buffer[..count]);
            bytes += count as u64;
        }
        if bytes != archive.bytes || format!("{:x}", digest.finalize()) != archive.sha256 {
            return Err("待保留的镜像归档已变化。".into());
        }
        output.sync_all().map_err(|_| "无法持久化本机镜像归档。")?;
        fs::hard_link(&partial, &target).map_err(|_| "无法确认本机镜像归档（可能已存在）。")?;
        if fs::remove_file(&partial).is_err() {
            let _ = fs::remove_file(&target);
            return Err("无法清理本机镜像归档临时文件。".into());
        }
        #[cfg(unix)]
        if File::open(directory)
            .and_then(|dir| dir.sync_all())
            .is_err()
        {
            let _ = fs::remove_file(&target);
            return Err("无法持久化本机镜像归档目录。".into());
        }
        if let Err(error) = record(bytes, &archive.sha256) {
            if fs::remove_file(&target).is_err() {
                return Err(format!("{error}；本机归档清理失败，需检查任务 {task_id}。"));
            }
            return Err(error);
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(partial);
    }
    result
}

pub fn retain(task_id: &str, service: &str, archive: &SavedArchive) -> Result<(), String> {
    retain_in(
        &archive_root()?,
        task_id,
        service,
        archive,
        |bytes, sha256| release_store::record_archive(task_id, service, bytes, sha256),
    )
}

#[allow(dead_code)] // Batch execution will retain each shared archive before any target upload.
pub fn retain_shared(
    batch_id: &str,
    service: &str,
    platform: &str,
    archive: &SavedArchive,
) -> Result<(), String> {
    let key = batch_archive_key(service, platform)?;
    retain_in(
        &batch_archive_root()?,
        batch_id,
        &key,
        archive,
        |bytes, sha256| {
            crate::batch_store::record_archive(
                batch_id,
                service,
                platform,
                bytes,
                sha256,
                &archive.image_ids,
            )
        },
    )
}

#[allow(dead_code)] // A read-only check; future transfer must copy and verify again.
pub fn verify_shared(batch_id: &str, service: &str, platform: &str) -> Result<(), String> {
    let (bytes, sha256, _, _) = crate::batch_store::archive_record(batch_id, service, platform)?;
    verify_shared_recorded(batch_id, service, platform, bytes, &sha256)
}

pub(crate) fn verify_shared_recorded(
    batch_id: &str,
    service: &str,
    platform: &str,
    bytes: u64,
    sha256: &str,
) -> Result<(), String> {
    let key = batch_archive_key(service, platform)?;
    verify_in(&batch_archive_root()?, batch_id, &key, bytes, sha256)
}

pub(crate) fn verify_shared_identity_recorded(
    batch_id: &str,
    service: &str,
    platform: &str,
    reference: &str,
    bytes: u64,
    sha256: &str,
    image_ids: &[String],
) -> Result<(), String> {
    verify_shared_identity_in(
        &batch_archive_root()?,
        batch_id,
        service,
        platform,
        reference,
        bytes,
        sha256,
        image_ids,
    )
}

#[allow(clippy::too_many_arguments)]
fn verify_shared_identity_in(
    root: &Path,
    batch_id: &str,
    service: &str,
    platform: &str,
    reference: &str,
    bytes: u64,
    sha256: &str,
    image_ids: &[String],
) -> Result<(), String> {
    let key = batch_archive_key(service, platform)?;
    let copy = copy_verified_in(root, batch_id, &key, bytes, sha256, image_ids.to_vec())?;
    if crate::build::archive_image_ids(&copy.path, reference, platform)? != image_ids {
        return Err("共享归档镜像 ID 与已登记身份不符。".into());
    }
    Ok(())
}

#[allow(dead_code)] // The per-host transfer must use a fresh verified copy, never the shared path directly.
pub fn copy_verified_shared(
    batch_id: &str,
    service: &str,
    platform: &str,
) -> Result<SavedArchive, String> {
    let key = batch_archive_key(service, platform)?;
    let (bytes, sha256, image_ids, reference) =
        crate::batch_store::archive_record(batch_id, service, platform)?;
    let copy = copy_verified_in(
        &batch_archive_root()?,
        batch_id,
        &key,
        bytes,
        &sha256,
        image_ids,
    )?;
    if crate::build::archive_image_ids(&copy.path, &reference, platform)? != copy.image_ids {
        return Err("共享归档镜像 ID 与已登记身份不符。".into());
    }
    Ok(copy)
}

fn verify_in(
    root: &Path,
    task_id: &str,
    service: &str,
    expected_bytes: u64,
    expected_sha256: &str,
) -> Result<(), String> {
    let path = checked_existing_archive_path(root, task_id, service)?;
    let mut file = File::open(&path).map_err(|_| "本机镜像归档已丢失。")?;
    let mut digest = Sha256::new();
    let mut bytes = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count = file
            .read(&mut buffer)
            .map_err(|_| "无法读取本机镜像归档。")?;
        if count == 0 {
            break;
        }
        bytes += count as u64;
        digest.update(&buffer[..count]);
    }
    if bytes != expected_bytes || format!("{:x}", digest.finalize()) != expected_sha256 {
        return Err("本机镜像归档校验失败。".into());
    }
    Ok(())
}

pub fn verify(task_id: &str, service: &str) -> Result<(), String> {
    let (bytes, sha256) = release_store::archive_record(task_id, service)?;
    verify_in(&archive_root()?, task_id, service, bytes, &sha256)
}

pub struct LocalArchiveCandidate {
    pub task_id: String,
    pub services: usize,
    pub verified_bytes: u64,
}

pub struct LocalArchivePreview {
    pub candidates: Vec<LocalArchiveCandidate>,
    pub unavailable: Vec<String>,
    pub total_tasks: usize,
    pub blocked: bool,
}

fn preview_from_plan(
    plan: RetentionPlan,
    mut inspect: impl FnMut(&str) -> Result<(usize, u64), String>,
) -> LocalArchivePreview {
    let mut preview = LocalArchivePreview {
        candidates: Vec::new(),
        unavailable: Vec::new(),
        total_tasks: plan.entries.len(),
        blocked: plan.unresolved,
    };
    if plan.unresolved {
        return preview;
    }
    for entry in plan.entries {
        if entry.class != RetentionClass::HistoricalReview {
            continue;
        }
        match inspect(&entry.task_id) {
            Ok((services, verified_bytes)) if services > 0 && verified_bytes > 0 => {
                preview.candidates.push(LocalArchiveCandidate {
                    task_id: entry.task_id,
                    services,
                    verified_bytes,
                });
            }
            _ => preview.unavailable.push(entry.task_id),
        }
    }
    preview
}

pub fn preview_local_reclaim(project_id: i64) -> Result<LocalArchivePreview, String> {
    let plan = release_store::retention_plan(project_id)?;
    Ok(preview_from_plan(plan, |task_id| {
        let services = release_store::planned_archive_services(task_id)?;
        if services.is_empty() || services.iter().any(|(_, recorded)| !recorded) {
            return Err("历史镜像归档记录不完整。".into());
        }
        let mut bytes = 0u64;
        for (service, _) in &services {
            let (size, _) = release_store::archive_record(task_id, service)?;
            verify(task_id, service)?;
            bytes = bytes.checked_add(size).ok_or("归档总字节数溢出。")?;
        }
        Ok((services.len(), bytes))
    }))
}

pub fn remove_historical(task_id: &str) -> Result<u64, String> {
    let root = archive_root()?;
    let services = release_store::planned_archive_services(task_id)?;
    if services.is_empty() {
        return Err("历史任务没有可核对的本机归档计划。".into());
    }
    let mut removed = 0u64;
    for (service, recorded) in services {
        if !recorded {
            continue;
        }
        let path = archive_path(&root, task_id, &service)?;
        if !path.exists() {
            continue;
        }
        let path = checked_existing_archive_path(&root, task_id, &service)?;
        verify(task_id, &service)?;
        let bytes = fs::metadata(&path)
            .map_err(|_| "无法核对历史归档大小。")?
            .len();
        fs::remove_file(&path).map_err(|_| "无法删除历史镜像归档。")?;
        removed = removed.checked_add(bytes).ok_or("归档释放字节数溢出。")?;
    }
    let directory = root.join(task_id);
    if directory.exists() {
        let meta = fs::symlink_metadata(&directory).map_err(|_| "无法核对归档任务目录。")?;
        if meta.file_type().is_symlink() || !meta.is_dir() {
            return Err("归档任务目录不可信。".into());
        }
        fs::remove_dir(&directory).map_err(|_| "归档任务目录含未登记文件，需人工核对。")?;
    }
    Ok(removed)
}

pub fn remove_orphaned(id: &str, batch: bool) -> Result<(), String> {
    let root = if batch {
        batch_archive_root()?
    } else {
        archive_root()?
    };
    remove_orphaned_in(&root, id)
}

fn remove_orphaned_in(root: &Path, id: &str) -> Result<(), String> {
    uuid::Uuid::parse_str(id).map_err(|_| "归档任务 ID 无效。")?;
    let directory = root.join(id);
    if !directory.exists() {
        return Ok(());
    }
    let root = root.canonicalize().map_err(|_| "无法核对归档根目录。")?;
    let metadata = fs::symlink_metadata(&directory).map_err(|_| "无法核对归档任务目录。")?;
    if !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || directory
            .canonicalize()
            .map_err(|_| "无法核对归档路径。")?
            .parent()
            != Some(root.as_path())
    {
        return Err("归档任务目录不可信。".into());
    }
    for entry in fs::read_dir(&directory).map_err(|_| "无法读取归档任务目录。")? {
        let entry = entry.map_err(|_| "无法读取归档文件。")?;
        let metadata = entry
            .path()
            .symlink_metadata()
            .map_err(|_| "无法核对归档文件。")?;
        let name = entry.file_name();
        let name = name.to_str().ok_or("归档文件名无效。")?;
        let managed = name.strip_suffix(".tar").is_some_and(|stem| {
            stem.len() == 64 && stem.bytes().all(|byte| byte.is_ascii_hexdigit())
        }) || name.strip_suffix(".partial").is_some_and(|stem| {
            stem.len() == 32 && stem.bytes().all(|byte| byte.is_ascii_hexdigit())
        });
        if !metadata.is_file() || metadata.file_type().is_symlink() || !managed {
            return Err("归档目录包含未登记文件，需人工核对。".into());
        }
        fs::remove_file(entry.path()).map_err(|_| "无法删除旧镜像归档。")?;
    }
    fs::remove_dir(directory).map_err(|_| "无法删除旧归档目录。".into())
}

fn copy_verified_in(
    root: &Path,
    task_id: &str,
    service: &str,
    expected_bytes: u64,
    expected_sha256: &str,
    image_ids: Vec<String>,
) -> Result<SavedArchive, String> {
    let source = checked_existing_archive_path(root, task_id, service)?;
    let path = std::env::temp_dir().join(format!(
        "devingpanel-rollback-{}.tar",
        uuid::Uuid::new_v4().simple()
    ));
    let mut archive = SavedArchive {
        path,
        bytes: 0,
        sha256: String::new(),
        image_ids,
    };
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut output = options
        .open(&archive.path)
        .map_err(|_| "无法创建本机回滚镜像临时归档。")?;
    let mut input = File::open(source).map_err(|_| "历史镜像归档已丢失。")?;
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count = input
            .read(&mut buffer)
            .map_err(|_| "历史镜像归档读取失败。")?;
        if count == 0 {
            break;
        }
        output
            .write_all(&buffer[..count])
            .map_err(|_| "回滚镜像临时归档写入失败。")?;
        archive.bytes += count as u64;
        hash.update(&buffer[..count]);
    }
    output
        .sync_all()
        .map_err(|_| "无法保存回滚镜像临时归档。")?;
    archive.sha256 = format!("{:x}", hash.finalize());
    if archive.bytes != expected_bytes || archive.sha256 != expected_sha256 {
        return Err("历史镜像归档校验失败，未开始上传。".into());
    }
    Ok(archive)
}

pub fn copy_verified(
    task_id: &str,
    service: &str,
    image_ids: Vec<String>,
) -> Result<SavedArchive, String> {
    let (bytes, sha256) = release_store::archive_record(task_id, service)?;
    copy_verified_in(
        &archive_root()?,
        task_id,
        service,
        bytes,
        &sha256,
        image_ids,
    )
}

#[cfg(test)]
mod tests {
    use super::{
        archive_path, batch_archive_key, copy_verified_in, preview_from_plan, remove_orphaned_in,
        retain_in, verify_in, verify_shared_identity_in,
    };
    use crate::build::SavedArchive;
    use crate::release_store::{RetentionClass, RetentionEntry, RetentionPlan};
    use sha2::{Digest, Sha256};

    #[test]
    fn orphan_cleanup_removes_managed_partial_and_preserves_unknown_files() {
        let root =
            std::env::temp_dir().join(format!("archive-orphan-test-{}", uuid::Uuid::new_v4()));
        let id = uuid::Uuid::new_v4().to_string();
        let directory = root.join(&id);
        std::fs::create_dir_all(&directory).unwrap();
        let partial = directory.join(format!("{}.partial", uuid::Uuid::new_v4().simple()));
        std::fs::write(&partial, b"partial").unwrap();
        remove_orphaned_in(&root, &id).unwrap();
        assert!(!directory.exists());

        std::fs::create_dir(&directory).unwrap();
        let unknown = directory.join("notes.txt");
        std::fs::write(&unknown, b"keep").unwrap();
        assert!(remove_orphaned_in(&root, &id).is_err());
        assert_eq!(std::fs::read(&unknown).unwrap(), b"keep");
        std::fs::remove_file(unknown).unwrap();
        std::fs::remove_dir(directory).unwrap();
        std::fs::remove_dir(root).unwrap();
    }

    #[test]
    fn shared_archive_key_separates_architectures() {
        let amd = batch_archive_key("web", "linux/amd64").unwrap();
        let arm = batch_archive_key("web", "linux/arm64").unwrap();
        assert_ne!(
            archive_path(
                std::path::Path::new("root"),
                &uuid::Uuid::new_v4().to_string(),
                &amd
            )
            .unwrap()
            .file_name(),
            archive_path(
                std::path::Path::new("root"),
                &uuid::Uuid::new_v4().to_string(),
                &arm
            )
            .unwrap()
            .file_name()
        );
    }

    #[test]
    fn ready_identity_check_rejects_ids_not_in_retained_tar() {
        let parent = std::env::temp_dir().join(format!("shared-identity-{}", uuid::Uuid::new_v4()));
        let root = parent.join("batch-archives");
        std::fs::create_dir(&parent).unwrap();
        let source_path = parent.join("source.tar");
        let actual = format!("sha256:{}", "a".repeat(64));
        let reference = "app:v1-amd64";
        {
            let mut tar = tar::Builder::new(std::fs::File::create(&source_path).unwrap());
            let json = format!(
                r#"[{{"Config":"{}.json","RepoTags":["{reference}"],"Layers":[]}}]"#,
                "a".repeat(64)
            );
            let mut header = tar::Header::new_gnu();
            header.set_size(json.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            tar.append_data(&mut header, "manifest.json", json.as_bytes())
                .unwrap();
            tar.finish().unwrap();
        }
        let bytes = std::fs::read(&source_path).unwrap();
        let archive = SavedArchive {
            path: source_path,
            bytes: bytes.len() as u64,
            sha256: format!("{:x}", Sha256::digest(&bytes)),
            image_ids: vec![actual.clone()],
        };
        let id = uuid::Uuid::new_v4().to_string();
        let key = batch_archive_key("web", "linux/amd64").unwrap();
        retain_in(&root, &id, &key, &archive, |_, _| Ok(())).unwrap();
        assert!(
            verify_shared_identity_in(
                &root,
                &id,
                "web",
                "linux/amd64",
                reference,
                archive.bytes,
                &archive.sha256,
                &[format!("sha256:{}", "b".repeat(64))]
            )
            .is_err()
        );
        verify_shared_identity_in(
            &root,
            &id,
            "web",
            "linux/amd64",
            reference,
            archive.bytes,
            &archive.sha256,
            &[actual],
        )
        .unwrap();
        std::fs::remove_file(archive_path(&root, &id, &key).unwrap()).unwrap();
        std::fs::remove_dir(root.join(id)).unwrap();
        std::fs::remove_dir(root).unwrap();
        drop(archive);
        std::fs::remove_dir(parent).unwrap();
    }

    #[test]
    fn preview_only_inspects_historical_candidates_and_never_authorizes_deletion() {
        let entries = [
            ("current", RetentionClass::LatestCompleted),
            ("previous", RetentionClass::PreviousCompleted),
            ("old", RetentionClass::HistoricalReview),
            ("broken", RetentionClass::HistoricalReview),
            ("uncertain", RetentionClass::UncertainHistory),
        ]
        .into_iter()
        .map(|(task_id, class)| RetentionEntry {
            task_id: task_id.into(),
            class,
        })
        .collect();
        let mut inspected = Vec::new();
        let preview = preview_from_plan(
            RetentionPlan {
                unresolved: false,
                entries,
            },
            |id| {
                inspected.push(id.to_owned());
                if id == "old" {
                    Ok((2, 1024))
                } else {
                    Err("missing".into())
                }
            },
        );
        assert_eq!(inspected, ["old", "broken"]);
        assert_eq!(preview.total_tasks, 5);
        assert_eq!(preview.candidates.len(), 1);
        assert_eq!(preview.candidates[0].verified_bytes, 1024);
        assert_eq!(preview.unavailable, ["broken"]);
        let held = preview_from_plan(
            RetentionPlan {
                unresolved: true,
                entries: vec![RetentionEntry {
                    task_id: "old".into(),
                    class: RetentionClass::HistoricalReview,
                }],
            },
            |_| panic!("unresolved history must not be inspected"),
        );
        assert!(held.candidates.is_empty());
    }

    #[test]
    fn read_only_archive_checks_reject_symlinked_tar() {
        let root = std::env::temp_dir().join(format!("archive-link-test-{}", uuid::Uuid::new_v4()));
        let id = uuid::Uuid::new_v4().to_string();
        let dir = root.join(&id);
        std::fs::create_dir_all(&dir).unwrap();
        let outside = root.join("other.tar");
        std::fs::write(&outside, b"archive content").unwrap();
        let path = archive_path(&root, &id, "web").unwrap();
        #[cfg(windows)]
        let linked = std::os::windows::fs::symlink_file(&outside, &path).is_ok();
        #[cfg(unix)]
        let linked = std::os::unix::fs::symlink(&outside, &path).is_ok();
        if linked {
            let digest = format!("{:x}", Sha256::digest(b"archive content"));
            assert!(verify_in(&root, &id, "web", 15, &digest).is_err());
            assert!(copy_verified_in(&root, &id, "web", 15, &digest, vec![]).is_err());
            std::fs::remove_file(path).unwrap();
        }
        std::fs::remove_file(outside).unwrap();
        std::fs::remove_dir(dir).unwrap();
        std::fs::remove_dir(root).unwrap();
    }

    #[test]
    fn retains_verified_archive_without_overwriting_or_keeping_partial_copy() {
        let root = std::env::temp_dir().join(format!("archive-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let input = root.join("source.tar");
        std::fs::write(&input, b"archive content").unwrap();
        let source = SavedArchive {
            path: input,
            bytes: 15,
            sha256: format!("{:x}", Sha256::digest(b"archive content")),
            image_ids: vec![],
        };
        let id = uuid::Uuid::new_v4().to_string();
        let path = archive_path(&root, &id, "web").unwrap();
        retain_in(&root, &id, "web", &source, |_, _| {
            Err("database failed".into())
        })
        .unwrap_err();
        assert!(!path.exists());
        assert_eq!(
            std::fs::read_dir(path.parent().unwrap()).unwrap().count(),
            0
        );
        retain_in(&root, &id, "web", &source, |bytes, digest| {
            assert_eq!(bytes, 15);
            assert_eq!(digest, source.sha256);
            Ok(())
        })
        .unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"archive content");
        let copy = copy_verified_in(
            &root,
            &id,
            "web",
            15,
            &source.sha256,
            vec!["test-id".into()],
        )
        .unwrap();
        assert_eq!(std::fs::read(&copy.path).unwrap(), b"archive content");
        let copy_path = copy.path.clone();
        drop(copy);
        assert!(!copy_path.exists());
        assert!(copy_verified_in(&root, &id, "web", 15, &"0".repeat(64), vec![]).is_err());
        verify_in(&root, &id, "web", 15, &source.sha256).unwrap();
        std::fs::write(&path, b"changed content").unwrap();
        assert!(verify_in(&root, &id, "web", 15, &source.sha256).is_err());
        assert!(retain_in(&root, &id, "web", &source, |_, _| Ok(())).is_err());
        assert!(archive_path(&root, "../unsafe", "web").is_err());
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(root.join(id)).unwrap();
        drop(source);
        std::fs::remove_dir(root).unwrap();
    }
}
