use std::fs;
use std::io;
use std::path::PathBuf;

pub fn data_dir() -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    let base = std::env::var_os("APPDATA").map(PathBuf::from);
    #[cfg(target_os = "macos")]
    let base = std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|path| path.join("Library/Application Support"));
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")));

    base.map(|path| path.join("devingPanel"))
}

fn theme_path() -> Option<PathBuf> {
    data_dir().map(|path| path.join("theme"))
}

pub fn load_theme() -> Option<String> {
    fs::read_to_string(theme_path()?)
        .ok()
        .map(|s| s.trim().to_owned())
}

pub fn save_theme(value: &str) -> io::Result<()> {
    let path = theme_path().ok_or_else(|| io::Error::other("无法确定本机配置目录"))?;
    fs::create_dir_all(path.parent().expect("theme path has a parent"))?;
    fs::write(path, value)
}

fn language_path() -> Option<PathBuf> {
    data_dir().map(|path| path.join("language"))
}

pub fn load_language() -> Option<String> {
    fs::read_to_string(language_path()?)
        .ok()
        .map(|value| value.trim().to_owned())
}

pub fn save_language(value: &str) -> io::Result<()> {
    let path = language_path().ok_or_else(|| io::Error::other("无法确定本机配置目录"))?;
    fs::create_dir_all(path.parent().expect("language path has a parent"))?;
    fs::write(path, value)
}
