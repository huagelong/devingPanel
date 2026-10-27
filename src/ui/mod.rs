mod pages;

use crate::compose::{self, Service, ServiceSummary};
use crate::docker::{self, DockerStatus};
use crate::env_store::{self, EnvInput, EnvRow};
use crate::host_store::{self, Host};
use crate::preferences;
use crate::project_store::{self, Project};
use crate::release;
use crate::release_materials;
use crate::release_store;
use crate::ssh::{self, TestResult};
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
    release_tasks: Vec<release_store::TaskSnapshot>,
    release_tasks_loading: bool,
    release_tasks_error: Option<String>,
    release_project_tasks: HashMap<i64, Vec<release_store::TaskSnapshot>>,
    release_project_loading: HashSet<i64>,
    release_project_errors: HashMap<i64, String>,
    release_observations: HashMap<String, Result<Vec<ssh::ObservedContainer>, String>>,
    release_image_checks: HashMap<String, Result<Vec<(String, bool)>, String>>,
    release_image_checking: HashSet<String>,
    previous_version_checks: HashMap<String, Result<release::PreviousVersionCheck, String>>,
    previous_version_checking: HashSet<String>,
    release_checking: HashSet<String>,
    release_resolving: HashSet<String>,
    release_material_checks: HashMap<String, Result<MaterialCheck, String>>,
    release_material_checking: HashSet<String>,
    release_recovery_busy: bool,
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
            let result = cx
                .background_executor()
                .spawn(async move { release_materials::recover_prepared() })
                .await;
            let _ = view.update(cx, |view, cx| {
                match result {
                    Ok(count) if count > 0 => view.refresh_release_tasks(None, cx),
                    Err(error) => view.release_tasks_error = Some(error),
                    _ => {}
                }
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
            release_tasks: Vec::new(),
            release_tasks_loading: false,
            release_tasks_error: None,
            release_project_tasks: HashMap::new(),
            release_project_loading: HashSet::new(),
            release_project_errors: HashMap::new(),
            release_observations: HashMap::new(),
            release_image_checks: HashMap::new(),
            release_image_checking: HashSet::new(),
            previous_version_checks: HashMap::new(),
            previous_version_checking: HashSet::new(),
            release_checking: HashSet::new(),
            release_resolving: HashSet::new(),
            release_material_checks: HashMap::new(),
            release_material_checking: HashSet::new(),
            release_recovery_busy: false,
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
