#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]

mod audit;
mod backup;
mod batch_store;
mod build;
mod compose;
mod database;
mod docker;
mod env_store;
mod host_store;
mod local_process;
mod preferences;
mod project_config;
mod project_store;
mod registry;
mod release;
mod release_archive;
mod release_materials;
mod release_store;
mod retention;
mod rollback;
mod rollback_store;
mod security;
mod ssh;
mod stack_release;
mod ui;

use gpui_kit::*;
use std::borrow::Cow;

struct AppAssets;

impl AssetSource for AppAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        match path {
            "branding/deving-panel.png" => Ok(Some(Cow::Borrowed(include_bytes!(
                "../.github/packaging/deving-panel.png"
            )))),
            _ => assets::AllAssets.load(path),
        }
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut paths = assets::AllAssets.list(path)?;
        if "branding/deving-panel.png".starts_with(path) {
            paths.push("branding/deving-panel.png".into());
        }
        Ok(paths)
    }
}

fn main() {
    application().with_assets(AppAssets).run(|cx| {
        init(cx);
        let options = WindowOptions {
            titlebar: Some(TitlebarOptions {
                title: Some("devingPanel".into()),
                #[cfg(target_os = "windows")]
                appears_transparent: true,
                ..Default::default()
            }),
            #[cfg(target_os = "linux")]
            icon: Some(std::sync::Arc::new(
                image::load_from_memory(include_bytes!("../.github/packaging/deving-panel.png"))
                    .expect("Invalid application icon")
                    .to_rgba8(),
            )),
            ..Default::default()
        };
        open_window(options, cx, |window, cx| {
            cx.new(|cx| ui::AppView::new(window, cx))
        })
        .expect("Failed to open window");
    });
}
