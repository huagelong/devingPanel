mod pages;

use crate::batch_store;
use crate::compose::{self, Service, ServiceSummary};
use crate::docker::{self, DockerStatus};
use crate::env_store::{self, EnvInput, EnvRow};
use crate::host_store::{self, Host};
use crate::preferences;
use crate::project_store::{self, Project};
use crate::release;
use crate::release_archive;
use crate::release_materials;
use crate::release_store;
use crate::rollback_store;
use crate::ssh::{self, TestResult};
use crate::stack_release;
use gpui_kit::InteractiveElement;
use gpui_kit::component::Icon;
use gpui_kit::component::WindowExt as _;
use gpui_kit::component::input::{InputEvent, InputState};
use gpui_kit::component::{Theme, ThemeMode};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use std::collections::{HashMap, HashSet};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::Instant;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Page {
    Dashboard,
    Hosts,
    Projects,
    Deploy,
    Containers,
    Settings,
}

impl Page {
    const ALL: [Self; 6] = [
        Self::Dashboard,
        Self::Hosts,
        Self::Projects,
        Self::Deploy,
        Self::Containers,
        Self::Settings,
    ];

    fn title(self) -> &'static str {
        match self {
            Self::Dashboard => "总览",
            Self::Hosts => "主机",
            Self::Projects => "项目",
            Self::Deploy => "发布",
            Self::Containers => "容器",
            Self::Settings => "设置",
        }
    }

    fn icon(self) -> assets::IconName {
        match self {
            Self::Dashboard => assets::IconName::LayoutDashboard,
            Self::Hosts => assets::IconName::Server,
            Self::Projects => assets::IconName::Folder,
            Self::Deploy => assets::IconName::Rocket,
            Self::Containers => assets::IconName::Box,
            Self::Settings => assets::IconName::Settings,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DeployTab {
    Execute,
    History,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ContainerTab {
    Containers,
    Images,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum HostTab {
    Monitor,
    Info,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ProjectTab {
    Build,
    Compose,
    Environment,
    Targets,
    History,
}

#[derive(Clone)]
enum HostTestState {
    Checking,
    Connected {
        docker_version: String,
        compose_version: String,
        system_kernel: String,
        architecture: Option<String>,
        disk_available_kib: u64,
        sftp_v3_handshake: bool,
        checked_at: Instant,
        stale: bool,
    },
    Failed(String),
    NeedsTrust,
    DockerMissing {
        os_id: String,
        os_version: String,
        system_kernel: String,
        architecture: Option<String>,
    },
}

struct BuildForm {
    project_id: i64,
    image: Entity<InputState>,
    tag_rule: Entity<InputState>,
    platform: String,
    args: Vec<BuildArgRow>,
}

struct BuildArgRow {
    key: Entity<InputState>,
    value: Entity<InputState>,
}

struct TargetForm {
    project_id: i64,
    group: String,
    host_ids: Vec<i64>,
}

struct EnvForm {
    project_id: i64,
    host_id: Option<i64>,
    rows: Vec<EnvFormRow>,
}

struct EnvFormRow {
    id: u64,
    name: Entity<InputState>,
    value: Entity<InputState>,
    credential_id: Option<String>,
    sensitive: bool,
}

impl BuildForm {
    fn new(project: &Project, window: &mut Window, cx: &mut Context<AppView>) -> Self {
        Self {
            project_id: project.id,
            image: cx.new(|cx| InputState::new(window, cx).default_value(project.image.clone())),
            tag_rule: cx
                .new(|cx| InputState::new(window, cx).default_value(project.tag_rule.clone())),
            platform: project.platform.clone(),
            args: project
                .build_args
                .iter()
                .map(|(key, value)| BuildArgRow {
                    key: cx.new(|cx| InputState::new(window, cx).default_value(key.clone())),
                    value: cx.new(|cx| InputState::new(window, cx).default_value(value.clone())),
                })
                .collect(),
        }
    }

    fn next_platform(&mut self) {
        self.platform = match self.platform.as_str() {
            "auto" => "linux/amd64",
            "linux/amd64" => "linux/arm64",
            _ => "auto",
        }
        .into();
    }

    fn platform_label(&self) -> &'static str {
        match self.platform.as_str() {
            "linux/amd64" => "linux/amd64",
            "linux/arm64" => "linux/arm64",
            _ => "按主机自动（默认）",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SettingsSection {
    Retention,
    Appearance,
    Registry,
    Backup,
    Environment,
    Security,
}

impl SettingsSection {
    const ALL: [Self; 6] = [
        Self::Retention,
        Self::Appearance,
        Self::Registry,
        Self::Backup,
        Self::Environment,
        Self::Security,
    ];

    fn title(self) -> &'static str {
        match self {
            Self::Retention => "发布保留策略",
            Self::Appearance => "外观",
            Self::Registry => "镜像仓库",
            Self::Backup => "备份与恢复",
            Self::Environment => "本地环境",
            Self::Security => "安全",
        }
    }

    fn icon(self) -> assets::IconName {
        match self {
            Self::Retention => assets::IconName::Package,
            Self::Appearance => assets::IconName::Sun,
            Self::Registry => assets::IconName::Server,
            Self::Backup => assets::IconName::Cloud,
            Self::Environment => assets::IconName::Terminal,
            Self::Security => assets::IconName::ShieldCheck,
        }
    }
}

// Match both HSL palettes in webui/styles.css; views never define colors locally.
#[derive(Clone, Copy)]
struct Palette {
    background: Hsla,
    foreground: Hsla,
    muted: Hsla,
    border: Hsla,
    accent: Hsla,
    success: Hsla,
    warning: Hsla,
    destructive: Hsla,
}
impl Palette {
    const BACKGROUND: Hsla = Hsla {
        h: 0.0,
        s: 0.0,
        l: 1.0,
        a: 1.0,
    };
    const FOREGROUND: Hsla = Hsla {
        h: 240.0 / 360.0,
        s: 0.10,
        l: 0.039,
        a: 1.0,
    };
    const MUTED: Hsla = Hsla {
        h: 240.0 / 360.0,
        s: 0.038,
        l: 0.461,
        a: 1.0,
    };
    const BORDER: Hsla = Hsla {
        h: 240.0 / 360.0,
        s: 0.059,
        l: 0.90,
        a: 1.0,
    };
    const ACCENT: Hsla = Hsla {
        h: 240.0 / 360.0,
        s: 0.048,
        l: 0.959,
        a: 1.0,
    };
    const SUCCESS: Hsla = Hsla {
        h: 142.1 / 360.0,
        s: 0.762,
        l: 0.363,
        a: 1.0,
    };
    const DESTRUCTIVE: Hsla = Hsla {
        h: 0.0,
        s: 0.842,
        l: 0.602,
        a: 1.0,
    };
    const WARNING: Hsla = Hsla {
        h: 32.1 / 360.0,
        s: 0.946,
        l: 0.437,
        a: 1.0,
    };

    const LIGHT: Self = Self {
        background: Self::BACKGROUND,
        foreground: Self::FOREGROUND,
        muted: Self::MUTED,
        border: Self::BORDER,
        accent: Self::ACCENT,
        success: Self::SUCCESS,
        warning: Self::WARNING,
        destructive: Self::DESTRUCTIVE,
    };

    const DARK: Self = Self {
        background: hsl(240.0, 0.10, 0.039),
        foreground: hsl(0.0, 0.0, 0.98),
        muted: hsl(240.0, 0.05, 0.649),
        border: hsl(240.0, 0.037, 0.159),
        accent: hsl(240.0, 0.037, 0.159),
        success: hsl(142.1, 0.706, 0.453),
        warning: hsl(43.0, 0.96, 0.56),
        destructive: hsl(0.0, 0.628, 0.306),
    };
}

const fn hsl(degrees: f32, saturation: f32, lightness: f32) -> Hsla {
    Hsla {
        h: degrees / 360.0,
        s: saturation,
        l: lightness,
        a: 1.0,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ThemeChoice {
    Light,
    Dark,
    System,
}

impl ThemeChoice {
    const ALL: [Self; 3] = [Self::Light, Self::Dark, Self::System];

    fn saved(self) -> &'static str {
        match self {
            Self::Light => "light",
            Self::Dark => "dark",
            Self::System => "system",
        }
    }

    fn from_saved(value: Option<String>) -> Self {
        match value.as_deref() {
            Some("light") => Self::Light,
            Some("dark") => Self::Dark,
            _ => Self::System,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Light => "浅色",
            Self::Dark => "深色",
            Self::System => "跟随系统",
        }
    }

    fn next(self) -> Self {
        match self {
            Self::Light => Self::Dark,
            Self::Dark => Self::System,
            Self::System => Self::Light,
        }
    }

    fn is_dark(self, appearance: WindowAppearance) -> bool {
        match self {
            Self::Light => false,
            Self::Dark => true,
            Self::System => matches!(
                appearance,
                WindowAppearance::Dark | WindowAppearance::VibrantDark
            ),
        }
    }
}

struct MaterialCheck {
    compose_files: usize,
    has_env: bool,
    archives: usize,
    planned_archives: usize,
}

pub(crate) struct AppView {
    page: Page,
    menu_open: bool,
    docker: DockerStatus,
    docker_generation: u64,
    deploy_tab: DeployTab,
    deploy_project_id: Option<i64>,
    deploy_host_ids: Vec<i64>,
    deploy_parallel: usize,
    deploy_tag: Entity<InputState>,
    deploy_tag_generation: u64,
    deploy_tag_loading: bool,
    deploy_tag_error: Option<String>,
    deploy_tag_plan: Option<Result<Vec<release::PlannedImage>, String>>,
    deploy_batch_plan: Option<Result<stack_release::BatchPlan, String>>,
    deploy_batch_check: Option<Result<Arc<stack_release::BatchPreflight>, String>>,
    deploy_batch_check_loading: bool,
    deploy_batch_check_generation: u64,
    deploy_batch_check_cancel: Option<Arc<AtomicBool>>,
    deploy_tag_plan_loading: bool,
    deploy_tag_plan_generation: u64,
    deploy_env_loading: bool,
    deploy_env_result: Option<Result<(), String>>,
    deploy_env_generation: u64,
    deploy_build_platforms: Option<Result<Vec<String>, String>>,
    deploy_build_loading: bool,
    deploy_build_generation: u64,
    deploy_config_result: Option<Result<(), String>>,
    deploy_config_loading: bool,
    deploy_config_generation: u64,
    deploy_target_result: Option<Result<Vec<(i64, String)>, String>>,
    deploy_target_loading: bool,
    deploy_target_generation: u64,
    deploy_target_cancel: Option<Arc<AtomicBool>>,
    deploy_run_busy: bool,
    deploy_run_project_id: Option<i64>,
    deploy_run_host_id: Option<i64>,
    deploy_run_stage: String,
    deploy_run_log: Vec<String>,
    deploy_run_transfer: Option<(u64, u64)>,
    deploy_run_started: Option<Instant>,
    deploy_run_elapsed_secs: u64,
    release_tasks: Vec<release_store::TaskSnapshot>,
    release_batches: Vec<batch_store::BatchRecord>,
    release_tasks_loading: bool,
    release_tasks_refresh_pending: bool,
    release_tasks_error: Option<String>,
    release_history_selected: Option<String>,
    release_history_loaded_task: Option<release_store::TaskSnapshot>,
    release_history_loading_task: Option<String>,
    release_history_load_error: Option<String>,
    release_project_tasks: HashMap<i64, Vec<release_store::TaskSnapshot>>,
    release_project_batches: HashMap<i64, Vec<batch_store::BatchRecord>>,
    batch_resolving: HashSet<String>,
    batch_operation_stage: HashMap<String, String>,
    release_project_loading: HashSet<i64>,
    release_project_refresh_pending: HashSet<i64>,
    release_project_errors: HashMap<i64, String>,
    rollback_tasks: Vec<rollback_store::TaskSnapshot>,
    rollback_project_tasks: HashMap<i64, Vec<rollback_store::TaskSnapshot>>,
    rollback_previewing: HashSet<String>,
    rollback_running: Option<String>,
    rollback_stage: String,
    #[cfg(test)]
    rollback_confirmed_pair: Option<(String, String)>,
    retention_reports: HashMap<i64, Result<release_store::RetentionPlan, String>>,
    retention_loading: HashSet<i64>,
    local_archive_previews: HashMap<i64, Result<release_archive::LocalArchivePreview, String>>,
    local_archive_preview_loading: HashSet<i64>,
    retention_epoch: u64,
    current_version_checks: HashMap<String, Result<release::CurrentVersionCheck, String>>,
    current_version_loading: HashSet<String>,
    release_observations: HashMap<String, Result<Vec<ssh::ObservedContainer>, String>>,
    release_image_checks: HashMap<String, Result<Vec<(String, bool)>, String>>,
    release_image_checking: HashSet<String>,
    release_upload_inspections: HashMap<String, Result<release::UnknownUploadInspection, String>>,
    release_upload_inspecting: HashSet<String>,
    release_upload_inspection_generation: u64,
    previous_version_checks: HashMap<String, Result<release::PreviousVersionCheck, String>>,
    previous_version_checking: HashSet<String>,
    release_checking: HashSet<String>,
    release_resolving: HashSet<String>,
    release_material_checks: HashMap<String, Result<MaterialCheck, String>>,
    release_material_checking: HashSet<String>,
    release_recovery_busy: bool,
    release_recovery_error: Option<String>,
    container_tab: ContainerTab,
    container_host_id: Option<i64>,
    container_inventory: Option<ssh::Inventory>,
    container_loading: bool,
    container_error: Option<String>,
    container_generation: u64,
    container_operations: HashSet<(i64, String)>,
    prune_busy: bool,
    active_log_cancel: Option<Arc<AtomicBool>>,
    settings_section: SettingsSection,
    settings_menu_open: bool,
    theme: ThemeChoice,
    dark: bool,
    theme_error: Option<String>,
    projects: Vec<Project>,
    project_loaded: bool,
    project_error: Option<String>,
    hosts: Vec<Host>,
    host_loaded: bool,
    host_error: Option<String>,
    dashboard_metrics: HashMap<i64, (Instant, Result<ssh::MonitorSnapshot, ssh::MonitorError>)>,
    dashboard_generation: u64,
    dashboard_cancel: Option<Arc<AtomicBool>>,
    credential_warning: Option<String>,
    host_group: String,
    host_id: Option<i64>,
    host_tab: HostTab,
    host_monitor: Option<ssh::MonitorSnapshot>,
    host_monitor_loading: bool,
    host_monitor_error: Option<String>,
    host_monitor_generation: u64,
    host_monitor_cancel: Option<Arc<AtomicBool>>,
    host_tests: HashMap<i64, HostTestState>,
    host_testing: Option<i64>,
    host_installing: Option<i64>,
    host_install_stage: Option<String>,
    host_install_error: Option<(i64, String)>,
    host_mirror_configuring: Option<i64>,
    host_mirror_error: Option<(i64, String)>,
    host_test_generation: u64,
    project_id: Option<i64>,
    project_tab: ProjectTab,
    build_form: Option<BuildForm>,
    build_saving: bool,
    target_form: Option<TargetForm>,
    target_saving: bool,
    target_menu_open: bool,
    env_form: Option<EnvForm>,
    env_loading: bool,
    env_saving: bool,
    env_error: Option<String>,
    env_generation: u64,
    env_row_seq: u64,
    env_scope_menu_open: bool,
    compose_saving: bool,
    compose_summary_open: bool,
    compose_summary_loading: bool,
    compose_summary_generation: u64,
    compose_summary: Option<Result<Vec<ServiceSummary>, String>>,
    service_options: Vec<Service>,
    service_project_id: Option<i64>,
    service_generation: u64,
    service_loading: bool,
    service_saving: bool,
    service_error: Option<String>,
    service_menu_open: bool,
    dialog_seq: u64,
    _appearance: Subscription,
    _deploy_tag_subscription: Subscription,
}

impl AppView {
    pub(crate) fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let deploy_tag = cx.new(|cx| InputState::new(window, cx).placeholder("正在生成发布标识…"));
        let tag_subscription = cx.subscribe(&deploy_tag, |view, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                view.schedule_tag_plan(cx);
            }
        });
        let theme = ThemeChoice::from_saved(preferences::load_theme());
        let dark = theme.is_dark(window.appearance());
        Theme::change(
            if dark {
                ThemeMode::Dark
            } else {
                ThemeMode::Light
            },
            Some(window),
            cx,
        );
        let appearance = cx.observe_window_appearance(window, |view, window, cx| {
            if view.theme == ThemeChoice::System {
                view.dark = view.theme.is_dark(window.appearance());
                Theme::change(
                    if view.dark {
                        ThemeMode::Dark
                    } else {
                        ThemeMode::Light
                    },
                    Some(window),
                    cx,
                );
                cx.notify();
            }
        });
        cx.spawn(async move |view, cx| {
            let status = cx
                .background_executor()
                .spawn(async move { docker::probe() })
                .await;
            let _ = view.update(cx, |view, cx| {
                if view.docker_generation == 0 {
                    view.docker = status;
                }
                cx.notify();
            });
        })
        .detach();
        cx.spawn_in(window, async move |view, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { project_store::list() })
                .await;
            let _ = cx.update(|window, app| {
                let _ = view.update(app, |view, cx| {
                    if !view.project_loaded {
                        view.project_loaded = true;
                        match result {
                            Ok(projects) => {
                                view.projects = projects;
                                if view.page == Page::Deploy
                                    && let Some(id) =
                                        view.projects.first().map(|project| project.id)
                                {
                                    view.select_deploy_project(id, window, cx);
                                }
                            }
                            Err(error) => view.project_error = Some(error),
                        }
                    }
                    cx.notify();
                });
            });
        })
        .detach();
        cx.spawn(async move |view, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { host_store::list() })
                .await;
            let _ = view.update(cx, |view, cx| {
                if !view.host_loaded {
                    view.host_loaded = true;
                    match result {
                        Ok((hosts, warning)) => {
                            view.hosts = hosts;
                            view.credential_warning = warning;
                            if view.page == Page::Dashboard {
                                view.start_dashboard_monitor(cx);
                            }
                            if view.page == Page::Deploy && view.deploy_project_id.is_some() {
                                view.refresh_deploy_environment(cx);
                            }
                            if view.page == Page::Containers
                                && let Some(id) = view.hosts.first().map(|host| host.id)
                            {
                                view.select_container_host(id, cx);
                            }
                            if view.project_tab == ProjectTab::Targets
                                && let Some(id) = view.project_id
                            {
                                view.open_target_form(id);
                            }
                        }
                        Err(error) => view.host_error = Some(error),
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.spawn(async move |view, cx| {
            let (prepared, batches) = cx
                .background_executor()
                .spawn(async move {
                    let prepared = release_materials::recover_prepared();
                    let batches = release_materials::recover_interrupted_batches();
                    (prepared, batches)
                })
                .await;
            let _ = view.update(cx, |view, cx| {
                if prepared.as_ref().is_ok_and(|count| *count > 0)
                    || batches.as_ref().is_ok_and(|count| *count > 0)
                {
                    view.refresh_release_tasks(None, cx);
                }
                let errors = prepared
                    .err()
                    .into_iter()
                    .chain(batches.err())
                    .collect::<Vec<_>>();
                view.release_recovery_error = (!errors.is_empty()).then(|| errors.join("；"));
                cx.notify();
            });
        })
        .detach();
        Self {
            page: Page::Dashboard,
            menu_open: false,
            docker: DockerStatus::Checking,
            docker_generation: 0,
            deploy_tab: DeployTab::Execute,
            deploy_project_id: None,
            deploy_host_ids: Vec::new(),
            deploy_parallel: 1,
            deploy_tag,
            deploy_tag_generation: 0,
            deploy_tag_loading: false,
            deploy_tag_error: None,
            deploy_tag_plan: None,
            deploy_batch_plan: None,
            deploy_batch_check: None,
            deploy_batch_check_loading: false,
            deploy_batch_check_generation: 0,
            deploy_batch_check_cancel: None,
            deploy_tag_plan_loading: false,
            deploy_tag_plan_generation: 0,
            deploy_env_loading: false,
            deploy_env_result: None,
            deploy_env_generation: 0,
            deploy_build_platforms: None,
            deploy_build_loading: false,
            deploy_build_generation: 0,
            deploy_config_result: None,
            deploy_config_loading: false,
            deploy_config_generation: 0,
            deploy_target_result: None,
            deploy_target_loading: false,
            deploy_target_generation: 0,
            deploy_target_cancel: None,
            deploy_run_busy: false,
            deploy_run_project_id: None,
            deploy_run_host_id: None,
            deploy_run_stage: String::new(),
            deploy_run_log: Vec::new(),
            deploy_run_transfer: None,
            deploy_run_started: None,
            deploy_run_elapsed_secs: 0,
            release_tasks: Vec::new(),
            release_batches: Vec::new(),
            release_tasks_loading: false,
            release_tasks_refresh_pending: false,
            release_tasks_error: None,
            release_history_selected: None,
            release_history_loaded_task: None,
            release_history_loading_task: None,
            release_history_load_error: None,
            release_project_tasks: HashMap::new(),
            batch_resolving: HashSet::new(),
            batch_operation_stage: HashMap::new(),
            release_project_batches: HashMap::new(),
            release_project_loading: HashSet::new(),
            release_project_refresh_pending: HashSet::new(),
            release_project_errors: HashMap::new(),
            rollback_tasks: Vec::new(),
            rollback_project_tasks: HashMap::new(),
            rollback_previewing: HashSet::new(),
            rollback_running: None,
            rollback_stage: String::new(),
            #[cfg(test)]
            rollback_confirmed_pair: None,
            retention_reports: HashMap::new(),
            retention_loading: HashSet::new(),
            local_archive_previews: HashMap::new(),
            local_archive_preview_loading: HashSet::new(),
            retention_epoch: 0,
            current_version_checks: HashMap::new(),
            current_version_loading: HashSet::new(),
            release_observations: HashMap::new(),
            release_image_checks: HashMap::new(),
            release_image_checking: HashSet::new(),
            release_upload_inspections: HashMap::new(),
            release_upload_inspecting: HashSet::new(),
            release_upload_inspection_generation: 0,
            previous_version_checks: HashMap::new(),
            previous_version_checking: HashSet::new(),
            release_checking: HashSet::new(),
            release_resolving: HashSet::new(),
            release_material_checks: HashMap::new(),
            release_material_checking: HashSet::new(),
            release_recovery_busy: false,
            release_recovery_error: None,
            container_tab: ContainerTab::Containers,
            container_host_id: None,
            container_inventory: None,
            container_loading: false,
            container_error: None,
            container_generation: 0,
            container_operations: HashSet::new(),
            prune_busy: false,
            active_log_cancel: None,
            settings_section: SettingsSection::Retention,
            settings_menu_open: false,
            theme,
            dark,
            theme_error: None,
            projects: Vec::new(),
            project_loaded: false,
            project_error: None,
            hosts: Vec::new(),
            host_loaded: false,
            host_error: None,
            dashboard_metrics: HashMap::new(),
            dashboard_generation: 0,
            dashboard_cancel: None,
            credential_warning: None,
            host_group: "all".into(),
            host_id: None,
            host_tab: HostTab::Monitor,
            host_monitor: None,
            host_monitor_loading: false,
            host_monitor_error: None,
            host_monitor_generation: 0,
            host_monitor_cancel: None,
            host_tests: HashMap::new(),
            host_testing: None,
            host_installing: None,
            host_install_stage: None,
            host_install_error: None,
            host_mirror_configuring: None,
            host_mirror_error: None,
            host_test_generation: 0,
            project_id: None,
            project_tab: ProjectTab::Build,
            build_form: None,
            build_saving: false,
            target_form: None,
            target_saving: false,
            target_menu_open: false,
            env_form: None,
            env_loading: false,
            env_saving: false,
            env_error: None,
            env_generation: 0,
            env_row_seq: 0,
            env_scope_menu_open: false,
            compose_saving: false,
            compose_summary_open: false,
            compose_summary_loading: false,
            compose_summary_generation: 0,
            compose_summary: None,
            service_options: Vec::new(),
            service_project_id: None,
            service_generation: 0,
            service_loading: false,
            service_saving: false,
            service_error: None,
            service_menu_open: false,
            dialog_seq: 0,
            _appearance: appearance,
            _deploy_tag_subscription: tag_subscription,
        }
    }

    fn set_theme(&mut self, theme: ThemeChoice, window: &mut Window, cx: &mut Context<Self>) {
        self.theme = theme;
        self.dark = theme.is_dark(window.appearance());
        Theme::change(
            if self.dark {
                ThemeMode::Dark
            } else {
                ThemeMode::Light
            },
            Some(window),
            cx,
        );
        self.theme_error = preferences::save_theme(theme.saved())
            .err()
            .map(|err| format!("主题偏好保存失败：{err}"));
        cx.notify();
    }

    fn refresh_docker(&mut self, cx: &mut Context<Self>) {
        self.docker_generation = self.docker_generation.wrapping_add(1);
        let generation = self.docker_generation;
        self.docker = DockerStatus::Checking;
        cx.notify();
        cx.spawn(async move |view, cx| {
            let status = cx
                .background_executor()
                .spawn(async { docker::probe() })
                .await;
            let _ = view.update(cx, |view, cx| {
                if view.docker_generation == generation {
                    view.docker = status;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn palette(&self) -> Palette {
        if self.dark {
            Palette::DARK
        } else {
            Palette::LIGHT
        }
    }

    fn docker_color(&self) -> Hsla {
        match self.docker {
            DockerStatus::Checking => self.palette().muted,
            DockerStatus::Available(_) => self.palette().success,
            DockerStatus::Unavailable => self.palette().destructive,
        }
    }

    fn navigate(&mut self, page: Page, window: &mut Window, cx: &mut Context<Self>) {
        if self.page == Page::Deploy && page != Page::Deploy {
            self.clear_batch_preflight();
            if let Some(cancel) = self.deploy_target_cancel.take() {
                cancel.store(true, Ordering::Relaxed);
            }
            self.deploy_target_generation = self.deploy_target_generation.wrapping_add(1);
            self.deploy_tag_plan_generation = self.deploy_tag_plan_generation.wrapping_add(1);
            self.deploy_tag_plan_loading = false;
        }
        if self.page == Page::Dashboard && page != Page::Dashboard {
            if let Some(cancel) = self.dashboard_cancel.take() {
                cancel.store(true, Ordering::Relaxed);
            }
            self.dashboard_generation = self.dashboard_generation.wrapping_add(1);
        }
        if self.page == Page::Hosts && page != Page::Hosts {
            if let Some(cancel) = self.host_monitor_cancel.take() {
                cancel.store(true, Ordering::Relaxed);
            }
            self.host_monitor_generation = self.host_monitor_generation.wrapping_add(1);
        }
        if self.page == Page::Containers && page != Page::Containers {
            self.container_generation = self.container_generation.wrapping_add(1);
            if let Some(cancel) = self.active_log_cancel.take() {
                cancel.store(true, Ordering::Relaxed);
                if window.has_active_sheet(cx) {
                    window.close_sheet(cx);
                }
            }
        }
        self.page = page;
        if page == Page::Dashboard && self.host_loaded {
            self.start_dashboard_monitor(cx);
        }
        if page == Page::Projects {
            self.project_id = None;
            self.build_form = None;
            self.compose_summary_open = false;
            self.compose_summary_generation = self.compose_summary_generation.wrapping_add(1);
            self.target_form = None;
            self.env_form = None;
            self.env_generation = self.env_generation.wrapping_add(1);
            self.env_scope_menu_open = false;
            self.target_menu_open = false;
            self.service_menu_open = false;
        }
        if page == Page::Hosts {
            self.host_id = None;
        }
        if page == Page::Containers {
            let id = self
                .container_host_id
                .filter(|id| self.hosts.iter().any(|host| host.id == *id))
                .or_else(|| self.hosts.first().map(|host| host.id));
            if let Some(id) = id {
                self.switch_container_host(id, window, cx);
            }
        }
        if page == Page::Deploy {
            self.deploy_tab = DeployTab::Execute;
            self.refresh_build_capabilities(cx);
            let id = self
                .deploy_project_id
                .filter(|id| self.projects.iter().any(|p| p.id == *id))
                .or_else(|| self.projects.first().map(|p| p.id));
            if let Some(id) = id {
                if self.deploy_project_id != Some(id)
                    || self
                        .projects
                        .iter()
                        .any(|project| project.id == id && project.manifest_mode)
                {
                    self.select_deploy_project(id, window, cx);
                } else if let Some(project) = self.projects.iter().find(|project| project.id == id)
                {
                    self.deploy_host_ids
                        .retain(|host_id| project.host_ids.contains(host_id));
                    self.deploy_parallel =
                        self.deploy_parallel.min(self.deploy_host_ids.len().max(1));
                    self.refresh_deploy_environment(cx);
                }
            }
        }
        self.menu_open = false;
        self.settings_menu_open = false;
        cx.notify();
    }

    fn toggle_menu(&mut self, cx: &mut Context<Self>) {
        self.menu_open = !self.menu_open;
        cx.notify();
    }

    fn sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut links = div().flex().flex_col().gap_1().pt_6();
        for page in Page::ALL {
            let selected = page == self.page;
            links = links.child(
                div()
                    .id(page.title())
                    .h(px(40.))
                    .px_3()
                    .flex()
                    .items_center()
                    .gap_3()
                    .rounded_md()
                    .text_size(px(14.))
                    .text_color(if selected {
                        self.palette().foreground
                    } else {
                        self.palette().muted
                    })
                    .when(selected, |el| {
                        el.bg(self.palette().accent)
                            .font_weight(FontWeight::SEMIBOLD)
                    })
                    .cursor_pointer()
                    .on_click(
                        cx.listener(move |this, _, window, cx| this.navigate(page, window, cx)),
                    )
                    .child(Icon::new(page.icon()).size(px(18.)))
                    .child(page.title()),
            );
        }

        div()
            .w(px(224.))
            .h_full()
            .flex_shrink_0()
            .flex()
            .flex_col()
            .border_r_1()
            .border_color(self.palette().border)
            .bg(self.palette().background)
            .p_3()
            .child(
                div()
                    .h(px(44.))
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .font_weight(FontWeight::BOLD)
                    .text_size(px(16.))
                    .child(
                        div()
                            .size(px(28.))
                            .rounded_md()
                            .bg(self.palette().foreground)
                            .text_color(self.palette().background)
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(Icon::new(assets::IconName::SquareTerminal).size(px(17.))),
                    )
                    .child("devingPanel"),
            )
            .child(links)
            .child(div().flex_1())
            .child(
                div()
                    .px_3()
                    .pb_3()
                    .text_size(px(12.))
                    .text_color(self.palette().muted)
                    .child("仅 SSH · 无远端 Agent"),
            )
    }

    fn topbar(&self, mobile: bool, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .h(px(56.))
            .w_full()
            .flex_shrink_0()
            .border_b_1()
            .border_color(self.palette().border)
            .when(mobile, |el| el.px_4())
            .when(!mobile, |el| el.px_8())
            .flex()
            .items_center()
            .justify_between()
            .text_size(px(14.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .when(mobile, |el| {
                        el.child(
                            div()
                                .id("mobile-menu-toggle")
                                .cursor_pointer()
                                .on_click(cx.listener(|this, _, _, cx| this.toggle_menu(cx)))
                                .child(Icon::new(assets::IconName::Menu).size(px(18.))),
                        )
                    })
                    .child(
                        div()
                            .text_color(self.palette().muted)
                            .child(match self.page {
                                Page::Settings => {
                                    format!("设置  ›  {}", self.settings_section.title())
                                }
                                Page::Projects => self
                                    .project_id
                                    .and_then(|id| {
                                        self.projects.iter().find(|project| project.id == id)
                                    })
                                    .map(|project| format!("项目  ›  {}", project.name))
                                    .unwrap_or_else(|| "项目".into()),
                                Page::Hosts => self
                                    .host_id
                                    .and_then(|id| self.hosts.iter().find(|host| host.id == id))
                                    .map(|host| format!("主机  ›  {}", host.name))
                                    .unwrap_or_else(|| "主机".into()),
                                page => page.title().to_owned(),
                            }),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_4()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(div().size(px(8.)).rounded_full().bg(self.docker_color()))
                            .when(!mobile, |el| el.child(self.docker.label())),
                    )
                    .child(
                        div()
                            .id("theme-toggle")
                            .cursor_pointer()
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.set_theme(this.theme.next(), window, cx);
                            }))
                            .child(Icon::new(assets::IconName::Sun).size(px(18.))),
                    ),
            )
    }

    fn mobile_menu(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut menu = div()
            .flex()
            .flex_col()
            .border_b_1()
            .border_color(self.palette().border)
            .p_3();
        for page in Page::ALL {
            menu = menu.child(
                div()
                    .id(page.title())
                    .h(px(40.))
                    .flex()
                    .items_center()
                    .gap_3()
                    .px_3()
                    .rounded_md()
                    .when(page == self.page, |el| el.bg(self.palette().accent))
                    .cursor_pointer()
                    .on_click(
                        cx.listener(move |this, _, window, cx| this.navigate(page, window, cx)),
                    )
                    .child(Icon::new(page.icon()).size(px(18.)))
                    .child(page.title()),
            );
        }
        menu
    }
}

impl Render for AppView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mobile = window.viewport_size().width < px(720.);
        div()
            .size_full()
            .flex()
            .bg(self.palette().background)
            .text_color(self.palette().foreground)
            .font_family(ui_font())
            .when(!mobile, |el| el.child(self.sidebar(cx)))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .flex()
                    .flex_col()
                    .child(self.topbar(mobile, cx))
                    .when(mobile && self.menu_open, |el| {
                        el.child(self.mobile_menu(cx))
                    })
                    .child(
                        div()
                            .id("content-scroll")
                            .flex_1()
                            .min_w_0()
                            .overflow_y_scroll()
                            .when(mobile, |el| el.px_4())
                            .when(!mobile, |el| el.px_8())
                            .py_6()
                            .child(div().w_full().min_w_0().max_w(px(1280.)).mx_auto().child(
                                self.content(
                                    window.viewport_size().width < px(1024.),
                                    window.viewport_size().width,
                                    cx,
                                ),
                            )),
                    ),
            )
    }
}

fn ui_font() -> &'static str {
    #[cfg(target_os = "windows")]
    {
        "Microsoft YaHei UI"
    }
    #[cfg(target_os = "macos")]
    {
        "PingFang SC"
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        "Noto Sans CJK SC"
    }
}

fn mono_font() -> &'static str {
    #[cfg(target_os = "windows")]
    {
        "Consolas"
    }
    #[cfg(target_os = "macos")]
    {
        "Menlo"
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        "DejaVu Sans Mono"
    }
}

#[cfg(test)]
mod tests {
    use super::{Palette, ThemeChoice};

    #[gpui_kit::test]
    #[ignore = "headless GUI test requires an explicit isolated APPDATA under target/isolated-tests"]
    fn isolated_deploy_confirmation_renders_without_starting_release(
        cx: &mut gpui_kit::TestAppContext,
    ) {
        use super::{AppView, DeployTab, Page};
        use gpui_kit::component::WindowExt as _;
        use gpui_kit::{AppContext as _, Modifiers, WindowOptions, px, size};
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
                .starts_with("ui-headless-")
        );
        assert!(
            std::fs::read_dir(&root).unwrap().next().is_none(),
            "headless test requires a fresh empty APPDATA"
        );
        let conn = crate::database::connect().unwrap();
        conn.execute("INSERT INTO hosts (id, name, address, port, username, auth, group_name, credential_id, trusted_fingerprint)
            VALUES (1, 'vps-smoke', '127.0.0.1', 1, 'test', 'password', 'test', 'missing-test-vault', 'SHA256:test')", []).unwrap();
        let project = crate::project_store::create_from_manifest(
            &std::env::current_dir()
                .unwrap()
                .join("tests/fixtures/stack-v2-success"),
        )
        .unwrap();
        let host = crate::host_store::list().unwrap().0.remove(0);
        cx.update(gpui_kit::init);
        let (handle, view_entity) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| AppView::new(window, cx))
            })
            .unwrap()
        });
        cx.update_window(handle, |_, window, cx| {
            view_entity.update(cx, |view, cx| {
                view.page = Page::Deploy;
                view.projects = vec![project.clone()];
                view.project_loaded = true;
                view.hosts = vec![host.clone()];
                view.host_loaded = true;
                view.deploy_project_id = Some(project.id);
                view.deploy_host_ids = vec![host.id];
                view.deploy_tag
                    .update(cx, |tag, cx| tag.set_value("ui-smoke", window, cx));
                view.deploy_config_result = Some(Ok(()));
                view.deploy_env_result = Some(Ok(()));
                view.deploy_target_result = Some(Ok(vec![(host.id, "linux/amd64".into())]));
                view.deploy_build_platforms = Some(Ok(vec!["linux/amd64".into()]));
                view.deploy_tag_plan = Some(Ok(crate::release::plan_images(
                    &project,
                    "ui-smoke",
                    &[(host.id, "linux/amd64".into())],
                )
                .unwrap()));
                assert!(view.deploy_ready(&project, &[host.id]));
                assert!(!view.deploy_run_busy);
                assert!(view.batch_resolving.is_empty());
            })
        })
        .unwrap();
        cx.update_window(handle, |_, window, cx| {
            view_entity.update(cx, |view, cx| {
                view.deploy_host_ids = vec![host.id, 999];
                assert!(!view.deploy_ready(&project, &view.deploy_host_ids));
                cx.notify();
            });
            window.draw(cx).clear(cx);
        })
        .unwrap();
        let mut test_window = gpui_kit::VisualTestContext::from_window(handle, cx);
        assert!(test_window.debug_bounds("multi-host-unavailable").is_some());
        let disabled_batch_check = test_window
            .debug_bounds("batch-preflight-check-button")
            .unwrap();
        test_window.simulate_click(disabled_batch_check.center(), Modifiers::default());
        let disabled_start = test_window.debug_bounds("start-deploy-button").unwrap();
        test_window.simulate_click(disabled_start.center(), Modifiers::default());
        cx.update_window(handle, |_, window, cx| {
            assert!(!window.has_active_dialog(cx));
            view_entity.update(cx, |view, cx| {
                assert!(view.deploy_batch_check.is_none());
                let mut batch_project = project.clone();
                batch_project.host_ids = vec![host.id, 999];
                let mut second = host.clone();
                second.id = 999;
                second.name = "isolated-second".into();
                view.projects = vec![batch_project.clone()];
                view.hosts.push(second.clone());
                view.deploy_parallel = 1;
                let target = |host: crate::host_store::Host, daemon: &str| {
                    crate::stack_release::BatchTargetIdentity {
                        host_id: host.id,
                        host,
                        platform: "linux/amd64".into(),
                        docker_endpoint: "unix:///var/run/docker.sock".into(),
                        docker_daemon_id: daemon.into(),
                    }
                };
                view.deploy_batch_check = Some(Ok(std::sync::Arc::new(
                    crate::stack_release::BatchPreflight {
                        plan: crate::stack_release::BatchPlan {
                            builds: vec![crate::stack_release::BatchBuild {
                                service: "web".into(),
                                platform: "linux/amd64".into(),
                                reference: "isolated:web".into(),
                            }],
                            hosts: vec![],
                        },
                        config_sha256: "isolated".into(),
                        builds: vec![],
                        compose_files: vec![],
                        env_files: vec![vec![], vec![]],
                        targets: vec![target(host.clone(), "TEST:ONE"), target(second, "TEST:TWO")],
                    },
                )));
                view.deploy_tag_plan = Some(Ok(vec![crate::release::PlannedImage {
                    platform: "linux/amd64".into(),
                    reference: "isolated:web".into(),
                }]));
                assert!(view.batch_start_ready(&batch_project, &view.deploy_host_ids));
                view.deploy_parallel = 2;
                assert!(view.batch_start_ready(&batch_project, &view.deploy_host_ids));
                view.deploy_parallel = 3;
                assert!(!view.batch_start_ready(&batch_project, &view.deploy_host_ids));
                view.deploy_parallel = 1;
                cx.notify();
            });
            window.draw(cx).clear(cx);
        })
        .unwrap();
        let gated_start = test_window.debug_bounds("start-deploy-button").unwrap();
        test_window.simulate_click(gated_start.center(), Modifiers::default());
        cx.update_window(handle, |_, window, cx| {
            assert!(!window.has_active_dialog(cx));
            view_entity.update(cx, |view, cx| view.confirm_batch_deploy(window, cx));
            assert!(window.has_active_dialog(cx));
            window.draw(cx).clear(cx);
        })
        .unwrap();
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(300));
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| window.draw(cx).clear(cx))
            .unwrap();
        let cancel_batch = test_window.debug_bounds("batch-start-cancel").unwrap();
        test_window.simulate_click(cancel_batch.center(), Modifiers::default());
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(300));
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| {
            assert!(!window.has_active_dialog(cx));
            view_entity.update(cx, |view, cx| {
                let project = &view.projects[0];
                assert!(view.batch_start_ready(project, &view.deploy_host_ids));
                view.confirm_batch_deploy(window, cx);
            });
            assert!(window.has_active_dialog(cx));
            window.draw(cx).clear(cx);
        })
        .unwrap();
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(300));
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| window.draw(cx).clear(cx))
            .unwrap();
        let confirm_batch = test_window.debug_bounds("batch-start-confirm").unwrap();
        test_window.simulate_click(confirm_batch.center(), Modifiers::default());
        for _ in 0..20 {
            cx.executor()
                .advance_clock(std::time::Duration::from_millis(200));
            cx.run_until_parked();
            if !cx.read(|app| view_entity.read(app).deploy_run_busy) {
                break;
            }
        }
        assert!(!cx.read(|app| view_entity.read(app).deploy_run_busy));
        assert!(cx.read(|app| view_entity.read(app).deploy_run_stage.contains("未完成")));
        assert!(crate::batch_store::recent(None).unwrap().is_empty());
        cx.update_window(handle, |_, window, cx| {
            view_entity.update(cx, |view, cx| {
                view.projects = vec![project.clone()];
                view.deploy_host_ids = vec![host.id];
                view.clear_batch_preflight();
                view.deploy_tag_plan = Some(Ok(crate::release::plan_images(
                    &project,
                    "ui-smoke",
                    &[(host.id, "linux/amd64".into())],
                )
                .unwrap()));
                assert!(view.deploy_ready(&project, &[host.id]));
                assert!(!view.deploy_run_busy);
                cx.notify();
            });
            window.draw(cx).clear(cx);
        })
        .unwrap();
        assert!(crate::batch_store::recent(None).unwrap().is_empty());
        cx.update_window(handle, |_, window, cx| {
            window.draw(cx).clear(cx);
        })
        .unwrap();
        let start = test_window
            .debug_bounds("start-deploy-button")
            .expect("start button rendered");
        test_window.simulate_click(start.center(), Modifiers::default());
        cx.update_window(handle, |_, window, cx| {
            assert!(window.has_active_dialog(cx));
            window.draw(cx).clear(cx);
        })
        .unwrap();
        let cancel = test_window
            .debug_bounds("deploy-cancel-button")
            .expect("cancel button rendered");
        test_window.simulate_click(cancel.center(), Modifiers::default());
        cx.update_window(handle, |_, window, cx| {
            assert!(!window.has_active_dialog(cx));
            view_entity.update(cx, |view, cx| {
                view.rollback_running = Some("active-rollback".into());
                assert!(!view.deploy_ready(&project, &[host.id]));
                view.rollback_running = None;
                view.confirm_rollback(
                    crate::rollback::RollbackPreview {
                        project_id: project.id,
                        source_id: uuid::Uuid::new_v4().to_string(),
                        target_id: uuid::Uuid::new_v4().to_string(),
                        project_name: project.name.clone(),
                        host_name: host.name.clone(),
                        host_endpoint: format!("{}:{}", host.address, host.port),
                        source_services: vec!["web: sample:new".into()],
                        observed: Vec::new(),
                        target_services: vec!["web: sample:old".into()],
                        missing_images: Vec::new(),
                    },
                    window,
                    cx,
                );
                assert!(window.has_active_dialog(cx));
            })
        })
        .unwrap();
        cx.update_window(handle, |_, window, cx| {
            window.draw(cx).clear(cx);
            assert!(window.has_active_dialog(cx));
        })
        .unwrap();
        assert!(
            test_window
                .debug_bounds("rollback-confirm-button")
                .is_some()
        );
        test_window.simulate_resize(size(px(390.), px(844.)));
        cx.update_window(handle, |_, window, cx| {
            window.draw(cx).clear(cx);
            assert!(window.has_active_dialog(cx));
        })
        .unwrap();
        let narrow_confirm = test_window.debug_bounds("rollback-confirm-button").unwrap();
        assert!(
            narrow_confirm.origin.x >= px(0.)
                && narrow_confirm.origin.x + narrow_confirm.size.width <= px(390.)
        );
        test_window.simulate_click(narrow_confirm.center(), Modifiers::default());
        for _ in 0..10 {
            cx.executor()
                .advance_clock(std::time::Duration::from_millis(250));
            cx.run_until_parked();
            if cx.read(|app| view_entity.read(app).rollback_running.is_none()) {
                break;
            }
        }
        assert!(cx.read(|app| view_entity.read(app).rollback_running.is_none()));
        assert!(
            crate::rollback_store::recent(Some(project.id))
                .unwrap()
                .is_empty()
        );
        cx.update_window(handle, |_, window, cx| {
            window.draw(cx).clear(cx);
        })
        .unwrap();
        let narrow_start = test_window.debug_bounds("start-deploy-button").unwrap();
        assert!(
            narrow_start.origin.x >= px(0.)
                && narrow_start.origin.x + narrow_start.size.width <= px(390.)
        );
        test_window.simulate_resize(size(px(1280.), px(900.)));
        cx.update_window(handle, |_, window, cx| {
            window.draw(cx).clear(cx);
        })
        .unwrap();
        let start_again = test_window.debug_bounds("start-deploy-button").unwrap();
        test_window.simulate_click(start_again.center(), Modifiers::default());
        cx.update_window(handle, |_, window, cx| {
            window.draw(cx).clear(cx);
        })
        .unwrap();
        let confirm = test_window
            .debug_bounds("deploy-confirm-button")
            .expect("confirm button rendered");
        test_window.simulate_click(confirm.center(), Modifiers::default());
        for _ in 0..10 {
            cx.executor()
                .advance_clock(std::time::Duration::from_millis(250));
            cx.run_until_parked();
            if !cx.read(|app| view_entity.read(app).deploy_run_busy) {
                break;
            }
        }
        assert!(!cx.read(|app| view_entity.read(app).deploy_run_busy));
        assert!(cx.read(|app| view_entity.read(app).deploy_run_stage.contains("未完成")));
        assert_eq!(
            crate::release_store::recent(Some(project.id))
                .unwrap()
                .len(),
            0
        );
        let history_id = uuid::Uuid::new_v4().to_string();
        cx.update_window(handle, |_, window, cx| {
            view_entity.update(cx, |view, _cx| {
                view.deploy_tab = DeployTab::History;
                view.release_tasks = vec![crate::release_store::TaskSnapshot {
                    id: history_id.clone(),
                    project_id: project.id,
                    project_name: project.name.clone(),
                    host: host.clone(),
                    docker_endpoint: Some("unix:///var/run/docker.sock".into()),
                    docker_daemon_id: Some("TEST:DAEMON".into()),
                    phase: "checked".into(),
                    outcome: "complete".into(),
                    health_result: Some("healthy".into()),
                    detail: "已完成".into(),
                    started_at: "2026-10-27 10:00:00".into(),
                    execution_finished_at: Some("2026-10-27 10:01:30".into()),
                    image_reference: "sample:v1-amd64".into(),
                }];
                view.release_batches = vec![crate::batch_store::BatchRecord {
                    id: uuid::Uuid::new_v4().to_string(),
                    project_id: project.id,
                    config_sha256: "a".repeat(64),
                    parallelism: 1,
                    state: "paused".into(),
                    created_at: "2026-10-27 10:00:00".into(),
                    images: vec![crate::batch_store::BatchImageRecord {
                        service: "web".into(),
                        platform: "linux/amd64".into(),
                        reference: "sample:v1-amd64".into(),
                        archive_bytes: Some(123),
                        archive_sha256: Some("b".repeat(64)),
                        image_ids: None,
                    }],
                    targets: vec![
                        crate::batch_store::BatchTargetRecord {
                            ordinal: 0,
                            host: host.clone(),
                            platform: "linux/amd64".into(),
                            docker_endpoint: "unix:///var/run/docker.sock".into(),
                            docker_daemon_id: "TEST:DAEMON".into(),
                            env_sha256: None,
                            state: "healthy".into(),
                            release_task_id: Some(history_id.clone()),
                            task_outcome: Some("complete".into()),
                            attempt_task_ids: vec![history_id.clone()],
                        },
                        crate::batch_store::BatchTargetRecord {
                            ordinal: 1,
                            host: host.clone(),
                            platform: "linux/amd64".into(),
                            docker_endpoint: "unix:///var/run/docker.sock".into(),
                            docker_daemon_id: "OTHER:DAEMON".into(),
                            env_sha256: None,
                            state: "waiting".into(),
                            release_task_id: None,
                            task_outcome: None,
                            attempt_task_ids: Vec::new(),
                        },
                    ],
                }];
                view.release_tasks_loading = false;
            });
            window.draw(cx).clear(cx);
        })
        .unwrap();
        assert!(test_window.debug_bounds("batch-history-card").is_some());
        test_window.simulate_resize(size(px(1700.), px(900.)));
        cx.update_window(handle, |_, window, cx| window.draw(cx).clear(cx))
            .unwrap();
        cx.update_window(handle, |_, window, cx| {
            view_entity.update(cx, |view, _| {
                let batch = &mut view.release_batches[0];
                batch.state = "building".into();
                for target in &mut batch.targets {
                    target.state = "waiting".into();
                    target.release_task_id = None;
                    target.task_outcome = None;
                }
            });
            window.draw(cx).clear(cx);
        })
        .unwrap();
        let terminate = test_window.debug_bounds("batch-terminate-button").unwrap();
        test_window.simulate_click(terminate.center(), Modifiers::default());
        cx.update_window(handle, |_, window, cx| window.draw(cx).clear(cx))
            .unwrap();
        let cancel = test_window.debug_bounds("batch-resolution-cancel").unwrap();
        test_window.simulate_click(cancel.center(), Modifiers::default());
        assert!(crate::batch_store::recent(None).unwrap().is_empty());
        cx.update_window(handle, |_, window, cx| {
            view_entity.update(cx, |view, _| {
                let batch = &mut view.release_batches[0];
                batch.targets[0].release_task_id = Some(history_id.clone());
            });
            window.draw(cx).clear(cx);
        })
        .unwrap();
        assert!(test_window.debug_bounds("batch-terminate-button").is_none());
        cx.update_window(handle, |_, window, cx| {
            view_entity.update(cx, |view, _| {
                let batch = &mut view.release_batches[0];
                batch.state = "paused".into();
                batch.targets[0].state = "healthy".into();
                batch.targets[0].task_outcome = Some("complete".into());
            });
            window.draw(cx).clear(cx);
        })
        .unwrap();
        let batch_detail = test_window
            .debug_bounds("batch-task-detail-button")
            .unwrap();
        test_window.simulate_click(batch_detail.center(), Modifiers::default());
        assert_eq!(
            cx.read(|app| view_entity.read(app).release_history_selected.clone()),
            Some(history_id.clone())
        );
        cx.update_window(handle, |_, window, cx| {
            view_entity.update(cx, |view, cx| {
                view.release_history_selected = None;
                cx.notify();
            });
            window.draw(cx).clear(cx);
        })
        .unwrap();
        assert!(test_window.debug_bounds("batch-skip-button").is_none());
        cx.update_window(handle, |_, window, cx| {
            view_entity.update(cx, |view, _| {
                let batch = &mut view.release_batches[0];
                batch.targets[1].state = "failed".into();
                batch.targets[1].release_task_id = Some("failed-test-task".into());
                batch.targets[1].task_outcome = Some("failed".into());
                let mut failed = view.release_tasks[0].clone();
                failed.id = "failed-test-task".into();
                failed.phase = "checked".into();
                failed.outcome = "failed".into();
                failed.health_result = Some("failed".into());
                view.release_tasks.push(failed);
            });
            window.draw(cx).clear(cx);
        })
        .unwrap();
        assert!(test_window.debug_bounds("batch-skip-button").is_some());
        assert!(test_window.debug_bounds("batch-terminate-button").is_some());
        let skip = test_window.debug_bounds("batch-skip-button").unwrap();
        test_window.simulate_click(skip.center(), Modifiers::default());
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(300));
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| window.draw(cx).clear(cx))
            .unwrap();
        let cancel = test_window.debug_bounds("batch-resolution-cancel").unwrap();
        test_window.simulate_click(cancel.center(), Modifiers::default());
        assert!(cx.read(|app| view_entity.read(app).batch_resolving.is_empty()));
        assert!(crate::batch_store::recent(None).unwrap().is_empty());
        cx.update_window(handle, |_, window, cx| window.draw(cx).clear(cx))
            .unwrap();
        assert!(
            test_window
                .debug_bounds("batch-resolution-cancel")
                .is_none(),
            "resolution dialog remained open"
        );
        cx.update_window(handle, |_, window, cx| {
            view_entity.update(cx, |view, _| {
                view.release_batches[0].state = "running".into();
                view.release_batches[0].targets[1].state = "skipped".into();
            });
            window.draw(cx).clear(cx);
        })
        .unwrap();
        assert!(test_window.debug_bounds("batch-terminate-button").is_some());
        assert!(test_window.debug_bounds("batch-skip-button").is_none());
        cx.update_window(handle, |_, window, cx| {
            view_entity.update(cx, |view, _| {
                view.release_tasks
                    .retain(|task| task.id != "failed-test-task");
                view.release_batches[0].state = "paused".into();
                view.release_batches[0].targets[1].state = "waiting".into();
                view.release_batches[0].targets[1].release_task_id = None;
                view.release_batches[0].targets[1].task_outcome = None;
                view.release_batches[0].state = "running".into();
                assert_eq!(
                    super::pages::next_confirmed_batch_host(&view.release_batches[0]),
                    Some(host.id)
                );
                view.release_batches[0].targets[0].task_outcome = Some("blocked".into());
                assert_eq!(
                    super::pages::next_confirmed_batch_host(&view.release_batches[0]),
                    None
                );
                view.release_batches[0].targets[0].task_outcome = Some("complete".into());
                view.release_batches[0].targets[1].state = "unknown".into();
                assert_eq!(
                    super::pages::next_confirmed_batch_host(&view.release_batches[0]),
                    None
                );
                view.release_batches[0].targets[1].state = "waiting".into();
                view.release_batches[0].targets[0].state = "waiting".into();
                view.release_batches[0].targets[0].release_task_id = None;
                view.release_batches[0].targets[0].task_outcome = None;
                assert_eq!(
                    super::pages::next_confirmed_batch_host(&view.release_batches[0]),
                    Some(host.id),
                    "an interrupted first-host retry must remain resumable"
                );
                view.release_batches[0].targets[0].attempt_task_ids.clear();
                assert_eq!(
                    super::pages::next_confirmed_batch_host(&view.release_batches[0]),
                    None,
                    "an unstarted first host is not a retry"
                );
                view.release_batches[0].targets[0].state = "healthy".into();
                view.release_batches[0].targets[0].release_task_id = Some(history_id.clone());
                view.release_batches[0].targets[0].task_outcome = Some("complete".into());
                view.release_batches[0].targets[0].attempt_task_ids = vec![history_id.clone()];
                view.release_batches[0].parallelism = 2;
                assert_eq!(
                    super::pages::next_confirmed_batch_host(&view.release_batches[0]),
                    Some(host.id),
                    "a resolved first sibling must allow continuing the remaining wave"
                );
                assert!(!view.deploy_run_busy);
            });
            window.draw(cx).clear(cx);
        })
        .unwrap();
        assert!(test_window.debug_bounds("batch-history-card").is_some());
        assert!(test_window.debug_bounds("batch-terminate-button").is_some());
        let continue_button = test_window.debug_bounds("batch-continue-button").unwrap();
        test_window.simulate_click(continue_button.center(), Modifiers::default());
        cx.update_window(handle, |_, window, cx| window.draw(cx).clear(cx))
            .unwrap();
        let cancel = test_window.debug_bounds("batch-continue-cancel").unwrap();
        test_window.simulate_click(cancel.center(), Modifiers::default());
        assert!(cx.read(|app| view_entity.read(app).batch_resolving.is_empty()));
        assert!(crate::batch_store::recent(None).unwrap().is_empty());
        let detail = test_window
            .debug_bounds("history-detail-button")
            .expect("history detail rendered");
        test_window.simulate_click(detail.center(), Modifiers::default());
        assert_eq!(
            cx.read(|app| view_entity.read(app).release_history_selected.clone()),
            Some(history_id.clone())
        );
        cx.update_window(handle, |_, window, cx| {
            window.draw(cx).clear(cx);
        })
        .unwrap();
        let collapse = test_window.debug_bounds("history-detail-button").unwrap();
        test_window.simulate_click(collapse.center(), Modifiers::default());
        assert!(cx.read(|app| view_entity.read(app).release_history_selected.is_none()));
        cx.update_window(handle, |_, window, cx| {
            view_entity.update(cx, |view, _| {
                view.release_tasks[0].phase = "uploading".into();
                view.release_tasks[0].outcome = "unknown".into();
                view.release_tasks[0].health_result = None;
                view.release_batches[0].state = "unknown".into();
                view.release_batches[0].targets[0].state = "unknown".into();
                view.release_batches[0].targets[0].task_outcome = Some("unknown".into());
                view.release_history_selected = None;
            });
            window.draw(cx).clear(cx);
        })
        .unwrap();
        let unknown_detail = test_window
            .debug_bounds("batch-task-detail-button")
            .unwrap();
        test_window.simulate_click(unknown_detail.center(), Modifiers::default());
        cx.update_window(handle, |_, window, cx| window.draw(cx).clear(cx))
            .unwrap();
        assert_eq!(
            cx.read(|app| view_entity.read(app).release_history_selected.clone()),
            Some(history_id.clone())
        );
        assert!(
            test_window
                .debug_bounds("release-upload-inspect-button")
                .is_some()
        );
        let disabled_resume = test_window
            .debug_bounds("release-upload-resume-button")
            .unwrap();
        test_window.simulate_click(disabled_resume.center(), Modifiers::default());
        assert!(cx.read(|app| view_entity.read(app).batch_resolving.is_empty()));
        cx.update_window(handle, |_, window, cx| {
            view_entity.update(cx, |view, _| {
                view.release_upload_inspections.insert(
                    history_id.clone(),
                    Ok(crate::release::UnknownUploadInspection {
                        containers: vec![],
                        archives: vec![crate::ssh::RemoteArchiveObservation {
                            service: "web".into(),
                            directory: "/tmp/devingpanel.aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
                            state: "reserved".into(),
                            expected_bytes: 100,
                            actual_bytes: Some(40),
                            digest_matches: None,
                        }],
                    }),
                );
            });
            window.draw(cx).clear(cx);
        })
        .unwrap();
        test_window.simulate_resize(size(px(1700.), px(1800.)));
        cx.update_window(handle, |_, window, cx| window.draw(cx).clear(cx))
            .unwrap();
        let resume = test_window
            .debug_bounds("release-upload-resume-button")
            .unwrap();
        test_window.simulate_click(resume.center(), Modifiers::default());
        cx.update_window(handle, |_, window, cx| window.draw(cx).clear(cx))
            .unwrap();
        let cancel = test_window.debug_bounds("batch-resume-cancel").unwrap();
        test_window.simulate_click(cancel.center(), Modifiers::default());
        assert!(cx.read(|app| view_entity.read(app).batch_resolving.is_empty()));
        assert!(crate::batch_store::recent(None).unwrap().is_empty());
        cx.update_window(handle, |_, window, cx| {
            view_entity.update(cx, |view, _| {
                view.release_tasks[0].phase = "deploying".into();
                view.release_upload_inspections.clear();
                view.release_history_selected = Some(history_id.clone());
            });
            window.draw(cx).clear(cx);
        })
        .unwrap();
        assert!(
            test_window
                .debug_bounds("release-reconcile-health-button")
                .is_some()
        );
        assert!(cx.read(|app| view_entity.read(app).release_resolving.is_empty()));
        assert!(crate::batch_store::recent(None).unwrap().is_empty());
        cx.update_window(handle, |_, window, cx| {
            view_entity.update(cx, |view, cx| {
                let mut other = project.clone();
                other.id += 1000;
                view.projects.push(other.clone());
                view.select_deploy_project(other.id, window, cx);
                assert!(view.deploy_run_project_id.is_none());
                assert!(view.deploy_run_host_id.is_none());
                assert!(view.deploy_run_stage.is_empty());
                assert!(view.deploy_run_transfer.is_none());
            });
        })
        .unwrap();
        cx.update_window(handle, |_, _, cx| {
            view_entity.update(cx, |view, _| {
                let batch_id = view.release_batches[0].id.clone();
                let global = &mut view.release_batches[0];
                global.state = "paused".into();
                view.release_project_batches.insert(
                    project.id,
                    vec![crate::batch_store::BatchRecord {
                        id: batch_id.clone(),
                        project_id: project.id,
                        config_sha256: "a".repeat(64),
                        parallelism: 1,
                        state: "building".into(),
                        created_at: "2026-10-27 10:00:00".into(),
                        images: vec![],
                        targets: vec![],
                    }],
                );
                assert_eq!(
                    view.history_batch(Some(project.id), &batch_id)
                        .unwrap()
                        .state,
                    "building"
                );
                assert_eq!(view.history_batch(None, &batch_id).unwrap().state, "paused");
                assert!(
                    view.history_batch(Some(project.id + 1), &batch_id)
                        .is_none()
                );
            });
        })
        .unwrap();
    }

    #[gpui_kit::test]
    #[ignore = "requires a fresh isolated APPDATA under target/isolated-tests; no Docker or SSH operations"]
    fn isolated_gui_resolves_unstarted_and_failed_batches(cx: &mut gpui_kit::TestAppContext) {
        use super::{AppView, DeployTab, Page};
        use gpui_kit::component::WindowExt as _;
        use gpui_kit::{
            AppContext as _, Modifiers, ScrollDelta, ScrollWheelEvent, WindowOptions, point, px,
            size,
        };

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
                .starts_with("ui-batch-terminate-")
        );
        assert!(std::fs::read_dir(&root).unwrap().next().is_none());

        let conn = crate::database::connect().unwrap();
        conn.execute(
            "INSERT INTO hosts (id, name, address, port, username, auth, group_name,
             credential_id, trusted_fingerprint)
             VALUES (1, 'vps-smoke', '127.0.0.1', 1, 'test', 'password', 'test',
             'missing-test-vault', 'SHA256:test')",
            [],
        )
        .unwrap();
        let project = crate::project_store::create_from_manifest(
            &std::env::current_dir()
                .unwrap()
                .join("tests/fixtures/stack-v2-success"),
        )
        .unwrap();
        let batch_id = uuid::Uuid::new_v4().to_string();
        conn.execute(
            "INSERT INTO release_batches (id, project_id, config_sha256, state)
             VALUES (?1, ?2, ?3, 'building')",
            rusqlite::params![batch_id, project.id, "a".repeat(64)],
        )
        .unwrap();
        for ordinal in 0..2 {
            conn.execute(
                "INSERT INTO release_batch_targets (batch_id, ordinal, host_id, host_name,
                 host_group, host_address, host_port, host_username, host_auth, host_fingerprint,
                 platform, docker_endpoint, docker_daemon_id, state)
                 VALUES (?1, ?2, ?3, ?4, 'test', '127.0.0.1', 22, 'test', 'password',
                 'SHA256:test', 'linux/amd64', 'unix:///var/run/docker.sock', ?5, 'waiting')",
                rusqlite::params![
                    batch_id,
                    ordinal,
                    ordinal + 1,
                    format!("host-{ordinal}"),
                    format!("DAEMON:{ordinal}")
                ],
            )
            .unwrap();
        }
        let guard = crate::release_materials::acquire_guard(project.id).unwrap();
        assert!(crate::batch_store::terminate_unstarted(&batch_id).is_err());
        assert_eq!(
            crate::batch_store::get(&batch_id).unwrap().state,
            "building"
        );
        drop(guard);

        cx.update(gpui_kit::init);
        let (handle, view_entity) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| AppView::new(window, cx))
            })
            .unwrap()
        });
        cx.update_window(handle, |_, window, cx| {
            view_entity.update(cx, |view, cx| {
                view.page = Page::Deploy;
                view.deploy_tab = DeployTab::History;
                view.projects = vec![project.clone()];
                view.project_loaded = true;
                view.release_batches = vec![crate::batch_store::get(&batch_id).unwrap()];
                view.release_tasks_loading = false;
                cx.notify();
            });
            window.draw(cx).clear(cx);
        })
        .unwrap();
        let mut test_window = gpui_kit::VisualTestContext::from_window(handle, cx);
        test_window.simulate_resize(size(px(1700.), px(900.)));
        cx.update_window(handle, |_, window, cx| window.draw(cx).clear(cx))
            .unwrap();
        let button = test_window.debug_bounds("batch-terminate-button").unwrap();
        test_window.simulate_click(button.center(), Modifiers::default());
        cx.update_window(handle, |_, window, cx| {
            assert!(window.has_active_dialog(cx));
            window.draw(cx).clear(cx);
        })
        .unwrap();
        let confirm = test_window
            .debug_bounds("batch-resolution-confirm")
            .unwrap();
        test_window.simulate_click(confirm.center(), Modifiers::default());
        for _ in 0..20 {
            cx.executor()
                .advance_clock(std::time::Duration::from_millis(250));
            cx.run_until_parked();
            if cx.read(|app| view_entity.read(app).batch_resolving.is_empty()) {
                break;
            }
        }
        assert!(cx.read(|app| view_entity.read(app).batch_resolving.is_empty()));
        assert_eq!(
            crate::batch_store::get(&batch_id).unwrap().state,
            "terminated"
        );
        for _ in 0..20 {
            if cx.read(|app| {
                view_entity
                    .read(app)
                    .history_batch(None, &batch_id)
                    .is_some_and(|batch| batch.state == "terminated")
            }) {
                break;
            }
            cx.executor()
                .advance_clock(std::time::Duration::from_millis(250));
            cx.run_until_parked();
        }
        assert!(cx.read(|app| {
            view_entity
                .read(app)
                .history_batch(None, &batch_id)
                .is_some_and(|batch| batch.state == "terminated")
        }));

        let failed_batch_id = uuid::Uuid::new_v4().to_string();
        conn.execute(
            "INSERT INTO release_batches (id, project_id, config_sha256, parallelism, state)
             VALUES (?1, ?2, ?3, 2, 'paused')",
            rusqlite::params![failed_batch_id, project.id, "b".repeat(64)],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO release_tasks (id, project_id, host_id, project_name, host_name,
             host_address, host_port, host_username, host_auth, host_fingerprint,
             docker_endpoint, docker_daemon_id, image_reference, platform, config_sha256,
             phase, outcome, health_result)
             VALUES ('healthy-batch-task', ?1, 1, 'smoke', 'first', '127.0.0.1', 22,
             'test', 'password', 'SHA256:test', 'unix:///var/run/docker.sock',
             'DAEMON:FIRST', 'smoke:v1', 'linux/amd64', 'digest', 'checked',
             'complete', 'healthy'),
             ('failed-batch-task', ?1, 2, 'smoke', 'second', '127.0.0.1', 22,
             'test', 'password', 'SHA256:test', 'unix:///var/run/docker.sock',
             'DAEMON:SECOND', 'smoke:v1', 'linux/amd64', 'digest', 'checked',
             'failed', 'failed')",
            [project.id],
        )
        .unwrap();
        for (ordinal, state, task_id) in [
            (0, "healthy", Some("healthy-batch-task")),
            (1, "failed", Some("failed-batch-task")),
            (2, "waiting", None),
        ] {
            conn.execute(
                "INSERT INTO release_batch_targets (batch_id, ordinal, host_id, host_name,
                 host_group, host_address, host_port, host_username, host_auth, host_fingerprint,
                 platform, docker_endpoint, docker_daemon_id, state, release_task_id)
                 VALUES (?1, ?2, ?3, ?4, 'test', '127.0.0.1', 22, 'test', 'password',
                 'SHA256:test', 'linux/amd64', 'unix:///var/run/docker.sock', ?5, ?6, ?7)",
                rusqlite::params![
                    failed_batch_id,
                    ordinal,
                    ordinal + 1,
                    format!("host-{ordinal}"),
                    format!("DAEMON:{ordinal}"),
                    state,
                    task_id
                ],
            )
            .unwrap();
        }
        conn.execute_batch(
            "INSERT INTO release_batch_attempts (batch_id, ordinal, attempt, task_id)
             SELECT batch_id, ordinal, 1, release_task_id FROM release_batch_targets
             WHERE release_task_id IN ('healthy-batch-task', 'failed-batch-task')",
        )
        .unwrap();
        cx.update_window(handle, |_, window, cx| {
            view_entity.update(cx, |view, cx| {
                view.release_batches = vec![crate::batch_store::get(&failed_batch_id).unwrap()];
                view.release_tasks = crate::release_store::recent(None).unwrap();
                cx.notify();
            });
            window.draw(cx).clear(cx);
        })
        .unwrap();
        let retry = test_window.debug_bounds("batch-retry-button").unwrap();
        test_window.simulate_click(retry.center(), Modifiers::default());
        cx.update_window(handle, |_, window, cx| {
            assert!(window.has_active_dialog(cx));
            window.draw(cx).clear(cx);
        })
        .unwrap();
        let cancel = test_window.debug_bounds("batch-retry-cancel").unwrap();
        test_window.simulate_click(cancel.center(), Modifiers::default());
        cx.update_window(handle, |_, window, cx| {
            assert!(!window.has_active_dialog(cx));
            window.draw(cx).clear(cx);
        })
        .unwrap();
        assert_eq!(
            crate::batch_store::get(&failed_batch_id).unwrap().state,
            "paused"
        );
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM release_tasks WHERE project_id = ?1",
                [project.id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 2);
        let skip = test_window.debug_bounds("batch-skip-button").unwrap();
        assert!(test_window.debug_bounds("batch-continue-button").is_none());
        test_window.simulate_click(skip.center(), Modifiers::default());
        cx.update_window(handle, |_, window, cx| window.draw(cx).clear(cx))
            .unwrap();
        let confirm = test_window
            .debug_bounds("batch-resolution-confirm")
            .unwrap();
        test_window.simulate_click(confirm.center(), Modifiers::default());
        for _ in 0..20 {
            cx.executor()
                .advance_clock(std::time::Duration::from_millis(250));
            cx.run_until_parked();
            if cx.read(|app| view_entity.read(app).batch_resolving.is_empty()) {
                break;
            }
        }
        let skipped = crate::batch_store::get(&failed_batch_id).unwrap();
        assert_eq!(skipped.state, "running");
        assert_eq!(skipped.targets[0].state, "healthy");
        assert_eq!(skipped.targets[1].state, "skipped");
        assert_eq!(skipped.targets[2].state, "waiting");
        assert!(skipped.targets[2].release_task_id.is_none());
        for _ in 0..20 {
            if cx.read(|app| {
                view_entity
                    .read(app)
                    .history_batch(None, &failed_batch_id)
                    .is_some_and(|batch| {
                        batch.state == "running"
                            && batch.targets[1].state == "skipped"
                            && batch.targets[2].state == "waiting"
                    })
            }) {
                break;
            }
            cx.executor()
                .advance_clock(std::time::Duration::from_millis(250));
            cx.run_until_parked();
        }
        assert!(cx.read(|app| {
            view_entity
                .read(app)
                .history_batch(None, &failed_batch_id)
                .is_some_and(|batch| {
                    batch.state == "running"
                        && batch.targets[1].state == "skipped"
                        && batch.targets[2].state == "waiting"
                })
        }));
        cx.update_window(handle, |_, window, cx| window.draw(cx).clear(cx))
            .unwrap();
        let continue_button = test_window.debug_bounds("batch-continue-button").unwrap();
        test_window.simulate_click(continue_button.center(), Modifiers::default());
        cx.update_window(handle, |_, window, cx| {
            assert!(window.has_active_dialog(cx));
            window.draw(cx).clear(cx);
        })
        .unwrap();
        let cancel = test_window.debug_bounds("batch-continue-cancel").unwrap();
        test_window.simulate_click(cancel.center(), Modifiers::default());
        assert!(
            crate::batch_store::get(&failed_batch_id).unwrap().targets[2]
                .release_task_id
                .is_none()
        );
        cx.update_window(handle, |_, window, cx| window.draw(cx).clear(cx))
            .unwrap();
        let terminate = test_window.debug_bounds("batch-terminate-button").unwrap();
        test_window.simulate_click(terminate.center(), Modifiers::default());
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(300));
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| window.draw(cx).clear(cx))
            .unwrap();
        let confirm = test_window
            .debug_bounds("batch-resolution-confirm")
            .unwrap();
        test_window.simulate_click(confirm.center(), Modifiers::default());
        for _ in 0..20 {
            cx.executor()
                .advance_clock(std::time::Duration::from_millis(250));
            cx.run_until_parked();
            if cx.read(|app| view_entity.read(app).batch_resolving.is_empty()) {
                break;
            }
        }
        let terminated = crate::batch_store::get(&failed_batch_id).unwrap();
        assert_eq!(terminated.state, "terminated");
        assert_eq!(terminated.targets[0].state, "healthy");
        assert_eq!(terminated.targets[1].state, "skipped");
        assert_eq!(terminated.targets[2].state, "waiting");
        for _ in 0..20 {
            if cx.read(|app| {
                view_entity
                    .read(app)
                    .history_batch(None, &failed_batch_id)
                    .is_some_and(|batch| batch.state == "terminated")
            }) {
                break;
            }
            cx.executor()
                .advance_clock(std::time::Duration::from_millis(250));
            cx.run_until_parked();
        }
        assert!(cx.read(|app| {
            view_entity
                .read(app)
                .history_batch(None, &failed_batch_id)
                .is_some_and(|batch| batch.state == "terminated")
        }));

        cx.update_window(handle, |_, window, cx| {
            view_entity.update(cx, |view, cx| {
                view.release_tasks.clear();
                view.release_history_selected = None;
                cx.notify();
            });
            window.draw(cx).clear(cx);
        })
        .unwrap();
        let detail = test_window
            .debug_bounds("batch-task-detail-button")
            .unwrap();
        test_window.simulate_click(detail.center(), Modifiers::default());
        for _ in 0..20 {
            cx.executor()
                .advance_clock(std::time::Duration::from_millis(250));
            cx.run_until_parked();
            if cx.read(|app| view_entity.read(app).release_history_loading_task.is_none()) {
                break;
            }
        }
        let selected = cx
            .read(|app| view_entity.read(app).release_history_selected.clone())
            .unwrap();
        assert!(["healthy-batch-task", "failed-batch-task"].contains(&selected.as_str()));
        assert_eq!(
            cx.read(|app| {
                view_entity
                    .read(app)
                    .release_history_loaded_task
                    .as_ref()
                    .map(|task| task.id.clone())
            }),
            Some(selected)
        );
        assert!(cx.read(|app| view_entity.read(app).release_history_load_error.is_none()));
        cx.update_window(handle, |_, window, cx| window.draw(cx).clear(cx))
            .unwrap();
        assert!(
            test_window
                .debug_bounds("release-task-detail-card")
                .is_some()
        );

        test_window.simulate_resize(size(px(760.), px(1600.)));
        cx.update_window(handle, |_, window, cx| window.draw(cx).clear(cx))
            .unwrap();
        let scroll = test_window.debug_bounds("batch-history-scroll").unwrap();
        assert!(scroll.origin.x >= px(0.));
        assert!(scroll.origin.x + scroll.size.width <= px(760.));
        let visible = |bounds: gpui_kit::Bounds<gpui_kit::Pixels>| {
            bounds.origin.x >= scroll.origin.x
                && bounds.origin.x + bounds.size.width <= scroll.origin.x + scroll.size.width
        };
        let mut action = test_window
            .debug_bounds("batch-task-detail-button")
            .unwrap();
        assert!(
            !visible(action),
            "narrow batch table did not overflow horizontally"
        );
        for delta in [-1200., 1200.] {
            if visible(action) {
                break;
            }
            test_window.simulate_event(ScrollWheelEvent {
                position: point(scroll.center().x, action.center().y),
                delta: ScrollDelta::Pixels(point(px(delta), px(0.))),
                modifiers: Modifiers::default(),
                touch_phase: Default::default(),
            });
            cx.update_window(handle, |_, window, cx| window.draw(cx).clear(cx))
                .unwrap();
            action = test_window
                .debug_bounds("batch-task-detail-button")
                .unwrap();
        }
        assert!(
            visible(action),
            "narrow batch action stayed outside the scroll viewport"
        );

        conn.execute(
            "INSERT INTO release_tasks (id, project_id, host_id, project_name, host_name,
             host_address, host_port, host_username, host_auth, host_fingerprint,
             docker_endpoint, docker_daemon_id, image_reference, platform, config_sha256,
             phase, outcome, health_result)
             SELECT 'older-failed-task', project_id, host_id, project_name, host_name,
             host_address, host_port, host_username, host_auth, host_fingerprint,
             docker_endpoint, docker_daemon_id, image_reference, platform, config_sha256,
             phase, outcome, health_result FROM release_tasks WHERE id = 'failed-batch-task'",
            [],
        )
        .unwrap();
        conn.execute_batch(
            "UPDATE release_batch_attempts SET attempt = 2
             WHERE task_id = 'failed-batch-task';
             INSERT INTO release_batch_attempts (batch_id, ordinal, attempt, task_id)
             SELECT batch_id, ordinal, 1, 'older-failed-task'
             FROM release_batch_targets WHERE release_task_id = 'failed-batch-task';",
        )
        .unwrap();
        test_window.simulate_resize(size(px(1700.), px(900.)));
        cx.update_window(handle, |_, window, cx| {
            view_entity.update(cx, |view, cx| {
                view.release_batches = vec![crate::batch_store::get(&failed_batch_id).unwrap()];
                view.release_history_selected = None;
                view.release_history_loaded_task = None;
                cx.notify();
            });
            window.draw(cx).clear(cx);
        })
        .unwrap();
        let older = test_window
            .debug_bounds("batch-attempt-detail-button")
            .unwrap();
        test_window.simulate_click(older.center(), Modifiers::default());
        for _ in 0..20 {
            cx.executor()
                .advance_clock(std::time::Duration::from_millis(250));
            cx.run_until_parked();
            if cx.read(|app| view_entity.read(app).release_history_loading_task.is_none()) {
                break;
            }
        }
        assert_eq!(
            cx.read(|app| {
                view_entity
                    .read(app)
                    .release_history_loaded_task
                    .as_ref()
                    .map(|task| task.id.clone())
            }),
            Some("older-failed-task".into())
        );
    }

    #[gpui_kit::test]
    #[ignore = "read-only VPS audit plus GUI skip/terminate on a copied failed LAN batch"]
    fn isolated_gui_lan_resolves_audited_health_failure(cx: &mut gpui_kit::TestAppContext) {
        use super::{AppView, DeployTab, Page};
        use gpui_kit::component::WindowExt as _;
        use gpui_kit::{AppContext as _, Modifiers, WindowOptions, px, size};

        let appdata = std::path::PathBuf::from(std::env::var_os("APPDATA").unwrap())
            .canonicalize()
            .unwrap();
        let root = appdata.parent().unwrap();
        assert!(
            root.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("ui-lan-resolve-")
        );
        assert_eq!(
            root.parent().unwrap(),
            std::env::current_dir()
                .unwrap()
                .join("target/isolated-tests")
                .canonicalize()
                .unwrap()
        );
        let action = std::env::var("DEVINGPANEL_TEST_BATCH_RESOLUTION").unwrap();
        assert!(matches!(action.as_str(), "skip" | "terminate"));
        let batch_id = "11c78bf5-86e2-44b4-8248-02b7595cf404";
        let failed_task = "f2d2510d-f16c-4f1c-afaf-ffc6c883ded8";
        let batch = crate::batch_store::get(batch_id).unwrap();
        assert_eq!(batch.project_id, 1856624452);
        assert_eq!(batch.state, "paused");
        assert_eq!(batch.targets[0].state, "healthy");
        assert_eq!(batch.targets[1].state, "failed");
        assert_eq!(
            batch.targets[1].release_task_id.as_deref(),
            Some(failed_task)
        );
        assert_eq!(
            crate::release_store::get_task(failed_task).unwrap().outcome,
            "failed"
        );
        let conn = crate::database::connect().unwrap();
        let pointer_count = || -> i64 {
            conn.query_row(
                "SELECT COUNT(*) FROM project_current_version WHERE project_id = ?1",
                [batch.project_id],
                |row| row.get(0),
            )
            .unwrap()
        };
        assert_eq!(pointer_count(), 1);

        let mut baselines = Vec::new();
        for (target, (address, daemon)) in batch.targets.iter().zip([
            ("192.168.63.129", "882622c2-a12a-4ac3-99d4-c3ab389f2641"),
            ("192.168.63.130", "ac70e19f-a489-49f3-82f0-0a4ad66ca2e6"),
        ]) {
            assert_eq!(target.host.address, address);
            assert_eq!(target.docker_daemon_id, daemon);
            assert_eq!(
                crate::ssh::docker_identity_for_host(target.host.clone())
                    .unwrap()
                    .1,
                daemon
            );
            let mut ids = crate::ssh::inventory(target.host.id)
                .unwrap()
                .containers
                .iter()
                .map(|container| container.id.clone())
                .collect::<Vec<_>>();
            ids.sort();
            baselines.push(ids);
        }
        assert!(
            crate::ssh::observe_release_project(
                &crate::release_store::get_task(failed_task).unwrap()
            )
            .unwrap()
            .containers
            .is_empty()
        );

        cx.update(gpui_kit::init);
        let (handle, entity) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| AppView::new(window, cx))
            })
            .unwrap()
        });
        cx.update_window(handle, |_, window, cx| {
            entity.update(cx, |view, cx| {
                view.page = Page::Deploy;
                view.deploy_tab = DeployTab::History;
                view.release_batches = vec![crate::batch_store::get(batch_id).unwrap()];
                view.release_tasks_loading = false;
                cx.notify();
            });
            window.draw(cx).clear(cx);
        })
        .unwrap();
        let mut visual = gpui_kit::VisualTestContext::from_window(handle, cx);
        visual.simulate_resize(size(px(1700.), px(900.)));
        cx.update_window(handle, |_, window, cx| window.draw(cx).clear(cx))
            .unwrap();
        let button = visual
            .debug_bounds(if action == "skip" {
                "batch-skip-button"
            } else {
                "batch-terminate-button"
            })
            .unwrap();
        visual.simulate_click(button.center(), Modifiers::default());
        cx.update_window(handle, |_, window, cx| {
            assert!(window.has_active_dialog(cx));
            window.draw(cx).clear(cx);
        })
        .unwrap();
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(350));
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| window.draw(cx).clear(cx))
            .unwrap();
        let confirm = visual.debug_bounds("batch-resolution-confirm").unwrap();
        visual.simulate_click(confirm.center(), Modifiers::default());
        let expected = if action == "skip" {
            "complete"
        } else {
            "terminated"
        };
        let started = std::time::Instant::now();
        loop {
            cx.executor()
                .advance_clock(std::time::Duration::from_millis(250));
            cx.run_until_parked();
            let saved = crate::batch_store::get(batch_id).unwrap();
            if saved.state == expected {
                break;
            }
            assert!(
                started.elapsed() < std::time::Duration::from_secs(30),
                "GUI {action} did not settle"
            );
        }
        let saved = crate::batch_store::get(batch_id).unwrap();
        assert_eq!(saved.targets[0].state, "healthy");
        assert_eq!(
            saved.targets[1].state,
            if action == "skip" {
                "skipped"
            } else {
                "failed"
            }
        );
        assert_eq!(pointer_count(), 1);
        assert_eq!(
            crate::release_store::get_task(failed_task).unwrap().outcome,
            "failed"
        );
        for _ in 0..20 {
            if cx.read(|app| {
                entity
                    .read(app)
                    .history_batch(None, batch_id)
                    .is_some_and(|batch| batch.state == expected)
            }) {
                break;
            }
            cx.executor()
                .advance_clock(std::time::Duration::from_millis(250));
            cx.run_until_parked();
        }
        assert!(cx.read(|app| {
            entity
                .read(app)
                .history_batch(None, batch_id)
                .is_some_and(|batch| batch.state == expected)
        }));
        for (target, before) in saved.targets.iter().zip(&baselines) {
            let mut after = crate::ssh::inventory(target.host.id)
                .unwrap()
                .containers
                .iter()
                .map(|container| container.id.clone())
                .collect::<Vec<_>>();
            after.sort();
            assert_eq!(&after, before);
        }
        eprintln!("LAN GUI {action} resolved copied health failure: batch={batch_id}");
    }

    #[gpui_kit::test]
    #[ignore = "one-shot live LAN health failure GUI audit; cleanup is separately verified"]
    fn isolated_gui_lan_resolves_live_first_host_failure(cx: &mut gpui_kit::TestAppContext) {
        use super::{AppView, DeployTab, Page};
        use gpui_kit::component::WindowExt as _;
        use gpui_kit::{AppContext as _, Modifiers, WindowOptions, px, size};

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
        let stage = std::env::var("DEVINGPANEL_TEST_LAN_GUI_STAGE").unwrap();
        assert!(matches!(stage.as_str(), "before" | "after"));
        let batch_id = "0f657b6d-5704-478b-9c4b-77891812b52a";
        let task_id = "9ff19800-bdb8-4e48-b228-bce2c8550419";
        let batch = crate::batch_store::get(batch_id).unwrap();
        assert_eq!(batch.project_id, 1856624464);
        assert_eq!(batch.state, "paused");
        assert_eq!(batch.targets[0].state, "failed");
        assert_eq!(batch.targets[0].host.id, 2);
        assert_eq!(batch.targets[0].release_task_id.as_deref(), Some(task_id));
        assert_eq!(batch.targets[1].state, "waiting");
        assert!(batch.targets[1].release_task_id.is_none());
        let task = crate::release_store::get_task(task_id).unwrap();
        assert_eq!(
            (
                task.phase.as_str(),
                task.outcome.as_str(),
                task.health_result.as_deref()
            ),
            ("checked", "blocked", Some("failed"))
        );
        let project = crate::project_store::effective(batch.project_id).unwrap();
        assert!(
            std::path::Path::new(&project.path)
                .canonicalize()
                .unwrap()
                .starts_with(root)
        );
        let mut baselines = Vec::new();
        for (target, (address, daemon)) in batch.targets.iter().zip([
            ("192.168.63.130", "ac70e19f-a489-49f3-82f0-0a4ad66ca2e6"),
            ("192.168.63.129", "882622c2-a12a-4ac3-99d4-c3ab389f2641"),
        ]) {
            assert_eq!(target.host.address, address);
            assert_eq!(target.docker_daemon_id, daemon);
            assert_eq!(
                crate::ssh::docker_identity_for_host(target.host.clone())
                    .unwrap()
                    .1,
                daemon
            );
            let mut ids = crate::ssh::inventory(target.host.id)
                .unwrap()
                .containers
                .iter()
                .map(|container| container.id.clone())
                .collect::<Vec<_>>();
            ids.sort();
            baselines.push(ids);
        }
        let expected_containers = if stage == "before" { 4 } else { 0 };
        assert_eq!(
            crate::ssh::observe_release_project(&task)
                .unwrap()
                .containers
                .len(),
            expected_containers
        );

        cx.update(gpui_kit::init);
        let (handle, entity) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| AppView::new(window, cx))
            })
            .unwrap()
        });
        cx.update_window(handle, |_, window, cx| {
            entity.update(cx, |view, cx| {
                view.page = Page::Deploy;
                view.deploy_tab = DeployTab::History;
                view.projects = vec![project.clone()];
                view.project_loaded = true;
                view.release_tasks = vec![task.clone()];
                view.release_batches = vec![crate::batch_store::get(batch_id).unwrap()];
                view.release_tasks_loading = false;
                view.release_history_selected = Some(task_id.into());
                cx.notify();
            });
            window.draw(cx).clear(cx);
        })
        .unwrap();
        let mut visual = gpui_kit::VisualTestContext::from_window(handle, cx);
        visual.simulate_resize(size(px(1700.), px(1200.)));
        cx.update_window(handle, |_, window, cx| window.draw(cx).clear(cx))
            .unwrap();
        assert!(visual.debug_bounds("batch-skip-button").is_none());
        assert!(visual.debug_bounds("batch-terminate-button").is_none());
        let inspect = visual.debug_bounds("release-inspect-button").unwrap();
        visual.simulate_click(inspect.center(), Modifiers::default());
        let started = std::time::Instant::now();
        loop {
            cx.executor()
                .advance_clock(std::time::Duration::from_millis(200));
            cx.run_until_parked();
            let count = cx.read(|app| {
                entity
                    .read(app)
                    .release_observations
                    .get(task_id)
                    .and_then(|result| result.as_ref().ok().map(Vec::len))
            });
            if let Some(count) = count {
                assert_eq!(count, expected_containers);
                break;
            }
            assert!(started.elapsed() < std::time::Duration::from_secs(60));
        }
        cx.update_window(handle, |_, window, cx| window.draw(cx).clear(cx))
            .unwrap();
        if stage == "before" {
            assert!(
                visual
                    .debug_bounds("release-confirm-absence-button")
                    .is_none()
            );
            assert_eq!(
                crate::release_store::get_task(task_id).unwrap().outcome,
                "blocked"
            );
            assert_eq!(crate::batch_store::get(batch_id).unwrap().state, "paused");
        } else {
            let close = visual
                .debug_bounds("release-confirm-absence-button")
                .unwrap();
            visual.simulate_click(close.center(), Modifiers::default());
            cx.update_window(handle, |_, window, cx| {
                assert!(window.has_active_dialog(cx));
                window.draw(cx).clear(cx);
            })
            .unwrap();
            std::thread::sleep(std::time::Duration::from_millis(350));
            cx.run_until_parked();
            cx.update_window(handle, |_, window, cx| window.draw(cx).clear(cx))
                .unwrap();
            let confirm = visual
                .debug_bounds("release-absence-confirm-button")
                .unwrap();
            visual.simulate_click(confirm.center(), Modifiers::default());
            let dialog_open = cx
                .update_window(handle, |_, window, cx| window.has_active_dialog(cx))
                .unwrap();
            assert!(
                !dialog_open,
                "absence confirmation did not close its dialog"
            );
            let started = std::time::Instant::now();
            while crate::release_store::get_task(task_id).unwrap().outcome != "failed" {
                cx.executor()
                    .advance_clock(std::time::Duration::from_millis(200));
                cx.run_until_parked();
                if started.elapsed() >= std::time::Duration::from_secs(60) {
                    let (resolving, notifications) = cx
                        .update_window(handle, |_, window, cx| {
                            (
                                entity.read(cx).release_resolving.contains(task_id),
                                window.notifications(cx).len(),
                            )
                        })
                        .unwrap();
                    panic!(
                        "absence closure timed out: resolving={resolving}, notifications={notifications}"
                    );
                }
            }
            for _ in 0..30 {
                cx.executor()
                    .advance_clock(std::time::Duration::from_millis(200));
                cx.run_until_parked();
                if cx.read(|app| {
                    entity
                        .read(app)
                        .history_batch(None, batch_id)
                        .is_some_and(|batch| {
                            batch.targets[0].task_outcome.as_deref() == Some("failed")
                        })
                }) {
                    break;
                }
            }
            assert!(cx.read(|app| {
                entity
                    .read(app)
                    .history_batch(None, batch_id)
                    .is_some_and(|batch| batch.targets[0].task_outcome.as_deref() == Some("failed"))
            }));
            cx.update_window(handle, |_, window, cx| {
                entity.update(cx, |view, cx| {
                    view.release_batches = vec![crate::batch_store::get(batch_id).unwrap()];
                    view.release_history_selected = None;
                    cx.notify();
                });
                window.draw(cx).clear(cx);
            })
            .unwrap();
            let skip = visual.debug_bounds("batch-skip-button").unwrap();
            visual.simulate_click(skip.center(), Modifiers::default());
            cx.update_window(handle, |_, window, cx| {
                assert!(window.has_active_dialog(cx));
                window.draw(cx).clear(cx);
            })
            .unwrap();
            cx.executor()
                .advance_clock(std::time::Duration::from_millis(350));
            cx.run_until_parked();
            cx.update_window(handle, |_, window, cx| window.draw(cx).clear(cx))
                .unwrap();
            let confirm = visual.debug_bounds("batch-resolution-confirm").unwrap();
            visual.simulate_click(confirm.center(), Modifiers::default());
            let started = std::time::Instant::now();
            loop {
                cx.executor()
                    .advance_clock(std::time::Duration::from_millis(200));
                cx.run_until_parked();
                let batch = crate::batch_store::get(batch_id).unwrap();
                if batch.state == "running" && batch.targets[0].state == "skipped" {
                    break;
                }
                assert!(started.elapsed() < std::time::Duration::from_secs(30));
            }
            cx.update_window(handle, |_, window, cx| {
                entity.update(cx, |view, cx| {
                    view.release_batches = vec![crate::batch_store::get(batch_id).unwrap()];
                    cx.notify();
                });
                window.draw(cx).clear(cx);
            })
            .unwrap();
            let terminate = visual.debug_bounds("batch-terminate-button").unwrap();
            visual.simulate_click(terminate.center(), Modifiers::default());
            cx.update_window(handle, |_, window, cx| {
                assert!(window.has_active_dialog(cx));
                window.draw(cx).clear(cx);
            })
            .unwrap();
            cx.executor()
                .advance_clock(std::time::Duration::from_millis(350));
            cx.run_until_parked();
            cx.update_window(handle, |_, window, cx| window.draw(cx).clear(cx))
                .unwrap();
            let confirm = visual.debug_bounds("batch-resolution-confirm").unwrap();
            visual.simulate_click(confirm.center(), Modifiers::default());
            let started = std::time::Instant::now();
            while crate::batch_store::get(batch_id).unwrap().state != "terminated" {
                cx.executor()
                    .advance_clock(std::time::Duration::from_millis(200));
                cx.run_until_parked();
                assert!(started.elapsed() < std::time::Duration::from_secs(30));
            }
            let final_batch = crate::batch_store::get(batch_id).unwrap();
            assert_eq!(final_batch.targets[0].state, "skipped");
            assert_eq!(final_batch.targets[1].state, "waiting");
            assert!(final_batch.targets[1].release_task_id.is_none());
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
        eprintln!("LAN live health failure GUI {stage}: batch={batch_id}, task={task_id}");
    }

    #[gpui_kit::test]
    #[ignore = "one-shot LAN GUI retry; requires an isolated copy of the audited LAN APPDATA"]
    fn isolated_gui_lan_batch_retries_closed_attempt(cx: &mut gpui_kit::TestAppContext) {
        use super::{AppView, DeployTab, Page};
        use gpui_kit::component::WindowExt as _;
        use gpui_kit::{AppContext as _, Modifiers, WindowOptions, px, size};
        use std::sync::{Arc, atomic::AtomicBool};

        let appdata = std::path::PathBuf::from(std::env::var_os("APPDATA").unwrap())
            .canonicalize()
            .unwrap();
        let root = appdata.parent().unwrap();
        assert!(
            root.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("ui-lan-retry-gui-")
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
                &format!("name: stack-v2-retry-gui-{}", uuid::Uuid::new_v4().simple()),
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
        let tag = format!("lanretrygui{}", uuid::Uuid::new_v4().simple());
        let plan = crate::stack_release::preflight_batch(
            project.id,
            &tag,
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
        let mut baselines = Vec::new();
        for (target, (address, daemon)) in plan.targets.iter().zip([
            ("192.168.63.129", "882622c2-a12a-4ac3-99d4-c3ab389f2641"),
            ("192.168.63.130", "ac70e19f-a489-49f3-82f0-0a4ad66ca2e6"),
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
        let first = crate::stack_release::run_batch_sequential(
            project.id,
            &plan,
            Arc::new(AtomicBool::new(false)),
            |_| {},
        )
        .unwrap_or_else(|error| panic!("first host: {}", error.message));
        let (batch_id, first_task) = match first {
            crate::stack_release::BatchProgress::AwaitingConfirmation { batch_id, task_id } => {
                (batch_id, task_id)
            }
            crate::stack_release::BatchProgress::Complete { .. } => {
                panic!("first host skipped confirmation")
            }
        };
        assert_eq!(
            crate::release::confirm_healthy_and_complete(&first_task).unwrap(),
            4
        );
        let confirmed =
            crate::stack_release::reconfirm_batch(&batch_id, Arc::new(AtomicBool::new(false)))
                .unwrap();
        let guard = crate::release_materials::acquire_guard(project.id).unwrap();
        let prepared = crate::stack_release::prepare_batch_host(
            project.id,
            &batch_id,
            &confirmed,
            1,
            guard.into(),
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
        let failed_task = prepared.task_id.clone();
        drop(prepared);
        crate::batch_store::close_pretransfer(&failed_task).unwrap();
        assert_eq!(crate::batch_store::get(&batch_id).unwrap().state, "paused");

        cx.update(gpui_kit::init);
        let (handle, entity) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| AppView::new(window, cx))
            })
            .unwrap()
        });
        cx.update_window(handle, |_, window, cx| {
            entity.update(cx, |view, cx| {
                view.page = Page::Deploy;
                view.deploy_tab = DeployTab::History;
                view.projects = vec![project.clone()];
                view.project_loaded = true;
                view.release_batches = vec![crate::batch_store::get(&batch_id).unwrap()];
                view.release_tasks = crate::release_store::recent(None).unwrap();
                view.release_tasks_loading = false;
                cx.notify();
            });
            window.draw(cx).clear(cx);
        })
        .unwrap();
        let mut visual = gpui_kit::VisualTestContext::from_window(handle, cx);
        visual.simulate_resize(size(px(1700.), px(900.)));
        cx.update_window(handle, |_, window, cx| window.draw(cx).clear(cx))
            .unwrap();
        let retry = visual.debug_bounds("batch-retry-button").unwrap();
        visual.simulate_click(retry.center(), Modifiers::default());
        cx.update_window(handle, |_, window, cx| {
            assert!(window.has_active_dialog(cx));
            window.draw(cx).clear(cx);
        })
        .unwrap();
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(350));
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| window.draw(cx).clear(cx))
            .unwrap();
        let confirm = visual.debug_bounds("batch-retry-confirm").unwrap();
        visual.simulate_click(confirm.center(), Modifiers::default());
        let started = std::time::Instant::now();
        loop {
            cx.executor()
                .advance_clock(std::time::Duration::from_millis(250));
            cx.run_until_parked();
            if !cx.read(|app| entity.read(app).batch_resolving.contains(&batch_id)) {
                break;
            }
            assert!(
                started.elapsed() < std::time::Duration::from_secs(300),
                "GUI retry did not settle; audit batch {batch_id}"
            );
        }
        let retried = crate::batch_store::get(&batch_id).unwrap();
        assert_eq!(retried.state, "running");
        assert_eq!(retried.targets[1].state, "running");
        assert_eq!(retried.targets[1].attempt_task_ids.len(), 2);
        let new_task = retried.targets[1].release_task_id.as_ref().unwrap();
        assert_ne!(new_task, &failed_task);
        let task = crate::release_store::get_task(new_task).unwrap();
        assert_eq!(
            (
                task.phase.as_str(),
                task.outcome.as_str(),
                task.health_result.as_deref()
            ),
            ("checked", "blocked", Some("healthy"))
        );
        assert_eq!(
            crate::release::confirm_healthy_and_complete(new_task).unwrap(),
            4
        );
        assert_eq!(
            crate::batch_store::get(&batch_id).unwrap().state,
            "complete"
        );
        for (target, before) in retried.targets.iter().zip(&baselines) {
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
            "LAN GUI retry complete: project={}, batch={}, old={}, new={}",
            project.id, batch_id, failed_task, new_task
        );
    }

    #[gpui_kit::test]
    #[ignore = "one-shot LAN history GUI confirmation and continuation; requires a new isolated project ID"]
    fn isolated_gui_lan_batch_confirms_and_continues(cx: &mut gpui_kit::TestAppContext) {
        use super::{AppView, DeployTab, Page};
        use gpui_kit::component::WindowExt as _;
        use gpui_kit::{AppContext as _, Modifiers, WindowOptions, px, size};
        use std::sync::{Arc, atomic::AtomicBool};

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
        let project_id: i64 = std::env::var("DEVINGPANEL_TEST_LAN_GUI_PROJECT_ID")
            .expect("new isolated LAN project ID required")
            .parse()
            .unwrap();
        assert!(project_id > 1_000_000_000);
        let project = crate::project_store::effective(project_id).unwrap();
        assert!(project.name.starts_with("stack-v2-gui-"));
        assert!(
            std::path::Path::new(&project.path)
                .canonicalize()
                .unwrap()
                .starts_with(root)
        );
        assert_eq!(project.host_ids, [1, 2]);
        assert!(
            crate::batch_store::recent(Some(project.id))
                .unwrap()
                .is_empty()
        );
        let mut baseline = Vec::new();
        for (host_id, address, daemon) in [
            (1, "192.168.63.129", "882622c2-a12a-4ac3-99d4-c3ab389f2641"),
            (2, "192.168.63.130", "ac70e19f-a489-49f3-82f0-0a4ad66ca2e6"),
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
        let tag = format!("langui{}", uuid::Uuid::new_v4().simple());
        let snapshot = crate::stack_release::preflight_batch(
            project.id,
            &tag,
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
        for target in &snapshot.targets {
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
        let first = crate::stack_release::run_batch_sequential(
            project.id,
            &snapshot,
            Arc::new(AtomicBool::new(false)),
            |_| {},
        )
        .unwrap_or_else(|error| panic!("batch {:?}: {}", error.batch_id, error.message));
        let (batch_id, first_id) = match first {
            crate::stack_release::BatchProgress::AwaitingConfirmation { batch_id, task_id } => {
                (batch_id, task_id)
            }
            _ => panic!("first host did not await confirmation"),
        };
        let batch = crate::batch_store::get(&batch_id).unwrap();
        assert_eq!(batch.targets[1].state, "waiting");
        assert!(batch.targets[1].release_task_id.is_none());
        assert_eq!(
            crate::release_store::get_task(&first_id).unwrap().outcome,
            "blocked"
        );
        let mut first_containers = crate::ssh::inventory(1)
            .unwrap()
            .containers
            .iter()
            .map(|container| container.id.clone())
            .collect::<Vec<_>>();
        first_containers.sort();
        assert_eq!(first_containers.len(), baseline[0].len() + 4);
        assert!(baseline[0].iter().all(|id| first_containers.contains(id)));
        let mut second_containers = crate::ssh::inventory(2)
            .unwrap()
            .containers
            .iter()
            .map(|container| container.id.clone())
            .collect::<Vec<_>>();
        second_containers.sort();
        assert_eq!(second_containers, baseline[1]);

        cx.update(gpui_kit::init);
        let (handle, entity) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| AppView::new(window, cx))
            })
            .unwrap()
        });
        cx.update_window(handle, |_, window, cx| {
            entity.update(cx, |view, cx| {
                view.page = Page::Deploy;
                view.deploy_tab = DeployTab::History;
                view.projects = vec![project.clone()];
                view.project_loaded = true;
                view.release_tasks = vec![crate::release_store::get_task(&first_id).unwrap()];
                view.release_batches = vec![crate::batch_store::get(&batch_id).unwrap()];
                view.release_tasks_loading = false;
                view.release_history_selected = Some(first_id.clone());
                cx.notify();
            });
            window.draw(cx).clear(cx);
        })
        .unwrap();
        let mut visual = gpui_kit::VisualTestContext::from_window(handle, cx);
        visual.simulate_resize(size(px(1700.), px(1200.)));
        cx.update_window(handle, |_, window, cx| window.draw(cx).clear(cx))
            .unwrap();
        let confirm = visual
            .debug_bounds("release-confirm-healthy-button")
            .unwrap();
        visual.simulate_click(confirm.center(), Modifiers::default());
        cx.update_window(handle, |_, window, cx| {
            assert!(window.has_active_dialog(cx));
            window.draw(cx).clear(cx);
        })
        .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(350));
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| window.draw(cx).clear(cx))
            .unwrap();
        let confirm = visual
            .debug_bounds("release-complete-confirm-button")
            .unwrap();
        visual.simulate_click(confirm.center(), Modifiers::default());
        cx.update_window(handle, |_, window, cx| {
            assert!(!window.has_active_dialog(cx));
        })
        .unwrap();
        let started = std::time::Instant::now();
        while crate::release_store::get_task(&first_id).unwrap().outcome != "complete" {
            cx.executor()
                .advance_clock(std::time::Duration::from_millis(200));
            cx.run_until_parked();
            assert!(
                started.elapsed() < std::time::Duration::from_secs(120),
                "first GUI confirmation did not complete; audit batch {batch_id}"
            );
        }
        assert_eq!(
            crate::batch_store::get(&batch_id).unwrap().targets[0].state,
            "healthy"
        );
        cx.update_window(handle, |_, window, cx| {
            entity.update(cx, |view, cx| {
                view.release_tasks = vec![crate::release_store::get_task(&first_id).unwrap()];
                view.release_batches = vec![crate::batch_store::get(&batch_id).unwrap()];
                view.release_history_selected = None;
                cx.notify();
            });
            window.draw(cx).clear(cx);
        })
        .unwrap();
        let next = visual.debug_bounds("batch-continue-button").unwrap();
        visual.simulate_click(next.center(), Modifiers::default());
        cx.update_window(handle, |_, window, cx| {
            assert!(window.has_active_dialog(cx));
            window.draw(cx).clear(cx);
        })
        .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(350));
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| window.draw(cx).clear(cx))
            .unwrap();
        let confirm = visual.debug_bounds("batch-continue-confirm").unwrap();
        visual.simulate_click(confirm.center(), Modifiers::default());
        cx.update_window(handle, |_, window, cx| {
            assert!(!window.has_active_dialog(cx));
        })
        .unwrap();
        let started = std::time::Instant::now();
        let second_id = loop {
            cx.executor()
                .advance_clock(std::time::Duration::from_millis(200));
            cx.run_until_parked();
            let batch = crate::batch_store::get(&batch_id).unwrap();
            assert!(
                matches!(batch.state.as_str(), "running" | "complete"),
                "GUI continuation requires audit, batch {batch_id}: {}",
                batch.state
            );
            if let Some(id) = &batch.targets[1].release_task_id {
                let task = crate::release_store::get_task(id).unwrap();
                if task.phase == "checked" {
                    break id.clone();
                }
            }
            assert!(
                started.elapsed() < std::time::Duration::from_secs(1800),
                "second GUI continuation did not settle; audit batch {batch_id}: {}",
                batch.state
            );
        };
        let second = crate::release_store::get_task(&second_id).unwrap();
        assert_eq!(
            (second.outcome.as_str(), second.health_result.as_deref()),
            ("blocked", Some("healthy"))
        );
        assert_eq!(
            crate::batch_store::get(&batch_id).unwrap().targets[1].state,
            "running"
        );
        cx.update_window(handle, |_, window, cx| {
            entity.update(cx, |view, cx| {
                view.release_tasks = vec![second];
                view.release_batches = vec![crate::batch_store::get(&batch_id).unwrap()];
                view.release_history_selected = Some(second_id.clone());
                cx.notify();
            });
            window.draw(cx).clear(cx);
        })
        .unwrap();
        let confirm = visual
            .debug_bounds("release-confirm-healthy-button")
            .unwrap();
        visual.simulate_click(confirm.center(), Modifiers::default());
        cx.update_window(handle, |_, window, cx| {
            assert!(window.has_active_dialog(cx));
            window.draw(cx).clear(cx);
        })
        .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(350));
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| window.draw(cx).clear(cx))
            .unwrap();
        let confirm = visual
            .debug_bounds("release-complete-confirm-button")
            .unwrap();
        visual.simulate_click(confirm.center(), Modifiers::default());
        cx.update_window(handle, |_, window, cx| {
            assert!(!window.has_active_dialog(cx));
        })
        .unwrap();
        let started = std::time::Instant::now();
        while crate::batch_store::get(&batch_id).unwrap().state != "complete" {
            cx.executor()
                .advance_clock(std::time::Duration::from_millis(200));
            cx.run_until_parked();
            assert!(
                started.elapsed() < std::time::Duration::from_secs(120),
                "second GUI confirmation did not complete; audit batch {batch_id}"
            );
        }
        let batch = crate::batch_store::get(&batch_id).unwrap();
        assert!(batch.targets.iter().all(|target| target.state == "healthy"));
        for (host_id, before) in [(1, &baseline[0]), (2, &baseline[1])] {
            let mut after = crate::ssh::inventory(host_id)
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
            "LAN GUI confirmed and continued: project={}, batch={}, tasks={},{}",
            project.id, batch_id, first_id, second_id
        );
    }

    #[gpui_kit::test]
    #[ignore = "one-shot GUI confirmation of an audited isolated LAN batch; do not redeploy"]
    fn isolated_gui_lan_existing_first_confirmation(cx: &mut gpui_kit::TestAppContext) {
        use super::{AppView, DeployTab, Page};
        use gpui_kit::component::WindowExt as _;
        use gpui_kit::{AppContext as _, Modifiers, WindowOptions, px, size};

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
                .starts_with("ui-lan-recovery-")
        );
        let batch_id = std::env::var("DEVINGPANEL_TEST_LAN_BATCH_ID").unwrap();
        let batch = crate::batch_store::get(&batch_id).unwrap();
        assert_eq!(batch.project_id, 1856624454);
        let project = crate::project_store::effective(batch.project_id).unwrap();
        assert!(
            std::path::Path::new(&project.path)
                .canonicalize()
                .unwrap()
                .starts_with(appdata.parent().unwrap())
        );
        let first_id = batch.targets[0].release_task_id.as_ref().unwrap().clone();
        assert_eq!(batch.targets[0].state, "running");
        assert_eq!(batch.targets[1].state, "waiting");
        assert!(batch.targets[1].release_task_id.is_none());
        let conn = crate::database::connect().unwrap();
        let pointers: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM project_current_version WHERE project_id = ?1",
                [batch.project_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(pointers, 0);
        let task = crate::release_store::get_task(&first_id).unwrap();
        assert_eq!(
            (
                task.phase.as_str(),
                task.outcome.as_str(),
                task.health_result.as_deref()
            ),
            ("checked", "blocked", Some("healthy"))
        );
        assert_eq!(task.host.address, "192.168.63.129");
        assert_eq!(
            crate::ssh::docker_identity_for_host(task.host.clone())
                .unwrap()
                .1,
            "882622c2-a12a-4ac3-99d4-c3ab389f2641"
        );
        let observed = crate::ssh::observe_release_project(&task).unwrap();
        assert_eq!(observed.containers.len(), 4);
        assert!(observed.containers.iter().all(|container| {
            container.status == "running" && container.health.as_deref() == Some("healthy")
        }));

        cx.update(gpui_kit::init);
        let (handle, entity) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| AppView::new(window, cx))
            })
            .unwrap()
        });
        cx.update_window(handle, |_, window, cx| {
            entity.update(cx, |view, cx| {
                view.page = Page::Deploy;
                view.deploy_tab = DeployTab::History;
                view.release_tasks = vec![task.clone()];
                view.release_batches = vec![crate::batch_store::get(&batch_id).unwrap()];
                view.release_tasks_loading = false;
                view.release_history_selected = Some(first_id.clone());
                cx.notify();
            });
            window.draw(cx).clear(cx);
        })
        .unwrap();
        let mut visual = gpui_kit::VisualTestContext::from_window(handle, cx);
        visual.simulate_resize(size(px(1700.), px(1200.)));
        cx.update_window(handle, |_, window, cx| window.draw(cx).clear(cx))
            .unwrap();
        let button = visual
            .debug_bounds("release-confirm-healthy-button")
            .unwrap();
        eprintln!("open confirmation bounds: {button:?}");
        visual.simulate_click(button.center(), Modifiers::default());
        cx.update_window(handle, |_, window, cx| {
            assert!(window.has_active_dialog(cx));
            window.draw(cx).clear(cx);
        })
        .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(350));
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| window.draw(cx).clear(cx))
            .unwrap();
        let confirm = visual
            .debug_bounds("release-complete-confirm-button")
            .unwrap();
        eprintln!("dialog confirmation bounds: {confirm:?}");
        visual.simulate_click(confirm.center(), Modifiers::default());
        cx.update_window(handle, |_, window, cx| {
            eprintln!(
                "after click: dialog={}, resolving={}",
                window.has_active_dialog(cx),
                entity.read(cx).release_resolving.contains(&first_id)
            );
            window.draw(cx).clear(cx);
        })
        .unwrap();
        let started = std::time::Instant::now();
        loop {
            cx.executor()
                .advance_clock(std::time::Duration::from_millis(200));
            cx.run_until_parked();
            let current = crate::release_store::get_task(&first_id).unwrap();
            if current.outcome == "complete" {
                break;
            }
            assert!(
                started.elapsed() < std::time::Duration::from_secs(45),
                "GUI confirmation did not complete: {} / {}, dialog or backend requires audit",
                current.phase,
                current.outcome
            );
        }
        assert_eq!(
            crate::batch_store::get(&batch_id).unwrap().targets[0].state,
            "healthy"
        );
        eprintln!("LAN first GUI confirmation complete: batch={batch_id}, task={first_id}");
    }

    #[gpui_kit::test]
    #[ignore = "one-shot GUI continuation of the next host in an audited LAN batch"]
    fn isolated_gui_lan_existing_second_continuation(cx: &mut gpui_kit::TestAppContext) {
        use super::{AppView, DeployTab, Page};
        use gpui_kit::component::WindowExt as _;
        use gpui_kit::{AppContext as _, Modifiers, WindowOptions, px, size};

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
                .starts_with("ui-lan-recovery-")
        );
        let batch_id = std::env::var("DEVINGPANEL_TEST_LAN_BATCH_ID").unwrap();
        let batch = crate::batch_store::get(&batch_id).unwrap();
        assert_eq!(batch.project_id, 1856624454);
        let project = crate::project_store::effective(batch.project_id).unwrap();
        assert!(
            std::path::Path::new(&project.path)
                .canonicalize()
                .unwrap()
                .starts_with(appdata.parent().unwrap())
        );
        assert_eq!(batch.state, "running");
        assert_eq!(batch.targets[0].state, "healthy");
        assert_eq!(batch.targets[1].state, "waiting");
        assert!(batch.targets[1].release_task_id.is_none());
        let first_id = batch.targets[0].release_task_id.as_ref().unwrap();
        let first = crate::release_store::get_task(first_id).unwrap();
        assert_eq!(
            (
                first.phase.as_str(),
                first.outcome.as_str(),
                first.health_result.as_deref()
            ),
            ("checked", "complete", Some("healthy"))
        );
        let mut baseline = Vec::new();
        for (target, address, daemon, count) in [
            (
                &batch.targets[0],
                "192.168.63.129",
                "882622c2-a12a-4ac3-99d4-c3ab389f2641",
                36,
            ),
            (
                &batch.targets[1],
                "192.168.63.130",
                "ac70e19f-a489-49f3-82f0-0a4ad66ca2e6",
                24,
            ),
        ] {
            assert_eq!(target.host.address, address);
            assert_eq!(target.docker_daemon_id, daemon);
            assert_eq!(
                crate::ssh::docker_identity_for_host(target.host.clone())
                    .unwrap()
                    .1,
                daemon
            );
            let mut ids = crate::ssh::inventory(target.host.id)
                .unwrap()
                .containers
                .iter()
                .map(|container| container.id.clone())
                .collect::<Vec<_>>();
            ids.sort();
            assert_eq!(ids.len(), count);
            baseline.push(ids);
        }
        let observed = crate::ssh::observe_release_project(&first).unwrap();
        assert_eq!(observed.containers.len(), 4);
        assert!(observed.containers.iter().all(|container| {
            container.status == "running" && container.health.as_deref() == Some("healthy")
        }));
        let second_probe = crate::release_store::TaskSnapshot {
            id: "namespace-probe".into(),
            project_id: batch.project_id,
            project_name: first.project_name.clone(),
            host: batch.targets[1].host.clone(),
            docker_endpoint: Some(batch.targets[1].docker_endpoint.clone()),
            docker_daemon_id: Some(batch.targets[1].docker_daemon_id.clone()),
            phase: "prepared".into(),
            outcome: "running".into(),
            health_result: None,
            detail: String::new(),
            started_at: String::new(),
            execution_finished_at: None,
            image_reference: first.image_reference.clone(),
        };
        crate::ssh::assert_empty_project_namespace(&second_probe).unwrap();

        cx.update(gpui_kit::init);
        let (handle, entity) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| AppView::new(window, cx))
            })
            .unwrap()
        });
        cx.update_window(handle, |_, window, cx| {
            entity.update(cx, |view, cx| {
                view.page = Page::Deploy;
                view.deploy_tab = DeployTab::History;
                view.release_tasks = vec![first];
                view.release_batches = vec![crate::batch_store::get(&batch_id).unwrap()];
                view.release_tasks_loading = false;
                view.release_history_selected = None;
                cx.notify();
            });
            window.draw(cx).clear(cx);
        })
        .unwrap();
        let mut visual = gpui_kit::VisualTestContext::from_window(handle, cx);
        visual.simulate_resize(size(px(1700.), px(1200.)));
        cx.update_window(handle, |_, window, cx| window.draw(cx).clear(cx))
            .unwrap();
        let button = visual.debug_bounds("batch-continue-button").unwrap();
        eprintln!("continue button bounds: {button:?}");
        visual.simulate_click(button.center(), Modifiers::default());
        cx.update_window(handle, |_, window, cx| {
            assert!(window.has_active_dialog(cx));
            window.draw(cx).clear(cx);
        })
        .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(350));
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| window.draw(cx).clear(cx))
            .unwrap();
        let confirm = visual.debug_bounds("batch-continue-confirm").unwrap();
        let cancel = visual.debug_bounds("batch-continue-cancel").unwrap();
        eprintln!("continue dialog bounds: confirm={confirm:?}, cancel={cancel:?}");
        assert_eq!(confirm.origin.y, cancel.origin.y);
        visual.simulate_click(confirm.center(), Modifiers::default());
        cx.update_window(handle, |_, window, cx| {
            assert!(!window.has_active_dialog(cx));
            window.draw(cx).clear(cx);
        })
        .unwrap();
        let started = std::time::Instant::now();
        let second_id = loop {
            cx.executor()
                .advance_clock(std::time::Duration::from_millis(200));
            cx.run_until_parked();
            let batch = crate::batch_store::get(&batch_id).unwrap();
            assert!(
                matches!(batch.state.as_str(), "running" | "complete"),
                "GUI continuation requires audit, batch {batch_id}: {}",
                batch.state
            );
            if let Some(id) = &batch.targets[1].release_task_id {
                let task = crate::release_store::get_task(id).unwrap();
                if task.phase == "checked" {
                    break id.clone();
                }
            }
            assert!(
                started.elapsed() < std::time::Duration::from_secs(600),
                "GUI continuation did not settle; audit batch {batch_id}: {}",
                batch.state
            );
        };
        let second = crate::release_store::get_task(&second_id).unwrap();
        assert_eq!(
            (second.outcome.as_str(), second.health_result.as_deref()),
            ("blocked", Some("healthy"))
        );
        assert_eq!(
            crate::batch_store::get(&batch_id).unwrap().targets[1].state,
            "running"
        );
        for (host_id, before, added) in [(1, &baseline[0], 0), (2, &baseline[1], 4)] {
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
        eprintln!("LAN GUI continued second host: batch={batch_id}, task={second_id}");
    }

    #[gpui_kit::test]
    #[ignore = "one-shot GUI next-wave click and real third-host deployment in an isolated LAN batch"]
    fn isolated_gui_lan_three_host_next_wave(cx: &mut gpui_kit::TestAppContext) {
        use super::{AppView, DeployTab, Page};
        use gpui_kit::component::WindowExt as _;
        use gpui_kit::{AppContext as _, Modifiers, WindowOptions, px, size};

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
        let project = crate::project_store::effective(batch.project_id).unwrap();
        assert!(
            std::path::Path::new(&project.path)
                .canonicalize()
                .unwrap()
                .starts_with(root)
        );
        let mut baseline = Vec::new();
        let mut tasks = Vec::new();
        for (index, target) in batch.targets.iter().enumerate() {
            let mut ids = crate::ssh::inventory(target.host.id)
                .unwrap()
                .containers
                .into_iter()
                .map(|container| container.id)
                .collect::<Vec<_>>();
            ids.sort();
            baseline.push(ids);
            if index < 2 {
                assert_eq!(target.state, "healthy");
                let task = crate::release_store::get_task(target.release_task_id.as_ref().unwrap())
                    .unwrap();
                assert_eq!(
                    (
                        task.phase.as_str(),
                        task.outcome.as_str(),
                        task.health_result.as_deref()
                    ),
                    ("checked", "complete", Some("healthy"))
                );
                tasks.push(task);
            }
        }
        let conn = crate::database::connect().unwrap();
        let pointers: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM project_current_version WHERE project_id = ?1",
                [batch.project_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(pointers, 2);
        cx.update(gpui_kit::init);
        let (handle, entity) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| AppView::new(window, cx))
            })
            .unwrap()
        });
        cx.update_window(handle, |_, window, cx| {
            entity.update(cx, |view, cx| {
                view.page = Page::Deploy;
                view.deploy_tab = DeployTab::History;
                view.release_tasks = tasks;
                view.release_batches = vec![crate::batch_store::get(&batch_id).unwrap()];
                view.release_tasks_loading = false;
                view.release_history_selected = None;
                cx.notify();
            });
            window.draw(cx).clear(cx);
        })
        .unwrap();
        let mut visual = gpui_kit::VisualTestContext::from_window(handle, cx);
        visual.simulate_resize(size(px(1700.), px(1200.)));
        cx.update_window(handle, |_, window, cx| window.draw(cx).clear(cx))
            .unwrap();
        let button = visual.debug_bounds("batch-continue-button").unwrap();
        visual.simulate_click(button.center(), Modifiers::default());
        cx.update_window(handle, |_, window, cx| {
            assert!(window.has_active_dialog(cx));
            window.draw(cx).clear(cx);
        })
        .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(350));
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| window.draw(cx).clear(cx))
            .unwrap();
        let confirm = visual.debug_bounds("batch-continue-confirm").unwrap();
        visual.simulate_click(confirm.center(), Modifiers::default());
        cx.update_window(handle, |_, window, cx| {
            assert!(!window.has_active_dialog(cx));
            window.draw(cx).clear(cx);
        })
        .unwrap();
        let started = std::time::Instant::now();
        let third_id = loop {
            cx.executor()
                .advance_clock(std::time::Duration::from_millis(200));
            cx.run_until_parked();
            let current = crate::batch_store::get(&batch_id).unwrap();
            assert_eq!(
                current.state, "running",
                "GUI continuation requires audit: {}",
                current.state
            );
            if let Some(id) = &current.targets[2].release_task_id {
                let task = crate::release_store::get_task(id).unwrap();
                if task.phase == "checked" {
                    break id.clone();
                }
            }
            assert!(
                started.elapsed() < std::time::Duration::from_secs(600),
                "GUI next wave did not settle; audit batch {batch_id}"
            );
        };
        let third = crate::release_store::get_task(&third_id).unwrap();
        assert_eq!(
            (third.outcome.as_str(), third.health_result.as_deref()),
            ("blocked", Some("healthy"))
        );
        let refresh_started = std::time::Instant::now();
        loop {
            cx.executor()
                .advance_clock(std::time::Duration::from_millis(200));
            cx.run_until_parked();
            if cx.read(|app| {
                let view = entity.read(app);
                !view.batch_resolving.contains(&batch_id)
                    && view.release_batches.iter().any(|item| {
                        item.id == batch_id
                            && item.targets[2].release_task_id.as_deref() == Some(third_id.as_str())
                    })
            }) {
                break;
            }
            assert!(
                refresh_started.elapsed() < std::time::Duration::from_secs(30),
                "GUI next-wave result was not refreshed"
            );
        }
        for (index, target) in batch.targets.iter().enumerate() {
            let mut after = crate::ssh::inventory(target.host.id)
                .unwrap()
                .containers
                .into_iter()
                .map(|container| container.id)
                .collect::<Vec<_>>();
            after.sort();
            assert_eq!(
                after.len(),
                baseline[index].len() + if index == 2 { 4 } else { 0 }
            );
            assert!(baseline[index].iter().all(|id| after.contains(id)));
        }
        assert_eq!(
            crate::release::confirm_healthy_and_complete(&third_id).unwrap(),
            4
        );
        assert_eq!(
            crate::batch_store::get(&batch_id).unwrap().state,
            "complete"
        );
        eprintln!("LAN GUI next wave complete: batch={batch_id}, task={third_id}");
    }

    #[gpui_kit::test]
    #[ignore = "one-shot GUI health confirmation of the second host in an audited LAN batch"]
    fn isolated_gui_lan_existing_second_confirmation(cx: &mut gpui_kit::TestAppContext) {
        use super::{AppView, DeployTab, Page};
        use gpui_kit::component::WindowExt as _;
        use gpui_kit::{AppContext as _, Modifiers, WindowOptions, px, size};

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
                .starts_with("ui-lan-batch-")
        );
        let batch_id = "33917694-1a25-41f2-9f0e-fa3ec3631090";
        let batch = crate::batch_store::get(batch_id).unwrap();
        assert_eq!(batch.project_id, 1856624453);
        assert_eq!(batch.state, "running");
        assert_eq!(batch.targets[0].state, "healthy");
        assert_eq!(batch.targets[1].state, "running");
        let second_id = batch.targets[1].release_task_id.as_ref().unwrap();
        let task = crate::release_store::get_task(second_id).unwrap();
        assert_eq!(
            (
                task.phase.as_str(),
                task.outcome.as_str(),
                task.health_result.as_deref()
            ),
            ("checked", "blocked", Some("healthy"))
        );
        assert_eq!(task.host.address, "192.168.63.130");
        assert_eq!(
            crate::ssh::docker_identity_for_host(task.host.clone())
                .unwrap()
                .1,
            "ac70e19f-a489-49f3-82f0-0a4ad66ca2e6"
        );
        let observed = crate::ssh::observe_release_project(&task).unwrap();
        assert_eq!(observed.containers.len(), 4);
        assert!(observed.containers.iter().all(|container| {
            container.status == "running" && container.health.as_deref() == Some("healthy")
        }));
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
        assert_eq!(
            pointers()[0].1,
            *batch.targets[0].release_task_id.as_ref().unwrap()
        );

        cx.update(gpui_kit::init);
        let (handle, entity) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| AppView::new(window, cx))
            })
            .unwrap()
        });
        cx.update_window(handle, |_, window, cx| {
            entity.update(cx, |view, cx| {
                view.page = Page::Deploy;
                view.deploy_tab = DeployTab::History;
                view.release_tasks = vec![task.clone()];
                view.release_batches = vec![crate::batch_store::get(batch_id).unwrap()];
                view.release_tasks_loading = false;
                view.release_history_selected = Some(second_id.clone());
                cx.notify();
            });
            window.draw(cx).clear(cx);
        })
        .unwrap();
        let mut visual = gpui_kit::VisualTestContext::from_window(handle, cx);
        visual.simulate_resize(size(px(1700.), px(1200.)));
        cx.update_window(handle, |_, window, cx| window.draw(cx).clear(cx))
            .unwrap();
        let button = visual
            .debug_bounds("release-confirm-healthy-button")
            .unwrap();
        visual.simulate_click(button.center(), Modifiers::default());
        cx.update_window(handle, |_, window, cx| {
            assert!(window.has_active_dialog(cx));
            window.draw(cx).clear(cx);
        })
        .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(350));
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| window.draw(cx).clear(cx))
            .unwrap();
        let confirm = visual
            .debug_bounds("release-complete-confirm-button")
            .unwrap();
        visual.simulate_click(confirm.center(), Modifiers::default());
        cx.update_window(handle, |_, window, cx| {
            assert!(!window.has_active_dialog(cx));
            window.draw(cx).clear(cx);
        })
        .unwrap();
        let started = std::time::Instant::now();
        while crate::batch_store::get(batch_id).unwrap().state != "complete" {
            cx.executor()
                .advance_clock(std::time::Duration::from_millis(200));
            cx.run_until_parked();
            assert!(
                started.elapsed() < std::time::Duration::from_secs(120),
                "second GUI confirmation did not complete; audit batch {batch_id}"
            );
        }
        let batch = crate::batch_store::get(batch_id).unwrap();
        assert!(batch.targets.iter().all(|target| target.state == "healthy"));
        assert_eq!(
            pointers(),
            vec![
                (
                    "882622c2-a12a-4ac3-99d4-c3ab389f2641".into(),
                    batch.targets[0].release_task_id.clone().unwrap()
                ),
                (
                    "ac70e19f-a489-49f3-82f0-0a4ad66ca2e6".into(),
                    second_id.clone()
                ),
            ]
        );
        eprintln!("LAN GUI second confirmation complete: batch={batch_id}, task={second_id}");
    }

    #[test]
    #[ignore = "read-only audit of a completed LAN GUI batch; fixture health may expire"]
    fn isolated_lan_completed_gui_batch_remote_audit() {
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
                .starts_with("ui-lan-batch-")
        );
        let batch = crate::batch_store::get("33917694-1a25-41f2-9f0e-fa3ec3631090").unwrap();
        assert_eq!(batch.project_id, 1856624453);
        assert_eq!(batch.state, "complete");
        for (target, address, daemon, count) in [
            (
                &batch.targets[0],
                "192.168.63.129",
                "882622c2-a12a-4ac3-99d4-c3ab389f2641",
                32,
            ),
            (
                &batch.targets[1],
                "192.168.63.130",
                "ac70e19f-a489-49f3-82f0-0a4ad66ca2e6",
                24,
            ),
        ] {
            assert_eq!(target.host.address, address);
            assert_eq!(target.docker_daemon_id, daemon);
            assert_eq!(
                crate::ssh::docker_identity_for_host(target.host.clone())
                    .unwrap()
                    .1,
                daemon
            );
            assert_eq!(
                crate::ssh::inventory(target.host.id)
                    .unwrap()
                    .containers
                    .len(),
                count
            );
            let task =
                crate::release_store::get_task(target.release_task_id.as_ref().unwrap()).unwrap();
            let observed = crate::ssh::observe_release_project(&task).unwrap();
            assert_eq!(observed.containers.len(), 4);
            assert!(observed.containers.iter().all(|container| {
                container.status == "running" && container.health.as_deref() == Some("healthy")
            }));
        }
    }

    #[gpui_kit::test]
    #[ignore = "one-shot LAN GUI refusal of missing Compose completion marker; no remote writes"]
    fn isolated_gui_lan_missing_marker_stays_unknown(cx: &mut gpui_kit::TestAppContext) {
        use super::{AppView, DeployTab, Page};
        use gpui_kit::component::WindowExt as _;
        use gpui_kit::{AppContext as _, Modifiers, WindowOptions, px, size};

        let appdata = std::path::PathBuf::from(std::env::var_os("APPDATA").unwrap())
            .canonicalize()
            .unwrap();
        assert_eq!(
            appdata.parent().unwrap(),
            std::env::current_dir()
                .unwrap()
                .join("target/isolated-tests/ui-lan-batch-9e60e3b6e00c4cfe86d5b437f28f3c19")
                .canonicalize()
                .unwrap()
        );
        let batch_id = "95c6bb3e-137b-4f54-b37e-b9ec27784e01";
        let task_id = "86a7b61d-602f-4120-b99a-51cd0ba32abf";
        let batch = crate::batch_store::get(batch_id).unwrap();
        assert_eq!(batch.project_id, 1856624451);
        assert_eq!(batch.state, "unknown");
        assert_eq!(batch.targets[0].state, "unknown");
        assert_eq!(batch.targets[0].release_task_id.as_deref(), Some(task_id));
        assert_eq!(batch.targets[1].state, "waiting");
        assert!(batch.targets[1].release_task_id.is_none());
        let task = crate::release_store::get_task(task_id).unwrap();
        assert_eq!(
            (task.phase.as_str(), task.outcome.as_str()),
            ("deploying", "unknown")
        );
        assert!(task.health_result.is_none());
        assert!(crate::ssh::verify_start_completed(&task).is_err());
        let conn = crate::database::connect().unwrap();
        let counts =
            || {
                let audits: i64 = conn.query_row(
                "SELECT COUNT(*) FROM release_task_health_reconciliations WHERE task_id = ?1",
                [task_id],
                |row| row.get(0),
            ).unwrap();
                let pointers: i64 = conn
                    .query_row(
                        "SELECT COUNT(*) FROM project_current_version WHERE project_id = ?1",
                        [batch.project_id],
                        |row| row.get(0),
                    )
                    .unwrap();
                (audits, pointers)
            };
        assert_eq!(counts(), (0, 0));
        let mut baseline = Vec::new();
        for (target, address, daemon, count) in [
            (
                &batch.targets[0],
                "192.168.63.129",
                "882622c2-a12a-4ac3-99d4-c3ab389f2641",
                32,
            ),
            (
                &batch.targets[1],
                "192.168.63.130",
                "ac70e19f-a489-49f3-82f0-0a4ad66ca2e6",
                24,
            ),
        ] {
            assert_eq!(target.host.address, address);
            assert_eq!(target.docker_daemon_id, daemon);
            assert_eq!(
                crate::ssh::docker_identity_for_host(target.host.clone())
                    .unwrap()
                    .1,
                daemon
            );
            let mut ids = crate::ssh::inventory(target.host.id)
                .unwrap()
                .containers
                .iter()
                .map(|container| container.id.clone())
                .collect::<Vec<_>>();
            ids.sort();
            assert_eq!(ids.len(), count);
            baseline.push(ids);
        }
        assert_eq!(
            crate::ssh::observe_release_project(&task)
                .unwrap()
                .containers
                .len(),
            4
        );

        cx.update(gpui_kit::init);
        let (handle, entity) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| AppView::new(window, cx))
            })
            .unwrap()
        });
        cx.update_window(handle, |_, window, cx| {
            entity.update(cx, |view, cx| {
                view.page = Page::Deploy;
                view.deploy_tab = DeployTab::History;
                view.release_tasks = vec![task.clone()];
                view.release_batches = vec![crate::batch_store::get(batch_id).unwrap()];
                view.release_tasks_loading = false;
                view.release_history_selected = Some(task_id.into());
                cx.notify();
            });
            window.draw(cx).clear(cx);
        })
        .unwrap();
        let mut visual = gpui_kit::VisualTestContext::from_window(handle, cx);
        visual.simulate_resize(size(px(1700.), px(1200.)));
        cx.update_window(handle, |_, window, cx| window.draw(cx).clear(cx))
            .unwrap();
        let notifications_before = cx
            .update_window(handle, |_, window, cx| window.notifications(cx).len())
            .unwrap();
        let button = visual
            .debug_bounds("release-reconcile-health-button")
            .unwrap();
        visual.simulate_click(button.center(), Modifiers::default());
        cx.update_window(handle, |_, window, cx| {
            assert!(window.has_active_dialog(cx));
            window.draw(cx).clear(cx);
        })
        .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(350));
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| window.draw(cx).clear(cx))
            .unwrap();
        let confirm = visual
            .debug_bounds("release-reconcile-confirm-button")
            .unwrap();
        let cancel = visual.debug_bounds("release-reconcile-cancel").unwrap();
        assert_eq!(confirm.origin.y, cancel.origin.y);
        visual.simulate_click(confirm.center(), Modifiers::default());
        cx.update_window(handle, |_, window, cx| {
            assert!(!window.has_active_dialog(cx));
        })
        .unwrap();
        let started = std::time::Instant::now();
        loop {
            cx.executor()
                .advance_clock(std::time::Duration::from_millis(200));
            cx.run_until_parked();
            if !cx.read(|app| entity.read(app).release_resolving.contains(task_id)) {
                break;
            }
            assert!(
                started.elapsed() < std::time::Duration::from_secs(60),
                "GUI read-only reconciliation did not settle"
            );
        }
        assert!(
            cx.update_window(handle, |_, window, cx| window.notifications(cx).len())
                .unwrap()
                > notifications_before,
            "GUI reconciliation did not report its refusal"
        );
        let task_after = crate::release_store::get_task(task_id).unwrap();
        assert_eq!(
            (task_after.phase.as_str(), task_after.outcome.as_str()),
            ("deploying", "unknown")
        );
        assert!(task_after.health_result.is_none());
        let batch_after = crate::batch_store::get(batch_id).unwrap();
        assert_eq!(batch_after.state, "unknown");
        assert_eq!(batch_after.targets[0].state, "unknown");
        assert_eq!(batch_after.targets[1].state, "waiting");
        assert!(batch_after.targets[1].release_task_id.is_none());
        assert_eq!(counts(), (0, 0));
        for (target, before) in batch.targets.iter().zip(&baseline) {
            let mut after = crate::ssh::inventory(target.host.id)
                .unwrap()
                .containers
                .iter()
                .map(|container| container.id.clone())
                .collect::<Vec<_>>();
            after.sort();
            assert_eq!(&after, before);
        }
        eprintln!("LAN missing-marker GUI recovery refused: batch={batch_id}, task={task_id}");
    }

    #[gpui_kit::test]
    #[ignore = "one-shot GUI resume of an audited partial LAN upload"]
    fn isolated_gui_lan_partial_upload_resumes_first_host(cx: &mut gpui_kit::TestAppContext) {
        use super::{AppView, DeployTab, Page};
        use gpui_kit::component::WindowExt as _;
        use gpui_kit::{AppContext as _, Modifiers, WindowOptions, px, size};

        let appdata = std::path::PathBuf::from(std::env::var_os("APPDATA").unwrap())
            .canonicalize()
            .unwrap();
        let root = appdata.parent().unwrap();
        assert_eq!(
            root,
            std::env::current_dir()
                .unwrap()
                .join("target/isolated-tests/ui-lan-batch-20261030-gui-resume")
                .canonicalize()
                .unwrap()
        );
        let batch_id = "00a18c00-67e7-4d5a-a510-0003735118c1";
        let task_id = "f5ae8406-b619-40af-a2fc-f69a8f0f059d";
        let batch = crate::batch_store::get(batch_id).unwrap();
        assert_eq!(batch.project_id, 1856624456);
        assert_eq!(batch.state, "unknown");
        assert_eq!(batch.targets[0].state, "unknown");
        assert_eq!(batch.targets[0].release_task_id.as_deref(), Some(task_id));
        assert_eq!(batch.targets[1].state, "waiting");
        assert!(batch.targets[1].release_task_id.is_none());
        let project = crate::project_store::effective(batch.project_id).unwrap();
        assert!(
            std::path::Path::new(&project.path)
                .canonicalize()
                .unwrap()
                .starts_with(root)
        );
        let task = crate::release_store::get_task(task_id).unwrap();
        assert_eq!(
            (task.phase.as_str(), task.outcome.as_str()),
            ("uploading", "unknown")
        );
        let inspection = crate::release::inspect_unknown_upload(task_id).unwrap();
        assert!(inspection.containers.is_empty());
        assert_eq!(inspection.archives.len(), 1);
        let offset = inspection.archives[0].actual_bytes.unwrap();
        assert!(offset > 0 && offset < inspection.archives[0].expected_bytes);
        crate::ssh::assert_empty_project_namespace(&task).unwrap();
        let mut baseline = Vec::new();
        for (target, address, daemon, count) in [
            (
                &batch.targets[0],
                "192.168.63.129",
                "882622c2-a12a-4ac3-99d4-c3ab389f2641",
                36,
            ),
            (
                &batch.targets[1],
                "192.168.63.130",
                "ac70e19f-a489-49f3-82f0-0a4ad66ca2e6",
                28,
            ),
        ] {
            assert_eq!(target.host.address, address);
            assert_eq!(target.docker_daemon_id, daemon);
            assert_eq!(
                crate::ssh::docker_identity_for_host(target.host.clone())
                    .unwrap()
                    .1,
                daemon
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
            assert_eq!(ids.len(), count);
            baseline.push(ids);
        }

        cx.update(gpui_kit::init);
        let (handle, entity) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| AppView::new(window, cx))
            })
            .unwrap()
        });
        cx.update_window(handle, |_, window, cx| {
            entity.update(cx, |view, cx| {
                view.page = Page::Deploy;
                view.deploy_tab = DeployTab::History;
                view.release_tasks = vec![task.clone()];
                view.release_batches = vec![crate::batch_store::get(batch_id).unwrap()];
                view.release_tasks_loading = false;
                view.release_history_selected = Some(task_id.into());
                cx.notify();
            });
            window.draw(cx).clear(cx);
        })
        .unwrap();
        let mut visual = gpui_kit::VisualTestContext::from_window(handle, cx);
        visual.simulate_resize(size(px(1700.), px(1800.)));
        cx.update_window(handle, |_, window, cx| window.draw(cx).clear(cx))
            .unwrap();
        let inspect = visual
            .debug_bounds("release-upload-inspect-button")
            .unwrap();
        visual.simulate_click(inspect.center(), Modifiers::default());
        let started = std::time::Instant::now();
        loop {
            cx.executor()
                .advance_clock(std::time::Duration::from_millis(200));
            cx.run_until_parked();
            if cx.read(|app| {
                entity
                    .read(app)
                    .release_upload_inspections
                    .contains_key(task_id)
            }) {
                break;
            }
            assert!(started.elapsed() < std::time::Duration::from_secs(120));
        }
        assert!(cx.read(|app| entity.read(app).release_upload_inspections[task_id].is_ok()));
        cx.update_window(handle, |_, window, cx| window.draw(cx).clear(cx))
            .unwrap();
        let resume = visual.debug_bounds("release-upload-resume-button").unwrap();
        visual.simulate_click(resume.center(), Modifiers::default());
        cx.update_window(handle, |_, window, cx| {
            assert!(window.has_active_dialog(cx));
            window.draw(cx).clear(cx);
        })
        .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(350));
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| window.draw(cx).clear(cx))
            .unwrap();
        let confirm = visual.debug_bounds("batch-resume-confirm").unwrap();
        visual.simulate_click(confirm.center(), Modifiers::default());
        cx.update_window(handle, |_, window, cx| {
            assert!(!window.has_active_dialog(cx))
        })
        .unwrap();
        let started = std::time::Instant::now();
        loop {
            cx.executor()
                .advance_clock(std::time::Duration::from_millis(200));
            cx.run_until_parked();
            if !cx.read(|app| entity.read(app).batch_resolving.contains(batch_id)) {
                break;
            }
            assert!(
                started.elapsed() < std::time::Duration::from_secs(300),
                "GUI upload resume did not settle; audit batch {batch_id}"
            );
        }
        let first = crate::release_store::get_task(task_id).unwrap();
        assert_eq!(
            (
                first.phase.as_str(),
                first.outcome.as_str(),
                first.health_result.as_deref()
            ),
            ("checked", "blocked", Some("healthy")),
            "audit batch {batch_id} before any retry"
        );
        let after = crate::batch_store::get(batch_id).unwrap();
        assert_eq!(after.state, "running");
        assert_eq!(after.targets[0].state, "running");
        assert_eq!(after.targets[1].state, "waiting");
        assert!(after.targets[1].release_task_id.is_none());
        let conn = crate::database::connect().unwrap();
        let pointers: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM project_current_version WHERE project_id = ?1",
                [after.project_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(pointers, 0);
        let observed = crate::ssh::observe_release_project(&first).unwrap();
        assert_eq!(observed.containers.len(), 4);
        assert!(observed.containers.iter().all(|container| {
            container.status == "running" && container.health.as_deref() == Some("healthy")
        }));
        for (target, before) in after.targets.iter().zip(&baseline) {
            let mut ids = crate::ssh::inventory(target.host.id)
                .unwrap()
                .containers
                .iter()
                .map(|container| container.id.clone())
                .collect::<Vec<_>>();
            ids.sort();
            if target.host.id == 1 {
                assert_eq!(ids.len(), before.len() + 4);
                assert!(before.iter().all(|id| ids.contains(id)));
            } else {
                assert_eq!(&ids, before);
            }
        }
        let archives = crate::release_store::uploaded_archives(task_id).unwrap();
        assert_eq!(archives.len(), 2);
        assert!(archives.iter().all(|archive| archive.state == "cleaned"));
        eprintln!(
            "LAN GUI resumed first host from {offset} bytes: batch={batch_id}, task={task_id}"
        );
    }

    #[gpui_kit::test]
    #[ignore = "one-shot LAN GUI read-only recovery of a marked Compose startup"]
    fn isolated_gui_lan_marked_start_recovers_to_confirmation(cx: &mut gpui_kit::TestAppContext) {
        use super::{AppView, DeployTab, Page};
        use gpui_kit::component::WindowExt as _;
        use gpui_kit::{AppContext as _, Modifiers, WindowOptions, px, size};

        let appdata = std::path::PathBuf::from(std::env::var_os("APPDATA").unwrap())
            .canonicalize()
            .unwrap();
        let root = appdata.parent().unwrap();
        assert_eq!(
            root,
            std::env::current_dir()
                .unwrap()
                .join("target/isolated-tests/ui-lan-recovery-5c842ae5a3de461684a8f2b1bd0400d3")
                .canonicalize()
                .unwrap()
        );
        let batch_id = "0e1ffb0e-25ba-4537-a76e-a3c164ffaf0b";
        let task_id = "004cd188-9511-4eaa-b224-4bf971265776";
        let batch = crate::batch_store::get(batch_id).unwrap();
        assert_eq!(batch.project_id, 1856624454);
        assert_eq!(batch.state, "unknown");
        assert_eq!(batch.targets[0].state, "unknown");
        assert_eq!(batch.targets[0].release_task_id.as_deref(), Some(task_id));
        assert_eq!(batch.targets[1].state, "waiting");
        assert!(batch.targets[1].release_task_id.is_none());
        let project = crate::project_store::effective(batch.project_id).unwrap();
        assert!(
            std::path::Path::new(&project.path)
                .canonicalize()
                .unwrap()
                .starts_with(root)
        );
        let task = crate::release_store::get_task(task_id).unwrap();
        assert_eq!(
            (task.phase.as_str(), task.outcome.as_str()),
            ("deploying", "unknown")
        );
        assert!(task.health_result.is_none());
        crate::ssh::verify_start_completed(&task).unwrap();
        let observed = crate::ssh::observe_release_project(&task).unwrap();
        assert_eq!(observed.containers.len(), 4);
        assert!(observed.containers.iter().all(|container| {
            container.status == "running" && container.health.as_deref() == Some("healthy")
        }));
        let conn = crate::database::connect().unwrap();
        let audits: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM release_task_health_reconciliations WHERE task_id = ?1",
                [task_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(audits, 0);
        let pointers = || {
            conn.query_row(
                "SELECT COUNT(*) FROM project_current_version WHERE project_id = ?1",
                [batch.project_id],
                |row| row.get::<_, i64>(0),
            )
            .unwrap()
        };
        assert_eq!(pointers(), 0);
        let mut baseline = Vec::new();
        for (target, address, daemon, count) in [
            (
                &batch.targets[0],
                "192.168.63.129",
                "882622c2-a12a-4ac3-99d4-c3ab389f2641",
                36,
            ),
            (
                &batch.targets[1],
                "192.168.63.130",
                "ac70e19f-a489-49f3-82f0-0a4ad66ca2e6",
                24,
            ),
        ] {
            assert_eq!(target.host.address, address);
            assert_eq!(target.docker_daemon_id, daemon);
            assert_eq!(
                crate::ssh::docker_identity_for_host(target.host.clone())
                    .unwrap()
                    .1,
                daemon
            );
            let mut ids = crate::ssh::inventory(target.host.id)
                .unwrap()
                .containers
                .iter()
                .map(|container| container.id.clone())
                .collect::<Vec<_>>();
            ids.sort();
            assert_eq!(ids.len(), count);
            baseline.push(ids);
        }

        cx.update(gpui_kit::init);
        let (handle, entity) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| AppView::new(window, cx))
            })
            .unwrap()
        });
        cx.update_window(handle, |_, window, cx| {
            entity.update(cx, |view, cx| {
                view.page = Page::Deploy;
                view.deploy_tab = DeployTab::History;
                view.release_tasks = vec![task.clone()];
                view.release_batches = vec![crate::batch_store::get(batch_id).unwrap()];
                view.release_tasks_loading = false;
                view.release_history_selected = Some(task_id.into());
                cx.notify();
            });
            window.draw(cx).clear(cx);
        })
        .unwrap();
        let mut visual = gpui_kit::VisualTestContext::from_window(handle, cx);
        visual.simulate_resize(size(px(1700.), px(1200.)));
        cx.update_window(handle, |_, window, cx| window.draw(cx).clear(cx))
            .unwrap();
        let button = visual
            .debug_bounds("release-reconcile-health-button")
            .unwrap();
        visual.simulate_click(button.center(), Modifiers::default());
        cx.update_window(handle, |_, window, cx| {
            assert!(window.has_active_dialog(cx));
            window.draw(cx).clear(cx);
        })
        .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(350));
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| window.draw(cx).clear(cx))
            .unwrap();
        let confirm = visual
            .debug_bounds("release-reconcile-confirm-button")
            .unwrap();
        let cancel = visual.debug_bounds("release-reconcile-cancel").unwrap();
        assert_eq!(confirm.origin.y, cancel.origin.y);
        visual.simulate_click(confirm.center(), Modifiers::default());
        cx.update_window(handle, |_, window, cx| {
            assert!(!window.has_active_dialog(cx));
        })
        .unwrap();
        let started = std::time::Instant::now();
        loop {
            cx.executor()
                .advance_clock(std::time::Duration::from_millis(200));
            cx.run_until_parked();
            let current = crate::release_store::get_task(task_id).unwrap();
            if current.phase == "checked" {
                assert_eq!(
                    (current.outcome.as_str(), current.health_result.as_deref()),
                    ("blocked", Some("healthy"))
                );
                break;
            }
            assert!(
                started.elapsed() < std::time::Duration::from_secs(120),
                "GUI marked-start recovery did not settle; audit batch {batch_id}"
            );
        }
        let batch_after = crate::batch_store::get(batch_id).unwrap();
        assert_eq!(batch_after.state, "running");
        assert_eq!(batch_after.targets[0].state, "running");
        assert_eq!(batch_after.targets[1].state, "waiting");
        assert!(batch_after.targets[1].release_task_id.is_none());
        assert_eq!(pointers(), 0);
        let (source_phase, containers): (String, i64) = conn.query_row(
            "SELECT source_phase, observed_containers FROM release_task_health_reconciliations WHERE task_id = ?1",
            [task_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        ).unwrap();
        assert_eq!((source_phase.as_str(), containers), ("deploying", 4));
        for (target, before) in batch.targets.iter().zip(&baseline) {
            let mut after = crate::ssh::inventory(target.host.id)
                .unwrap()
                .containers
                .iter()
                .map(|container| container.id.clone())
                .collect::<Vec<_>>();
            after.sort();
            assert_eq!(&after, before);
        }
        eprintln!(
            "LAN marked-start GUI recovery awaits confirmation: batch={batch_id}, task={task_id}"
        );
    }

    #[gpui_kit::test]
    #[ignore = "read-only GUI refusal for a partially created LAN stack without a completion marker"]
    fn isolated_gui_lan_partial_start_stays_unknown(cx: &mut gpui_kit::TestAppContext) {
        use super::{AppView, DeployTab, Page};
        use gpui_kit::component::WindowExt as _;
        use gpui_kit::{AppContext as _, Modifiers, WindowOptions, px, size};

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
        let batch_id = "0e1ffb0e-25ba-4537-a76e-a3c164ffaf0b";
        let task_id = "006cf747-ae46-428e-a3ca-f896a7c31c9a";
        let batch = crate::batch_store::get(batch_id).unwrap();
        assert_eq!(batch.project_id, 1856624454);
        assert_eq!(batch.state, "unknown");
        assert_eq!(batch.targets[0].state, "healthy");
        assert_eq!(batch.targets[1].state, "unknown");
        assert_eq!(batch.targets[1].release_task_id.as_deref(), Some(task_id));
        let task = crate::release_store::get_task(task_id).unwrap();
        assert_eq!(
            (task.phase.as_str(), task.outcome.as_str()),
            ("deploying", "unknown")
        );
        assert!(task.health_result.is_none());
        assert!(crate::ssh::verify_start_completed(&task).is_err());
        let observed = crate::ssh::observe_release_project(&task).unwrap();
        assert_eq!(observed.containers.len(), 4);
        assert_eq!(
            observed
                .containers
                .iter()
                .filter(|c| c.status == "running")
                .count(),
            2
        );
        assert_eq!(
            observed
                .containers
                .iter()
                .filter(|c| c.status == "created")
                .count(),
            2
        );
        let conn = crate::database::connect().unwrap();
        let counts =
            || {
                let audits: i64 = conn.query_row(
                "SELECT COUNT(*) FROM release_task_health_reconciliations WHERE task_id = ?1",
                [task_id],
                |row| row.get(0),
            ).unwrap();
                let pointers: i64 = conn
                    .query_row(
                        "SELECT COUNT(*) FROM project_current_version WHERE project_id = ?1",
                        [batch.project_id],
                        |row| row.get(0),
                    )
                    .unwrap();
                (audits, pointers)
            };
        assert_eq!(counts(), (0, 1));
        let mut baseline = Vec::new();
        for (target, address, daemon, count) in [
            (
                &batch.targets[0],
                "192.168.63.129",
                "882622c2-a12a-4ac3-99d4-c3ab389f2641",
                36,
            ),
            (
                &batch.targets[1],
                "192.168.63.130",
                "ac70e19f-a489-49f3-82f0-0a4ad66ca2e6",
                28,
            ),
        ] {
            assert_eq!(target.host.address, address);
            assert_eq!(target.docker_daemon_id, daemon);
            assert_eq!(
                crate::ssh::docker_identity_for_host(target.host.clone())
                    .unwrap()
                    .1,
                daemon
            );
            let mut ids = crate::ssh::inventory(target.host.id)
                .unwrap()
                .containers
                .iter()
                .map(|container| container.id.clone())
                .collect::<Vec<_>>();
            ids.sort();
            assert_eq!(ids.len(), count);
            baseline.push(ids);
        }

        cx.update(gpui_kit::init);
        let (handle, entity) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| AppView::new(window, cx))
            })
            .unwrap()
        });
        cx.update_window(handle, |_, window, cx| {
            entity.update(cx, |view, cx| {
                view.page = Page::Deploy;
                view.deploy_tab = DeployTab::History;
                view.release_tasks = vec![task.clone()];
                view.release_batches = vec![crate::batch_store::get(batch_id).unwrap()];
                view.release_tasks_loading = false;
                view.release_history_selected = Some(task_id.into());
                cx.notify();
            });
            window.draw(cx).clear(cx);
        })
        .unwrap();
        let mut visual = gpui_kit::VisualTestContext::from_window(handle, cx);
        visual.simulate_resize(size(px(1700.), px(1200.)));
        cx.update_window(handle, |_, window, cx| window.draw(cx).clear(cx))
            .unwrap();
        for action in [
            "batch-continue-button",
            "batch-skip-button",
            "batch-terminate-button",
        ] {
            assert!(
                visual.debug_bounds(action).is_none(),
                "{action} must remain unavailable"
            );
        }
        let notifications_before = cx
            .update_window(handle, |_, window, cx| window.notifications(cx).len())
            .unwrap();
        let button = visual
            .debug_bounds("release-reconcile-health-button")
            .unwrap();
        visual.simulate_click(button.center(), Modifiers::default());
        cx.update_window(handle, |_, window, cx| {
            assert!(window.has_active_dialog(cx));
            window.draw(cx).clear(cx);
        })
        .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(350));
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| window.draw(cx).clear(cx))
            .unwrap();
        let confirm = visual
            .debug_bounds("release-reconcile-confirm-button")
            .unwrap();
        let cancel = visual.debug_bounds("release-reconcile-cancel").unwrap();
        assert_eq!(confirm.origin.y, cancel.origin.y);
        visual.simulate_click(confirm.center(), Modifiers::default());
        cx.update_window(handle, |_, window, cx| {
            assert!(!window.has_active_dialog(cx));
        })
        .unwrap();
        let started = std::time::Instant::now();
        loop {
            cx.executor()
                .advance_clock(std::time::Duration::from_millis(200));
            cx.run_until_parked();
            if !cx.read(|app| entity.read(app).release_resolving.contains(task_id)) {
                break;
            }
            assert!(
                started.elapsed() < std::time::Duration::from_secs(60),
                "GUI read-only reconciliation did not settle"
            );
        }
        assert!(
            cx.update_window(handle, |_, window, cx| window.notifications(cx).len())
                .unwrap()
                > notifications_before,
            "GUI reconciliation did not report its refusal"
        );
        let after = crate::release_store::get_task(task_id).unwrap();
        assert_eq!(
            (after.phase.as_str(), after.outcome.as_str()),
            ("deploying", "unknown")
        );
        assert!(after.health_result.is_none());
        let batch_after = crate::batch_store::get(batch_id).unwrap();
        assert_eq!(batch_after.state, "unknown");
        assert_eq!(batch_after.targets[0].state, "healthy");
        assert_eq!(batch_after.targets[1].state, "unknown");
        assert_eq!(counts(), (0, 1));
        for (target, before) in batch.targets.iter().zip(&baseline) {
            let mut after = crate::ssh::inventory(target.host.id)
                .unwrap()
                .containers
                .iter()
                .map(|container| container.id.clone())
                .collect::<Vec<_>>();
            after.sort();
            assert_eq!(&after, before);
        }
        eprintln!("LAN partial-start GUI recovery refused: batch={batch_id}, task={task_id}");
    }

    #[gpui_kit::test]
    #[ignore = "one-shot LAN batch GUI click; requires fresh ui-lan-batch APPDATA, password, fixed images and zero containers on two trusted test hosts"]
    fn isolated_gui_lan_batch_first_host_waits_for_confirmation(cx: &mut gpui_kit::TestAppContext) {
        use super::{AppView, Page};
        use gpui_kit::component::WindowExt as _;
        use gpui_kit::{AppContext as _, Modifiers, WindowOptions, px, size};
        use std::sync::{Arc, atomic::AtomicBool};

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
        assert!(std::fs::read_dir(&appdata).unwrap().next().is_none());
        let password = std::env::var("DEVINGPANEL_TEST_LAN_PASSWORD")
            .expect("isolated LAN test password is required");
        let conn = crate::database::connect().unwrap();
        let fingerprint = "SHA256:+YD+qOVvzV7TP3DUXlFSIXadVdH8aNaUoeYLfEGcKE4";
        for (name, address) in [
            ("vps-smoke", "192.168.63.129"),
            ("vps-second", "192.168.63.130"),
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
        }
        drop(password);

        let project_id = 1_000_000_001 + (uuid::Uuid::new_v4().as_u128() % 900_000_000) as i64;
        conn.execute(
            "INSERT INTO projects (id, name, path, dockerfile, image)
             VALUES (?1, 'isolated-id-reservation', '.', 'Dockerfile', 'reserved-image')",
            [project_id - 1],
        )
        .unwrap();
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
                "name: stack-v2-success",
                &format!("name: stack-v2-lan-{project_id}"),
            )
            .replace(
                "    - group: test\n      host: vps-smoke",
                "    - group: test\n      host: vps-smoke\n    - group: test\n      host: vps-second",
            ),
        )
        .unwrap();
        let project = crate::project_store::create_from_manifest(&project_dir).unwrap();
        assert_eq!(project.id, project_id);
        assert_eq!(project.host_ids, [1, 2]);
        let hosts = crate::host_store::list().unwrap().0;
        let before = project
            .host_ids
            .iter()
            .map(|id| {
                let inventory = crate::ssh::inventory(*id).unwrap();
                assert!(inventory.containers.is_empty());
                assert_eq!(inventory.images.len(), 2);
                inventory
                    .containers
                    .iter()
                    .map(|container| container.id.clone())
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        let tag = format!("lan{}", uuid::Uuid::new_v4().simple());
        let snapshot = Arc::new(
            crate::stack_release::preflight_batch(
                project.id,
                &tag,
                Arc::new(AtomicBool::new(false)),
            )
            .unwrap(),
        );
        assert_eq!(snapshot.targets.len(), 2);
        for (target, address, daemon_id) in [
            (
                &snapshot.targets[0],
                "192.168.63.129",
                "882622c2-a12a-4ac3-99d4-c3ab389f2641",
            ),
            (
                &snapshot.targets[1],
                "192.168.63.130",
                "ac70e19f-a489-49f3-82f0-0a4ad66ca2e6",
            ),
        ] {
            assert_eq!(target.host.address, address);
            assert_eq!(target.docker_daemon_id, daemon_id);
        }
        for target in &snapshot.targets {
            let probe = crate::release_store::TaskSnapshot {
                id: "namespace-probe".into(),
                project_id,
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
            assert!(
                crate::ssh::observe_release_project(&probe)
                    .unwrap()
                    .containers
                    .is_empty()
            );
        }
        assert!(
            crate::batch_store::recent(Some(project.id))
                .unwrap()
                .is_empty()
        );

        cx.update(gpui_kit::init);
        let (handle, entity) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| AppView::new(window, cx))
            })
            .unwrap()
        });
        cx.update_window(handle, |_, window, cx| {
            entity.update(cx, |view, cx| {
                view.page = Page::Deploy;
                view.projects = vec![project.clone()];
                view.project_loaded = true;
                view.hosts = hosts.clone();
                view.host_loaded = true;
                view.deploy_project_id = Some(project.id);
                view.deploy_host_ids = project.host_ids.clone();
                view.deploy_parallel = 1;
                view.deploy_tag
                    .update(cx, |input, cx| input.set_value(tag.clone(), window, cx));
                view.deploy_config_result = Some(Ok(()));
                view.deploy_env_result = Some(Ok(()));
                view.deploy_target_result = Some(Ok(snapshot
                    .targets
                    .iter()
                    .map(|target| (target.host_id, target.platform.clone()))
                    .collect()));
                view.deploy_tag_plan = Some(Ok(snapshot
                    .plan
                    .builds
                    .iter()
                    .map(|build| crate::release::PlannedImage {
                        platform: build.platform.clone(),
                        reference: build.reference.clone(),
                    })
                    .collect()));
                view.deploy_batch_check = Some(Ok(snapshot.clone()));
                assert!(view.batch_start_ready(&project, &project.host_ids));
                view.confirm_batch_deploy(window, cx);
            });
            window.draw(cx).clear(cx);
        })
        .unwrap();
        let mut window = gpui_kit::VisualTestContext::from_window(handle, cx);
        window.simulate_resize(size(px(1700.), px(1200.)));
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(500));
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| window.draw(cx).clear(cx))
            .unwrap();
        cx.read(|app| {
            let view = entity.read(app);
            assert!(
                view.batch_start_ready(&project, &project.host_ids),
                "batch snapshot changed before confirmation"
            );
        });
        let confirm = window.debug_bounds("batch-start-confirm").unwrap();
        assert!(confirm.origin.y >= px(0.) && confirm.origin.y + confirm.size.height <= px(1200.));
        window.simulate_click(confirm.center(), Modifiers::default());
        cx.update_window(handle, |_, window, cx| {
            assert!(
                !window.has_active_dialog(cx),
                "batch confirmation was not clicked"
            );
        })
        .unwrap();
        assert!(
            cx.read(|app| {
                let view = entity.read(app);
                view.deploy_run_busy || view.deploy_run_stage == "当前主机健康，等待人工确认"
            }),
            "batch confirmation did not start: {}",
            cx.read(|app| entity.read(app).deploy_run_stage.clone())
        );
        let started = std::time::Instant::now();
        while cx.read(|app| entity.read(app).deploy_run_busy) {
            cx.executor()
                .advance_clock(std::time::Duration::from_millis(200));
            cx.run_until_parked();
            assert!(
                started.elapsed() < std::time::Duration::from_secs(1800),
                "LAN batch execution did not settle; inspect isolated database before any retry"
            );
        }
        let batches = crate::batch_store::recent(Some(project.id)).unwrap();
        assert_eq!(
            batches.len(),
            1,
            "batch not created: {}",
            cx.read(|app| entity.read(app).deploy_run_stage.clone())
        );
        let batch = &batches[0];
        assert_eq!(batch.state, "running", "batch {} requires audit", batch.id);
        assert_eq!(batch.targets[0].state, "running");
        assert_eq!(batch.targets[1].state, "waiting");
        assert!(batch.targets[1].release_task_id.is_none());
        let task =
            crate::release_store::get_task(batch.targets[0].release_task_id.as_ref().unwrap())
                .unwrap();
        assert_eq!(
            (
                task.phase.as_str(),
                task.outcome.as_str(),
                task.health_result.as_deref()
            ),
            ("checked", "blocked", Some("healthy"))
        );
        let after = project
            .host_ids
            .iter()
            .map(|id| {
                crate::ssh::inventory(*id)
                    .unwrap()
                    .containers
                    .iter()
                    .map(|container| container.id.clone())
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        assert_eq!(after[0].len(), before[0].len() + 4);
        assert_eq!(after[1], before[1]);
        let pointers: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM project_current_version WHERE project_id=?1",
                [project.id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(pointers, 0);
        eprintln!(
            "LAN batch first host awaits confirmation: project={}, batch={}, task={}",
            project.id, batch.id, task.id
        );
    }

    #[gpui_kit::test]
    #[ignore = "one-shot guarded batch GUI click to trusted VPS; requires copied isolated APPDATA, a unique project, vault credentials and local Docker"]
    fn isolated_gui_initial_batch_first_host_to_vps(cx: &mut gpui_kit::TestAppContext) {
        use super::{AppView, Page};
        use gpui_kit::component::WindowExt as _;
        use gpui_kit::{AppContext as _, Modifiers, WindowOptions, px, size};
        let appdata = std::path::PathBuf::from(std::env::var_os("APPDATA").unwrap())
            .canonicalize()
            .unwrap();
        let root = appdata.parent().unwrap();
        assert!(
            root.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("ui-batch-live-")
        );
        assert_eq!(
            root.parent().unwrap(),
            std::env::current_dir()
                .unwrap()
                .join("target/isolated-tests")
                .canonicalize()
                .unwrap()
        );
        let source = root.join("project");
        let manifest = crate::project_config::load(&source).unwrap();
        assert_eq!(manifest.name, "stack-v2-gui-batch-1000900003");
        let conn = crate::database::connect().unwrap();
        conn.execute(
            "INSERT INTO projects (id, name, path, dockerfile, image)
            VALUES (1000900002, 'isolated-id-reservation', '.', 'Dockerfile', 'reserved-image')",
            [],
        )
        .unwrap();
        let project = crate::project_store::create_from_manifest(&source).unwrap();
        assert_eq!(project.id, 1_000_900_003);
        assert_eq!(
            std::path::Path::new(&project.path).canonicalize().unwrap(),
            source.canonicalize().unwrap()
        );
        assert_eq!(project.host_ids, [1, 2]);
        assert_eq!(project.compose_files, ["compose.yaml"]);
        assert!(
            crate::release_store::recent(Some(project.id))
                .unwrap()
                .is_empty()
        );
        assert!(
            crate::batch_store::recent(Some(project.id))
                .unwrap()
                .is_empty()
        );
        let hosts = crate::host_store::list().unwrap().0;
        let history = crate::batch_store::recent(Some(900002)).unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].state, "complete");
        let prior_tasks = crate::release_store::recent(Some(900002)).unwrap();
        assert_eq!(prior_tasks.len(), 2);
        for host_id in &project.host_ids {
            let current = hosts.iter().find(|host| host.id == *host_id).unwrap();
            let saved = history[0]
                .targets
                .iter()
                .find(|target| target.host.id == *host_id)
                .unwrap();
            assert!(saved.host == *current);
            assert_eq!(
                current.address,
                if *host_id == 2 {
                    "43.162.99.10"
                } else {
                    "129.211.23.223"
                }
            );
            let mut probe = prior_tasks
                .iter()
                .find(|task| task.host.id == *host_id)
                .unwrap()
                .clone();
            assert_eq!(
                probe.docker_daemon_id.as_deref(),
                Some(saved.docker_daemon_id.as_str())
            );
            probe.project_id = project.id;
            assert!(
                crate::ssh::observe_release_project(&probe)
                    .unwrap()
                    .containers
                    .is_empty()
            );
            crate::ssh::assert_empty_project_namespace(&probe).unwrap();
        }
        let baseline = project
            .host_ids
            .iter()
            .map(|id| {
                let inventory = crate::ssh::inventory(*id).unwrap();
                let mut ids = inventory
                    .containers
                    .iter()
                    .map(|item| item.id.clone())
                    .collect::<Vec<_>>();
                ids.sort();
                ids
            })
            .collect::<Vec<_>>();
        assert_eq!(baseline.len(), 2);
        let tag = format!("gui{}", uuid::Uuid::new_v4().simple());
        let snapshot = std::sync::Arc::new(
            crate::stack_release::preflight_batch(
                project.id,
                &tag,
                std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            )
            .unwrap(),
        );
        cx.update(gpui_kit::init);
        let (handle, entity) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| AppView::new(window, cx))
            })
            .unwrap()
        });
        cx.update_window(handle, |_, window, cx| {
            entity.update(cx, |view, cx| {
                view.page = Page::Deploy;
                view.projects = vec![project.clone()];
                view.project_loaded = true;
                view.hosts = hosts.clone();
                view.host_loaded = true;
                view.deploy_project_id = Some(project.id);
                view.deploy_host_ids = project.host_ids.clone();
                view.deploy_parallel = 1;
                view.deploy_tag
                    .update(cx, |input, cx| input.set_value(tag.clone(), window, cx));
                view.deploy_config_result = Some(Ok(()));
                view.deploy_env_result = Some(Ok(()));
                view.deploy_target_result = Some(Ok(snapshot
                    .targets
                    .iter()
                    .map(|target| (target.host_id, target.platform.clone()))
                    .collect()));
                view.deploy_tag_plan = Some(Ok(snapshot
                    .plan
                    .builds
                    .iter()
                    .map(|build| crate::release::PlannedImage {
                        platform: build.platform.clone(),
                        reference: build.reference.clone(),
                    })
                    .collect()));
                view.deploy_batch_check = Some(Ok(snapshot.clone()));
                assert!(view.batch_start_ready(&project, &project.host_ids));
                view.confirm_batch_deploy(window, cx);
            });
            window.draw(cx).clear(cx);
        })
        .unwrap();
        let mut window = gpui_kit::VisualTestContext::from_window(handle, cx);
        window.simulate_resize(size(px(1700.), px(1200.)));
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(500));
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| window.draw(cx).clear(cx))
            .unwrap();
        let confirm = window.debug_bounds("batch-start-confirm").unwrap();
        assert!(confirm.origin.y >= px(0.) && confirm.origin.y + confirm.size.height <= px(1200.));
        window.simulate_click(confirm.center(), Modifiers::default());
        cx.update_window(handle, |_, window, cx| {
            assert!(!window.has_active_dialog(cx));
        })
        .unwrap();
        let started = std::time::Instant::now();
        while cx.read(|app| entity.read(app).deploy_run_busy) {
            cx.executor()
                .advance_clock(std::time::Duration::from_millis(200));
            cx.run_until_parked();
            assert!(
                started.elapsed() < std::time::Duration::from_secs(1800),
                "batch GUI execution did not settle; inspect isolated database before any retry"
            );
        }
        let batches = crate::batch_store::recent(Some(project.id)).unwrap();
        assert_eq!(
            batches.len(),
            1,
            "preflight/execute did not create a batch: {}",
            cx.read(|app| entity.read(app).deploy_run_stage.clone())
        );
        let batch = &batches[0];
        assert_eq!(batch.state, "running", "batch {} requires audit", batch.id);
        assert_eq!(batch.targets[0].state, "running");
        assert_eq!(batch.targets[1].state, "waiting");
        assert!(batch.targets[1].release_task_id.is_none());
        let task =
            crate::release_store::get_task(batch.targets[0].release_task_id.as_ref().unwrap())
                .unwrap();
        assert_eq!(
            (
                task.phase.as_str(),
                task.outcome.as_str(),
                task.health_result.as_deref()
            ),
            ("checked", "blocked", Some("healthy"))
        );
        assert_eq!(
            crate::release_store::uploaded_archives(&task.id)
                .unwrap()
                .iter()
                .filter(|archive| archive.state == "cleaned")
                .count(),
            2
        );
        let conn = crate::database::connect().unwrap();
        let pointers: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM project_current_version WHERE project_id=?1",
                [project.id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            pointers, 0,
            "version pointer moved before human confirmation"
        );
        let after = project
            .host_ids
            .iter()
            .map(|id| {
                let inventory = crate::ssh::inventory(*id).unwrap();
                let mut ids = inventory
                    .containers
                    .iter()
                    .map(|item| item.id.clone())
                    .collect::<Vec<_>>();
                ids.sort();
                ids
            })
            .collect::<Vec<_>>();
        assert!(baseline[0].iter().all(|id| after[0].contains(id)));
        assert_eq!(after[0].len(), baseline[0].len() + 4);
        assert_eq!(after[1], baseline[1]);
        eprintln!(
            "batch GUI first host healthy: project={}, batch={}, containers before={:?}, after={:?}",
            project.id,
            batch.id,
            baseline.iter().map(Vec::len).collect::<Vec<_>>(),
            after.iter().map(Vec::len).collect::<Vec<_>>()
        );
    }

    #[gpui_kit::test]
    #[ignore = "one-shot GUI click to trusted VPS; requires isolated high-ID project, vault credential and local Docker"]
    fn isolated_gui_confirmed_release_to_vps(cx: &mut gpui_kit::TestAppContext) {
        use super::{AppView, Page};
        use gpui_kit::{AppContext as _, Modifiers, WindowOptions};
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
                .starts_with("devingpanel-v20-series-")
        );
        let conn = crate::database::connect().unwrap();
        for sql in [
            "SELECT COUNT(*) FROM pending_credentials",
            "SELECT COUNT(*) FROM pending_env_credentials",
            "SELECT COUNT(*) FROM hosts WHERE deleting != 0",
            "SELECT COUNT(*) FROM release_tasks t JOIN release_materials m ON m.task_id = t.id WHERE t.phase = 'prepared' AND t.outcome IN ('running', 'unknown', 'failed')",
        ] {
            let pending: i64 = conn.query_row(sql, [], |row| row.get(0)).unwrap();
            assert_eq!(
                pending, 0,
                "isolated database has startup cleanup work: {sql}"
            );
        }
        let project_id: i64 = std::env::var("DEVINGPANEL_TEST_SERIES_PROJECT_ID")
            .unwrap()
            .parse()
            .unwrap();
        assert!(project_id > 1_000_000_000);
        let project = crate::project_store::effective(project_id).unwrap();
        let data = root.join("devingPanel").canonicalize().unwrap();
        assert!(project.manifest_mode && project.name.starts_with("stack-series-"));
        assert!(
            std::path::Path::new(&project.path)
                .canonicalize()
                .unwrap()
                .starts_with(&data)
        );
        assert_eq!(project.host_ids.len(), 1);
        let host = crate::host_store::list()
            .unwrap()
            .0
            .into_iter()
            .find(|host| {
                host.id == project.host_ids[0] && host.name == "vps-smoke" && host.group == "test"
            })
            .unwrap();
        let previous = crate::release_store::current_completed_for(
            &crate::release_store::recent(Some(project_id)).unwrap()[0],
        )
        .unwrap();
        assert_eq!(
            (host.address.as_str(), host.port, host.username.as_str()),
            ("129.211.23.223", 22, "root")
        );
        assert_eq!(host.address, previous.host.address);
        assert_eq!(host.port, previous.host.port);
        assert_eq!(host.username, previous.host.username);
        assert_eq!(host.auth, previous.host.auth);
        assert_eq!(host.trusted_fingerprint, previous.host.trusted_fingerprint);
        let (endpoint, daemon) = crate::ssh::docker_identity_for_host(host.clone()).unwrap();
        assert_eq!(Some(endpoint.as_str()), previous.docker_endpoint.as_deref());
        assert_eq!(Some(daemon.as_str()), previous.docker_daemon_id.as_deref());
        let before = crate::release::inspect_current_version(&previous.id).unwrap();
        assert_eq!(
            before.matched_task_id.as_deref(),
            Some(previous.id.as_str())
        );
        assert!(before.all_healthy);
        let snapshot = crate::release_materials::load(&previous.id).unwrap();
        let images = crate::release_store::historical_images(&previous.id).unwrap();
        assert_eq!(
            crate::ssh::verify_running_config_hashes(&previous, &snapshot, &images)
                .unwrap()
                .containers
                .len(),
            4
        );
        let prior_ids = crate::release_store::recent(Some(project_id))
            .unwrap()
            .into_iter()
            .map(|task| task.id)
            .collect::<std::collections::HashSet<_>>();
        let tag = format!("gui{}", uuid::Uuid::new_v4().simple());
        cx.update(gpui_kit::init);
        let (handle, view_entity) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| AppView::new(window, cx))
            })
            .unwrap()
        });
        cx.update_window(handle, |_, window, cx| {
            view_entity.update(cx, |view, cx| {
                view.page = Page::Deploy;
                view.projects = vec![project.clone()];
                view.project_loaded = true;
                view.hosts = vec![host.clone()];
                view.host_loaded = true;
                view.deploy_project_id = Some(project_id);
                view.deploy_host_ids = vec![host.id];
                view.deploy_tag
                    .update(cx, |input, cx| input.set_value(tag.clone(), window, cx));
                view.deploy_config_result = Some(Ok(()));
                view.deploy_env_result = Some(Ok(()));
                view.deploy_target_result = Some(Ok(vec![(host.id, "linux/amd64".into())]));
                view.deploy_build_platforms = Some(Ok(vec!["linux/amd64".into()]));
                view.deploy_tag_plan = Some(Ok(crate::release::plan_images(
                    &project,
                    &tag,
                    &[(host.id, "linux/amd64".into())],
                )
                .unwrap()));
                assert!(view.deploy_ready(&project, &[host.id]));
            });
            window.draw(cx).clear(cx);
        })
        .unwrap();
        let mut window = gpui_kit::VisualTestContext::from_window(handle, cx);
        let start = window.debug_bounds("start-deploy-button").unwrap();
        window.simulate_click(start.center(), Modifiers::default());
        cx.update_window(handle, |_, window, cx| {
            window.draw(cx).clear(cx);
        })
        .unwrap();
        let confirm = window.debug_bounds("deploy-confirm-button").unwrap();
        window.simulate_click(confirm.center(), Modifiers::default());
        assert_eq!(
            cx.read(|app| view_entity.read(app).deploy_run_project_id),
            Some(project_id)
        );
        let started = std::time::Instant::now();
        let mut last_report = started;
        while cx.read(|app| view_entity.read(app).deploy_run_busy) {
            cx.executor()
                .advance_clock(std::time::Duration::from_millis(200));
            cx.run_until_parked();
            if last_report.elapsed() >= std::time::Duration::from_secs(60) {
                eprintln!(
                    "GUI release still active after {} seconds; waiting for a settled result",
                    started.elapsed().as_secs()
                );
                last_report = std::time::Instant::now();
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(!cx.read(|app| view_entity.read(app).deploy_run_busy));
        assert!(cx.read(|app| view_entity.read(app).deploy_run_stage.contains("发布完成")));
        let tasks = crate::release_store::recent(Some(project_id)).unwrap();
        assert_eq!(tasks.len(), prior_ids.len() + 1);
        let published = tasks
            .iter()
            .find(|task| !prior_ids.contains(&task.id))
            .expect("new GUI release must exist");
        assert_eq!(
            (published.phase.as_str(), published.outcome.as_str()),
            ("checked", "complete")
        );
        assert_eq!(published.health_result.as_deref(), Some("healthy"));
        let current = crate::release::inspect_current_version(&published.id).unwrap();
        assert_eq!(
            current.matched_task_id.as_deref(),
            Some(published.id.as_str())
        );
        assert!(current.all_healthy);
    }

    #[gpui_kit::test]
    #[ignore = "one-shot GUI rollback click on explicit isolated high-ID VPS project"]
    fn isolated_gui_confirmed_rollback_to_vps(cx: &mut gpui_kit::TestAppContext) {
        use super::{AppView, DeployTab, Page};
        use gpui_kit::component::WindowExt as _;
        use gpui_kit::{AppContext as _, Modifiers, WindowOptions};
        use sha2::{Digest, Sha256};
        const EXPECTED_SOURCE: &str = "ba6717cf-7d58-488a-8a31-9393bd3df38c";
        const EXPECTED_TARGET: &str = "67795719-9538-4c2e-be5c-67b40d8dcca0";
        let fingerprint = |task_id: &str| {
            let services = crate::release_store::confirmed_services(task_id).unwrap();
            let text = services
                .iter()
                .map(|item| format!("{}|{}|{};", item.service, item.reference, item.image_id))
                .collect::<String>();
            format!("{:x}", Sha256::digest(text.as_bytes()))
        };
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
                .starts_with("devingpanel-v20-series-")
        );
        let conn = crate::database::connect().unwrap();
        for sql in [
            "SELECT COUNT(*) FROM pending_credentials",
            "SELECT COUNT(*) FROM pending_env_credentials",
            "SELECT COUNT(*) FROM hosts WHERE deleting != 0",
            "SELECT COUNT(*) FROM release_tasks t JOIN release_materials m ON m.task_id = t.id WHERE t.phase = 'prepared' AND t.outcome IN ('running', 'unknown', 'failed')",
        ] {
            let pending: i64 = conn.query_row(sql, [], |row| row.get(0)).unwrap();
            assert_eq!(pending, 0, "startup cleanup work exists: {sql}");
        }
        let project_id: i64 = std::env::var("DEVINGPANEL_TEST_SERIES_PROJECT_ID")
            .unwrap()
            .parse()
            .unwrap();
        assert!(project_id > 1_000_000_000);
        let project = crate::project_store::effective(project_id).unwrap();
        let data = root.join("devingPanel").canonicalize().unwrap();
        assert!(project.manifest_mode && project.name.starts_with("stack-series-"));
        assert!(
            std::path::Path::new(&project.path)
                .canonicalize()
                .unwrap()
                .starts_with(&data)
        );
        assert_eq!(project.host_ids.len(), 1);
        let source = crate::release_store::current_completed_for(
            &crate::release_store::recent(Some(project_id)).unwrap()[0],
        )
        .unwrap();
        assert_eq!(source.id, EXPECTED_SOURCE);
        assert_eq!(
            fingerprint(EXPECTED_SOURCE),
            "a24b9a80b44e571fa122c69e08f3a95fc714fe310b84c96d62043b0c67742cdd"
        );
        assert_eq!(
            fingerprint(EXPECTED_TARGET),
            "52ce03e25c65a120b9a71af3018298a45068e81d24fac78838d2751cb5d54a82"
        );
        assert_eq!(
            (
                source.phase.as_str(),
                source.outcome.as_str(),
                source.health_result.as_deref()
            ),
            ("checked", "complete", Some("healthy"))
        );
        let host = crate::host_store::list()
            .unwrap()
            .0
            .into_iter()
            .find(|host| {
                host.id == project.host_ids[0] && host.name == "vps-smoke" && host.group == "test"
            })
            .unwrap();
        assert_eq!(
            (host.address.as_str(), host.port, host.username.as_str()),
            ("129.211.23.223", 22, "root")
        );
        assert_eq!(
            (
                host.address.as_str(),
                host.port,
                host.username.as_str(),
                host.auth.as_str(),
                host.trusted_fingerprint.as_deref()
            ),
            (
                source.host.address.as_str(),
                source.host.port,
                source.host.username.as_str(),
                source.host.auth.as_str(),
                source.host.trusted_fingerprint.as_deref()
            )
        );
        let (endpoint, daemon) = crate::ssh::docker_identity_for_host(host.clone()).unwrap();
        assert_eq!(Some(endpoint.as_str()), source.docker_endpoint.as_deref());
        assert_eq!(Some(daemon.as_str()), source.docker_daemon_id.as_deref());
        let before = crate::release::inspect_current_version(&source.id).unwrap();
        assert_eq!(before.matched_task_id.as_deref(), Some(source.id.as_str()));
        assert!(before.all_healthy);
        let preview = crate::rollback::preview(&source.id).unwrap();
        assert_eq!(preview.source_id, source.id);
        assert_eq!(preview.project_id, project_id);
        assert_eq!(preview.target_id, EXPECTED_TARGET);
        let target_id = preview.target_id.clone();
        let prior_rollbacks = crate::rollback_store::recent(Some(project_id))
            .unwrap()
            .len();
        cx.update(gpui_kit::init);
        let (handle, view_entity) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| AppView::new(window, cx))
            })
            .unwrap()
        });
        cx.update_window(handle, |_, window, cx| {
            view_entity.update(cx, |view, _cx| {
                view.page = Page::Deploy;
                view.deploy_tab = DeployTab::History;
                view.projects = vec![project.clone()];
                view.project_loaded = true;
                view.release_tasks = vec![source.clone()];
                view.release_tasks_loading = false;
            });
            window.draw(cx).clear(cx);
        })
        .unwrap();
        let mut window = gpui_kit::VisualTestContext::from_window(handle, cx);
        let entry = window
            .debug_bounds("release-rollback-button")
            .expect("history rollback button rendered");
        window.simulate_click(entry.center(), Modifiers::default());
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| {
            assert!(window.has_active_dialog(cx));
            window.draw(cx).clear(cx);
        })
        .unwrap();
        assert_eq!(
            cx.read(|app| view_entity.read(app).rollback_confirmed_pair.clone()),
            Some((EXPECTED_SOURCE.into(), EXPECTED_TARGET.into()))
        );
        assert_eq!(
            crate::rollback_store::candidate(EXPECTED_SOURCE)
                .unwrap()
                .target_task_id,
            EXPECTED_TARGET
        );
        let confirm = window.debug_bounds("rollback-confirm-button").unwrap();
        window.simulate_click(confirm.center(), Modifiers::default());
        let started = std::time::Instant::now();
        let mut last_report = started;
        while cx.read(|app| view_entity.read(app).rollback_running.is_some()) {
            cx.executor()
                .advance_clock(std::time::Duration::from_millis(200));
            cx.run_until_parked();
            if last_report.elapsed() >= std::time::Duration::from_secs(60) {
                eprintln!(
                    "GUI rollback still active after {} seconds; waiting for settled result",
                    started.elapsed().as_secs()
                );
                last_report = std::time::Instant::now();
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let rollbacks = crate::rollback_store::recent(Some(project_id)).unwrap();
        assert_eq!(rollbacks.len(), prior_rollbacks + 1);
        assert_eq!(
            (rollbacks[0].phase.as_str(), rollbacks[0].outcome.as_str()),
            ("checked", "complete")
        );
        assert_eq!(
            (
                rollbacks[0].source_task_id.as_str(),
                rollbacks[0].target_task_id.as_str()
            ),
            (source.id.as_str(), target_id.as_str())
        );
        let after = crate::release::inspect_current_version(&source.id).unwrap();
        assert_eq!(after.matched_task_id.as_deref(), Some(target_id.as_str()));
        assert!(after.all_healthy);
    }

    #[test]
    fn theme_selection_cycles_and_restores() {
        assert_eq!(ThemeChoice::System.next(), ThemeChoice::Light);
        assert_eq!(ThemeChoice::Light.next(), ThemeChoice::Dark);
        assert_eq!(ThemeChoice::Dark.next(), ThemeChoice::System);
        for choice in ThemeChoice::ALL {
            assert_eq!(ThemeChoice::from_saved(Some(choice.saved().into())), choice);
        }
        assert_eq!(
            ThemeChoice::from_saved(Some("invalid".into())),
            ThemeChoice::System
        );
    }

    #[test]
    fn palettes_follow_webui_theme_tokens() {
        assert_eq!(Palette::LIGHT.background.l, 1.0);
        assert_eq!(Palette::LIGHT.border.l, 0.90);
        assert_eq!(Palette::DARK.background.l, 0.039);
        assert_eq!(Palette::DARK.border.l, 0.159);
    }
}
