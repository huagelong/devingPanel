use crate::build::SavedArchive;
use crate::preferences;
use crate::release_store;
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

fn archive_root() -> Result<PathBuf, String> {
    Ok(preferences::data_dir()
        .ok_or("无法确定本机配置目录。")?
        .join("release-archives"))
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
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(directory)
        .map_err(|_| "无法创建本机发布归档目录。")?;
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

fn verify_in(
    root: &Path,
    task_id: &str,
    service: &str,
    expected_bytes: u64,
    expected_sha256: &str,
) -> Result<(), String> {
    let path = archive_path(root, task_id, service)?;
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

#[cfg(test)]
mod tests {
    use super::{archive_path, retain_in, verify_in};
    use crate::build::SavedArchive;
    use sha2::{Digest, Sha256};

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
