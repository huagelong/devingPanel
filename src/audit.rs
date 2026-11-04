use crate::preferences;
use chrono::Utc;
use serde::Serialize;
use std::fs::{OpenOptions, create_dir_all};
use std::io::Write;

#[derive(Serialize)]
struct Event<'a> {
    time: String,
    operation: &'a str,
    subject: &'a str,
    id: String,
    stage: &'a str,
    result: &'a str,
}

pub fn directory() -> Result<std::path::PathBuf, String> {
    Ok(preferences::data_dir()
        .ok_or("无法确定本机数据目录。")?
        .join("audit"))
}

pub fn record(
    operation: &'static str,
    subject: &'static str,
    id: impl ToString,
    stage: &'static str,
    success: bool,
) {
    if let Err(error) = write(operation, subject, id.to_string(), stage, success) {
        eprintln!("审计日志写入失败：{error}");
    }
}

fn write(
    operation: &str,
    subject: &str,
    id: String,
    stage: &str,
    success: bool,
) -> Result<(), String> {
    let directory = directory()?;
    create_dir_all(&directory).map_err(|_| "无法创建审计目录。")?;
    let event = Event {
        time: Utc::now().to_rfc3339(),
        operation,
        subject,
        id,
        stage,
        result: if success { "success" } else { "failure" },
    };
    let line = serde_json::to_string(&event).map_err(|_| "无法序列化审计事件。")?;
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(directory.join("events.jsonl"))
        .map_err(|_| "无法打开审计文件。")?;
    file.lock().map_err(|_| "无法锁定审计文件。")?;
    writeln!(file, "{line}").map_err(|_| "无法写入审计文件。".into())
}

#[cfg(test)]
mod tests {
    use super::Event;

    #[test]
    fn audit_event_has_only_fixed_metadata() {
        let event = Event {
            time: "now".into(),
            operation: "create",
            subject: "host",
            id: "7".into(),
            stage: "completed",
            result: "success",
        };
        let value = serde_json::to_value(event).unwrap();
        assert_eq!(value.as_object().unwrap().len(), 6);
        assert!(value.get("detail").is_none());
    }
}
