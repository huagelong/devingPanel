mod batch_store;
mod build;
mod compose;
mod database;
mod docker;
mod env_store;
mod host_store;
mod preferences;
mod project_config;
mod project_store;
mod release;
mod release_archive;
mod release_materials;
mod release_store;
mod rollback;
mod rollback_store;
mod ssh;
mod stack_release;
mod ui;

use gpui_kit::*;

fn main() {
    application().with_assets(assets::AllAssets).run(|cx| {
        init(cx);
        open_window(WindowOptions::default(), cx, |window, cx| {
            cx.new(|cx| ui::AppView::new(window, cx))
        })
        .expect("Failed to open window");
    });
}
