use super::*;
use crate::host_store::{HostUpdate, NewHost};
use crate::project_store::{BuildConfig, NewProject};
use chrono::{Local, NaiveDateTime};
use gpui_kit::component::Disableable;
use gpui_kit::component::WindowExt;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{Input, InputState, Textarea, TextareaState};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::popover::Popover;
use gpui_kit::component::progress::Progress;
use gpui_kit::component::radio::Radio;
use gpui_kit::component::scroll::ScrollableElement;
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::path::Path;
use std::process::{Command, Stdio};
use std::rc::Rc;
use std::sync::mpsc::{self, TryRecvError};
use std::time::Duration;

fn generate_release_tag(project: &Project) -> Result<String, String> {
    let timestamp = Local::now().format("%Y%m%d-%H%M%S").to_string();
    let mut tag = project.tag_rule.replace("{timestamp}", &timestamp);
    if tag.contains("{git-sha}") {
        let output = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&project.path)
            .stderr(Stdio::null())
            .output()
            .map_err(|_| "无法读取项目 Git SHA，请核对代码目录。")?;
        let sha = String::from_utf8_lossy(&output.stdout);
        let sha = sha.trim();
        if !output.status.success()
            || !matches!(sha.len(), 40 | 64)
            || !sha.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err("无法读取项目 Git SHA，请核对代码目录。".into());
        }
        tag = tag.replace("{git-sha}", sha);
    }
    Ok(tag)
}

fn history_tag(reference: &str) -> String {
    let tag = reference
        .rsplit_once(':')
        .map(|(_, tag)| tag)
        .unwrap_or(reference);
    tag.strip_suffix("-amd64")
        .or_else(|| tag.strip_suffix("-arm64"))
        .unwrap_or(tag)
        .to_owned()
}

fn history_duration(start: &str, finished: Option<&str>) -> String {
    let Some(finished) = finished else {
        return "—".into();
    };
    let parse = |value| NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S");
    match (parse(start), parse(finished)) {
        (Ok(start), Ok(finished)) => {
            let secs = (finished - start).num_seconds().max(0);
            format!("{}分{}秒", secs / 60, secs % 60)
        }
        _ => "—".into(),
    }
}

fn batch_phase_label(phase: &str) -> &'static str {
    match phase {
        "prepared" => "等待构建",
        "building" | "built" => "本机构建",
        "uploading" | "uploaded" => "镜像传输",
        "loading" | "loaded" => "镜像加载",
        "awaiting_deployment" | "staging" | "staged" => "准备 Compose",
        "validating" | "validated" => "校验 Compose",
        "deploying" => "启动服务",
        "checking" | "checked" => "健康检查",
        _ => "阶段待核实",
    }
}

fn resumable_upload_inspection(check: &release::UnknownUploadInspection) -> bool {
    check.containers.is_empty()
        && check
            .archives
            .iter()
            .all(|archive| match archive.actual_bytes {
                None => archive.state == "reserved",
                Some(size) if size < archive.expected_bytes => archive.state == "reserved",
                Some(size) if size == archive.expected_bytes => {
                    archive.digest_matches == Some(true)
                }
                _ => false,
            })
}

pub(super) fn next_confirmed_batch_host(batch: &batch_store::BatchRecord) -> Option<i64> {
    if batch.state != "running" {
        return None;
    }
    let index = batch
        .targets
        .iter()
        .position(|target| target.state == "waiting")?;
    if index == 0
        || batch.targets[index].release_task_id.is_some()
        || batch.targets[index + 1..]
            .iter()
            .any(|target| target.state != "waiting" || target.release_task_id.is_some())
        || batch.targets[..index].iter().any(|target| {
            let expected = match target.state.as_str() {
                "healthy" => "complete",
                "skipped" => "failed",
                _ => return true,
            };
            target.release_task_id.is_none() || target.task_outcome.as_deref() != Some(expected)
        })
    {
        return None;
    }
    Some(batch.targets[index].host.id)
}

enum DeployRunEvent {
    Stage(String),
    Transfer { sent: u64, total: u64 },
    Finished(Result<(String, usize), String>),
}

const MULTI_HOST_START_ENABLED: bool = false;

enum BatchStartEvent {
    Stage(String),
    Transfer { sent: u64, total: u64 },
    Finished(Result<stack_release::BatchProgress, stack_release::BatchRunError>),
}

fn transfer_progress(event: &str) -> Option<(u64, u64)> {
    let text = event.strip_prefix("镜像传输 ")?.strip_suffix(" 字节")?;
    let (sent, total) = text.split_once('/')?;
    let (sent, total) = (sent.parse::<u64>().ok()?, total.parse::<u64>().ok()?);
    (total > 0 && sent <= total).then_some((sent, total))
}

enum RollbackRunEvent {
    Stage(String),
    Finished(Result<String, String>),
}

fn safe_deploy_stage(event: &str) -> Option<String> {
    // Build output can contain source or credential data; only display coordinator-owned stages.
    const STAGES: &[&str] = &[
        "构建镜像：",
        "核对构建后 Compose：",
        "传输镜像：",
        "远端加载镜像：",
        "上传 Compose 和环境配置",
        "校验所有 Compose 服务",
        "启动全部 Compose 服务",
        "核对全部服务健康状态",
        "核对全部 Compose 服务健康状态",
    ];
    STAGES
        .iter()
        .find(|stage| event.starts_with(**stage))
        .map(|stage| {
            if stage.ends_with('：') {
                stage.trim_end_matches('：').to_owned()
            } else {
                (*stage).to_owned()
            }
        })
}

fn safe_rollback_stage(event: &str) -> Option<&'static str> {
    const STAGES: &[&str] = &[
        "恢复历史镜像：",
        "上传历史 Compose 和环境快照",
        "核对目标版本全部服务配置",
        "恢复上一健康版本",
        "核对恢复后的全部服务健康状态",
    ];
    STAGES
        .iter()
        .find(|stage| event.starts_with(**stage))
        .map(|stage| {
            if stage.ends_with('：') {
                "恢复历史镜像"
            } else {
                *stage
            }
        })
}

fn same_deploy_host(a: &Host, b: &Host) -> bool {
    a.id == b.id
        && a.name == b.name
        && a.address == b.address
        && a.port == b.port
        && a.username == b.username
        && a.group == b.group
        && a.auth == b.auth
        && a.trusted_fingerprint == b.trusted_fingerprint
}

fn display_path(path: &Path, narrow: bool, first_line: usize) -> String {
    let text = path.display().to_string();
    if !narrow {
        return text;
    }
    let mut result = String::new();
    let mut line = 0;
    let mut limit = first_line;
    for segment in text.split_inclusive(['\\', '/']) {
        if line > 0 && line + segment.chars().count() > limit {
            result.push('\n');
            line = 0;
            limit = 30;
        }
        for ch in segment.chars() {
            if line >= limit {
                result.push('\n');
                line = 0;
                limit = 30;
            }
            result.push(ch);
            line += 1;
        }
    }
    result
}

struct DialogSave {
    seq: u64,
    submitting: Rc<Cell<bool>>,
    alive: Rc<Cell<bool>>,
}

#[derive(Clone)]
struct FingerprintChallenge {
    id: i64,
    address: String,
    port: u16,
    previous: Option<String>,
    fingerprint: String,
}

struct LogSheetState {
    lines: VecDeque<String>,
    follow: bool,
    running: bool,
    message: Option<String>,
}

impl LogSheetState {
    fn new() -> Self {
        Self {
            lines: VecDeque::new(),
            follow: true,
            running: true,
            message: None,
        }
    }

    fn push_line(&mut self, line: String) {
        if self.lines.len() == 1000 {
            self.lines.pop_front();
        }
        self.lines.push_back(line);
    }
}

impl AppView {
    fn panel(
        &self,
        title: &'static str,
        description: &'static str,
        body: impl IntoElement,
    ) -> impl IntoElement {
        div()
            .w_full()
            .min_w_0()
            .border_1()
            .border_color(self.palette().border)
            .rounded_lg()
            .flex()
            .flex_col()
            .child(
                div()
                    .p_6()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .text_size(px(18.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(title),
                    )
                    .child(
                        div()
                            .w_full()
                            .min_w_0()
                            .whitespace_normal()
                            .text_size(px(14.))
                            .text_color(self.palette().muted)
                            .child(description),
                    ),
            )
            .child(body)
    }

    fn page_head(
        &self,
        title: impl Into<SharedString>,
        action: impl IntoElement,
    ) -> impl IntoElement {
        div()
            .flex()
            .items_center()
            .justify_between()
            .gap_4()
            .child(
                div()
                    .text_size(px(24.))
                    .font_weight(FontWeight::BOLD)
                    .child(title.into()),
            )
            .child(action)
    }

    fn empty_card(&self, title: &'static str, icon: assets::IconName) -> impl IntoElement {
        div()
            .w_full()
            .min_h(px(164.))
            .border_1()
            .border_color(self.palette().border)
            .rounded_lg()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_4()
            .child(Icon::new(icon).size(px(24.)))
            .child(title)
    }

    fn tab(
        &self,
        id: &'static str,
        title: &'static str,
        selected: bool,
        cx: &mut Context<Self>,
        on_click: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) -> AnyElement {
        div()
            .id(id)
            .h(px(32.))
            .px_3()
            .flex()
            .items_center()
            .rounded_md()
            .when(selected, |el| el.bg(self.palette().background))
            .text_color(if selected {
                self.palette().foreground
            } else {
                self.palette().muted
            })
            .when(selected, |el| el.font_weight(FontWeight::SEMIBOLD))
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, window, cx| on_click(this, window, cx)))
            .child(title)
            .into_any_element()
    }

    fn form_field(label: &'static str, input: impl IntoElement) -> impl IntoElement {
        div()
            .w_full()
            .flex()
            .flex_col()
            .gap_2()
            .child(label)
            .child(input)
    }

    fn open_host_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.dialog_seq = self.dialog_seq.wrapping_add(1);
        let dialog_seq = self.dialog_seq;
        let muted = self.palette().muted;
        let name = cx.new(|cx| InputState::new(window, cx).placeholder("例如 web-1"));
        let address = cx.new(|cx| InputState::new(window, cx).placeholder("IP 或域名"));
        let port = cx.new(|cx| InputState::new(window, cx).default_value("22"));
        let user = cx.new(|cx| InputState::new(window, cx).default_value("root"));
        let group = cx.new(|cx| InputState::new(window, cx).placeholder("可选"));
        let password = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("SSH 密码")
                .masked(true)
        });
        let private_key = cx.new(|cx| TextareaState::new(window, cx).placeholder("粘贴私钥内容"));
        let use_key = Rc::new(Cell::new(false));
        let view = cx.weak_entity();
        let submitting = Rc::new(Cell::new(false));
        let alive = Rc::new(Cell::new(true));
        window.open_dialog(cx, move |dialog, _, _| {
            let save_name = name.clone();
            let save_address = address.clone();
            let save_port = port.clone();
            let save_user = user.clone();
            let save_group = group.clone();
            let save_password = password.clone();
            let save_private_key = private_key.clone();
            let save_use_key = use_key.clone();
            let password_radio = use_key.clone();
            let key_radio = use_key.clone();
            let save_view = view.clone();
            let save_submitting = submitting.clone();
            let save_alive = alive.clone();
            let cancel_submitting = submitting.clone();
            let cancel_alive = alive.clone();
            let close_alive = alive.clone();
            dialog
                .title("添加主机")
                .w(px(512.))
                .close_button(!submitting.get())
                .overlay_closable(!submitting.get())
                .keyboard(!submitting.get())
                .on_close(move |_, _, _| close_alive.set(false))
                .child(
                    div()
                        .w_full()
                        .flex()
                        .flex_col()
                        .gap_4()
                        .child(Self::form_field(
                            "别名",
                            Input::new(&name).w_full().disabled(submitting.get()),
                        ))
                        .child(Self::form_field(
                            "地址",
                            Input::new(&address).w_full().disabled(submitting.get()),
                        ))
                        .child(
                            div()
                                .flex()
                                .gap_4()
                                .child(div().flex_1().child(Self::form_field(
                                    "SSH 端口",
                                    Input::new(&port).w_full().disabled(submitting.get()),
                                )))
                                .child(div().flex_1().child(Self::form_field(
                                    "用户名",
                                    Input::new(&user).w_full().disabled(submitting.get()),
                                ))),
                        )
                        .child(Self::form_field(
                            "认证方式",
                            div()
                                .flex()
                                .gap_4()
                                .child(
                                    Radio::new("host-password-auth")
                                        .label("密码")
                                        .checked(!use_key.get())
                                        .disabled(submitting.get())
                                        .on_click(move |_, window, _| {
                                            password_radio.set(false);
                                            window.refresh();
                                        }),
                                )
                                .child(
                                    Radio::new("host-key-auth")
                                        .label("私钥")
                                        .checked(use_key.get())
                                        .disabled(submitting.get())
                                        .on_click(move |_, window, _| {
                                            key_radio.set(true);
                                            window.refresh();
                                        }),
                                ),
                        ))
                        .child(if use_key.get() {
                            Self::form_field(
                                "私钥",
                                Textarea::new(&private_key)
                                    .h(px(120.))
                                    .disabled(submitting.get()),
                            )
                            .into_any_element()
                        } else {
                            Self::form_field(
                                "密码",
                                Input::new(&password).w_full().disabled(submitting.get()),
                            )
                            .into_any_element()
                        })
                        .child(Self::form_field(
                            "分组",
                            Input::new(&group).w_full().disabled(submitting.get()),
                        ))
                        .child(div().text_size(px(12.)).text_color(muted).child(
                            "密码或私钥只写入系统凭据管理器；保存后从主机列表测试 SSH 和 Docker。",
                        )),
                )
                .footer(
                    div()
                        .flex()
                        .justify_end()
                        .gap_2()
                        .child(
                            Button::new("host-test")
                                .outline()
                                .label("测试连接")
                                .disabled(true),
                        )
                        .child(
                            Button::new("host-save")
                                .primary()
                                .label("保存")
                                .disabled(submitting.get())
                                .on_click(move |_, window, cx| {
                                    if save_submitting.replace(true) {
                                        return;
                                    }
                                    window.refresh();
                                    let input = NewHost {
                                        name: save_name.read(cx).value().to_string(),
                                        address: save_address.read(cx).value().to_string(),
                                        port: save_port.read(cx).value().to_string(),
                                        username: save_user.read(cx).value().to_string(),
                                        group: save_group.read(cx).value().to_string(),
                                        auth: if save_use_key.get() {
                                            "key"
                                        } else {
                                            "password"
                                        }
                                        .into(),
                                        credential: if save_use_key.get() {
                                            save_private_key.read(cx).value().to_string()
                                        } else {
                                            save_password.read(cx).value().to_string()
                                        },
                                    };
                                    if save_view
                                        .update(cx, |view, cx| {
                                            view.create_host(
                                                input,
                                                dialog_seq,
                                                save_submitting.clone(),
                                                save_alive.clone(),
                                                window,
                                                cx,
                                            );
                                        })
                                        .is_err()
                                    {
                                        save_submitting.set(false);
                                    }
                                }),
                        )
                        .child(
                            Button::new("host-cancel")
                                .label("取消")
                                .disabled(submitting.get())
                                .on_click(move |_, window, cx| {
                                    if !cancel_submitting.get() {
                                        cancel_alive.set(false);
                                        window.close_dialog(cx);
                                    }
                                }),
                        ),
                )
        });
    }

    fn create_host(
        &mut self,
        input: NewHost,
        dialog_seq: u64,
        submitting: Rc<Cell<bool>>,
        alive: Rc<Cell<bool>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.spawn_in(window, async move |view, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { host_store::create(input) })
                .await;
            submitting.set(false);
            let _ = cx.update(|window, app| match result {
                Ok(host) => {
                    let close_dialog = view
                        .update(app, |view, cx| {
                            let active = alive.get() && view.dialog_seq == dialog_seq;
                            view.hosts.insert(0, host);
                            view.host_loaded = true;
                            view.host_error = None;
                            view.reload_project_configs(window, cx);
                            if active {
                                view.page = Page::Hosts;
                                view.host_id = None;
                            }
                            cx.notify();
                            active
                        })
                        .unwrap_or(false);
                    if close_dialog {
                        window.close_dialog(app);
                    }
                    window.push_notification(
                        Notification::success("主机已保存；请从主机列表测试 SSH 和 Docker"),
                        app,
                    );
                }
                Err(error) => {
                    window.push_notification(Notification::error(error), app);
                    window.refresh();
                }
            });
        })
        .detach();
    }

    fn open_edit_host_dialog(&mut self, id: i64, window: &mut Window, cx: &mut Context<Self>) {
        if self.host_installing == Some(id) || self.host_mirror_configuring == Some(id) {
            window.push_notification(
                Notification::warning("Docker 配置操作期间不可编辑主机连接配置。"),
                cx,
            );
            return;
        }
        let Some(host) = self.hosts.iter().find(|host| host.id == id).cloned() else {
            return;
        };
        self.dialog_seq = self.dialog_seq.wrapping_add(1);
        let dialog_seq = self.dialog_seq;
        let name = cx.new(|cx| InputState::new(window, cx).default_value(host.name.clone()));
        let address = cx.new(|cx| InputState::new(window, cx).default_value(host.address.clone()));
        let port = cx.new(|cx| InputState::new(window, cx).default_value(host.port.to_string()));
        let user = cx.new(|cx| InputState::new(window, cx).default_value(host.username.clone()));
        let group = cx.new(|cx| InputState::new(window, cx).default_value(host.group.clone()));
        let password = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("已安全保存，留空不修改")
                .masked(true)
        });
        let private_key = cx.new(|cx| {
            TextareaState::new(window, cx).placeholder("已安全保存，留空不修改；粘贴新私钥")
        });
        let use_key = Rc::new(Cell::new(host.auth == "key"));
        let view = cx.weak_entity();
        let submitting = Rc::new(Cell::new(false));
        let alive = Rc::new(Cell::new(true));
        window.open_dialog(cx, move |dialog, _, _| {
            let save_name = name.clone();
            let save_address = address.clone();
            let save_port = port.clone();
            let save_user = user.clone();
            let save_group = group.clone();
            let save_password = password.clone();
            let save_private_key = private_key.clone();
            let save_use_key = use_key.clone();
            let password_radio = use_key.clone();
            let key_radio = use_key.clone();
            let save_view = view.clone();
            let save_submitting = submitting.clone();
            let save_alive = alive.clone();
            let cancel_submitting = submitting.clone();
            let cancel_alive = alive.clone();
            let close_alive = alive.clone();
            dialog
                .title("编辑主机")
                .w(px(512.))
                .close_button(!submitting.get())
                .overlay_closable(!submitting.get())
                .keyboard(!submitting.get())
                .on_close(move |_, _, _| close_alive.set(false))
                .child(
                    div()
                        .w_full()
                        .flex()
                        .flex_col()
                        .gap_4()
                        .child(Self::form_field(
                            "别名",
                            Input::new(&name).w_full().disabled(submitting.get()),
                        ))
                        .child(Self::form_field(
                            "地址",
                            Input::new(&address).w_full().disabled(submitting.get()),
                        ))
                        .child(
                            div()
                                .flex()
                                .gap_4()
                                .child(div().flex_1().child(Self::form_field(
                                    "SSH 端口",
                                    Input::new(&port).w_full().disabled(submitting.get()),
                                )))
                                .child(div().flex_1().child(Self::form_field(
                                    "用户名",
                                    Input::new(&user).w_full().disabled(submitting.get()),
                                ))),
                        )
                        .child(Self::form_field(
                            "认证方式",
                            div().flex().gap_4()
                                .child(Radio::new("host-edit-password-auth").label("密码")
                                    .checked(!use_key.get()).disabled(submitting.get())
                                    .on_click(move |_, window, _| { password_radio.set(false); window.refresh(); }))
                                .child(Radio::new("host-edit-key-auth").label("私钥")
                                    .checked(use_key.get()).disabled(submitting.get())
                                    .on_click(move |_, window, _| { key_radio.set(true); window.refresh(); })),
                        ))
                        .child(if use_key.get() {
                            Self::form_field("私钥", Textarea::new(&private_key).h(px(120.)).disabled(submitting.get())).into_any_element()
                        } else {
                            Self::form_field("密码", Input::new(&password).w_full().mask_toggle().disabled(submitting.get())).into_any_element()
                        })
                        .child(Self::form_field(
                            "分组",
                            Input::new(&group).w_full().disabled(submitting.get()),
                        ))
                        .child(
                            div().text_size(px(12.)).child(
                                "同方式留空保留已保存凭据；切换认证方式须填写新凭据。修改地址或端口将清除旧指纹信任。",
                            ),
                        ),
                )
                .footer(
                    div()
                        .flex()
                        .justify_end()
                        .gap_2()
                        .child(
                            Button::new("host-edit-cancel")
                                .label("取消")
                                .disabled(submitting.get())
                                .on_click(move |_, window, cx| {
                                    if !cancel_submitting.get() {
                                        cancel_alive.set(false);
                                        window.close_dialog(cx);
                                    }
                                }),
                        )
                        .child(
                            Button::new("host-edit-save")
                                .primary()
                                .label("保存")
                                .disabled(submitting.get())
                                .on_click(move |_, window, cx| {
                                    if save_submitting.replace(true) {
                                        return;
                                    }
                                    window.refresh();
                                    let input = HostUpdate {
                                        name: save_name.read(cx).value().to_string(),
                                        address: save_address.read(cx).value().to_string(),
                                        port: save_port.read(cx).value().to_string(),
                                        username: save_user.read(cx).value().to_string(),
                                        group: save_group.read(cx).value().to_string(),
                                        auth: if save_use_key.get() { "key" } else { "password" }.into(),
                                        credential: {
                                            let value = if save_use_key.get() {
                                                save_private_key.read(cx).value().to_string()
                                            } else {
                                                save_password.read(cx).value().to_string()
                                            };
                                            (!value.is_empty()).then_some(value)
                                        },
                                    };
                                    if save_view
                                        .update(cx, |view, cx| {
                                            view.update_host(
                                                id,
                                                input,
                                                DialogSave {
                                                    seq: dialog_seq,
                                                    submitting: save_submitting.clone(),
                                                    alive: save_alive.clone(),
                                                },
                                                window,
                                                cx,
                                            );
                                        })
                                        .is_err()
                                    {
                                        save_submitting.set(false);
                                    }
                                }),
                        ),
                )
        });
    }

    fn update_host(
        &mut self,
        id: i64,
        input: HostUpdate,
        dialog: DialogSave,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.host_installing == Some(id) || self.host_mirror_configuring == Some(id) {
            dialog.submitting.set(false);
            window.push_notification(
                Notification::warning("Docker 配置操作期间不可修改主机。"),
                cx,
            );
            return;
        }
        cx.spawn_in(window, async move |view, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { host_store::update(id, input) })
                .await;
            dialog.submitting.set(false);
            let _ = cx.update(|window, app| match result {
                Ok((host, warning)) => {
                    let close = view
                        .update(app, |view, cx| {
                            if let Some(existing) =
                                view.hosts.iter_mut().find(|existing| existing.id == id)
                            {
                                *existing = host;
                            }
                            view.dashboard_metrics.remove(&id);
                            if view.page == Page::Deploy && view.deploy_host_ids.contains(&id) {
                                view.refresh_deploy_environment(cx);
                            }
                            view.host_tests.remove(&id);
                            if view.host_testing == Some(id) {
                                view.host_test_generation =
                                    view.host_test_generation.wrapping_add(1);
                                view.host_testing = None;
                            }
                            view.reload_project_configs(window, cx);
                            cx.notify();
                            dialog.alive.get() && view.dialog_seq == dialog.seq
                        })
                        .unwrap_or(false);
                    if close {
                        window.close_dialog(app);
                    }
                    window.push_notification(
                        match warning {
                            Some(warning) => {
                                Notification::warning(format!("主机配置已更新；{warning}"))
                            }
                            None => Notification::success("主机配置已更新，连接状态待重新核实"),
                        },
                        app,
                    );
                }
                Err(error) => {
                    window.push_notification(Notification::error(error), app);
                    window.refresh();
                }
            });
        })
        .detach();
    }

    fn test_host(&mut self, id: i64, window: &mut Window, cx: &mut Context<Self>) {
        if self.host_testing.is_some()
            || self.host_installing.is_some()
            || self.host_mirror_configuring.is_some()
        {
            return;
        }
        self.host_testing = Some(id);
        self.host_test_generation = self.host_test_generation.wrapping_add(1);
        let generation = self.host_test_generation;
        self.host_tests.insert(id, HostTestState::Checking);
        cx.notify();
        cx.spawn_in(window, async move |view, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { ssh::test_host(id) })
                .await;
            let _ = cx.update(|window, app| {
                let challenge = view
                    .update(app, |view, cx| {
                        if view.host_test_generation != generation {
                            return None;
                        }
                        view.host_testing = None;
                        let challenge = match result {
                            Ok(TestResult::NeedsTrust {
                                address,
                                port,
                                previous,
                                fingerprint,
                            }) => {
                                view.host_tests.insert(id, HostTestState::NeedsTrust);
                                Some(FingerprintChallenge {
                                    id,
                                    address,
                                    port,
                                    previous,
                                    fingerprint,
                                })
                            }
                            Ok(TestResult::Connected {
                                docker_version,
                                compose_version,
                                system_kernel,
                                architecture,
                                disk_available_kib,
                                sftp_v3_handshake,
                            }) => {
                                let checked_at = Instant::now();
                                view.host_tests.insert(
                                    id,
                                    HostTestState::Connected {
                                        docker_version,
                                        compose_version,
                                        system_kernel,
                                        architecture,
                                        disk_available_kib,
                                        sftp_v3_handshake,
                                        checked_at,
                                        stale: false,
                                    },
                                );
                                if view.page == Page::Hosts
                                    && view.host_id == Some(id)
                                    && view.host_tab == HostTab::Monitor
                                {
                                    view.start_host_monitor(id, cx);
                                }
                                cx.spawn(async move |view, cx| {
                                    cx.background_executor()
                                        .timer(Duration::from_secs(60))
                                        .await;
                                    let _ = view.update(cx, |view, cx| {
                                        if let Some(HostTestState::Connected {
                                            checked_at: current,
                                            stale,
                                            ..
                                        }) = view.host_tests.get_mut(&id)
                                            && *current == checked_at
                                        {
                                            *stale = true;
                                            cx.notify();
                                        }
                                    });
                                })
                                .detach();
                                None
                            }
                            Ok(TestResult::DockerMissing {
                                os_id,
                                os_version,
                                system_kernel,
                                architecture,
                            }) => {
                                view.host_tests.insert(
                                    id,
                                    HostTestState::DockerMissing {
                                        os_id,
                                        os_version,
                                        system_kernel,
                                        architecture,
                                    },
                                );
                                if view.page == Page::Hosts && view.host_id == Some(id) {
                                    view.host_tab = HostTab::Info;
                                    if let Some(cancel) = view.host_monitor_cancel.take() {
                                        cancel.store(true, Ordering::Relaxed);
                                    }
                                }
                                None
                            }
                            Err(error) => {
                                view.host_tests.insert(id, HostTestState::Failed(error));
                                None
                            }
                        };
                        cx.notify();
                        challenge
                    })
                    .ok()
                    .flatten();
                if let Some(challenge) = challenge {
                    let _ = view.update(app, |view, cx| {
                        if view.page == Page::Hosts && view.hosts.iter().any(|host| host.id == id) {
                            view.confirm_host_fingerprint(challenge, window, cx);
                        }
                    });
                }
            });
        })
        .detach();
    }

    fn confirm_docker_install(&mut self, id: i64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(host) = self.hosts.iter().find(|host| host.id == id) else {
            return;
        };
        if host.username != "root"
            || !matches!(
                self.host_tests.get(&id),
                Some(HostTestState::DockerMissing { os_id, os_version, architecture: Some(platform), .. })
                    if os_id == "debian" && os_version == "13" && platform == "linux/amd64"
            )
        {
            return;
        }
        let name = host.name.clone();
        let view = cx.weak_entity();
        window.open_dialog(cx, move |dialog, _, _| {
            let tencent_view = view.clone();
            let official_view = view.clone();
            dialog
                .title("一键安装 Docker")
                .w(px(560.))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_3()
                        .child(format!("目标主机：{name} · Debian 13 / amd64 · root"))
                        .child("将添加 Docker CE apt 源与签名密钥，安装 Engine、CLI、containerd、Buildx、Compose，并启动 Docker 服务。可能改变系统软件包与网络配置。")
                        .child("仓库密钥须匹配 Docker 官方指纹 9DC8 5822 9FC7 DD38 854A E2D8 8D81 803C 0EBF CD88；不执行 curl | sh，也不设置未知的镜像加速器。")
                        .child("安装源：mirrors.cloud.tencent.com（腾讯云，国内推荐）或 download.docker.com（官方）。若源不可用会停止，不自动切换。请确认 VPS 有足够磁盘空间并允许 apt 安装。"),
                )
                .footer(
                    div()
                        .flex()
                        .justify_end()
                        .gap_2()
                        .child(Button::new("docker-install-cancel").label("取消").on_click(
                            |_, window, cx| window.close_dialog(cx),
                        ))
                        .child(Button::new("docker-install-official").label("使用官方源安装").on_click(
                            move |_, window, cx| {
                                window.close_dialog(cx);
                                let _ = official_view.update(cx, |view, cx| {
                                    view.start_docker_install(id, ssh::DockerInstallSource::Official, window, cx)
                                });
                            },
                        ))
                        .child(Button::new("docker-install-tencent").primary().label("使用腾讯云镜像安装").on_click(
                            move |_, window, cx| {
                                window.close_dialog(cx);
                                let _ = tencent_view.update(cx, |view, cx| {
                                    view.start_docker_install(id, ssh::DockerInstallSource::Tencent, window, cx)
                                });
                            },
                        )),
                )
        });
    }

    fn start_docker_install(
        &mut self,
        id: i64,
        source: ssh::DockerInstallSource,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.host_installing.is_some()
            || self.host_mirror_configuring.is_some()
            || !matches!(
                self.host_tests.get(&id),
                Some(HostTestState::DockerMissing { .. })
            )
        {
            return;
        }
        let Some(host) = self.hosts.iter().find(|host| host.id == id).cloned() else {
            return;
        };
        self.host_installing = Some(id);
        self.host_install_stage = Some("准备连接并核对系统…".into());
        self.host_install_error = None;
        cx.notify();
        let stage = Arc::new(Mutex::new(String::new()));
        let worker_stage = stage.clone();
        cx.spawn(async move |view, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(500))
                    .await;
                let next = stage
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .clone();
                let active = view
                    .update(cx, |view, cx| {
                        if view.host_installing != Some(id) {
                            return false;
                        }
                        if !next.is_empty() && view.host_install_stage.as_deref() != Some(&next) {
                            view.host_install_stage = Some(next);
                            cx.notify();
                        }
                        true
                    })
                    .unwrap_or(false);
                if !active {
                    break;
                }
            }
        })
        .detach();
        cx.spawn_in(window, async move |view, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    ssh::install_docker(host, source, |phase| {
                        *worker_stage
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner()) = phase.into();
                    })
                })
                .await;
            let _ = cx.update(|window, app| {
                let _ = view.update(app, |view, cx| {
                    view.host_installing = None;
                    view.host_install_stage = None;
                    if let Err(error) = &result {
                        view.host_install_error = Some((id, error.clone()));
                    }
                    cx.notify();
                });
                match result {
                    Ok(()) => {
                        window.push_notification(
                            Notification::success("Docker 已安装；正在重新测试连接"),
                            app,
                        );
                        let _ = view.update(app, |view, cx| view.test_host(id, window, cx));
                    }
                    Err(error) => window.push_notification(Notification::error(error), app),
                }
            });
        })
        .detach();
    }

    fn confirm_mirror_config(&mut self, id: i64, window: &mut Window, cx: &mut Context<Self>) {
        if self.host_installing.is_some() || self.host_mirror_configuring.is_some() {
            return;
        }
        let Some(host) = self.hosts.iter().find(|host| host.id == id) else {
            return;
        };
        if host.username != "root"
            || host.trusted_fingerprint.is_none()
            || !matches!(
                self.host_tests.get(&id),
                Some(HostTestState::Connected { stale: false, .. })
            )
        {
            return;
        }
        let view = cx.weak_entity();
        window.open_dialog(cx, move |dialog, _, _| {
            let confirm_view = view.clone();
            dialog
                .title("配置 Docker Hub 镜像源")
                .w(px(560.))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_3()
                        .child("将保留 daemon.json 中其他设置，仅将 registry-mirrors 设置为：")
                        .child("https://docker.1panel.live")
                        .child("https://docker.m.daocloud.io")
                        .child("操作会重启 Docker；如果有运行中的容器，将阻断而不修改。公共镜像源可能记录镜像名称，生产镜像请优先使用受控 Registry 并固定 digest。"),
                )
                .footer(
                    div()
                        .flex()
                        .justify_end()
                        .gap_2()
                        .child(Button::new("mirror-cancel").label("取消").on_click(
                            |_, window, cx| window.close_dialog(cx),
                        ))
                        .child(Button::new("mirror-confirm").primary().label("确认配置").on_click(
                            move |_, window, cx| {
                                window.close_dialog(cx);
                                let _ = confirm_view.update(cx, |view, cx| {
                                    view.start_mirror_config(id, window, cx)
                                });
                            },
                        )),
                )
        });
    }

    fn start_mirror_config(&mut self, id: i64, window: &mut Window, cx: &mut Context<Self>) {
        if self.host_installing.is_some() || self.host_mirror_configuring.is_some() {
            return;
        }
        let Some(host) = self.hosts.iter().find(|host| host.id == id).cloned() else {
            return;
        };
        self.host_mirror_configuring = Some(id);
        self.host_mirror_error = None;
        cx.notify();
        cx.spawn_in(window, async move |view, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { ssh::configure_registry_mirrors(host, |_| {}) })
                .await;
            let _ = cx.update(|window, app| {
                let _ = view.update(app, |view, cx| {
                    view.host_mirror_configuring = None;
                    if let Err(error) = &result {
                        view.host_mirror_error = Some((id, error.clone()));
                    }
                    cx.notify();
                });
                match result {
                    Ok(ssh::MirrorConfigResult::AlreadyConfigured) => window.push_notification(
                        Notification::success("Docker 镜像源已一致，无需重启"),
                        app,
                    ),
                    Ok(ssh::MirrorConfigResult::Updated) => {
                        window.push_notification(
                            Notification::success("Docker 镜像源已配置并验证"),
                            app,
                        );
                        let _ = view.update(app, |view, cx| view.test_host(id, window, cx));
                    }
                    Err(error) => window.push_notification(Notification::error(error), app),
                }
            });
        })
        .detach();
    }

    fn confirm_host_fingerprint(
        &mut self,
        challenge: FingerprintChallenge,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.dialog_seq = self.dialog_seq.wrapping_add(1);
        let seq = self.dialog_seq;
        let view = cx.weak_entity();
        let submitting = Rc::new(Cell::new(false));
        let alive = Rc::new(Cell::new(true));
        window.open_dialog(cx, move |dialog, _, _| {
            let save_view = view.clone();
            let save_challenge = challenge.clone();
            let save_submitting = submitting.clone();
            let save_alive = alive.clone();
            let cancel_alive = alive.clone();
            let close_alive = alive.clone();
            dialog
                .title(if challenge.previous.is_some() {
                    "主机指纹变化：连接已阻断"
                } else {
                    "首次连接：核对 SSH 主机指纹"
                })
                .w(px(560.))
                .close_button(!submitting.get())
                .overlay_closable(!submitting.get())
                .keyboard(!submitting.get())
                .on_close(move |_, _, _| close_alive.set(false))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_3()
                        .child(format!(
                            "连接目标：{}:{}",
                            challenge.address, challenge.port
                        ))
                        .when_some(challenge.previous.as_ref(), |el, old| {
                            el.child(format!("旧指纹：{old}"))
                        })
                        .child(
                            div()
                                .font_family(mono_font())
                                .child(format!("新指纹：{}", challenge.fingerprint)),
                        )
                        .child("请通过独立渠道核对服务器指纹。确认后才会保存信任并重新测试连接。"),
                )
                .footer(
                    div()
                        .flex()
                        .justify_end()
                        .gap_2()
                        .child(
                            Button::new("host-trust-cancel")
                                .label("取消")
                                .disabled(submitting.get())
                                .on_click(move |_, window, cx| {
                                    cancel_alive.set(false);
                                    window.close_dialog(cx);
                                }),
                        )
                        .child(
                            Button::new("host-trust-confirm")
                                .primary()
                                .label(if challenge.previous.is_some() {
                                    "更新可信指纹"
                                } else {
                                    "确认指纹"
                                })
                                .disabled(submitting.get())
                                .on_click(move |_, window, cx| {
                                    if save_submitting.replace(true) {
                                        return;
                                    }
                                    window.refresh();
                                    if save_view
                                        .update(cx, |view, cx| {
                                            view.save_host_fingerprint(
                                                save_challenge.clone(),
                                                DialogSave {
                                                    seq,
                                                    submitting: save_submitting.clone(),
                                                    alive: save_alive.clone(),
                                                },
                                                window,
                                                cx,
                                            );
                                        })
                                        .is_err()
                                    {
                                        save_submitting.set(false);
                                    }
                                }),
                        ),
                )
        });
    }

    fn save_host_fingerprint(
        &mut self,
        challenge: FingerprintChallenge,
        dialog: DialogSave,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.spawn_in(window, async move |view, cx| {
            let id = challenge.id;
            let result = cx
                .background_executor()
                .spawn(async move {
                    host_store::trust_fingerprint(
                        id,
                        &challenge.address,
                        challenge.port,
                        challenge.previous.as_deref(),
                        &challenge.fingerprint,
                    )?;
                    host_store::list()
                })
                .await;
            dialog.submitting.set(false);
            let _ = cx.update(|window, app| {
                let active = view
                    .update(app, |view, cx| {
                        let active = dialog.alive.get() && view.dialog_seq == dialog.seq;
                        match &result {
                            Ok((hosts, warning)) => {
                                view.hosts = hosts.clone();
                                view.credential_warning = warning.clone();
                            }
                            Err(_) => {
                                view.host_tests.insert(
                                    id,
                                    HostTestState::Failed("指纹保存失败，请重新测试连接。".into()),
                                );
                            }
                        }
                        cx.notify();
                        active
                    })
                    .unwrap_or(false);
                if active {
                    window.close_dialog(app);
                }
                match result {
                    Ok(_) => {
                        let _ = view.update(app, |view, cx| view.test_host(id, window, cx));
                    }
                    Err(error) => window.push_notification(Notification::error(error), app),
                }
            });
        })
        .detach();
    }

    fn confirm_host_delete(&mut self, id: i64, window: &mut Window, cx: &mut Context<Self>) {
        if self.host_installing == Some(id) || self.host_mirror_configuring == Some(id) {
            window.push_notification(
                Notification::warning("Docker 配置操作期间不可删除主机。"),
                cx,
            );
            return;
        }
        let Some(host) = self.hosts.iter().find(|host| host.id == id) else {
            return;
        };
        let name = host.name.clone();
        self.dialog_seq = self.dialog_seq.wrapping_add(1);
        let dialog_seq = self.dialog_seq;
        let view = cx.weak_entity();
        let submitting = Rc::new(Cell::new(false));
        let alive = Rc::new(Cell::new(true));
        window.open_dialog(cx, move |dialog, _, _| {
            let save_view = view.clone();
            let save_submitting = submitting.clone();
            let save_alive = alive.clone();
            let cancel_submitting = submitting.clone();
            let cancel_alive = alive.clone();
            let close_alive = alive.clone();
            dialog
                .title("删除主机")
                .w(px(480.))
                .close_button(!submitting.get())
                .overlay_closable(!submitting.get())
                .keyboard(!submitting.get())
                .on_close(move |_, _, _| close_alive.set(false))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_3()
                        .child(format!("确定删除「{name}」？"))
                        .child("仅删除本机配置与凭据，不影响 VPS 上的容器、镜像或数据。"),
                )
                .footer(
                    div()
                        .flex()
                        .justify_end()
                        .gap_2()
                        .child(
                            Button::new("delete-cancel")
                                .label("取消")
                                .disabled(submitting.get())
                                .on_click(move |_, window, cx| {
                                    if !cancel_submitting.get() {
                                        cancel_alive.set(false);
                                        window.close_dialog(cx);
                                    }
                                }),
                        )
                        .child(
                            Button::new("delete-confirm")
                                .danger()
                                .label("确认删除")
                                .disabled(submitting.get())
                                .on_click(move |_, window, cx| {
                                    if save_submitting.replace(true) {
                                        return;
                                    }
                                    window.refresh();
                                    if save_view
                                        .update(cx, |view, cx| {
                                            view.delete_host(
                                                id,
                                                dialog_seq,
                                                save_submitting.clone(),
                                                save_alive.clone(),
                                                window,
                                                cx,
                                            );
                                        })
                                        .is_err()
                                    {
                                        save_submitting.set(false);
                                    }
                                }),
                        ),
                )
        });
    }

    fn delete_host(
        &mut self,
        id: i64,
        dialog_seq: u64,
        submitting: Rc<Cell<bool>>,
        alive: Rc<Cell<bool>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.host_installing == Some(id) || self.host_mirror_configuring == Some(id) {
            submitting.set(false);
            window.push_notification(
                Notification::warning("Docker 配置操作期间不可删除主机。"),
                cx,
            );
            return;
        }
        cx.spawn_in(window, async move |view, cx| {
            let (result, fresh, projects) = cx
                .background_executor()
                .spawn(async move {
                    let result = host_store::delete(id);
                    let fresh = host_store::list();
                    let projects = result.as_ref().ok().map(|_| project_store::list());
                    (result, fresh, projects)
                })
                .await;
            submitting.set(false);
            let _ = cx.update(|window, app| {
                let close = view
                    .update(app, |view, cx| {
                        match fresh {
                            Ok((hosts, warning)) => {
                                view.hosts = hosts;
                                view.credential_warning = warning;
                                view.host_error = None;
                            }
                            Err(error) => view.host_error = Some(error),
                        }
                        if let Some(projects) = projects {
                            match projects {
                                Ok(projects) => {
                                    view.projects = projects;
                                    if let Some(form) = view.target_form.as_mut() {
                                        form.host_ids.retain(|host_id| *host_id != id);
                                        if !view.hosts.iter().any(|host| host.group == form.group) {
                                            form.group = view
                                                .hosts
                                                .first()
                                                .map(|host| host.group.clone())
                                                .unwrap_or_default();
                                        }
                                    }
                                }
                                Err(error) => view.project_error = Some(error),
                            }
                        }
                        if view.host_id == Some(id) && !view.hosts.iter().any(|host| host.id == id)
                        {
                            view.host_id = None;
                        }
                        if !view.hosts.iter().any(|host| host.id == id) {
                            view.host_tests.remove(&id);
                            if view.host_testing == Some(id) {
                                view.host_test_generation =
                                    view.host_test_generation.wrapping_add(1);
                                view.host_testing = None;
                            }
                        }
                        if result.is_ok()
                            && let Some(project_id) = view.deploy_project_id
                        {
                            view.refresh_deploy_if_current(project_id, cx);
                        }
                        cx.notify();
                        alive.get() && view.dialog_seq == dialog_seq
                    })
                    .unwrap_or(false);
                if close {
                    window.close_dialog(app);
                }
                match result {
                    Ok(warning) => window.push_notification(
                        match warning {
                            Some(warning) => {
                                Notification::warning(format!("主机已删除；{warning}"))
                            }
                            None => Notification::success("主机本地配置已删除，VPS 未受影响"),
                        },
                        app,
                    ),
                    Err(error) => window.push_notification(Notification::error(error), app),
                }
            });
        })
        .detach();
    }

    fn host_actions(&self, id: i64, cx: &mut Context<Self>) -> impl IntoElement {
        let view = cx.weak_entity();
        Popover::new(("host-actions", id as usize))
            .trigger(
                Button::new(("host-more", id as usize))
                    .icon(assets::IconName::Ellipsis)
                    .accessibility_label("更多操作"),
            )
            .content(move |_, _, cx| {
                let detail_view = view.clone();
                let edit_view = view.clone();
                let delete_view = view.clone();
                let detail_popover = cx.weak_entity();
                let edit_popover = cx.weak_entity();
                let delete_popover = cx.weak_entity();
                div()
                    .w(px(152.))
                    .p_1()
                    .flex()
                    .flex_col()
                    .child(
                        Button::new(("host-details", id as usize))
                            .label("详情")
                            .on_click(move |_, window, cx| {
                                let _ = detail_popover
                                    .update(cx, |state, cx| state.dismiss(window, cx));
                                let _ = detail_view.update(cx, |view, cx| {
                                    view.open_host_detail(id, cx);
                                });
                            }),
                    )
                    .child(
                        Button::new(("host-menu-edit", id as usize))
                            .label("编辑")
                            .on_click(move |_, window, cx| {
                                let _ =
                                    edit_popover.update(cx, |state, cx| state.dismiss(window, cx));
                                let _ = edit_view.update(cx, |view, cx| {
                                    view.open_edit_host_dialog(id, window, cx)
                                });
                            }),
                    )
                    .child(
                        Button::new(("host-menu-delete", id as usize))
                            .danger()
                            .label("删除")
                            .on_click(move |_, window, cx| {
                                let _ = delete_popover
                                    .update(cx, |state, cx| state.dismiss(window, cx));
                                let _ = delete_view.update(cx, |view, cx| {
                                    view.confirm_host_delete(id, window, cx)
                                });
                            }),
                    )
            })
    }

    fn open_host_detail(&mut self, id: i64, cx: &mut Context<Self>) {
        if !self.hosts.iter().any(|host| host.id == id) {
            return;
        }
        if let Some(cancel) = self.dashboard_cancel.take() {
            cancel.store(true, Ordering::Relaxed);
        }
        self.dashboard_generation = self.dashboard_generation.wrapping_add(1);
        self.page = Page::Hosts;
        self.host_id = Some(id);
        self.host_tab = HostTab::Monitor;
        self.start_host_monitor(id, cx);
    }

    fn start_host_monitor(&mut self, id: i64, cx: &mut Context<Self>) {
        let Some(host) = self.hosts.iter().find(|host| host.id == id).cloned() else {
            return;
        };
        if let Some(cancel) = self.host_monitor_cancel.take() {
            cancel.store(true, Ordering::Relaxed);
        }
        let cancel = Arc::new(AtomicBool::new(false));
        self.host_monitor_cancel = Some(cancel.clone());
        self.host_monitor_generation = self.host_monitor_generation.wrapping_add(1);
        let generation = self.host_monitor_generation;
        self.host_monitor = None;
        self.host_monitor_error = None;
        self.host_monitor_loading = true;
        cx.notify();
        cx.spawn(async move |view, cx| {
            loop {
                let active = view
                    .update(cx, |view, _| {
                        view.page == Page::Hosts
                            && view.host_id == Some(id)
                            && view.host_tab == HostTab::Monitor
                            && view.host_monitor_generation == generation
                    })
                    .unwrap_or(false);
                if !active || cancel.load(Ordering::Relaxed) {
                    break;
                }
                let result = cx
                    .background_executor()
                    .spawn({
                        let host = host.clone();
                        let cancel = cancel.clone();
                        async move { ssh::monitor_host(host, cancel) }
                    })
                    .await;
                let active = view
                    .update(cx, |view, cx| {
                        if view.page != Page::Hosts
                            || view.host_id != Some(id)
                            || view.host_tab != HostTab::Monitor
                            || view.host_monitor_generation != generation
                        {
                            return false;
                        }
                        view.host_monitor_loading = false;
                        view.host_monitor_cancel = None;
                        let success = match result {
                            Ok(snapshot) => {
                                view.host_monitor = Some(snapshot);
                                view.host_monitor_error = None;
                                true
                            }
                            Err(error) => {
                                view.host_monitor = None;
                                view.host_monitor_error = Some(error.message());
                                false
                            }
                        };
                        cx.notify();
                        if success {
                            view.host_monitor_cancel = Some(cancel.clone());
                        }
                        success
                    })
                    .unwrap_or(false);
                if !active {
                    break;
                }
                cx.background_executor().timer(Duration::from_secs(5)).await;
            }
        })
        .detach();
    }

    fn hosts_page(&self, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some(host) = self
            .host_id
            .and_then(|id| self.hosts.iter().find(|host| host.id == id))
        {
            return self.host_detail(host, cx).into_any_element();
        }
        div()
            .flex()
            .flex_col()
            .gap_6()
            .child(
                self.page_head(
                    "主机",
                    Button::new("host-new")
                        .primary()
                        .label("＋ 添加主机")
                        .on_click(
                            cx.listener(|this, _, window, cx| this.open_host_dialog(window, cx)),
                        ),
                ),
            )
            .child(
                div().flex().child(
                    Button::new("host-group")
                        .outline()
                        .label(if self.host_group == "all" {
                            "全部分组  ▾".into()
                        } else {
                            format!("{}  ▾", self.host_group)
                        })
                        .disabled(self.hosts.is_empty())
                        .on_click(cx.listener(|view, _, _, cx| {
                            let mut groups = view
                                .hosts
                                .iter()
                                .map(|host| host.group.clone())
                                .filter(|group| !group.is_empty())
                                .collect::<Vec<_>>();
                            groups.sort();
                            groups.dedup();
                            let next = if view.host_group == "all" {
                                groups.first().cloned().unwrap_or_else(|| "all".into())
                            } else {
                                groups
                                    .iter()
                                    .position(|group| group == &view.host_group)
                                    .and_then(|index| groups.get(index + 1))
                                    .cloned()
                                    .unwrap_or_else(|| "all".into())
                            };
                            view.host_group = next;
                            cx.notify();
                        })),
                ),
            )
            .when_some(self.credential_warning.as_ref(), |el, warning| {
                el.child(
                    div()
                        .p_4()
                        .border_1()
                        .border_color(self.palette().warning)
                        .rounded_md()
                        .text_color(self.palette().warning)
                        .child(warning.clone()),
                )
            })
            .child(if !self.host_loaded {
                self.empty_card("正在读取主机…", assets::IconName::Server)
                    .into_any_element()
            } else if let Some(error) = &self.host_error {
                self.panel(
                    "主机数据读取失败",
                    "",
                    div()
                        .px_6()
                        .pb_6()
                        .text_color(self.palette().destructive)
                        .child(error.clone()),
                )
                .into_any_element()
            } else if self.hosts.is_empty() {
                div()
                    .w_full()
                    .min_h(px(164.))
                    .border_1()
                    .border_color(self.palette().border)
                    .rounded_lg()
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap_4()
                    .child(Icon::new(assets::IconName::Server).size(px(24.)))
                    .child("还没有主机")
                    .child(
                        Button::new("host-empty-add")
                            .primary()
                            .label("添加主机")
                            .on_click(
                                cx.listener(|this, _, window, cx| {
                                    this.open_host_dialog(window, cx)
                                }),
                            ),
                    )
                    .into_any_element()
            } else {
                self.hosts_table(cx).into_any_element()
            })
            .into_any_element()
    }

    fn hosts_table(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut rows = div().flex().flex_col();
        for host in self
            .hosts
            .iter()
            .filter(|host| self.host_group == "all" || host.group == self.host_group)
        {
            let id = host.id;
            let (status, color) = self.host_status(id);
            rows = rows.child(
                div()
                    .h(px(60.))
                    .min_w(px(940.))
                    .border_t_1()
                    .border_color(self.palette().border)
                    .flex()
                    .items_center()
                    .px_4()
                    .child(
                        div()
                            .w(px(120.))
                            .flex_shrink_0()
                            .text_color(color)
                            .child(format!("● {status}")),
                    )
                    .child(
                        div()
                            .id(("host-name", id as usize))
                            .w(px(130.))
                            .flex_shrink_0()
                            .font_weight(FontWeight::SEMIBOLD)
                            .cursor_pointer()
                            .on_click(cx.listener(move |view, _, _, cx| {
                                view.open_host_detail(id, cx);
                            }))
                            .child(host.name.clone()),
                    )
                    .child(
                        div()
                            .w(px(220.))
                            .flex_shrink_0()
                            .font_family(mono_font())
                            .child(format!("{}:{}", host.address, host.port)),
                    )
                    .child(
                        div()
                            .w(px(130.))
                            .flex_shrink_0()
                            .child(host.username.clone()),
                    )
                    .child(div().w(px(140.)).flex_shrink_0().child(host.group.clone()))
                    .child(
                        div()
                            .w(px(105.))
                            .flex_shrink_0()
                            .text_color(self.palette().muted)
                            .child(match self.host_tests.get(&id) {
                                Some(HostTestState::Connected { checked_at, .. }) => {
                                    let elapsed = checked_at.elapsed().as_secs() / 60;
                                    if elapsed == 0 {
                                        "刚刚".into()
                                    } else {
                                        format!("{elapsed} 分钟前")
                                    }
                                }
                                _ => "—".into(),
                            }),
                    )
                    .child(
                        div()
                            .flex()
                            .gap_1()
                            .child(
                                Button::new(("host-test", id as usize))
                                    .icon(assets::IconName::RefreshCw)
                                    .accessibility_label("测试连接")
                                    .tooltip("测试 SSH、Docker 与 Compose")
                                    .disabled(self.host_testing.is_some())
                                    .on_click(cx.listener(move |view, _, window, cx| {
                                        view.test_host(id, window, cx)
                                    })),
                            )
                            .child(self.host_actions(id, cx)),
                    ),
            );
        }
        div()
            .id("host-table-scroll")
            .w_full()
            .overflow_x_scroll()
            .border_1()
            .border_color(self.palette().border)
            .rounded_lg()
            .child(
                div()
                    .h(px(44.))
                    .min_w(px(940.))
                    .flex()
                    .items_center()
                    .px_4()
                    .text_color(self.palette().muted)
                    .child(div().w(px(120.)).flex_shrink_0().child("状态"))
                    .child(div().w(px(130.)).flex_shrink_0().child("别名"))
                    .child(div().w(px(220.)).flex_shrink_0().child("地址"))
                    .child(div().w(px(130.)).flex_shrink_0().child("用户"))
                    .child(div().w(px(140.)).flex_shrink_0().child("分组"))
                    .child(div().w(px(105.)).flex_shrink_0().child("最近连接"))
                    .child("操作"),
            )
            .child(rows)
    }

    fn host_status(&self, id: i64) -> (String, Hsla) {
        match self.host_tests.get(&id) {
            Some(HostTestState::Checking) => ("测试中".into(), self.palette().muted),
            Some(HostTestState::Connected { stale: false, .. }) => {
                ("在线".into(), self.palette().success)
            }
            Some(HostTestState::Connected { stale: true, .. }) => {
                ("待复核".into(), self.palette().muted)
            }
            Some(HostTestState::NeedsTrust) => ("待确认指纹".into(), self.palette().warning),
            Some(HostTestState::DockerMissing { .. }) => {
                ("Docker 未安装".into(), self.palette().warning)
            }
            Some(HostTestState::Failed(_)) => ("连接失败".into(), self.palette().destructive),
            None => ("未连接".into(), self.palette().muted),
        }
    }

    fn monitor_metric(
        &self,
        id: &'static str,
        label: &'static str,
        value: String,
        percent: Option<f64>,
    ) -> impl IntoElement {
        let color = match percent {
            Some(value) if value > 90. => self.palette().destructive,
            Some(value) if value > 80. => self.palette().warning,
            _ => self.palette().foreground,
        };
        div()
            .flex_1()
            .min_w(px(145.))
            .border_1()
            .border_color(self.palette().border)
            .rounded_lg()
            .p_4()
            .flex()
            .flex_col()
            .gap_3()
            .child(div().text_color(self.palette().muted).child(label))
            .child(
                div()
                    .text_size(px(22.))
                    .font_weight(FontWeight::BOLD)
                    .text_color(color)
                    .child(value),
            )
            .when_some(percent, |el, percent| {
                el.child(Progress::new(id).value(percent.clamp(0., 100.) as f32))
            })
    }

    fn host_monitor_content(&self, host: &Host, cx: &mut Context<Self>) -> impl IntoElement {
        if self.host_monitor_loading {
            return self
                .empty_card("正在通过 SSH 读取资源…", assets::IconName::Server)
                .into_any_element();
        }
        if let Some(error) = &self.host_monitor_error {
            let id = host.id;
            let message = match self.host_tests.get(&id) {
                Some(HostTestState::Checking) => "正在测试 SSH 与 Docker…".into(),
                Some(HostTestState::Failed(latest)) => latest.clone(),
                Some(HostTestState::NeedsTrust) => "请先独立核对并确认 SSH 主机指纹。".into(),
                Some(HostTestState::DockerMissing { .. }) => {
                    "SSH 已连接，但 Docker 未安装；请到基本信息查看安装引导。".into()
                }
                _ => error.clone(),
            };
            return self
                .panel(
                    "资源读取失败",
                    "请核对主机指纹、凭据与 Docker 权限。",
                    div()
                        .px_6()
                        .pb_6()
                        .flex()
                        .flex_col()
                        .items_start()
                        .gap_3()
                        .child(div().text_color(self.palette().destructive).child(message))
                        .child(
                            Button::new("host-monitor-retry")
                                .label("测试连接 / 重连")
                                .disabled(self.host_testing.is_some())
                                .on_click(cx.listener(move |view, _, window, cx| {
                                    view.test_host(id, window, cx)
                                })),
                        ),
                )
                .into_any_element();
        }
        let Some(snapshot) = &self.host_monitor else {
            return self
                .empty_card("等待资源采样", assets::IconName::Server)
                .into_any_element();
        };
        let mut rows = div().flex().flex_col();
        for (index, item) in snapshot
            .containers
            .as_deref()
            .unwrap_or(&[])
            .iter()
            .enumerate()
        {
            let id = host.id;
            rows = rows.child(
                div()
                    .id(("host-monitor-container", index))
                    .min_w(px(820.))
                    .h(px(48.))
                    .border_t_1()
                    .border_color(self.palette().border)
                    .px_4()
                    .flex()
                    .items_center()
                    .cursor_pointer()
                    .on_click(cx.listener(move |view, _, window, cx| {
                        view.navigate(Page::Containers, window, cx);
                        view.switch_container_host(id, window, cx);
                    }))
                    .child(
                        div()
                            .w(px(170.))
                            .flex_shrink_0()
                            .truncate()
                            .child(item.name.clone()),
                    )
                    .child(
                        div()
                            .w(px(260.))
                            .flex_shrink_0()
                            .truncate()
                            .font_family(mono_font())
                            .child(item.image.clone()),
                    )
                    .child(
                        div()
                            .w(px(120.))
                            .flex_shrink_0()
                            .text_color(self.palette().success)
                            .child("running"),
                    )
                    .child(div().w(px(90.)).flex_shrink_0().child(item.cpu.clone()))
                    .child(item.memory.clone()),
            );
        }
        div()
            .flex()
            .flex_col()
            .gap_6()
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_4()
                    .child(self.monitor_metric(
                        "monitor-cpu",
                        "CPU",
                        format!("{:.0}%", snapshot.cpu_percent),
                        Some(snapshot.cpu_percent),
                    ))
                    .child(self.monitor_metric(
                        "monitor-memory",
                        "内存",
                        format!(
                            "{:.1} / {:.1} GiB",
                            snapshot.memory_used_kib as f64 / 1_048_576.,
                            snapshot.memory_total_kib as f64 / 1_048_576.
                        ),
                        Some(
                            snapshot.memory_used_kib as f64 * 100.
                                / snapshot.memory_total_kib as f64,
                        ),
                    ))
                    .child(self.monitor_metric(
                        "monitor-disk",
                        "磁盘",
                        format!("{:.0}%", snapshot.disk_percent),
                        Some(snapshot.disk_percent),
                    ))
                    .child(
                        self.monitor_metric(
                            "monitor-network",
                            "网络",
                            snapshot
                                .network_bytes_per_sec
                                .map(|(receive, send)| {
                                    format!(
                                        "↓ {:.1} KB/s  ↑ {:.1} KB/s",
                                        receive / 1000.,
                                        send / 1000.
                                    )
                                })
                                .unwrap_or_else(|| "默认路由接口不可用".into()),
                            None,
                        ),
                    ),
            )
            .child(self.panel(
                "运行的容器",
                "每次采样完成后约 5 秒刷新 · SSH 直连",
                if snapshot.containers.as_ref().is_none_or(Vec::is_empty) {
                    div()
                        .px_6()
                        .pb_6()
                        .text_color(self.palette().muted)
                        .child(if snapshot.containers.is_none() {
                            "Docker 容器列表不可用"
                        } else {
                            "暂无运行中的容器"
                        })
                        .into_any_element()
                } else {
                    div()
                        .id("host-monitor-table-scroll")
                        .overflow_x_scroll()
                        .child(
                            div()
                                .min_w(px(820.))
                                .h(px(40.))
                                .px_4()
                                .flex()
                                .items_center()
                                .text_color(self.palette().muted)
                                .child(div().w(px(170.)).flex_shrink_0().child("名称"))
                                .child(div().w(px(260.)).flex_shrink_0().child("镜像"))
                                .child(div().w(px(120.)).flex_shrink_0().child("状态"))
                                .child(div().w(px(90.)).flex_shrink_0().child("CPU"))
                                .child("内存"),
                        )
                        .child(rows)
                        .into_any_element()
                },
            ))
            .into_any_element()
    }

    fn host_info_field(&self, label: &'static str, value: String) -> impl IntoElement {
        div()
            .w(relative(0.45))
            .min_w(px(240.))
            .flex_shrink_0()
            .flex()
            .flex_col()
            .gap_1()
            .child(div().text_color(self.palette().muted).child(label))
            .child(div().min_w_0().whitespace_normal().child(value))
    }

    fn host_detail(&self, host: &Host, cx: &mut Context<Self>) -> impl IntoElement {
        let id = host.id;
        let (status, color) = self.host_status(id);
        let (system, docker, architecture) = match self.host_tests.get(&id) {
            Some(HostTestState::Connected {
                docker_version,
                compose_version,
                system_kernel,
                architecture,
                ..
            }) => (
                system_kernel.clone(),
                format!("{docker_version} / Compose {compose_version}"),
                architecture
                    .clone()
                    .unwrap_or_else(|| "不支持的架构".into()),
            ),
            Some(HostTestState::DockerMissing {
                os_id,
                os_version,
                system_kernel,
                architecture,
            }) => (
                format!("{os_id} {os_version} / {system_kernel}"),
                "未安装".into(),
                architecture
                    .clone()
                    .unwrap_or_else(|| "不支持的架构".into()),
            ),
            _ => (
                "待连接测试".into(),
                "待连接测试".into(),
                "待连接测试".into(),
            ),
        };
        let status_detail = match self.host_tests.get(&id) {
            Some(HostTestState::Connected {
                disk_available_kib,
                sftp_v3_handshake,
                stale: false,
                ..
            }) => format!(
                "根目录可用 {:.1} GiB · SFTP {}",
                *disk_available_kib as f64 / 1_048_576.,
                if *sftp_v3_handshake {
                    "v3 握手通过（写入待核实）"
                } else {
                    "v3 握手失败"
                }
            ),
            Some(HostTestState::Connected { stale: true, .. }) => {
                "上次结果已过期，请重新测试。".into()
            }
            Some(HostTestState::Failed(error)) => error.clone(),
            Some(HostTestState::NeedsTrust) => "等待独立核对 SSH 主机指纹。".into(),
            Some(HostTestState::DockerMissing { .. }) => {
                "SSH 已连接，Docker 未安装；发布和容器管理暂不可用。".into()
            }
            _ => "尚未完成 SSH 与 Docker 检查。".into(),
        };
        let docker_missing = matches!(
            self.host_tests.get(&id),
            Some(HostTestState::DockerMissing { .. })
        );
        let install_supported = host.username == "root"
            && matches!(
                self.host_tests.get(&id),
                Some(HostTestState::DockerMissing { os_id, os_version, architecture: Some(platform), .. })
                    if os_id == "debian" && os_version == "13" && platform == "linux/amd64"
            );
        let mirror_available = host.username == "root"
            && matches!(
                self.host_tests.get(&id),
                Some(HostTestState::Connected { stale: false, .. })
            );
        div()
            .flex()
            .flex_col()
            .gap_6()
            .child(
                self.page_head(
                    host.name.clone(),
                    div()
                        .flex()
                        .gap_2()
                        .child(Button::new("host-edit").label("编辑").on_click(cx.listener(
                            move |view, _, window, cx| view.open_edit_host_dialog(id, window, cx),
                        )))
                        .child(
                            Button::new("host-containers")
                                .label("打开容器管理")
                                .on_click(cx.listener(move |view, _, window, cx| {
                                    view.navigate(Page::Containers, window, cx);
                                    view.switch_container_host(id, window, cx);
                                    cx.notify();
                                })),
                        ),
                ),
            )
            .child(
                div().flex().child(
                    div()
                        .p_1()
                        .rounded_md()
                        .bg(self.palette().accent)
                        .flex()
                        .gap_1()
                        .child(self.tab(
                            "host-monitor",
                            "资源监控",
                            self.host_tab == HostTab::Monitor,
                            cx,
                            move |view, _, cx| {
                                view.host_tab = HostTab::Monitor;
                                view.start_host_monitor(id, cx);
                            },
                        ))
                        .child(self.tab(
                            "host-info",
                            "基本信息",
                            self.host_tab == HostTab::Info,
                            cx,
                            |view, _, cx| {
                                view.host_tab = HostTab::Info;
                                if let Some(cancel) = view.host_monitor_cancel.take() {
                                    cancel.store(true, Ordering::Relaxed);
                                }
                                view.host_monitor_generation =
                                    view.host_monitor_generation.wrapping_add(1);
                                cx.notify();
                            },
                        )),
                ),
            )
            .when(self.host_tab == HostTab::Monitor, |el| {
                el.child(self.host_monitor_content(host, cx))
            })
            .when(self.host_tab == HostTab::Info, |el| {
                el.child(
                    self.panel(
                        "基本信息",
                        "SSH 连接配置与远端环境",
                        div()
                            .px_6()
                            .pb_6()
                            .flex()
                            .flex_col()
                            .gap_6()
                            .child(
                                div()
                                    .flex()
                                    .flex_wrap()
                                    .gap_6()
                                    .child(self.host_info_field("地址", host.address.clone()))
                                    .child(self.host_info_field("SSH 端口", host.port.to_string()))
                                    .child(self.host_info_field("登录用户", host.username.clone()))
                                    .child(self.host_info_field(
                                        "认证方式",
                                        format!(
                                            "{}（凭据引用已保存）",
                                            if host.auth == "password" {
                                                "密码"
                                            } else {
                                                "私钥"
                                            }
                                        ),
                                    ))
                                    .child(self.host_info_field(
                                        "分组",
                                        if host.group.is_empty() {
                                            "未分组".into()
                                        } else {
                                            host.group.clone()
                                        },
                                    ))
                                    .child(self.host_info_field("系统 / 内核", system))
                                    .child(self.host_info_field("Docker 版本", docker))
                                    .child(self.host_info_field("架构", architecture))
                                    .child(self.host_info_field("Agent", "无".into())),
                            )
                            .when(docker_missing, |el| {
                                el.child(
                                    div()
                                        .border_1()
                                        .border_color(self.palette().warning)
                                        .rounded_lg()
                                        .p_4()
                                        .flex()
                                        .flex_col()
                                        .items_start()
                                        .gap_3()
                                        .child(div().text_color(self.palette().warning).child("SSH 已连接 · Docker 未安装"))
                                        .child(if install_supported {
                                            "国内推荐腾讯云 Docker CE apt 镜像；安装前需确认仓库签名，完成后自动重新测试。"
                                        } else {
                                            "一键安装目前仅支持 Debian 13 / amd64 且以 root 登录；请按 Docker 官方文档手动安装 Engine 与 Compose。"
                                        })
                                        .when_some(self.host_install_error.as_ref().filter(|(failed_id, _)| *failed_id == id), |el, (_, error)| {
                                            el.child(div().text_color(self.palette().destructive).child(error.clone()))
                                        })
                                        .when(install_supported, |el| {
                                            el.child(Button::new("docker-install-open")
                                                .label(if self.host_installing == Some(id) { "正在安装 Docker…" } else { "一键安装 Docker" })
                                                .disabled(self.host_installing.is_some())
                                                .on_click(cx.listener(move |view, _, window, cx| view.confirm_docker_install(id, window, cx))))
                                        })
                                        .when(self.host_installing == Some(id), |el| {
                                            el.child(self.host_install_stage.clone().unwrap_or_else(|| "安装中…".into()))
                                        }),
                                )
                            })
                            .when(mirror_available, |el| {
                                el.child(
                                    div()
                                        .border_1()
                                        .border_color(self.palette().border)
                                        .rounded_lg()
                                        .p_4()
                                        .flex()
                                        .flex_col()
                                        .items_start()
                                        .gap_3()
                                        .child("Docker Hub 镜像源")
                                        .child("已实测候选：1Panel、DaoCloud。配置时保留其他 daemon 设置；会重启 Docker，运行容器不为空则阻断。")
                                        .when_some(self.host_mirror_error.as_ref().filter(|(failed_id, _)| *failed_id == id), |el, (_, error)| {
                                            el.child(div().text_color(self.palette().destructive).child(error.clone()))
                                        })
                                        .child(Button::new("mirror-config-open")
                                            .label(if self.host_mirror_configuring == Some(id) { "正在配置镜像源…" } else { "配置镜像源" })
                                            .disabled(self.host_installing.is_some() || self.host_mirror_configuring.is_some())
                                            .on_click(cx.listener(move |view, _, window, cx| view.confirm_mirror_config(id, window, cx)))),
                                )
                            })
                            .child(div().text_color(color).child(format!(
                                "连接状态：{status} · 主机指纹{} · {status_detail}",
                                if host.trusted_fingerprint.is_some() {
                                    "已确认"
                                } else {
                                    "尚未确认"
                                }
                            ))),
                    ),
                )
            })
    }

    fn open_project_dialog(
        &mut self,
        id: Option<i64>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let existing = id.and_then(|id| self.projects.iter().find(|project| project.id == id));
        if id.is_some() && existing.is_none() {
            return;
        }
        if existing.is_some_and(|project| project.manifest_mode) {
            window.push_notification(
                Notification::warning(
                    "此项目由开发者维护 devingpanel.yaml，请编辑文件后重新读取。",
                ),
                cx,
            );
            return;
        }
        self.dialog_seq = self.dialog_seq.wrapping_add(1);
        let dialog_seq = self.dialog_seq;
        let name = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value(existing.map(|p| p.name.clone()).unwrap_or_default())
        });
        let path = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value(existing.map(|p| p.path.clone()).unwrap_or_default())
        });
        let dockerfile = cx.new(|cx| {
            InputState::new(window, cx).default_value(
                existing
                    .map(|p| p.dockerfile.clone())
                    .unwrap_or_else(|| "Dockerfile".into()),
            )
        });
        let view = cx.weak_entity();
        let submitting = Rc::new(Cell::new(false));
        let alive = Rc::new(Cell::new(true));
        window.open_dialog(cx, move |dialog, _, _| {
            let save_name = name.clone();
            let save_path = path.clone();
            let save_dockerfile = dockerfile.clone();
            let save_view = view.clone();
            let browse_view = view.clone();
            let browse_path = path.clone();
            let save_submitting = submitting.clone();
            let save_alive = alive.clone();
            let cancel_submitting = submitting.clone();
            let cancel_alive = alive.clone();
            let close_alive = alive.clone();
            dialog
                .title(if id.is_some() {
                    "编辑项目"
                } else {
                    "新建项目"
                })
                .w(px(640.))
                .close_button(!submitting.get())
                .overlay_closable(!submitting.get())
                .keyboard(!submitting.get())
                .on_close(move |_, _, _| close_alive.set(false))
                .child(
                    div()
                        .w_full()
                        .flex()
                        .flex_col()
                        .gap_4()
                        .child(Self::form_field("项目名", Input::new(&name).w_full()))
                        .child(Self::form_field(
                            "本地代码目录",
                            div()
                                .flex()
                                .gap_2()
                                .child(div().flex_1().child(Input::new(&path).w_full()))
                                .child(
                                    Button::new("browse-project")
                                        .label("浏览")
                                        .disabled(submitting.get())
                                        .on_click(move |_, window, cx| {
                                            let _ = browse_view.update(cx, |view, cx| {
                                                view.browse_project_directory(
                                                    browse_path.clone(),
                                                    window,
                                                    cx,
                                                );
                                            });
                                        }),
                                ),
                        ))
                        .child(Self::form_field(
                            "Dockerfile 路径",
                            Input::new(&dockerfile).w_full(),
                        ))
                        .child(
                            div()
                                .text_size(px(12.))
                                .child(if id.is_some() {
                                    "Dockerfile 路径相对于代码目录；更换目录会清空 Compose 文件与部署服务，需重新配置。"
                                } else {
                                    "Dockerfile 路径相对于本地代码目录。"
                                }),
                        ),
                )
                .footer(
                    div()
                        .flex()
                        .justify_end()
                        .gap_2()
                        .child(
                            Button::new("project-cancel")
                                .label("取消")
                                .disabled(submitting.get())
                                .on_click(move |_, window, cx| {
                                    if !cancel_submitting.get() {
                                        cancel_alive.set(false);
                                        window.close_dialog(cx);
                                    }
                                }),
                        )
                        .child(
                            Button::new("project-save")
                                .primary()
                                .label(if id.is_some() {
                                    "保存"
                                } else {
                                    "创建项目"
                                })
                                .disabled(submitting.get())
                                .on_click(move |_, window, cx| {
                                    if save_submitting.replace(true) {
                                        return;
                                    }
                                    window.refresh();
                                    let input = NewProject {
                                        name: save_name.read(cx).value().to_string(),
                                        path: save_path.read(cx).value().to_string(),
                                        dockerfile: save_dockerfile.read(cx).value().to_string(),
                                    };
                                    if save_view
                                        .update(cx, |view, cx| {
                                            view.save_project(
                                                id,
                                                input,
                                                DialogSave {
                                                    seq: dialog_seq,
                                                    submitting: save_submitting.clone(),
                                                    alive: save_alive.clone(),
                                                },
                                                window,
                                                cx,
                                            );
                                        })
                                        .is_err()
                                    {
                                        save_submitting.set(false);
                                    }
                                }),
                        ),
                )
        });
    }

    fn browse_project_directory(
        &mut self,
        input: Entity<InputState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let picker = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("选择本地代码目录".into()),
        });
        cx.spawn_in(window, async move |_, cx| {
            let result = picker.await;
            let _ = cx.update(|window, app| match result {
                Ok(Ok(Some(paths))) => {
                    if let Some(path) = paths.first() {
                        input.update(app, |input, cx| {
                            input.set_value(path.to_string_lossy().into_owned(), window, cx)
                        });
                    }
                }
                Ok(Ok(None)) => {}
                Ok(Err(error)) => {
                    window.push_notification(Notification::error(error.to_string()), app)
                }
                Err(error) => window.push_notification(Notification::error(error.to_string()), app),
            });
        })
        .detach();
    }

    fn import_configured_project(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let picker = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("选择包含 devingpanel.yaml 的项目目录".into()),
        });
        cx.spawn_in(window, async move |view, cx| {
            let path = match picker.await {
                Ok(Ok(Some(paths))) => paths.into_iter().next(),
                Ok(Ok(None)) => None,
                Ok(Err(error)) => {
                    let _ = cx.update(|window, app| {
                        window.push_notification(Notification::error(error.to_string()), app)
                    });
                    None
                }
                Err(error) => {
                    let _ = cx.update(|window, app| {
                        window.push_notification(Notification::error(error.to_string()), app)
                    });
                    None
                }
            };
            let Some(path) = path else { return };
            let result = cx
                .background_executor()
                .spawn(async move { project_store::create_from_manifest(&path) })
                .await;
            let _ = cx.update(|window, app| match result {
                Ok(project) => {
                    let id = project.id;
                    let _ = view.update(app, |view, cx| {
                        view.projects.insert(0, project);
                        view.project_loaded = true;
                        view.project_error = None;
                        view.select_project(id, window, cx);
                    });
                    window.push_notification(
                        Notification::success("已导入 devingpanel.yaml；请核对发布就绪状态"),
                        app,
                    );
                }
                Err(error) => window.push_notification(Notification::error(error), app),
            });
        })
        .detach();
    }

    fn reload_project_configs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        cx.spawn_in(window, async move |view, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { project_store::list() })
                .await;
            let _ = cx.update(|window, app| match result {
                Ok(projects) => {
                    let _ = view.update(app, |view, cx| {
                        view.projects = projects;
                        view.invalidate_compose_summary();
                        if let Some(id) = view.project_id {
                            view.build_form = view
                                .projects
                                .iter()
                                .find(|project| project.id == id)
                                .filter(|project| !project.manifest_mode)
                                .map(|project| BuildForm::new(project, window, cx));
                        }
                        if let Some(id) = view.deploy_project_id {
                            view.select_deploy_project(id, window, cx);
                        }
                        cx.notify();
                    });
                }
                Err(error) => window.push_notification(Notification::error(error), app),
            });
        })
        .detach();
    }

    fn confirm_adopt_manifest(&mut self, id: i64, window: &mut Window, cx: &mut Context<Self>) {
        if !self
            .projects
            .iter()
            .any(|project| project.id == id && !project.manifest_mode)
        {
            return;
        }
        let view = cx.weak_entity();
        window.open_dialog(cx, move |dialog, _, _| {
            let confirm_view = view.clone();
            dialog
                .title("改用 devingpanel.yaml")
                .w(px(520.))
                .child(div().flex().flex_col().gap_3()
                    .child("若项目目录已有 devingpanel.yaml，将读取并校验，绝不覆盖。否则从现有非秘密配置生成文件。")
                    .child("Build args 会作为明文写入项目目录；请先确认其中没有密码或令牌。SSH 凭据及敏感环境变量不会写入文件。")
                    .child("目标主机按 YAML 顺序重新绑定；移除绑定可能清理该主机的本地专属变量。迁移后项目构建、Compose 和目标主机设置由 YAML 唯一决定。"))
                .footer(div().flex().justify_end().gap_2()
                    .child(Button::new("adopt-cancel").label("取消")
                        .on_click(|_, window, cx| window.close_dialog(cx)))
                    .child(Button::new("adopt-confirm").primary().label("确认迁移")
                        .on_click(move |_, window, cx| {
                            window.close_dialog(cx);
                            let _ = confirm_view.update(cx, |view, cx| view.adopt_manifest(id, window, cx));
                        })))
        });
    }

    fn adopt_manifest(&mut self, id: i64, window: &mut Window, cx: &mut Context<Self>) {
        cx.spawn_in(window, async move |view, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { project_store::adopt_manifest(id) })
                .await;
            let _ = cx.update(|window, app| match result {
                Ok((project, warning)) => {
                    let _ = view.update(app, |view, cx| {
                        if let Some(existing) = view.projects.iter_mut().find(|item| item.id == id)
                        {
                            *existing = project;
                        }
                        view.build_form = None;
                        view.invalidate_compose_summary();
                        if view.deploy_project_id == Some(id) {
                            view.select_deploy_project(id, window, cx);
                        }
                        cx.notify();
                    });
                    window.push_notification(
                        match warning {
                            Some(warning) => Notification::warning(format!("已迁移；{warning}")),
                            None => Notification::success("项目已改用 devingpanel.yaml"),
                        },
                        app,
                    );
                }
                Err(error) => window.push_notification(Notification::error(error), app),
            });
        })
        .detach();
    }

    fn confirm_manifest_targets(&mut self, id: i64, window: &mut Window, cx: &mut Context<Self>) {
        if !self
            .projects
            .iter()
            .any(|project| project.id == id && project.manifest_mode)
        {
            return;
        }
        let view = cx.weak_entity();
        window.open_dialog(cx, move |dialog, _, _| {
            let confirm_view = view.clone();
            dialog.title("确认 YAML 目标主机")
                .w(px(520.))
                .child(div().flex().flex_col().gap_3()
                    .child("请先由管理员核对本机主机组、别名、SSH 凭据和指纹。")
                    .child("确认后按 devingpanel.yaml 顺序绑定目标；取消旧绑定可能清理对应的本机主机专属环境变量。"))
                .footer(div().flex().justify_end().gap_2()
                    .child(Button::new("manifest-target-cancel").label("取消")
                        .on_click(|_, window, cx| window.close_dialog(cx)))
                    .child(Button::new("manifest-target-confirm").primary().label("确认绑定")
                        .on_click(move |_, window, cx| {
                            window.close_dialog(cx);
                            let _ = confirm_view.update(cx, |view, cx| view.sync_manifest_targets(id, window, cx));
                        })))
        });
    }

    fn sync_manifest_targets(&mut self, id: i64, window: &mut Window, cx: &mut Context<Self>) {
        cx.spawn_in(window, async move |view, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { project_store::confirm_manifest_targets(id) })
                .await;
            let _ = cx.update(|window, app| match result {
                Ok((project, warning)) => {
                    let _ = view.update(app, |view, cx| {
                        if let Some(existing) = view.projects.iter_mut().find(|item| item.id == id)
                        {
                            *existing = project;
                        }
                        view.invalidate_compose_summary();
                        if view.deploy_project_id == Some(id) {
                            view.select_deploy_project(id, window, cx);
                        }
                        cx.notify();
                    });
                    window.push_notification(
                        match warning {
                            Some(warning) => {
                                Notification::warning(format!("目标已更新；{warning}"))
                            }
                            None => Notification::success("YAML 目标主机已核对"),
                        },
                        app,
                    );
                }
                Err(error) => window.push_notification(Notification::error(error), app),
            });
        })
        .detach();
    }

    fn save_project(
        &mut self,
        id: Option<i64>,
        input: NewProject,
        dialog: DialogSave,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.spawn_in(window, async move |view, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    match id {
                        Some(id) => project_store::update(id, input),
                        None => project_store::create(input),
                    }
                })
                .await;
            dialog.submitting.set(false);
            let _ = cx.update(|window, app| match result {
                Ok(project) => {
                    let close_dialog = view
                        .update(app, |view, cx| {
                            let active = dialog.alive.get() && view.dialog_seq == dialog.seq;
                            if active && id.is_none() {
                                view.project_id = Some(project.id);
                                view.project_tab = ProjectTab::Build;
                                view.build_form = Some(BuildForm::new(&project, window, cx));
                                view.page = Page::Projects;
                            }
                            if let Some(existing) =
                                view.projects.iter_mut().find(|p| p.id == project.id)
                            {
                                *existing = project.clone();
                            } else {
                                view.projects.insert(0, project.clone());
                            }
                            view.refresh_deploy_if_current(project.id, cx);
                            if id.is_some() && view.project_id == Some(project.id) {
                                view.refresh_services(project.id, cx);
                            }
                            view.project_loaded = true;
                            view.project_error = None;
                            cx.notify();
                            active
                        })
                        .unwrap_or(false);
                    if close_dialog {
                        window.close_dialog(app);
                    }
                    window.push_notification(
                        Notification::success(if id.is_some() {
                            "项目已保存"
                        } else {
                            "项目已创建，尚需完善配置"
                        }),
                        app,
                    );
                }
                Err(error) => {
                    window.push_notification(Notification::error(error), app);
                    window.refresh();
                }
            });
        })
        .detach();
    }

    fn project_empty(&self, title: &'static str, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .w_full()
            .min_h(px(164.))
            .border_1()
            .border_color(self.palette().border)
            .rounded_lg()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_4()
            .child(Icon::new(assets::IconName::Folder).size(px(24.)))
            .child(title)
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(
                        Button::new("project-empty-import")
                            .primary()
                            .label("导入已配置项目")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.import_configured_project(window, cx)
                            })),
                    )
                    .child(Button::new("project-empty-add").label("新建项目").on_click(
                        cx.listener(|this, _, window, cx| {
                            this.open_project_dialog(None, window, cx)
                        }),
                    )),
            )
    }

    fn projects_page(&self, narrow: bool, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some(project) = self
            .project_id
            .and_then(|id| self.projects.iter().find(|project| project.id == id))
        {
            return self.project_detail(project, narrow, cx).into_any_element();
        }
        div()
            .flex()
            .flex_col()
            .gap_6()
            .child(
                self.page_head(
                    "项目",
                    div()
                        .flex()
                        .gap_2()
                        .child(
                            Button::new("project-import-config")
                                .label("导入已配置项目")
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.import_configured_project(window, cx)
                                })),
                        )
                        .child(
                            Button::new("project-new")
                                .primary()
                                .label("＋ 新建项目")
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.open_project_dialog(None, window, cx)
                                })),
                        ),
                ),
            )
            .child(if !self.project_loaded {
                self.empty_card("正在读取项目…", assets::IconName::Folder)
                    .into_any_element()
            } else if let Some(error) = &self.project_error {
                div()
                    .p_4()
                    .border_1()
                    .border_color(self.palette().destructive)
                    .rounded_lg()
                    .text_color(self.palette().destructive)
                    .child(format!("项目数据读取失败：{error}"))
                    .into_any_element()
            } else if self.projects.is_empty() {
                self.project_empty("还没有项目", cx).into_any_element()
            } else {
                self.project_table(cx).into_any_element()
            })
            .into_any_element()
    }

    fn project_table(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut rows = div().flex().flex_col();
        for project in &self.projects {
            let id = project.id;
            rows = rows.child(
                div()
                    .h(px(60.))
                    .min_w(px(900.))
                    .border_t_1()
                    .border_color(self.palette().border)
                    .flex()
                    .items_center()
                    .px_4()
                    .child(
                        div()
                            .id(("project-name", id as usize))
                            .w(px(135.))
                            .flex_shrink_0()
                            .truncate()
                            .font_weight(FontWeight::SEMIBOLD)
                            .cursor_pointer()
                            .on_click(cx.listener(move |view, _, window, cx| {
                                view.select_project(id, window, cx)
                            }))
                            .child(project.name.clone()),
                    )
                    .child(
                        div()
                            .w(px(215.))
                            .flex_shrink_0()
                            .truncate()
                            .child(project.path.clone()),
                    )
                    .child(
                        div()
                            .w(px(130.))
                            .flex_shrink_0()
                            .child(project.host_ids.len().to_string()),
                    )
                    .child(
                        div()
                            .w(px(200.))
                            .flex_shrink_0()
                            .text_color(self.palette().muted)
                            .child("—"),
                    )
                    .child(
                        div()
                            .w(px(105.))
                            .flex_shrink_0()
                            .flex()
                            .items_center()
                            .gap_1()
                            .text_color(
                                if project.manifest_mode && project.config_error.is_none() {
                                    self.palette().success
                                } else {
                                    self.palette().warning
                                },
                            )
                            .child(div().size(px(7.)).rounded_full().bg(
                                if project.manifest_mode && project.config_error.is_none() {
                                    self.palette().success
                                } else {
                                    self.palette().warning
                                },
                            ))
                            .child(if project.manifest_mode {
                                if project.config_error.is_some() {
                                    "需管理员处理"
                                } else {
                                    "配置已读取"
                                }
                            } else {
                                "待完善"
                            }),
                    )
                    .child(
                        Button::new(("project-deploy", id as usize))
                            .primary()
                            .label("发布")
                            .tooltip("发布执行链尚未完成，暂不可一键发布")
                            .disabled(true),
                    )
                    .child(self.project_actions(id, cx)),
            );
        }
        div()
            .id("project-table-scroll")
            .w_full()
            .overflow_x_scroll()
            .border_1()
            .border_color(self.palette().border)
            .rounded_lg()
            .child(
                div()
                    .h(px(44.))
                    .min_w(px(900.))
                    .flex()
                    .items_center()
                    .px_4()
                    .text_color(self.palette().muted)
                    .text_size(px(14.))
                    .child(div().w(px(135.)).flex_shrink_0().child("项目名"))
                    .child(div().w(px(215.)).flex_shrink_0().child("本地路径"))
                    .child(div().w(px(130.)).flex_shrink_0().child("绑定主机数"))
                    .child(div().w(px(200.)).flex_shrink_0().child("最近发布"))
                    .child(div().w(px(105.)).flex_shrink_0().child("状态"))
                    .child("操作"),
            )
            .child(rows)
    }

    fn project_actions(&self, id: i64, cx: &mut Context<Self>) -> impl IntoElement {
        let view = cx.weak_entity();
        Popover::new(("project-actions", id as usize))
            .trigger(
                Button::new(("project-more", id as usize))
                    .icon(assets::IconName::Ellipsis)
                    .accessibility_label("更多操作"),
            )
            .content(move |_, _, cx| {
                let detail_view = view.clone();
                let edit_view = view.clone();
                let delete_view = view.clone();
                let detail_popover = cx.weak_entity();
                let edit_popover = cx.weak_entity();
                let delete_popover = cx.weak_entity();
                div()
                    .w(px(152.))
                    .p_1()
                    .flex()
                    .flex_col()
                    .child(
                        Button::new(("project-details", id as usize))
                            .label("详情")
                            .on_click(move |_, window, cx| {
                                let _ = detail_popover
                                    .update(cx, |state, cx| state.dismiss(window, cx));
                                let _ = detail_view
                                    .update(cx, |view, cx| view.select_project(id, window, cx));
                            }),
                    )
                    .child(
                        Button::new(("project-edit", id as usize))
                            .label("编辑")
                            .on_click(move |_, window, cx| {
                                let _ =
                                    edit_popover.update(cx, |state, cx| state.dismiss(window, cx));
                                let _ = edit_view.update(cx, |view, cx| {
                                    view.open_project_dialog(Some(id), window, cx)
                                });
                            }),
                    )
                    .child(
                        Button::new(("project-delete", id as usize))
                            .danger()
                            .label("删除")
                            .on_click(move |_, window, cx| {
                                let _ = delete_popover
                                    .update(cx, |state, cx| state.dismiss(window, cx));
                                let _ = delete_view.update(cx, |view, cx| {
                                    view.confirm_project_delete(id, window, cx)
                                });
                            }),
                    )
            })
    }

    fn confirm_project_delete(&mut self, id: i64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(project) = self.projects.iter().find(|project| project.id == id) else {
            return;
        };
        let name = project.name.clone();
        self.dialog_seq = self.dialog_seq.wrapping_add(1);
        let dialog_seq = self.dialog_seq;
        let view = cx.weak_entity();
        let submitting = Rc::new(Cell::new(false));
        let alive = Rc::new(Cell::new(true));
        window.open_dialog(cx, move |dialog, _, _| {
            let save_view = view.clone();
            let save_submitting = submitting.clone();
            let save_alive = alive.clone();
            let cancel_submitting = submitting.clone();
            let cancel_alive = alive.clone();
            let close_alive = alive.clone();
            dialog
                .title("删除项目")
                .w(px(480.))
                .close_button(!submitting.get())
                .overlay_closable(!submitting.get())
                .keyboard(!submitting.get())
                .on_close(move |_, _, _| close_alive.set(false))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_3()
                        .child(format!("确定删除「{name}」？"))
                        .child("只删除本机项目配置及其绑定，不删除远端服务或数据。"),
                )
                .footer(
                    div()
                        .flex()
                        .justify_end()
                        .gap_2()
                        .child(
                            Button::new("project-delete-cancel")
                                .label("取消")
                                .disabled(submitting.get())
                                .on_click(move |_, window, cx| {
                                    if !cancel_submitting.get() {
                                        cancel_alive.set(false);
                                        window.close_dialog(cx);
                                    }
                                }),
                        )
                        .child(
                            Button::new("project-delete-confirm")
                                .danger()
                                .label("确认删除")
                                .disabled(submitting.get())
                                .on_click(move |_, window, cx| {
                                    if save_submitting.replace(true) {
                                        return;
                                    }
                                    window.refresh();
                                    if save_view
                                        .update(cx, |view, cx| {
                                            view.delete_project(
                                                id,
                                                dialog_seq,
                                                save_submitting.clone(),
                                                save_alive.clone(),
                                                window,
                                                cx,
                                            );
                                        })
                                        .is_err()
                                    {
                                        save_submitting.set(false);
                                    }
                                }),
                        ),
                )
        });
    }

    fn delete_project(
        &mut self,
        id: i64,
        dialog_seq: u64,
        submitting: Rc<Cell<bool>>,
        alive: Rc<Cell<bool>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.spawn_in(window, async move |view, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { project_store::delete(id) })
                .await;
            submitting.set(false);
            let _ = cx.update(|window, app| {
                let close = view
                    .update(app, |view, cx| {
                        if result.is_ok() {
                            view.projects.retain(|project| project.id != id);
                            if view.project_id == Some(id) {
                                view.project_id = None;
                                view.build_form = None;
                                view.target_form = None;
                            }
                            if view.deploy_project_id == Some(id) {
                                view.clear_batch_preflight();
                                view.deploy_tag_plan_generation =
                                    view.deploy_tag_plan_generation.wrapping_add(1);
                                view.deploy_tag_plan = None;
                                view.deploy_batch_plan = None;
                                view.deploy_tag_plan_loading = false;
                                if let Some(cancel) = view.deploy_target_cancel.take() {
                                    cancel.store(true, Ordering::Relaxed);
                                }
                                view.deploy_target_generation =
                                    view.deploy_target_generation.wrapping_add(1);
                                view.deploy_config_generation =
                                    view.deploy_config_generation.wrapping_add(1);
                                view.deploy_env_generation =
                                    view.deploy_env_generation.wrapping_add(1);
                                view.deploy_project_id = None;
                                view.deploy_host_ids.clear();
                                view.deploy_target_result = None;
                                view.deploy_config_result = None;
                                view.deploy_env_result = None;
                                view.deploy_target_loading = false;
                                view.deploy_config_loading = false;
                                view.deploy_env_loading = false;
                                if view.page == Page::Deploy
                                    && let Some(next) =
                                        view.projects.first().map(|project| project.id)
                                {
                                    view.select_deploy_project(next, window, cx);
                                }
                            }
                        }
                        cx.notify();
                        alive.get() && view.dialog_seq == dialog_seq
                    })
                    .unwrap_or(false);
                if close {
                    window.close_dialog(app);
                }
                match result {
                    Ok(warning) => window.push_notification(
                        match warning {
                            Some(warning) => {
                                Notification::warning(format!("项目已删除；{warning}"))
                            }
                            None => Notification::success("项目本地配置已删除"),
                        },
                        app,
                    ),
                    Err(error) => window.push_notification(Notification::error(error), app),
                }
            });
        })
        .detach();
    }

    fn select_project(&mut self, id: i64, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(project) = self
            .projects
            .iter()
            .find(|project| project.id == id)
            .cloned()
        {
            self.project_id = Some(id);
            self.project_tab = ProjectTab::Build;
            self.build_form = Some(BuildForm::new(&project, window, cx));
            self.target_form = None;
            self.env_form = None;
            self.env_generation = self.env_generation.wrapping_add(1);
            self.invalidate_compose_summary();
            self.service_menu_open = false;
            self.refresh_services(id, cx);
            cx.notify();
        }
    }

    fn refresh_services(&mut self, id: i64, cx: &mut Context<Self>) {
        let Some(project) = self
            .projects
            .iter()
            .find(|project| project.id == id)
            .cloned()
        else {
            return;
        };
        self.service_project_id = Some(id);
        self.service_generation = self.service_generation.wrapping_add(1);
        let generation = self.service_generation;
        self.service_options.clear();
        self.service_error = None;
        self.service_menu_open = false;
        self.service_loading = !project.compose_files.is_empty();
        cx.notify();
        if !self.service_loading {
            return;
        }
        cx.spawn(async move |view, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { compose::inspect_services(&project) })
                .await;
            let _ = view.update(cx, |view, cx| {
                if view.service_project_id != Some(id) || view.service_generation != generation {
                    return;
                }
                view.service_loading = false;
                match result {
                    Ok(options) => view.service_options = options,
                    Err(error) => view.service_error = Some(error),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn select_service(
        &mut self,
        id: i64,
        service: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.service_saving
            || self.compose_saving
            || self.service_project_id != Some(id)
            || !self
                .service_options
                .iter()
                .any(|option| option.name == service && option.uses_project_image)
        {
            return;
        }
        let Some(project) = self
            .projects
            .iter()
            .find(|project| project.id == id)
            .cloned()
        else {
            return;
        };
        self.service_saving = true;
        self.service_menu_open = false;
        cx.notify();
        cx.spawn_in(window, async move |view, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    if !compose::inspect_services(&project)?
                        .iter()
                        .any(|option| option.name == service && option.uses_project_image)
                    {
                        return Err(
                            "目标服务须使用 ${DEVINGPANEL_IMAGE}，且不能包含远端 build。".into(),
                        );
                    }
                    project_store::set_service(id, service)
                })
                .await;
            let _ = cx.update(|window, app| {
                let _ = view.update(app, |view, cx| {
                    view.service_saving = false;
                    if let Ok(project) = &result
                        && let Some(existing) =
                            view.projects.iter_mut().find(|project| project.id == id)
                    {
                        *existing = project.clone();
                    }
                    if result.is_ok() {
                        view.refresh_deploy_if_current(id, cx);
                    }
                    cx.notify();
                });
                match result {
                    Ok(_) => window.push_notification(Notification::success("部署服务已保存"), app),
                    Err(error) => window.push_notification(Notification::error(error), app),
                }
            });
        })
        .detach();
    }

    fn save_build(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.build_saving {
            return;
        }
        let Some(form) = self.build_form.as_ref() else {
            return;
        };
        let id = form.project_id;
        let config = BuildConfig {
            image: form.image.read(cx).value().to_string(),
            tag_rule: form.tag_rule.read(cx).value().to_string(),
            platform: form.platform.clone(),
            build_args: form
                .args
                .iter()
                .map(|row| {
                    (
                        row.key.read(cx).value().to_string(),
                        row.value.read(cx).value().to_string(),
                    )
                })
                .collect(),
        };
        self.build_saving = true;
        cx.notify();
        cx.spawn_in(window, async move |view, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { project_store::update_build(id, config) })
                .await;
            let _ = cx.update(|window, app| {
                let _ = view.update(app, |view, cx| {
                    view.build_saving = false;
                    if let Ok(project) = &result
                        && let Some(existing) = view.projects.iter_mut().find(|p| p.id == id)
                    {
                        *existing = project.clone();
                        if view
                            .build_form
                            .as_ref()
                            .is_some_and(|form| form.project_id == id)
                        {
                            view.build_form = Some(BuildForm::new(project, window, cx));
                        }
                    }
                    if result.is_ok() {
                        view.refresh_deploy_if_current(id, cx);
                    }
                    cx.notify();
                });
                match result {
                    Ok(_) => window.push_notification(Notification::success("构建配置已保存"), app),
                    Err(error) => window.push_notification(Notification::error(error), app),
                }
            });
        })
        .detach();
    }

    fn add_build_arg(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.build_saving {
            return;
        }
        let row = BuildArgRow {
            key: cx.new(|cx| InputState::new(window, cx).placeholder("KEY")),
            value: cx.new(|cx| InputState::new(window, cx).placeholder("VALUE")),
        };
        if let Some(form) = self.build_form.as_mut() {
            form.args.push(row);
            cx.notify();
        }
    }

    fn remove_build_arg(&mut self, index: usize, cx: &mut Context<Self>) {
        if self.build_saving {
            return;
        }
        if let Some(form) = self.build_form.as_mut()
            && index < form.args.len()
        {
            form.args.remove(index);
            cx.notify();
        }
    }

    fn service_selector(&self, project: &Project, cx: &mut Context<Self>) -> impl IntoElement {
        let id = project.id;
        let matching = self.service_project_id == Some(id);
        let mut options = div().flex().flex_col().gap_1();
        if matching {
            for (index, service) in self.service_options.iter().enumerate() {
                let name = service.name.clone();
                let label = if service.uses_project_image {
                    name.clone()
                } else {
                    format!("{} · 未使用项目镜像", service.name)
                };
                options = options.child(
                    Button::new(("service-option", index))
                        .label(label)
                        .disabled(!service.uses_project_image || self.service_saving)
                        .on_click(cx.listener(move |view, _, window, cx| {
                            view.select_service(id, name.clone(), window, cx);
                        })),
                );
            }
        }
        let label = if !project.service.is_empty() {
            project.service.clone()
        } else if project.compose_files.is_empty() {
            "请先添加 Compose 文件".into()
        } else if self.service_loading {
            "正在读取服务…".into()
        } else {
            "请选择部署服务".into()
        };
        let stale = matching
            && !self.service_loading
            && !project.service.is_empty()
            && !self
                .service_options
                .iter()
                .any(|option| option.name == project.service && option.uses_project_image);
        div()
            .w_full()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .id("service-select")
                    .h(px(36.))
                    .w_full()
                    .border_1()
                    .border_color(self.palette().border)
                    .rounded_md()
                    .px_3()
                    .flex()
                    .items_center()
                    .justify_between()
                    .when(matching && !self.service_options.is_empty(), |el| {
                        el.cursor_pointer().on_click(cx.listener(|view, _, _, cx| {
                            view.service_menu_open = !view.service_menu_open;
                            cx.notify();
                        }))
                    })
                    .child(label)
                    .child("⌄"),
            )
            .when(self.service_menu_open && matching, |el| el.child(options))
            .when_some(
                self.service_error.as_ref().filter(|_| matching),
                |el, error| {
                    el.child(
                        div()
                            .text_size(px(12.))
                            .text_color(self.palette().warning)
                            .child(error.clone()),
                    )
                },
            )
            .when(stale, |el| {
                el.child(
                    div()
                        .text_size(px(12.))
                        .text_color(self.palette().warning)
                        .child("部署服务已失效，请重新选择。"),
                )
            })
    }

    fn pick_compose_files(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.compose_saving || self.service_saving {
            return;
        }
        let Some(id) = self.project_id else {
            return;
        };
        self.compose_saving = true;
        cx.notify();
        let picker = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some("选择 Compose 文件".into()),
        });
        cx.spawn_in(window, async move |view, cx| {
            let result = match picker.await {
                Ok(Ok(Some(paths))) if !paths.is_empty() => Some(
                    cx.background_executor()
                        .spawn(async move { project_store::add_compose_files(id, paths) })
                        .await,
                ),
                Ok(Ok(_)) => None,
                Ok(Err(error)) => Some(Err(error.to_string())),
                Err(error) => Some(Err(error.to_string())),
            };
            let _ = cx.update(|window, app| {
                let _ = view.update(app, |view, cx| {
                    view.compose_saving = false;
                    if let Some(Ok(project)) = &result
                        && let Some(existing) = view.projects.iter_mut().find(|p| p.id == id)
                    {
                        *existing = project.clone();
                        if view.project_id == Some(id) {
                            view.invalidate_compose_summary();
                            view.refresh_services(id, cx);
                        }
                    }
                    if matches!(&result, Some(Ok(_))) {
                        view.refresh_deploy_if_current(id, cx);
                    }
                    cx.notify();
                });
                match result {
                    Some(Ok(_)) => {
                        window.push_notification(Notification::success("Compose 文件已保存"), app)
                    }
                    Some(Err(error)) => window.push_notification(Notification::error(error), app),
                    None => {}
                }
            });
        })
        .detach();
    }

    fn set_compose_files(
        &mut self,
        id: i64,
        files: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.compose_saving || self.service_saving {
            return;
        }
        self.compose_saving = true;
        cx.notify();
        cx.spawn_in(window, async move |view, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { project_store::set_compose_files(id, files) })
                .await;
            let _ = cx.update(|window, app| {
                let _ = view.update(app, |view, cx| {
                    view.compose_saving = false;
                    if let Ok(project) = &result
                        && let Some(existing) = view.projects.iter_mut().find(|p| p.id == id)
                    {
                        *existing = project.clone();
                        if view.project_id == Some(id) {
                            view.invalidate_compose_summary();
                            view.refresh_services(id, cx);
                        }
                    }
                    if result.is_ok() {
                        view.refresh_deploy_if_current(id, cx);
                    }
                    cx.notify();
                });
                match result {
                    Ok(_) => {
                        window.push_notification(Notification::success("Compose 顺序已保存"), app)
                    }
                    Err(error) => window.push_notification(Notification::error(error), app),
                }
            });
        })
        .detach();
    }

    fn refresh_compose_status(&mut self, id: i64, cx: &mut Context<Self>) {
        cx.spawn(async move |view, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { project_store::missing_compose_files(id) })
                .await;
            if let Ok((files, missing)) = result {
                let _ = view.update(cx, |view, cx| {
                    if !view.compose_saving
                        && let Some(project) =
                            view.projects.iter_mut().find(|project| project.id == id)
                        && project.compose_files == files
                    {
                        project.missing_compose_files = missing;
                        cx.notify();
                    }
                });
            }
        })
        .detach();
    }

    fn invalidate_compose_summary(&mut self) {
        self.compose_summary_generation = self.compose_summary_generation.wrapping_add(1);
        self.compose_summary_open = false;
        self.compose_summary_loading = false;
        self.compose_summary = None;
    }

    fn toggle_compose_summary(&mut self, cx: &mut Context<Self>) {
        if self.compose_summary_open {
            self.invalidate_compose_summary();
            cx.notify();
            return;
        }
        let Some(project) = self
            .project_id
            .and_then(|id| self.projects.iter().find(|p| p.id == id))
            .cloned()
        else {
            return;
        };
        if project.compose_files.is_empty() || self.compose_saving {
            return;
        }
        self.compose_summary_open = true;
        self.compose_summary_loading = true;
        self.compose_summary = None;
        self.compose_summary_generation = self.compose_summary_generation.wrapping_add(1);
        let generation = self.compose_summary_generation;
        let id = project.id;
        cx.notify();
        cx.spawn(async move |view, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { compose::inspect_summary(&project) })
                .await;
            let _ = view.update(cx, |view, cx| {
                if view.project_id == Some(id) && view.compose_summary_generation == generation {
                    view.compose_summary_loading = false;
                    view.compose_summary = Some(result);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn build_panel(&self, project: &Project, narrow: bool, cx: &mut Context<Self>) -> AnyElement {
        let Some(form) = self.build_form.as_ref() else {
            return self
                .panel(
                    "构建配置",
                    "镜像在开发机构建，不占用 VPS 编译资源。",
                    div().px_6().pb_6().child("正在打开构建配置…"),
                )
                .into_any_element();
        };
        let mut arg_rows = div().flex().flex_col().gap_2();
        for (index, row) in form.args.iter().enumerate() {
            arg_rows = arg_rows.child(
                div()
                    .w_full()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(Input::new(&row.key).w_full().disabled(self.build_saving)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(Input::new(&row.value).w_full().disabled(self.build_saving)),
                    )
                    .child(
                        Button::new(("build-arg-remove", index))
                            .icon(assets::IconName::Trash)
                            .accessibility_label("删除行")
                            .disabled(self.build_saving)
                            .on_click(
                                cx.listener(move |view, _, _, cx| view.remove_build_arg(index, cx)),
                            ),
                    ),
            );
        }
        self.panel(
            "构建配置",
            "镜像在开发机构建，不占用 VPS 编译资源。",
            div()
                .px_6()
                .pb_6()
                .flex()
                .flex_col()
                .gap_5()
                .child(div().text_size(px(12.)).text_color(self.palette().muted).child(format!("本地目录：{}  ·  Dockerfile：{}", project.path, project.dockerfile)))
                .child(
                    div()
                        .flex()
                        .flex_wrap()
                        .gap_4()
                        .child(
                            div()
                                .when(narrow, |el| el.w_full())
                                .when(!narrow, |el| el.w(px(520.)))
                                .child(Self::form_field("镜像名", Input::new(&form.image).w_full().disabled(self.build_saving))),
                        )
                        .child(
                            div()
                                .when(narrow, |el| el.w_full())
                                .when(!narrow, |el| el.w(px(520.)))
                                .child(Self::form_field("Tag 规则", Input::new(&form.tag_rule).w_full().disabled(self.build_saving)))
                                .child(div().text_size(px(12.)).text_color(self.palette().muted).child("支持 {timestamp} / {git-sha}；发布时附加架构后缀，并检查标识冲突。")),
                        )
                        .child(
                            div()
                                .when(narrow, |el| el.w_full())
                                .when(!narrow, |el| el.w(px(520.)))
                                .child(Self::form_field(
                                    "目标平台",
                                    div()
                                        .id("build-platform")
                                        .h(px(36.))
                                        .w_full()
                                        .border_1()
                                        .border_color(self.palette().border)
                                        .rounded_md()
                                        .px_3()
                                        .flex()
                                        .items_center()
                                        .justify_between()
                                        .cursor_pointer()
                                        .on_click(cx.listener(|view, _, _, cx| {
                                            if !view.build_saving && let Some(form) = view.build_form.as_mut() {
                                                form.next_platform();
                                                cx.notify();
                                            }
                                        }))
                                        .child(form.platform_label())
                                        .child("⌄"),
                                )),
                        )
                        .child(
                            div()
                                .when(narrow, |el| el.w_full())
                                .when(!narrow, |el| el.w(px(520.)))
                                .child(Self::form_field(
                                    "部署服务",
                                    self.service_selector(project, cx),
                                )),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_2()
                        .child(div().font_weight(FontWeight::SEMIBOLD).child("Build args"))
                        .child(div().text_color(self.palette().muted).child("构建参数不是秘密传输机制，请勿填写密码或令牌。"))
                        .child(arg_rows)
                        .child(div().flex().child(Button::new("build-arg-add").label("＋ 添加变量").disabled(self.build_saving).on_click(cx.listener(|view, _, window, cx| view.add_build_arg(window, cx))))),
                )
                .child(div().flex().child(Button::new("build-save").primary().label("保存").disabled(self.build_saving).on_click(cx.listener(|view, _, window, cx| view.save_build(window, cx))))),
        )
        .into_any_element()
    }

    fn compose_panel(&self, project: &Project, cx: &mut Context<Self>) -> AnyElement {
        let id = project.id;
        let count = project.compose_files.len();
        let mut summary_rows = div().flex().flex_col().gap_2();
        if let Some(Ok(services)) = &self.compose_summary {
            for service in services {
                let mut ports = if service.ports.is_empty() {
                    String::new()
                } else {
                    service.ports.join(", ")
                };
                if service.hidden_ports > 0 {
                    if !ports.is_empty() {
                        ports.push('；');
                    }
                    ports.push_str(&format!("{} 项未展示", service.hidden_ports));
                }
                if ports.is_empty() {
                    ports = "无".into();
                }
                summary_rows = summary_rows.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(format!("服务 {} · 镜像 {}", service.name, service.image))
                        .child(format!("端口 {ports} · 卷 {} 项", service.volumes)),
                );
            }
        }
        let mut rows = div().flex().flex_col().gap_2();
        for (index, path) in project.compose_files.iter().enumerate() {
            let missing = project.missing_compose_files.contains(path);
            rows = rows.child(
                div()
                    .w_full()
                    .min_w_0()
                    .flex()
                    .items_center()
                    .gap_2()
                    .border_1()
                    .border_color(self.palette().border)
                    .rounded_md()
                    .p_2()
                    .child(
                        div()
                            .text_color(self.palette().muted)
                            .child(format!("{}.", index + 1)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .font_family(mono_font())
                            .child(path.clone())
                            .when(missing, |el| {
                                el.child(
                                    div()
                                        .text_color(self.palette().warning)
                                        .child("文件不存在或已移出项目目录"),
                                )
                            }),
                    )
                    .child(
                        Button::new(("compose-up", index))
                            .icon(assets::IconName::ArrowUp)
                            .accessibility_label("上移")
                            .disabled(index == 0 || self.compose_saving || self.service_saving)
                            .on_click(cx.listener(move |view, _, window, cx| {
                                if let Some(project) = view.projects.iter().find(|p| p.id == id) {
                                    let mut files = project.compose_files.clone();
                                    if index > 0 && index < files.len() {
                                        files.swap(index, index - 1);
                                        view.set_compose_files(id, files, window, cx);
                                    }
                                }
                            })),
                    )
                    .child(
                        Button::new(("compose-down", index))
                            .icon(assets::IconName::ArrowDown)
                            .accessibility_label("下移")
                            .disabled(
                                index + 1 == count || self.compose_saving || self.service_saving,
                            )
                            .on_click(cx.listener(move |view, _, window, cx| {
                                if let Some(project) = view.projects.iter().find(|p| p.id == id) {
                                    let mut files = project.compose_files.clone();
                                    if index + 1 < files.len() {
                                        files.swap(index, index + 1);
                                        view.set_compose_files(id, files, window, cx);
                                    }
                                }
                            })),
                    )
                    .child(
                        Button::new(("compose-remove", index))
                            .icon(assets::IconName::Trash)
                            .accessibility_label("移除")
                            .disabled(self.compose_saving || self.service_saving)
                            .on_click(cx.listener(move |view, _, window, cx| {
                                if let Some(project) = view.projects.iter().find(|p| p.id == id) {
                                    let mut files = project.compose_files.clone();
                                    if index < files.len() {
                                        files.remove(index);
                                        view.set_compose_files(id, files, window, cx);
                                    }
                                }
                            })),
                    ),
            );
        }
        self.panel(
            "Compose",
            "支持多个 compose 文件，按顺序合并（后者覆盖前者）。",
            div()
                .px_6()
                .pb_6()
                .flex()
                .flex_col()
                .gap_4()
                .when(count == 0, |el| el.child(div().text_color(self.palette().muted).child("尚未添加 Compose 文件")))
                .child(rows)
                .child(div().flex().child(Button::new("compose-add").label("＋ 添加 compose 文件").disabled(self.compose_saving || self.service_saving).on_click(cx.listener(|view, _, window, cx| view.pick_compose_files(window, cx)))))
                .child(div().text_color(self.palette().muted).child("首个文件须位于项目根目录；目标服务使用 image: ${DEVINGPANEL_IMAGE}。发布前仍须校验合并配置，禁止远端 build。"))
                .child(div().flex().child(Button::new("compose-summary")
                    .label(if self.compose_summary_open { "▾ 查看部署配置摘要" } else { "▸ 查看部署配置摘要" })
                    .disabled(count == 0 || self.compose_saving)
                    .on_click(cx.listener(|view, _, _, cx| view.toggle_compose_summary(cx)))))
                .when(self.compose_summary_open, |el| el.child(
                    div().w_full().rounded_md().bg(self.palette().accent).p_4()
                        .child(if self.compose_summary_loading {
                            div().child("正在解析本机 Compose 配置…").into_any_element()
                        } else if let Some(Err(error)) = &self.compose_summary {
                            div().text_color(self.palette().warning).child(error.clone()).into_any_element()
                        } else {
                            summary_rows.into_any_element()
                        })
                        .child(div().pt_2().text_color(self.palette().muted)
                    .child("当前解析快照；文件变化后请重新展开。这不是发布预检结果。"))
                )),
        )
        .into_any_element()
    }

    fn new_environment_row(
        &mut self,
        saved: EnvRow,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> EnvFormRow {
        self.env_row_seq = self.env_row_seq.wrapping_add(1);
        let name = cx.new(|cx| InputState::new(window, cx).default_value(saved.name));
        let value = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value(saved.value.unwrap_or_default())
                .placeholder(if saved.credential_id.is_some() {
                    "已安全保存，留空不修改"
                } else {
                    "VALUE"
                })
                .masked(true)
        });
        EnvFormRow {
            id: self.env_row_seq,
            name,
            value,
            sensitive: saved.credential_id.is_some(),
            credential_id: saved.credential_id,
        }
    }

    fn load_environment(
        &mut self,
        project_id: i64,
        host_id: Option<i64>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.env_saving {
            return;
        }
        self.env_generation = self.env_generation.wrapping_add(1);
        let generation = self.env_generation;
        self.env_scope_menu_open = false;
        self.env_loading = true;
        self.env_error = None;
        self.env_form = Some(EnvForm {
            project_id,
            host_id,
            rows: Vec::new(),
        });
        cx.notify();
        cx.spawn_in(window, async move |view, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { env_store::list(project_id, host_id) })
                .await;
            let _ = cx.update(|window, app| {
                let _ = view.update(app, |view, cx| {
                    if view.env_generation != generation || view.project_id != Some(project_id) {
                        return;
                    }
                    view.env_loading = false;
                    match result {
                        Ok((rows, warning)) => {
                            let rows = rows
                                .into_iter()
                                .map(|row| view.new_environment_row(row, window, cx))
                                .collect();
                            view.env_form = Some(EnvForm {
                                project_id,
                                host_id,
                                rows,
                            });
                            if let Some(warning) = warning {
                                view.credential_warning = Some(warning);
                            }
                        }
                        Err(error) => view.env_error = Some(error),
                    }
                    cx.notify();
                });
            });
        })
        .detach();
    }

    fn add_environment_row(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.env_loading || self.env_saving {
            return;
        }
        let mut row = self.new_environment_row(
            EnvRow {
                name: String::new(),
                value: None,
                credential_id: None,
            },
            window,
            cx,
        );
        row.sensitive = true;
        if let Some(form) = self.env_form.as_mut() {
            form.rows.push(row);
            cx.notify();
        }
    }

    fn save_environment(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.env_loading || self.env_saving {
            return;
        }
        let Some(form) = self.env_form.as_ref() else {
            return;
        };
        let project_id = form.project_id;
        let host_id = form.host_id;
        let mut inputs = Vec::new();
        for row in &form.rows {
            let value = row.value.read(cx).value().to_string();
            if !row.sensitive && row.credential_id.is_some() && value.is_empty() {
                window.push_notification(
                    Notification::error("改为非敏感变量时须重新填写值；该值将明文写入 SQLite。"),
                    cx,
                );
                return;
            }
            inputs.push(EnvInput {
                name: row.name.read(cx).value().trim().to_owned(),
                value: if row.sensitive && value.is_empty() && row.credential_id.is_some() {
                    None
                } else {
                    Some(value)
                },
                credential_id: row.credential_id.clone(),
                sensitive: row.sensitive,
            });
        }
        self.env_saving = true;
        cx.notify();
        cx.spawn_in(window, async move |view, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { env_store::save(project_id, host_id, inputs) })
                .await;
            let _ = cx.update(|window, app| {
                let _ = view.update(app, |view, cx| {
                    view.env_saving = false;
                    if let Ok((rows, warning)) = &result {
                        if view.env_form.as_ref().is_some_and(|form| {
                            form.project_id == project_id && form.host_id == host_id
                        }) {
                            let rows = rows
                                .iter()
                                .cloned()
                                .map(|row| view.new_environment_row(row, window, cx))
                                .collect();
                            view.env_form = Some(EnvForm {
                                project_id,
                                host_id,
                                rows,
                            });
                        }
                        if let Some(warning) = warning {
                            view.credential_warning = Some(warning.clone());
                        }
                    }
                    cx.notify();
                });
                match result {
                    Ok(_) => window.push_notification(Notification::success("环境变量已保存"), app),
                    Err(error) => window.push_notification(Notification::error(error), app),
                }
            });
        })
        .detach();
    }

    pub(super) fn open_target_form(&mut self, id: i64) {
        if self.target_saving {
            return;
        }
        let Some(project) = self.projects.iter().find(|project| project.id == id) else {
            return;
        };
        let group = project
            .host_ids
            .first()
            .and_then(|id| self.hosts.iter().find(|host| host.id == *id))
            .or_else(|| self.hosts.first())
            .map(|host| host.group.clone())
            .unwrap_or_default();
        self.target_form = Some(TargetForm {
            project_id: id,
            group,
            host_ids: project.host_ids.clone(),
        });
        self.target_menu_open = false;
    }

    fn save_targets(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.target_saving {
            return;
        }
        let Some(form) = self.target_form.as_ref() else {
            return;
        };
        let id = form.project_id;
        let host_ids = form.host_ids.clone();
        self.target_saving = true;
        cx.notify();
        cx.spawn_in(window, async move |view, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { project_store::set_hosts(id, host_ids) })
                .await;
            let _ = cx.update(|window, app| {
                let _ = view.update(app, |view, cx| {
                    view.target_saving = false;
                    if let Ok((project, _)) = &result
                        && let Some(existing) = view.projects.iter_mut().find(|p| p.id == id)
                    {
                        *existing = project.clone();
                        if view
                            .target_form
                            .as_ref()
                            .is_some_and(|form| form.project_id == id)
                        {
                            view.open_target_form(id);
                        }
                    }
                    if result.is_ok() {
                        view.refresh_deploy_if_current(id, cx);
                    }
                    cx.notify();
                });
                match result {
                    Ok((_, warning)) => window.push_notification(
                        match warning {
                            Some(warning) => {
                                Notification::warning(format!("目标主机已保存；{warning}"))
                            }
                            None => Notification::success("目标主机已保存"),
                        },
                        app,
                    ),
                    Err(error) => window.push_notification(Notification::error(error), app),
                }
            });
        })
        .detach();
    }

    fn targets_panel(&self, project: &Project, cx: &mut Context<Self>) -> impl IntoElement {
        let form = self
            .target_form
            .as_ref()
            .filter(|form| form.project_id == project.id);
        let group = form.map(|form| form.group.as_str()).unwrap_or_default();
        let selected = form
            .map(|form| form.host_ids.as_slice())
            .unwrap_or(&project.host_ids);
        let mut groups: Vec<String> = self.hosts.iter().map(|host| host.group.clone()).collect();
        groups.sort();
        groups.dedup();
        let mut group_options = div().flex().flex_col();
        for (index, option) in groups.iter().enumerate() {
            let value = option.clone();
            group_options = group_options.child(
                Button::new(("target-group-option", index))
                    .w_full()
                    .label(if option.is_empty() {
                        "未分组".into()
                    } else {
                        option.clone()
                    })
                    .on_click(cx.listener(move |view, _, _, cx| {
                        if let Some(form) = view.target_form.as_mut()
                            && form.group != value
                        {
                            form.group = value.clone();
                            form.host_ids.clear();
                        }
                        view.target_menu_open = false;
                        cx.notify();
                    })),
            );
        }
        let mut hosts: Vec<_> = self
            .hosts
            .iter()
            .filter(|host| host.group == group)
            .collect();
        hosts.sort_by_key(|host| {
            selected
                .iter()
                .position(|id| *id == host.id)
                .unwrap_or(usize::MAX)
        });
        let mut rows = div().flex().flex_col();
        for (index, host) in hosts.iter().enumerate() {
            let id = host.id;
            let (status, status_color) = self.host_status(id);
            let checked = selected.contains(&id);
            let name = host.name.clone();
            let view = cx.entity().downgrade();
            rows = rows.child(
                div()
                    .h(px(54.))
                    .min_w(px(760.))
                    .border_t_1()
                    .border_color(self.palette().border)
                    .px_4()
                    .flex()
                    .items_center()
                    .child(
                        div()
                            .w(px(145.))
                            .flex_shrink_0()
                            .flex()
                            .items_center()
                            .justify_end()
                            .gap_1()
                            .child(format!("{}", index + 1))
                            .child(
                                Button::new(("target-up", id as usize))
                                    .icon(assets::IconName::ArrowUp)
                                    .accessibility_label("上移")
                                    .disabled(!checked || index == 0 || self.target_saving)
                                    .on_click(cx.listener(move |view, _, _, cx| {
                                        if let Some(form) = view.target_form.as_mut()
                                            && let Some(position) =
                                                form.host_ids.iter().position(|value| *value == id)
                                            && position > 0
                                        {
                                            form.host_ids.swap(position, position - 1);
                                            cx.notify();
                                        }
                                    })),
                            )
                            .child(
                                Button::new(("target-down", id as usize))
                                    .icon(assets::IconName::ArrowDown)
                                    .accessibility_label("下移")
                                    .disabled(
                                        !checked
                                            || index + 1 >= selected.len()
                                            || self.target_saving,
                                    )
                                    .on_click(cx.listener(move |view, _, _, cx| {
                                        if let Some(form) = view.target_form.as_mut()
                                            && let Some(position) =
                                                form.host_ids.iter().position(|value| *value == id)
                                            && position + 1 < form.host_ids.len()
                                        {
                                            form.host_ids.swap(position, position + 1);
                                            cx.notify();
                                        }
                                    })),
                            ),
                    )
                    .child(
                        div().w(px(75.)).flex_shrink_0().child(
                            base::Checkbox::new(("target-check", id as usize))
                                .checked(checked)
                                .accessibility_label(format!("选择 {name}"))
                                .disabled(self.target_saving)
                                .on_change(move |_, _, _, app| {
                                    let _ = view.update(app, |view, cx| {
                                        if let Some(form) = view.target_form.as_mut() {
                                            if let Some(position) =
                                                form.host_ids.iter().position(|value| *value == id)
                                            {
                                                form.host_ids.remove(position);
                                            } else {
                                                form.host_ids.push(id);
                                            }
                                            cx.notify();
                                        }
                                    });
                                }),
                        ),
                    )
                    .child(
                        div().w(px(85.)).flex_shrink_0().child(
                            div()
                                .flex()
                                .items_center()
                                .gap_1()
                                .px_2()
                                .py_1()
                                .rounded_full()
                                .bg(self.palette().accent)
                                .text_color(status_color)
                                .child(div().size(px(7.)).rounded_full().bg(status_color))
                                .child(status),
                        ),
                    )
                    .child(
                        div()
                            .w(px(145.))
                            .flex_shrink_0()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(host.name.clone()),
                    )
                    .child(
                        div()
                            .w(px(190.))
                            .flex_shrink_0()
                            .child(host.address.clone()),
                    )
                    .child(div().w(px(120.)).flex_shrink_0().child(host.group.clone())),
            );
        }
        self.panel(
            "目标主机",
            "只能选择同一主机组内的主机。",
            div()
                .px_6()
                .pb_6()
                .flex()
                .flex_col()
                .gap_4()
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_2()
                        .child("主机组")
                        .child(
                            div()
                                .id("target-group")
                                .h(px(36.))
                                .w_full()
                                .border_1()
                                .border_color(self.palette().border)
                                .rounded_md()
                                .px_3()
                                .flex()
                                .items_center()
                                .justify_between()
                                .when(!groups.is_empty() && !self.target_saving, |el| {
                                    el.cursor_pointer().on_click(cx.listener(|view, _, _, cx| {
                                        view.target_menu_open = !view.target_menu_open;
                                        cx.notify();
                                    }))
                                })
                                .child(if group.is_empty() {
                                    "未分组".into()
                                } else {
                                    group.to_owned()
                                })
                                .child("⌄"),
                        )
                        .when(self.target_menu_open, |el| el.child(group_options)),
                )
                .child(if !self.host_loaded {
                    div().child("正在读取主机…").into_any_element()
                } else if let Some(error) = &self.host_error {
                    div()
                        .text_color(self.palette().destructive)
                        .child(error.clone())
                        .into_any_element()
                } else if hosts.is_empty() {
                    div()
                        .text_color(self.palette().muted)
                        .child("该主机组暂无主机，请先添加主机。")
                        .into_any_element()
                } else {
                    div()
                        .id("target-table-scroll")
                        .overflow_x_scroll()
                        .border_1()
                        .border_color(self.palette().border)
                        .rounded_lg()
                        .child(
                            div()
                                .min_w(px(760.))
                                .h(px(42.))
                                .px_4()
                                .flex()
                                .items_center()
                                .text_color(self.palette().muted)
                                .child(div().w(px(145.)).flex_shrink_0().child("顺序"))
                                .child(div().w(px(75.)).flex_shrink_0().child("选择"))
                                .child(div().w(px(85.)).flex_shrink_0().child("状态"))
                                .child(div().w(px(145.)).flex_shrink_0().child("主机名"))
                                .child(div().w(px(190.)).flex_shrink_0().child("地址"))
                                .child("分组"),
                        )
                        .child(rows)
                        .into_any_element()
                })
                .child(div().text_color(self.palette().muted).child(
                    "发布时按列表顺序逐台部署，健康检查通过后进入下一台；并行度大于 1 时按批处理。",
                ))
                .child(
                    div()
                        .text_color(self.palette().warning)
                        .child("取消绑定主机并保存后，该主机的专属环境变量也会从本机配置中移除。"),
                )
                .child(
                    div().flex().child(
                        Button::new("targets-save")
                            .primary()
                            .label("保存")
                            .disabled(
                                self.target_saving
                                    || !self.host_loaded
                                    || self.host_error.is_some()
                                    || form.is_none(),
                            )
                            .on_click(
                                cx.listener(|view, _, window, cx| view.save_targets(window, cx)),
                            ),
                    ),
                ),
        )
    }

    fn confirm_plaintext_environment(
        &mut self,
        row_id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let view = cx.weak_entity();
        window.open_dialog(cx, move |dialog, _, _| {
            let view = view.clone();
            dialog
                .title("关闭敏感存储？")
                .w(px(480.))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_3()
                        .child("此变量的新值会以明文写入本机 SQLite。")
                        .child("已安全保存的旧值不会自动回填；请重新填写要保存的非敏感值。"),
                )
                .footer(
                    div()
                        .flex()
                        .justify_end()
                        .gap_2()
                        .child(
                            Button::new("env-plain-cancel")
                                .label("取消")
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            Button::new("env-plain-confirm")
                                .danger()
                                .label("关闭敏感")
                                .on_click(move |_, window, cx| {
                                    let _ = view.update(cx, |view, cx| {
                                        if let Some(form) = view.env_form.as_mut()
                                            && let Some(row) =
                                                form.rows.iter_mut().find(|row| row.id == row_id)
                                        {
                                            row.sensitive = false;
                                            cx.notify();
                                        }
                                    });
                                    window.close_dialog(cx);
                                }),
                        ),
                )
        });
    }

    fn environment_panel(&self, project: &Project, cx: &mut Context<Self>) -> impl IntoElement {
        let form = self
            .env_form
            .as_ref()
            .filter(|form| form.project_id == project.id);
        let host_id = form.and_then(|form| form.host_id);
        let scope = host_id
            .and_then(|id| {
                self.hosts
                    .iter()
                    .find(|host| host.id == id)
                    .map(|host| host.name.clone())
            })
            .unwrap_or_else(|| "项目通用".into());
        let mut scope_options = div().flex().flex_col();
        let mut scopes = vec![(None, "项目通用".to_owned())];
        for id in &project.host_ids {
            if let Some(host) = self.hosts.iter().find(|host| host.id == *id) {
                scopes.push((Some(*id), host.name.clone()));
            }
        }
        for (index, (target, label)) in scopes.into_iter().enumerate() {
            let id = project.id;
            scope_options = scope_options.child(
                Button::new(("env-scope-option", index))
                    .w_full()
                    .label(label)
                    .on_click(cx.listener(move |view, _, window, cx| {
                        view.load_environment(id, target, window, cx);
                    })),
            );
        }
        let mut rows = div().flex().flex_col().gap_2();
        if let Some(form) = form {
            for row in &form.rows {
                let row_id = row.id;
                let checked = row.sensitive;
                let view = cx.entity().downgrade();
                rows = rows.child(
                    div()
                        .min_w(px(680.))
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .w(px(200.))
                                .child(Input::new(&row.name).w_full().disabled(self.env_saving)),
                        )
                        .child(
                            div().flex_1().min_w_0().child(
                                Input::new(&row.value)
                                    .w_full()
                                    .mask_toggle()
                                    .disabled(self.env_saving),
                            ),
                        )
                        .child(
                            div()
                                .w(px(85.))
                                .flex_shrink_0()
                                .flex()
                                .items_center()
                                .gap_1()
                                .child(
                                    base::Checkbox::new(("env-sensitive", row_id as usize))
                                        .checked(checked)
                                        .accessibility_label("敏感")
                                        .disabled(self.env_saving)
                                        .on_change(move |_, _, window, app| {
                                            let _ = view.update(app, |view, cx| {
                                                if checked {
                                                    view.confirm_plaintext_environment(
                                                        row_id, window, cx,
                                                    );
                                                } else if let Some(form) = view.env_form.as_mut()
                                                    && let Some(row) = form
                                                        .rows
                                                        .iter_mut()
                                                        .find(|row| row.id == row_id)
                                                {
                                                    row.sensitive = true;
                                                    cx.notify();
                                                }
                                            });
                                        }),
                                )
                                .child("敏感"),
                        )
                        .child(
                            Button::new(("env-remove", row_id as usize))
                                .icon(assets::IconName::Trash)
                                .accessibility_label("删除行")
                                .disabled(self.env_saving)
                                .on_click(cx.listener(move |view, _, _, cx| {
                                    if let Some(form) = view.env_form.as_mut() {
                                        form.rows.retain(|row| row.id != row_id);
                                        cx.notify();
                                    }
                                })),
                        ),
                );
            }
        }
        self.panel("环境变量", "主机专属值覆盖项目通用值；敏感值不写入 SQLite。",
            div().px_6().pb_6().flex().flex_col().gap_4()
                .child(div().flex().flex_col().gap_2().child("配置范围")
                    .child(div().id("env-scope").h(px(36.)).w_full().border_1()
                        .border_color(self.palette().border).rounded_md().px_3().flex()
                        .items_center().justify_between()
                        .when(!self.env_saving && !self.env_loading, |el| el.cursor_pointer()
                            .on_click(cx.listener(|view, _, _, cx| {
                                view.env_scope_menu_open = !view.env_scope_menu_open;
                                cx.notify();
                            })))
                        .child(scope).child("⌄"))
                    .when(self.env_scope_menu_open, |el| el.child(scope_options)))
                .child(if self.env_loading { div().child("正在读取环境变量…").into_any_element() }
                    else if let Some(error) = &self.env_error {
                        div().text_color(self.palette().destructive).child(error.clone()).into_any_element()
                    } else { div().id("env-rows-scroll").overflow_x_scroll().child(rows).into_any_element() })
                .child(div().flex().child(Button::new("env-add").label("＋ 添加变量")
                    .disabled(self.env_loading || self.env_saving || self.env_error.is_some())
                    .on_click(cx.listener(|view, _, window, cx| view.add_environment_row(window, cx)))))
                .child(div().rounded_md().bg(self.palette().accent).p_3()
                    .text_color(self.palette().muted)
                    .child("部署时写入远端 .env（权限 600），不纳入镜像。关闭“敏感”后，新值会以非秘密配置写入 SQLite；已保存的秘密需重新填写。"))
                .child(div().flex().child(Button::new("env-save").primary().label("保存")
                    .disabled(self.env_loading || self.env_saving || self.env_error.is_some() || form.is_none())
                    .on_click(cx.listener(|view, _, window, cx| view.save_environment(window, cx))))))
    }

    fn manifest_panel(&self, project: &Project) -> AnyElement {
        if let Some(error) = &project.config_error {
            return self
                .panel(
                    "项目配置待修复",
                    "请联系技术人员修复项目文件，或由管理员预先配置目标主机与凭据。",
                    div()
                        .px_6()
                        .pb_6()
                        .text_color(self.palette().destructive)
                        .child(error.clone()),
                )
                .into_any_element();
        }
        let (title, content) = match self.project_tab {
            ProjectTab::Build => (
                "构建配置",
                match crate::project_config::load(std::path::Path::new(&project.path)) {
                    Ok(manifest) => format!(
                        "共 {} 个镜像  ·  Tag：{}  ·  {}",
                        manifest.builds.len(),
                        manifest.tag_rule,
                        manifest
                            .builds
                            .iter()
                            .map(|build| format!(
                                "{}：{}  ·  {}  ·  {}",
                                build.service, build.image, build.platform, build.dockerfile
                            ))
                            .collect::<Vec<_>>()
                            .join("  /  ")
                    ),
                    Err(_) => "配置已变化，请重新读取项目。".into(),
                },
            ),
            ProjectTab::Compose => (
                "Compose",
                format!("按顺序合并：{}", project.compose_files.join("  →  ")),
            ),
            ProjectTab::Targets => (
                "目标主机",
                format!(
                    "按顺序发布：{}",
                    project
                        .host_ids
                        .iter()
                        .filter_map(|id| self
                            .hosts
                            .iter()
                            .find(|host| host.id == *id)
                            .map(|host| format!("{}/{}", host.group, host.name)))
                        .collect::<Vec<_>>()
                        .join("  →  ")
                ),
            ),
            _ => ("项目配置", String::new()),
        };
        self.panel(
            title,
            "配置由项目目录的 devingpanel.yaml 提供；此处只读。",
            div().px_6().pb_6().whitespace_normal().child(content),
        )
        .into_any_element()
    }

    pub(super) fn refresh_release_tasks(
        &mut self,
        project_id: Option<i64>,
        cx: &mut Context<Self>,
    ) {
        match project_id {
            Some(id) if !self.release_project_loading.insert(id) => {
                self.release_project_refresh_pending.insert(id);
                return;
            }
            None if self.release_tasks_loading => {
                self.release_tasks_refresh_pending = true;
                return;
            }
            None => self.release_tasks_loading = true,
            _ => {}
        }
        self.retention_epoch = self.retention_epoch.wrapping_add(1);
        self.retention_reports.clear();
        self.retention_loading.clear();
        self.local_archive_previews.clear();
        self.local_archive_preview_loading.clear();
        self.current_version_checks.clear();
        self.current_version_loading.clear();
        self.release_upload_inspection_generation =
            self.release_upload_inspection_generation.wrapping_add(1);
        self.release_upload_inspections.clear();
        self.release_upload_inspecting.clear();
        cx.notify();
        cx.spawn(async move |view, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    Ok::<_, String>((
                        release_store::recent(project_id)?,
                        rollback_store::recent(project_id)?,
                        batch_store::recent(project_id)?,
                    ))
                })
                .await;
            let _ = view.update(cx, |view, cx| {
                match (project_id, result) {
                    (Some(id), Ok((tasks, rollbacks, batches))) => {
                        view.release_project_loading.remove(&id);
                        view.release_project_tasks.insert(id, tasks);
                        view.rollback_project_tasks.insert(id, rollbacks);
                        view.release_project_batches.insert(id, batches);
                        view.release_project_errors.remove(&id);
                        view.retention_reports.remove(&id);
                    }
                    (Some(id), Err(error)) => {
                        view.release_project_loading.remove(&id);
                        view.release_project_tasks.remove(&id);
                        view.rollback_project_tasks.remove(&id);
                        view.release_project_batches.remove(&id);
                        view.release_project_errors.insert(id, error);
                    }
                    (None, Ok((tasks, rollbacks, batches))) => {
                        view.release_tasks_loading = false;
                        view.release_tasks = tasks;
                        view.rollback_tasks = rollbacks;
                        view.release_batches = batches;
                        view.release_tasks_error = None;
                    }
                    (None, Err(error)) => {
                        view.release_tasks_loading = false;
                        view.release_tasks.clear();
                        view.rollback_tasks.clear();
                        view.release_batches.clear();
                        view.release_tasks_error = Some(error);
                    }
                }
                view.previous_version_checks.clear();
                view.retention_epoch = view.retention_epoch.wrapping_add(1);
                view.retention_reports.clear();
                view.retention_loading.clear();
                view.local_archive_previews.clear();
                view.local_archive_preview_loading.clear();
                view.current_version_checks.clear();
                view.current_version_loading.clear();
                cx.notify();
                let repeat = match project_id {
                    Some(id) => view.release_project_refresh_pending.remove(&id),
                    None => std::mem::take(&mut view.release_tasks_refresh_pending),
                };
                if repeat {
                    view.refresh_release_tasks(project_id, cx);
                }
            });
        })
        .detach();
    }

    fn inspect_release_retention(&mut self, project_id: i64, cx: &mut Context<Self>) {
        if !self.retention_loading.insert(project_id) {
            return;
        }
        let epoch = self.retention_epoch;
        self.retention_reports.remove(&project_id);
        cx.notify();
        cx.spawn(async move |view, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { release_store::retention_plan(project_id) })
                .await;
            let _ = view.update(cx, |view, cx| {
                if view.retention_epoch != epoch {
                    return;
                }
                view.retention_loading.remove(&project_id);
                view.retention_reports.insert(project_id, result);
                cx.notify();
            });
        })
        .detach();
    }

    fn inspect_local_archive_preview(&mut self, project_id: i64, cx: &mut Context<Self>) {
        if !self.local_archive_preview_loading.insert(project_id) {
            return;
        }
        let epoch = self.retention_epoch;
        self.local_archive_previews.remove(&project_id);
        cx.notify();
        cx.spawn(async move |view, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { release_archive::preview_local_reclaim(project_id) })
                .await;
            let _ = view.update(cx, |view, cx| {
                if view.retention_epoch != epoch {
                    return;
                }
                view.local_archive_preview_loading.remove(&project_id);
                view.local_archive_previews.insert(project_id, result);
                cx.notify();
            });
        })
        .detach();
    }

    fn inspect_current_version(&mut self, id: String, cx: &mut Context<Self>) {
        if !self.current_version_loading.insert(id.clone()) {
            return;
        }
        let epoch = self.retention_epoch;
        self.current_version_checks.remove(&id);
        cx.notify();
        cx.spawn(async move |view, cx| {
            let key = id.clone();
            let result = cx
                .background_executor()
                .spawn(async move { release::inspect_current_version(&id) })
                .await;
            let _ = view.update(cx, |view, cx| {
                if view.retention_epoch != epoch {
                    return;
                }
                view.current_version_loading.remove(&key);
                view.current_version_checks.insert(key, result);
                cx.notify();
            });
        })
        .detach();
    }

    fn inspect_release_task(&mut self, id: String, cx: &mut Context<Self>) {
        if !self.release_checking.insert(id.clone()) {
            return;
        }
        self.release_observations.remove(&id);
        cx.notify();
        cx.spawn(async move |view, cx| {
            let key = id.clone();
            let result = cx
                .background_executor()
                .spawn(async move {
                    release::observe_task(&id).map(|(_, observation)| observation.containers)
                })
                .await;
            let _ = view.update(cx, |view, cx| {
                view.release_checking.remove(&key);
                view.release_observations.insert(key, result);
                cx.notify();
            });
        })
        .detach();
    }

    fn inspect_release_materials(&mut self, id: String, cx: &mut Context<Self>) {
        if !self.release_material_checking.insert(id.clone()) {
            return;
        }
        self.release_material_checks.remove(&id);
        cx.notify();
        cx.spawn(async move |view, cx| {
            let key = id.clone();
            let result = cx
                .background_executor()
                .spawn(async move {
                    let snapshot = release_materials::load(&id)?;
                    let services = release_store::planned_archive_services(&id)?;
                    let mut archived = 0;
                    for (service, has_archive) in &services {
                        if *has_archive {
                            crate::release_archive::verify(&id, service)?;
                            archived += 1;
                        }
                    }
                    Ok::<_, String>(MaterialCheck {
                        compose_files: snapshot.compose_files.len(),
                        has_env: !snapshot.env_file.is_empty(),
                        archives: archived,
                        planned_archives: services.len(),
                    })
                })
                .await;
            let _ = view.update(cx, |view, cx| {
                view.release_material_checking.remove(&key);
                view.release_material_checks.insert(key, result);
                cx.notify();
            });
        })
        .detach();
    }

    fn inspect_release_images(&mut self, id: String, cx: &mut Context<Self>) {
        if !self.release_image_checking.insert(id.clone()) {
            return;
        }
        self.release_image_checks.remove(&id);
        cx.notify();
        cx.spawn(async move |view, cx| {
            let key = id.clone();
            let result = cx
                .background_executor()
                .spawn(async move { release::observe_task_images(&id) })
                .await;
            let _ = view.update(cx, |view, cx| {
                view.release_image_checking.remove(&key);
                view.release_image_checks.insert(key, result);
                cx.notify();
            });
        })
        .detach();
    }

    fn inspect_release_upload(&mut self, id: String, cx: &mut Context<Self>) {
        let eligible = self
            .release_tasks
            .iter()
            .chain(
                self.release_project_tasks
                    .values()
                    .flat_map(|tasks| tasks.iter()),
            )
            .any(|task| task.id == id && task.phase == "uploading" && task.outcome == "unknown");
        if !eligible || !self.release_upload_inspecting.insert(id.clone()) {
            return;
        }
        let generation = self.release_upload_inspection_generation;
        self.release_upload_inspections.remove(&id);
        cx.notify();
        cx.spawn(async move |view, cx| {
            let key = id.clone();
            let result = cx
                .background_executor()
                .spawn(async move { release::inspect_unknown_upload(&id) })
                .await;
            let _ = view.update(cx, |view, cx| {
                if view.release_upload_inspection_generation != generation {
                    return;
                }
                view.release_upload_inspecting.remove(&key);
                view.release_upload_inspections.insert(key, result);
                cx.notify();
            });
        })
        .detach();
    }

    fn confirm_resume_unknown_upload(
        &mut self,
        task_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(check) = self
            .release_upload_inspections
            .get(&task_id)
            .and_then(|result| result.as_ref().ok())
        else {
            return;
        };
        if !resumable_upload_inspection(check) {
            return;
        }
        let Some(batch_id) = self
            .release_batches
            .iter()
            .chain(
                self.release_project_batches
                    .values()
                    .flat_map(|items| items.iter()),
            )
            .find(|batch| {
                batch.state == "unknown"
                    && batch.targets.iter().any(|target| {
                        target.state == "unknown"
                            && target.release_task_id.as_deref() == Some(&task_id)
                    })
            })
            .map(|batch| batch.id.clone())
        else {
            return;
        };
        if self.batch_resolving.contains(&batch_id) || self.deploy_run_busy {
            return;
        }
        let view = cx.weak_entity();
        window.open_dialog(cx, move |dialog, _, _| {
            let confirm_view = view.clone();
            let batch = batch_id.clone();
            let task = task_id.clone();
            dialog.title("确认断点续传当前主机").w(px(560.))
                .child(div().flex().flex_col().gap_3()
                    .child("将重新核对 YAML、目标主机与 Docker daemon、版本材料、远端容器和镜像；部分 tar 必须与本机归档前缀 SHA-256 一致，否则不写入。")
                    .child("只续传当前主机并继续加载与整栈健康检查；成功后仍须人工确认健康，不会自动部署下一台。网络中断时继续保留结果待核实和远端 tar，不自动重试。"))
                .footer(div().flex().justify_end().gap_2()
                    .child(Button::new("batch-resume-cancel").debug_selector(|| "batch-resume-cancel".into()).label("取消")
                        .on_click(|_, window, cx| window.close_dialog(cx)))
                    .child(Button::new("batch-resume-confirm").primary().label("确认核对并续传")
                        .on_click(move |_, window, cx| {
                            window.close_dialog(cx);
                            let batch = batch.clone();
                            let task = task.clone();
                            let _ = confirm_view.update(cx, |view, cx| view.start_resume_unknown_batch(batch, task, window, cx));
                        })))
        });
    }

    fn watch_batch_stage(
        &mut self,
        poll_id: String,
        poll_stage: Arc<Mutex<String>>,
        cx: &mut Context<Self>,
    ) {
        cx.spawn(async move |view, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(500))
                    .await;
                let next = poll_stage
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .clone();
                let active = view
                    .update(cx, |view, cx| {
                        if !view.batch_resolving.contains(&poll_id) {
                            return false;
                        }
                        if !next.is_empty()
                            && view.batch_operation_stage.get(&poll_id) != Some(&next)
                        {
                            view.batch_operation_stage.insert(poll_id.clone(), next);
                            cx.notify();
                        }
                        true
                    })
                    .unwrap_or(false);
                if !active {
                    break;
                }
            }
        })
        .detach();
    }

    fn start_resume_unknown_batch(
        &mut self,
        batch_id: String,
        task_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(project_id) = self
            .release_batches
            .iter()
            .chain(
                self.release_project_batches
                    .values()
                    .flat_map(|items| items.iter()),
            )
            .find(|batch| batch.id == batch_id && batch.state == "unknown")
            .map(|batch| batch.project_id)
        else {
            return;
        };
        if self.deploy_run_busy || !self.batch_resolving.insert(batch_id.clone()) {
            return;
        }
        self.batch_operation_stage
            .insert(batch_id.clone(), "重新核对批次材料…".into());
        cx.notify();
        let stage = Arc::new(Mutex::new(String::new()));
        let worker_stage = stage.clone();
        self.watch_batch_stage(batch_id.clone(), stage, cx);
        cx.spawn_in(window, async move |view, cx| {
            let run_id = batch_id.clone();
            let result = cx
                .background_executor()
                .spawn(async move {
                    stack_release::resume_unknown_batch_upload(
                        &run_id,
                        &task_id,
                        Arc::new(AtomicBool::new(false)),
                        |event| {
                            *worker_stage
                                .lock()
                                .unwrap_or_else(|poisoned| poisoned.into_inner()) = event.into();
                        },
                    )
                })
                .await;
            let _ = cx.update(|window, app| {
                let _ = view.update(app, |view, cx| {
                    view.batch_resolving.remove(&batch_id);
                    view.batch_operation_stage.remove(&batch_id);
                    view.refresh_release_tasks(None, cx);
                    view.refresh_release_tasks(Some(project_id), cx);
                    cx.notify();
                });
                window.push_notification(
                    match result {
                        Ok(stack_release::BatchProgress::AwaitingConfirmation { .. }) => {
                            Notification::success(
                                "当前主机已通过健康检查；请在历史中人工确认后再继续",
                            )
                        }
                        Ok(stack_release::BatchProgress::Complete { .. }) => {
                            Notification::success("批次已完成")
                        }
                        Err(error) => Notification::warning(format!(
                            "批次 {}：{}",
                            batch_id.chars().take(8).collect::<String>(),
                            error.message
                        )),
                    },
                    app,
                );
            });
        })
        .detach();
    }

    fn confirm_continue_batch(
        &mut self,
        batch_id: String,
        expected_host_id: i64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(batch) = self
            .release_batches
            .iter()
            .chain(
                self.release_project_batches
                    .values()
                    .flat_map(|items| items.iter()),
            )
            .find(|batch| batch.id == batch_id)
        else {
            return;
        };
        let next = next_confirmed_batch_host(batch);
        if next != Some(expected_host_id)
            || self.batch_resolving.contains(&batch_id)
            || self.deploy_run_busy
        {
            return;
        }
        let host_name = batch
            .targets
            .iter()
            .find(|target| target.host.id == expected_host_id)
            .map(|target| target.host.name.clone())
            .unwrap_or_default();
        let view = cx.weak_entity();
        window.open_dialog(cx, move |dialog, _, _| {
            let confirm_view = view.clone();
            let id = batch_id.clone();
            dialog.title("确认继续下一台主机").w(px(560.))
                .child(div().flex().flex_col().gap_3()
                    .child(format!("下一台：{host_name}。此前已健康的主机不会重建或回滚。"))
                    .child("将重新核对 YAML、逐主机环境、共享归档、可信 SSH 与 Docker daemon；然后上传镜像、启动完整 Compose 栈并核实全部服务。若出现失败或未知结果会暂停，不自动推进。"))
                .footer(div().flex().justify_end().gap_2()
                    .child(Button::new("batch-continue-cancel")
                        .debug_selector(|| "batch-continue-cancel".into()).label("取消")
                        .on_click(|_, window, cx| window.close_dialog(cx)))
                    .child(Button::new("batch-continue-confirm")
                        .debug_selector(|| "batch-continue-confirm".into())
                        .primary().label("确认继续")
                        .on_click(move |_, window, cx| {
                            window.close_dialog(cx);
                            let id = id.clone();
                            let _ = confirm_view.update(cx, |view, cx| {
                                view.start_continue_batch(id, expected_host_id, window, cx)
                            });
                        })))
        });
    }

    fn start_continue_batch(
        &mut self,
        batch_id: String,
        expected_host_id: i64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(batch) = self
            .release_batches
            .iter()
            .chain(
                self.release_project_batches
                    .values()
                    .flat_map(|items| items.iter()),
            )
            .find(|batch| batch.id == batch_id)
        else {
            return;
        };
        let next = next_confirmed_batch_host(batch);
        if self.deploy_run_busy
            || next != Some(expected_host_id)
            || !self.batch_resolving.insert(batch_id.clone())
        {
            return;
        }
        let project_id = batch.project_id;
        self.batch_operation_stage
            .insert(batch_id.clone(), "重新确认批次目标…".into());
        let stage = Arc::new(Mutex::new(String::new()));
        let worker_stage = stage.clone();
        self.watch_batch_stage(batch_id.clone(), stage, cx);
        cx.notify();
        cx.spawn_in(window, async move |view, cx| {
            let run_id = batch_id.clone();
            let result = cx
                .background_executor()
                .spawn(async move {
                    let cancel = Arc::new(AtomicBool::new(false));
                    let confirmed = stack_release::reconfirm_batch(&run_id, cancel.clone())
                        .map_err(|message| stack_release::BatchRunError {
                            batch_id: Some(run_id.clone()),
                            message,
                        })?;
                    stack_release::continue_batch_sequential(
                        &run_id,
                        Some(expected_host_id),
                        &confirmed,
                        cancel,
                        |event| {
                            *worker_stage
                                .lock()
                                .unwrap_or_else(|poisoned| poisoned.into_inner()) = event.into();
                        },
                    )
                })
                .await;
            let _ = cx.update(|window, app| {
                let _ = view.update(app, |view, cx| {
                    view.batch_resolving.remove(&batch_id);
                    view.batch_operation_stage.remove(&batch_id);
                    view.refresh_release_tasks(None, cx);
                    view.refresh_release_tasks(Some(project_id), cx);
                    cx.notify();
                });
                window.push_notification(
                    match result {
                        Ok(stack_release::BatchProgress::AwaitingConfirmation { .. }) => {
                            Notification::success("当前主机健康检查通过；请人工确认后再继续")
                        }
                        Ok(stack_release::BatchProgress::Complete { .. }) => {
                            Notification::success("批次全部目标已处理完毕")
                        }
                        Err(error) => Notification::warning(format!(
                            "批次 {}：{}",
                            batch_id.chars().take(8).collect::<String>(),
                            error.message
                        )),
                    },
                    app,
                );
            });
        })
        .detach();
    }

    fn inspect_previous_version(&mut self, id: String, cx: &mut Context<Self>) {
        if !self.previous_version_checking.insert(id.clone()) {
            return;
        }
        self.previous_version_checks.remove(&id);
        cx.notify();
        cx.spawn(async move |view, cx| {
            let key = id.clone();
            let result = cx
                .background_executor()
                .spawn(async move { release::inspect_previous_version(&id) })
                .await;
            let _ = view.update(cx, |view, cx| {
                view.previous_version_checking.remove(&key);
                view.previous_version_checks.insert(key, result);
                cx.notify();
            });
        })
        .detach();
    }

    fn retry_prepared_recovery(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.release_recovery_busy {
            return;
        }
        self.release_recovery_busy = true;
        cx.notify();
        cx.spawn_in(window, async move |view, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { release_materials::recover_prepared() })
                .await;
            let _ = cx.update(|window, app| {
                let _ = view.update(app, |view, cx| {
                    view.release_recovery_busy = false;
                    view.release_recovery_error = result.as_ref().err().cloned();
                    view.refresh_release_tasks(None, cx);
                    if let Some(project_id) = view.project_id {
                        view.refresh_release_tasks(Some(project_id), cx);
                    }
                    cx.notify();
                });
                window.push_notification(
                    match result {
                        Ok(count) => {
                            Notification::success(format!("已清理 {count} 条构建前中断任务"))
                        }
                        Err(error) => Notification::warning(error),
                    },
                    app,
                );
            });
        })
        .detach();
    }

    fn inspect_interrupted_batches(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.release_recovery_busy {
            return;
        }
        self.release_recovery_busy = true;
        cx.notify();
        cx.spawn_in(window, async move |view, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { release_materials::recover_interrupted_batches() })
                .await;
            let _ = cx.update(|window, app| {
                let _ = view.update(app, |view, cx| {
                    view.release_recovery_busy = false;
                    view.release_recovery_error = result.as_ref().err().cloned();
                    view.refresh_release_tasks(None, cx);
                    if let Some(project_id) = view.project_id {
                        view.refresh_release_tasks(Some(project_id), cx);
                    }
                    cx.notify();
                });
                window.push_notification(
                    match result {
                        Ok(count) => Notification::success(format!(
                            "{count} 条无执行锁的中断批次已标为待核实；未操作 VPS"
                        )),
                        Err(error) => Notification::warning(error),
                    },
                    app,
                );
            });
        })
        .detach();
    }

    fn confirm_close_absent_task(
        &mut self,
        id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let eligible = self
            .release_tasks
            .iter()
            .chain(
                self.release_project_tasks
                    .values()
                    .flat_map(|tasks| tasks.iter()),
            )
            .any(|task| {
                task.id == id
                    && task.docker_endpoint.is_some()
                    && task.docker_daemon_id.is_some()
                    && matches!(
                        (task.phase.as_str(), task.outcome.as_str()),
                        ("checked", "blocked") | ("checking", "unknown")
                    )
            });
        let observed_absent = self
            .release_observations
            .get(&id)
            .is_some_and(|result| result.as_ref().is_ok_and(Vec::is_empty));
        if !eligible || !observed_absent || self.release_resolving.contains(&id) {
            return;
        }
        let view = cx.weak_entity();
        window.open_dialog(cx, move |dialog, _, _| {
            let confirm_view = view.clone();
            let task_id = id.clone();
            dialog.title("确认关闭待核实任务").w(px(520.))
                .child(div().flex().flex_col().gap_3()
                    .child("只读核实未发现该 Compose 项目的容器。确认后会再次连接并核实；仅在仍无容器、任务阶段未变化时记录人工决定并解除项目锁定。")
                    .child("这不会删除远端镜像或部署文件，不会执行 Compose，也不代表先前部署成功。"))
                .footer(div().flex().justify_end().gap_2()
                    .child(Button::new("release-absence-cancel").label("取消")
                        .on_click(|_, window, cx| window.close_dialog(cx)))
                    .child(Button::new("release-absence-confirm").danger().label("确认无容器并关闭")
                        .on_click(move |_, window, cx| {
                            window.close_dialog(cx);
                            let id = task_id.clone();
                            let _ = confirm_view.update(cx, |view, cx| view.close_absent_task(id, window, cx));
                        })))
        });
    }

    fn close_absent_task(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(task_project_id) = self
            .release_tasks
            .iter()
            .chain(
                self.release_project_tasks
                    .values()
                    .flat_map(|tasks| tasks.iter()),
            )
            .find(|task| task.id == id)
            .map(|task| task.project_id)
        else {
            return;
        };
        if !self.release_resolving.insert(id.clone()) {
            return;
        }
        cx.notify();
        cx.spawn_in(window, async move |view, cx| {
            let key = id.clone();
            let result = cx
                .background_executor()
                .spawn(async move { release::confirm_absent_and_close(&id) })
                .await;
            let _ = cx.update(|window, app| {
                let _ = view.update(app, |view, cx| {
                    view.release_resolving.remove(&key);
                    view.release_observations.remove(&key);
                    if result.is_ok() {
                        view.refresh_release_tasks(None, cx);
                        view.refresh_release_tasks(Some(task_project_id), cx);
                    }
                    cx.notify();
                });
                window.push_notification(
                    match result {
                        Ok(()) => Notification::success("已记录人工核实并解除项目锁定"),
                        Err(error) => Notification::error(error),
                    },
                    app,
                );
            });
        })
        .detach();
    }

    fn confirm_close_predeploy_task(
        &mut self,
        id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let eligible = self
            .release_tasks
            .iter()
            .chain(
                self.release_project_tasks
                    .values()
                    .flat_map(|tasks| tasks.iter()),
            )
            .any(|task| task.id == id && task.phase == "uploading" && task.outcome == "unknown");
        if !eligible
            || self.release_resolving.contains(&id)
            || self.deploy_run_busy
            || self.rollback_running.is_some()
        {
            return;
        }
        let view = cx.weak_entity();
        window.open_dialog(cx, move |dialog, _, _| {
            let confirm_view = view.clone();
            let task_id = id.clone();
            dialog.title("核实并关闭上传中断任务").w(px(540.))
                .child(div().flex().flex_col().gap_3()
                    .child("该任务停在镜像传输阶段，尚未进入远端镜像加载或 Compose 启动。将重新核对当前版本指针、可信 Docker daemon、历史 Compose/.env 与逐服务配置哈希、镜像 ID 和健康状态。")
                    .child("请在无外部 Docker 修改的维护窗口确认。仅全部匹配时，才将本次任务标记失败并解除阻断；不会重试发布、执行 Compose、删除镜像或清理可能残留的远端临时 tar。临时文件仍需人工排查。"))
                .footer(div().flex().justify_end().gap_2()
                    .child(Button::new("predeploy-close-cancel").label("取消")
                        .on_click(|_, window, cx| window.close_dialog(cx)))
                    .child(Button::new("predeploy-close-confirm").danger().label("重新核实并关闭")
                        .on_click(move |_, window, cx| {
                            window.close_dialog(cx);
                            let id = task_id.clone();
                            let _ = confirm_view.update(cx, |view, cx| view.close_predeploy_task(id, window, cx));
                        })))
        });
    }

    fn close_predeploy_task(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(project_id) = self
            .release_tasks
            .iter()
            .chain(
                self.release_project_tasks
                    .values()
                    .flat_map(|tasks| tasks.iter()),
            )
            .find(|task| task.id == id)
            .map(|task| task.project_id)
        else {
            return;
        };
        if !self.release_resolving.insert(id.clone()) {
            return;
        }
        cx.notify();
        cx.spawn_in(window, async move |view, cx| {
            let key = id.clone();
            let result = cx.background_executor()
                .spawn(async move { release::confirm_predeploy_unchanged(&id) }).await;
            let _ = cx.update(|window, app| {
                let _ = view.update(app, |view, cx| {
                    view.release_resolving.remove(&key);
                    view.release_observations.remove(&key);
                    view.current_version_checks.remove(&key);
                    view.refresh_release_tasks(None, cx);
                    view.refresh_release_tasks(Some(project_id), cx);
                    cx.notify();
                });
                window.push_notification(match result {
                    Ok(count) => Notification::success(format!("已核实旧版本 {count} 个服务健康；上传中断任务已关闭，临时 tar 待人工检查")),
                    Err(error) => Notification::error(format!("{error}；仍保持发布阻断。")),
                }, app);
            });
        }).detach();
    }

    fn confirm_complete_task(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        let eligible = self
            .release_tasks
            .iter()
            .chain(
                self.release_project_tasks
                    .values()
                    .flat_map(|tasks| tasks.iter()),
            )
            .any(|task| {
                task.id == id
                    && task.phase == "checked"
                    && task.outcome == "blocked"
                    && task.health_result.as_deref() == Some("healthy")
            });
        if !eligible || self.release_resolving.contains(&id) {
            return;
        }
        let view = cx.weak_entity();
        window.open_dialog(cx, move |dialog, _, _| {
            let confirm_view = view.clone();
            let task_id = id.clone();
            dialog.title("确认健康发布完成").w(px(520.))
                .child(div().flex().flex_col().gap_3()
                    .child("将重新检查本机版本材料、远端自有镜像及整个 Compose 项目的容器和健康状态。只有身份和所有服务均匹配时才记录完成并解除项目阻断。")
                    .child("首次发布不要求已有上一版本；确认不会部署、重启或清理任何容器，也不会启用回滚按钮。旧任务或缺少服务计划的任务不能确认完成。"))
                .footer(div().flex().justify_end().gap_2()
                    .child(Button::new("release-complete-cancel").label("取消")
                        .on_click(|_, window, cx| window.close_dialog(cx)))
                    .child(Button::new("release-complete-confirm")
                        .debug_selector(|| "release-complete-confirm-button".into())
                        .label("重新核实并确认完成")
                        .on_click(move |_, window, cx| {
                            window.close_dialog(cx);
                            let id = task_id.clone();
                            let _ = confirm_view.update(cx, |view, cx| view.complete_healthy_task(id, window, cx));
                        })))
        });
    }

    fn complete_healthy_task(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(project_id) = self
            .release_tasks
            .iter()
            .chain(
                self.release_project_tasks
                    .values()
                    .flat_map(|tasks| tasks.iter()),
            )
            .find(|task| task.id == id)
            .map(|task| task.project_id)
        else {
            return;
        };
        if !self.release_resolving.insert(id.clone()) {
            return;
        }
        cx.notify();
        cx.spawn_in(window, async move |view, cx| {
            let key = id.clone();
            let result = cx
                .background_executor()
                .spawn(async move { release::confirm_healthy_and_complete(&id) })
                .await;
            let _ = cx.update(|window, app| {
                let _ = view.update(app, |view, cx| {
                    view.release_resolving.remove(&key);
                    view.release_observations.remove(&key);
                    view.release_image_checks.remove(&key);
                    view.previous_version_checks.clear();
                    view.refresh_release_tasks(None, cx);
                    view.refresh_release_tasks(Some(project_id), cx);
                    cx.notify();
                });
                window.push_notification(
                    match result {
                        Ok(count) => Notification::success(format!(
                            "已确认 {count} 个服务健康，发布记录已完成"
                        )),
                        Err(error) => Notification::error(error),
                    },
                    app,
                );
            });
        })
        .detach();
    }

    fn confirm_reconcile_health(
        &mut self,
        id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let start_unknown = self
            .release_tasks
            .iter()
            .chain(
                self.release_project_tasks
                    .values()
                    .flat_map(|tasks| tasks.iter()),
            )
            .find(|task| {
                task.id == id
                    && task.health_result.is_none()
                    && ((task.phase == "checking"
                        && matches!(task.outcome.as_str(), "unknown" | "running"))
                        || (task.phase == "deploying" && task.outcome == "unknown"))
            })
            .map(|task| task.phase == "deploying");
        let Some(start_unknown) = start_unknown else {
            return;
        };
        if self.release_resolving.contains(&id) {
            return;
        }
        let view = cx.weak_entity();
        window.open_dialog(cx, move |dialog, _, _| {
            let confirm_view = view.clone();
            let task_id = id.clone();
            dialog.title(if start_unknown {
                "核实启动响应不明的远端状态"
            } else {
                "重新核实未确认的健康结果"
            }).w(px(520.))
                .child(div().flex().flex_col().gap_3()
                    .child(if start_unknown {
                        "启动命令的客户端响应未确认。必须先核对该任务的远端启动完成标记，再只读核实版本材料、镜像及完整容器集合；仅全部服务与计划一致且 healthy 时恢复记录。缺少标记时仍保持待核实。"
                    } else {
                        "启动已确认，但健康核实结果不明。将重新只读核实全部版本材料、镜像、容器和健康状态；仅全部服务 healthy 时恢复记录。"
                    })
                    .child("不会重新执行 Compose、自动确认发布完成或继续下一台；恢复后仍须单独确认健康发布完成。"))
                .footer(div().flex().justify_end().gap_2()
                    .child(Button::new("release-reconcile-cancel")
                        .debug_selector(|| "release-reconcile-cancel".into())
                        .label("取消")
                        .on_click(|_, window, cx| window.close_dialog(cx)))
                    .child(Button::new("release-reconcile-confirm")
                        .debug_selector(|| "release-reconcile-confirm-button".into())
                        .label("只读核实并恢复记录")
                        .on_click(move |_, window, cx| {
                            window.close_dialog(cx);
                            let id = task_id.clone();
                            let _ = confirm_view.update(cx, |view, cx| view.reconcile_health_task(id, window, cx));
                        })))
        });
    }

    fn reconcile_health_task(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(project_id) = self
            .release_tasks
            .iter()
            .chain(
                self.release_project_tasks
                    .values()
                    .flat_map(|tasks| tasks.iter()),
            )
            .find(|task| task.id == id)
            .map(|task| task.project_id)
        else {
            return;
        };
        if !self.release_resolving.insert(id.clone()) {
            return;
        }
        cx.notify();
        cx.spawn_in(window, async move |view, cx| {
            let key = id.clone();
            let result = cx
                .background_executor()
                .spawn(async move { release::reconcile_interrupted_health(&id) })
                .await;
            let _ = cx.update(|window, app| {
                let _ = view.update(app, |view, cx| {
                    view.release_resolving.remove(&key);
                    view.release_observations.remove(&key);
                    view.refresh_release_tasks(None, cx);
                    view.refresh_release_tasks(Some(project_id), cx);
                    cx.notify();
                });
                window.push_notification(
                    match result {
                        Ok(count) => Notification::success(format!(
                            "已重新核实 {count} 个服务健康；仍需确认发布完成"
                        )),
                        Err(error) => Notification::warning(error),
                    },
                    app,
                );
            });
        })
        .detach();
    }

    fn confirm_reconcile_rollback(
        &mut self,
        id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let eligible = self
            .rollback_tasks
            .iter()
            .chain(
                self.rollback_project_tasks
                    .values()
                    .flat_map(|tasks| tasks.iter()),
            )
            .any(|task| {
                task.id == id
                    && matches!(
                        (task.phase.as_str(), task.outcome.as_str()),
                        ("checking", "unknown" | "running") | ("checked", "blocked")
                    )
            });
        if !eligible || self.release_resolving.contains(&id) {
            return;
        }
        let view = cx.weak_entity();
        window.open_dialog(cx, move |dialog, _, _| {
            let confirm_view = view.clone();
            let task_id = id.clone();
            dialog.title("只读核实回滚结果").w(px(520.))
                .child(div().flex().flex_col().gap_3()
                    .child("将重新核验目标历史材料、远端自有镜像和完整 Compose 项目容器；只有全部服务使用目标镜像且 healthy，才更新当前版本指针并解除阻断。")
                    .child("不会重新执行 Compose、上传镜像或清理临时归档；正在部署或镜像加载结果不明的任务不能使用。"))
                .footer(div().flex().justify_end().gap_2()
                    .child(Button::new("rollback-reconcile-cancel").label("取消")
                        .on_click(|_, window, cx| window.close_dialog(cx)))
                    .child(Button::new("rollback-reconcile-confirm").label("重新核实")
                        .on_click(move |_, window, cx| {
                            window.close_dialog(cx);
                            let id = task_id.clone();
                            let _ = confirm_view.update(cx, |view, cx| view.reconcile_rollback_task(id, window, cx));
                        })))
        });
    }

    fn reconcile_rollback_task(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(project_id) = self
            .rollback_tasks
            .iter()
            .chain(
                self.rollback_project_tasks
                    .values()
                    .flat_map(|tasks| tasks.iter()),
            )
            .find(|task| task.id == id)
            .map(|task| task.project_id)
        else {
            return;
        };
        if !self.release_resolving.insert(id.clone()) {
            return;
        }
        cx.notify();
        cx.spawn_in(window, async move |view, cx| {
            let key = id.clone();
            let result = cx
                .background_executor()
                .spawn(async move { crate::rollback::reconcile(&id) })
                .await;
            let _ = cx.update(|window, app| {
                let _ = view.update(app, |view, cx| {
                    view.release_resolving.remove(&key);
                    view.refresh_release_tasks(None, cx);
                    view.refresh_release_tasks(Some(project_id), cx);
                    cx.notify();
                });
                window.push_notification(
                    match result {
                        Ok(count) => Notification::success(format!(
                            "已只读确认 {count} 个服务恢复目标健康版本"
                        )),
                        Err(error) => Notification::warning(error),
                    },
                    app,
                );
            });
        })
        .detach();
    }

    fn preview_rollback(&mut self, source_id: String, window: &mut Window, cx: &mut Context<Self>) {
        if self.deploy_run_busy
            || self.rollback_running.is_some()
            || !self.rollback_previewing.insert(source_id.clone())
        {
            return;
        }
        cx.notify();
        cx.spawn_in(window, async move |view, cx| {
            let result = cx
                .background_executor()
                .spawn({
                    let id = source_id.clone();
                    async move { crate::rollback::preview(&id) }
                })
                .await;
            let _ = cx.update(|window, app| {
                let _ = view.update(app, |view, cx| {
                    view.rollback_previewing.remove(&source_id);
                    if let Ok(preview) = result {
                        view.confirm_rollback(preview, window, cx);
                    } else if let Err(error) = result {
                        window.push_notification(Notification::error(error), cx);
                    }
                    cx.notify();
                });
            });
        })
        .detach();
    }

    pub(super) fn confirm_rollback(
        &mut self,
        preview: crate::rollback::RollbackPreview,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.deploy_run_busy || self.rollback_running.is_some() {
            return;
        }
        #[cfg(test)]
        {
            self.rollback_confirmed_pair =
                Some((preview.source_id.clone(), preview.target_id.clone()));
        }
        let view = cx.weak_entity();
        window.open_dialog(cx, move |dialog, _, _| {
            let approved = preview.clone();
            let confirm_view = view.clone();
            dialog.title("确认回滚整个 Compose 项目").w(px(600.))
                .child(div().flex().flex_col().gap_3()
                    .child(format!("项目：{} · 主机：{} ({})", preview.project_name, preview.host_name, preview.host_endpoint))
                    .child(format!("源版本 {} → 健康目标版本 {}", preview.source_id, preview.target_id))
                    .child(format!("当前服务：{}", preview.source_services.join(" · ")))
                    .child(format!("目标服务：{}", preview.target_services.join(" · ")))
                    .child(format!("{} 份目标镜像 tag 缺失，将从本机已校验归档恢复。", preview.missing_images.len()))
                    .child("执行前会重新检查版本、归档、镜像和容器；Compose 可能重建或移除源版本的服务。结果不确定时保持阻断，绝不自动重试。"))
                .footer(div().flex().justify_end().gap_2()
                    .child(Button::new("rollback-cancel").label("取消")
                        .on_click(|_, window, cx| window.close_dialog(cx)))
                    .child(Button::new("rollback-confirm").danger().debug_selector(|| "rollback-confirm-button".into()).label("确认执行回滚")
                        .on_click(move |_, window, cx| {
                            window.close_dialog(cx);
                            let preview = approved.clone();
                            let _ = confirm_view.update(cx, |view, cx| view.start_rollback(preview, window, cx));
                        })))
        });
    }

    fn start_rollback(
        &mut self,
        preview: crate::rollback::RollbackPreview,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.deploy_run_busy || self.rollback_running.is_some() {
            return;
        }
        self.rollback_running = Some(preview.source_id.clone());
        self.rollback_stage = "重新核实回滚条件".into();
        cx.notify();
        let (tx, rx) = mpsc::channel::<RollbackRunEvent>();
        let project_id = preview.project_id;
        cx.background_executor()
            .spawn({
                async move {
                    let result = crate::rollback::run_confirmed(
                        &preview.source_id,
                        Some(&preview.target_id),
                        Some(&preview.observed),
                        Some(&preview.missing_images),
                        Arc::new(AtomicBool::new(false)),
                        |event| {
                            if let Some(stage) = safe_rollback_stage(event) {
                                let _ = tx.send(RollbackRunEvent::Stage(stage.into()));
                            }
                        },
                    );
                    let _ = tx.send(RollbackRunEvent::Finished(result));
                }
            })
            .detach();
        cx.spawn_in(window, async move |view, cx| {
            loop {
                match rx.try_recv() {
                    Ok(RollbackRunEvent::Stage(stage)) => {
                        let _ = view.update(cx, |view, cx| {
                            view.rollback_stage = stage;
                            cx.notify();
                        });
                    }
                    Ok(RollbackRunEvent::Finished(result)) => {
                        let _ = cx.update(|window, app| {
                            let _ = view.update(app, |view, cx| {
                                view.rollback_running = None;
                                view.rollback_stage.clear();
                                view.refresh_release_tasks(None, cx);
                                view.refresh_release_tasks(Some(project_id), cx);
                                cx.notify();
                            });
                            window.push_notification(
                                match result {
                                    Ok(id) => Notification::success(format!(
                                        "回滚 {id} 完成，目标服务全部健康"
                                    )),
                                    Err(error) => Notification::error(format!(
                                        "{error}；请在历史页核实，勿重复执行。"
                                    )),
                                },
                                app,
                            );
                        });
                        break;
                    }
                    Err(TryRecvError::Disconnected) => {
                        let _ = cx.update(|window, app| {
                            let _ = view.update(app, |view, cx| {
                                view.rollback_running = None;
                                view.rollback_stage = "执行中断，结果需人工核实".into();
                                view.refresh_release_tasks(None, cx);
                                view.refresh_release_tasks(Some(project_id), cx);
                                cx.notify();
                            });
                            window.push_notification(
                                Notification::error(
                                    "回滚执行异常中断，请先在历史页核实，不得重试。",
                                ),
                                app,
                            );
                        });
                        break;
                    }
                    Err(TryRecvError::Empty) => {
                        cx.background_executor()
                            .timer(Duration::from_millis(200))
                            .await
                    }
                }
            }
        })
        .detach();
    }

    pub(super) fn history_batch(
        &self,
        project_id: Option<i64>,
        batch_id: &str,
    ) -> Option<&batch_store::BatchRecord> {
        let batches = match project_id {
            Some(id) => self.release_project_batches.get(&id)?,
            None => &self.release_batches,
        };
        batches.iter().find(|batch| batch.id == batch_id)
    }

    fn confirm_batch_resolution(
        &mut self,
        project_id: Option<i64>,
        batch_id: String,
        host_id: Option<i64>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let batch = self.history_batch(project_id, &batch_id).filter(|batch| {
            matches!(
                batch.state.as_str(),
                "prepared" | "building" | "ready" | "paused" | "running"
            )
        });
        let Some(batch) = batch else { return };
        if self.batch_resolving.contains(&batch_id) {
            return;
        }
        let unstarted = matches!(batch.state.as_str(), "prepared" | "building" | "ready");
        if unstarted
            && (host_id.is_some()
                || batch
                    .targets
                    .iter()
                    .any(|target| target.state != "waiting" || target.release_task_id.is_some()))
        {
            return;
        }
        let host_name = if let Some(host_id) = host_id {
            if batch.state != "paused" {
                return;
            }
            let Some(target) = batch
                .targets
                .iter()
                .find(|target| target.host.id == host_id && target.state == "failed")
            else {
                return;
            };
            target.host.name.clone()
        } else {
            String::new()
        };
        let view = cx.weak_entity();
        window.open_dialog(cx, move |dialog, _, _| {
            let confirm_view = view.clone();
            let id = batch_id.clone();
            let title = if host_id.is_some() { "跳过失败主机" } else { "终止批次" };
            let detail = if host_id.is_some() {
                format!("将 {host_name} 标记为已跳过；该主机不会变成健康成功。若后面还有目标，本次只解除暂停，不会自动部署下一台。")
            } else if unstarted {
                "终止尚未开始远端发布的批次；可能残留的本机镜像和共享归档需人工核对。".into()
            } else {
                "终止剩余目标；此前已健康的主机保持现状，不执行回滚。".into()
            };
            dialog.title(title).w(px(520.))
                .child(div().flex().flex_col().gap_3()
                    .child(detail)
                    .child("确认时会重新核对数据库中的批次和任务状态。不会操作远端 Docker、删除镜像或归档；若有待核实结果则拒绝。"))
                .footer(div().flex().justify_end().gap_2()
                    .child(Button::new("batch-resolution-cancel")
                        .debug_selector(|| "batch-resolution-cancel".into()).label("取消")
                        .on_click(|_, window, cx| window.close_dialog(cx)))
                    .child(Button::new("batch-resolution-confirm")
                        .debug_selector(|| "batch-resolution-confirm".into())
                        .danger().label("确认处置")
                        .on_click(move |_, window, cx| {
                            window.close_dialog(cx);
                            let id = id.clone();
                            let _ = confirm_view.update(cx, |view, cx| {
                                view.resolve_batch(project_id, id, host_id, unstarted, window, cx)
                            });
                        })))
        });
    }

    fn resolve_batch(
        &mut self,
        source_project_id: Option<i64>,
        batch_id: String,
        host_id: Option<i64>,
        unstarted: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(batch) = self
            .history_batch(source_project_id, &batch_id)
            .filter(|batch| {
                matches!(
                    batch.state.as_str(),
                    "prepared" | "building" | "ready" | "paused" | "running"
                )
            })
        else {
            return;
        };
        if unstarted != matches!(batch.state.as_str(), "prepared" | "building" | "ready") {
            return;
        }
        let project_id = batch.project_id;
        if !self.batch_resolving.insert(batch_id.clone()) {
            return;
        }
        cx.notify();
        cx.spawn_in(window, async move |view, cx| {
            let operation_id = batch_id.clone();
            let result = cx
                .background_executor()
                .spawn(async move {
                    match host_id {
                        Some(host_id) => batch_store::skip_failed(&operation_id, host_id),
                        None if unstarted => batch_store::terminate_unstarted(&operation_id),
                        None => batch_store::terminate_resolved(&operation_id),
                    }
                })
                .await;
            let _ = cx.update(|window, app| {
                let _ = view.update(app, |view, cx| {
                    view.batch_resolving.remove(&batch_id);
                    view.refresh_release_tasks(None, cx);
                    view.refresh_release_tasks(Some(project_id), cx);
                    cx.notify();
                });
                window.push_notification(
                    match result {
                        Ok(()) if host_id.is_some() => {
                            Notification::success("已跳过失败主机；后续目标须重新确认后继续")
                        }
                        Ok(()) if unstarted => {
                            Notification::success("未启动批次已终止；请人工核对本机残留")
                        }
                        Ok(()) => Notification::success("批次已终止；已健康主机保持现状"),
                        Err(error) => Notification::warning(error),
                    },
                    app,
                );
            });
        })
        .detach();
    }

    fn batch_history_rows(
        &self,
        project_id: Option<i64>,
        tasks: &[release_store::TaskSnapshot],
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let batches = match project_id {
            Some(id) => self
                .release_project_batches
                .get(&id)
                .map(Vec::as_slice)
                .unwrap_or(&[]),
            None => self.release_batches.as_slice(),
        };
        let mut content = div().flex().flex_col().gap_3();
        if batches.is_empty() {
            return content.into_any_element();
        }
        content = content.child(div().text_color(self.palette().muted).child(format!(
            "批次记录 {} 条 · 仅已核实失败可跳过；未知结果不得自动重试",
            batches.len()
        )));
        for batch in batches {
            let healthy = batch
                .targets
                .iter()
                .filter(|target| target.state == "healthy")
                .count();
            let unknown = batch
                .targets
                .iter()
                .filter(|target| target.state == "unknown")
                .count();
            let waiting = batch
                .targets
                .iter()
                .filter(|target| target.state == "waiting")
                .count();
            let (status, color) = match batch.state.as_str() {
                "complete" if !batch.targets.is_empty() && healthy == batch.targets.len() => {
                    ("全部完成", self.palette().success)
                }
                "complete" => ("部分完成", self.palette().warning),
                "paused" => ("已暂停", self.palette().warning),
                "unknown" => ("结果待核实", self.palette().destructive),
                "terminated" => ("已终止", self.palette().muted),
                "ready" => ("等待分发", self.palette().foreground),
                "running" => ("进行中", self.palette().foreground),
                "prepared" => ("等待构建", self.palette().muted),
                "building" => ("本机构建中", self.palette().foreground),
                _ => ("状态待核实", self.palette().warning),
            };
            let resolving = self.batch_resolving.contains(&batch.id);
            let next_host_id = if resolving || self.deploy_run_busy {
                None
            } else {
                next_confirmed_batch_host(batch)
            };
            let unresolved = batch.targets.iter().any(|target| {
                matches!(target.state.as_str(), "running" | "unknown")
                    || matches!(
                        target.task_outcome.as_deref(),
                        Some("running" | "unknown" | "blocked")
                    )
            });
            let all_waiting = batch.targets.iter().all(|target| {
                target.state == "waiting"
                    && target.release_task_id.is_none()
                    && target.task_outcome.is_none()
            });
            let can_terminate = !resolving
                && ((matches!(batch.state.as_str(), "prepared" | "building" | "ready")
                    && all_waiting)
                    || (matches!(batch.state.as_str(), "paused" | "running")
                        && batch
                            .targets
                            .iter()
                            .all(|target| match target.state.as_str() {
                                "waiting" => {
                                    target.release_task_id.is_none()
                                        && target.task_outcome.is_none()
                                }
                                "healthy" | "failed" | "skipped" => {
                                    let expected = if target.state == "healthy" {
                                        "complete"
                                    } else {
                                        "failed"
                                    };
                                    target.release_task_id.is_some()
                                        && target.task_outcome.as_deref() == Some(expected)
                                }
                                _ => false,
                            })));
            let mut table = div().min_w(px(1280.)).flex().flex_col().child(
                div()
                    .h(px(38.))
                    .px_4()
                    .flex()
                    .items_center()
                    .bg(self.palette().accent)
                    .text_color(self.palette().muted)
                    .child(div().w(px(65.)).flex_shrink_0().child("顺序"))
                    .child(div().w(px(160.)).flex_shrink_0().child("主机"))
                    .child(div().w(px(135.)).flex_shrink_0().child("架构"))
                    .child(div().w(px(145.)).flex_shrink_0().child("阶段"))
                    .child(div().w(px(105.)).flex_shrink_0().child("健康"))
                    .child(div().w(px(170.)).flex_shrink_0().child("结果"))
                    .child(div().w(px(90.)).flex_shrink_0().child("耗时"))
                    .child(div().w(px(125.)).flex_shrink_0().child("任务 ID"))
                    .child("操作"),
            );
            for target in &batch.targets {
                let task = target
                    .release_task_id
                    .as_deref()
                    .and_then(|id| tasks.iter().find(|task| task.id == id));
                let pending_confirmation = task.is_some_and(|task| {
                    task.phase == "checked"
                        && task.outcome == "blocked"
                        && task.health_result.as_deref() == Some("healthy")
                });
                let phase = match task {
                    Some(task) => batch_phase_label(&task.phase),
                    None if target.release_task_id.is_some() => "任务未加载",
                    None => "等待",
                };
                let health = match task.and_then(|task| task.health_result.as_deref()) {
                    Some("healthy") => "健康",
                    Some("running_unverified") => "未验证就绪",
                    Some("failed") => "不健康",
                    _ => "—",
                };
                let elapsed = task.map_or_else(
                    || "—".into(),
                    |task| {
                        history_duration(&task.started_at, task.execution_finished_at.as_deref())
                    },
                );
                let state = if pending_confirmation {
                    "待人工确认健康"
                } else {
                    match target.state.as_str() {
                        "healthy" => "健康完成",
                        "failed" => "失败已确认",
                        "unknown" => "待核实",
                        "skipped" => "已跳过",
                        "running" => "进行中",
                        _ => "未开始",
                    }
                };
                let can_skip = batch.state == "paused"
                    && !resolving
                    && !unresolved
                    && target.state == "failed"
                    && target.release_task_id.is_some()
                    && target.task_outcome.as_deref() == Some("failed");
                let batch_id = batch.id.clone();
                let host_id = target.host.id;
                let task_id = task.map(|task| task.id.clone());
                table = table.child(
                    div()
                        .min_h(px(42.))
                        .px_4()
                        .flex()
                        .items_center()
                        .border_t_1()
                        .border_color(self.palette().border)
                        .child(
                            div()
                                .w(px(65.))
                                .flex_shrink_0()
                                .child((target.ordinal + 1).to_string()),
                        )
                        .child(
                            div()
                                .w(px(160.))
                                .flex_shrink_0()
                                .child(target.host.name.clone()),
                        )
                        .child(
                            div()
                                .w(px(135.))
                                .flex_shrink_0()
                                .child(target.platform.clone()),
                        )
                        .child(div().w(px(145.)).flex_shrink_0().child(phase))
                        .child(div().w(px(105.)).flex_shrink_0().child(health))
                        .child(div().w(px(170.)).flex_shrink_0().child(state))
                        .child(div().w(px(90.)).flex_shrink_0().child(elapsed))
                        .child(
                            div()
                                .w(px(125.))
                                .flex_shrink_0()
                                .font_family(mono_font())
                                .child(
                                    target
                                        .release_task_id
                                        .as_deref()
                                        .map(|id| id.chars().take(8).collect::<String>())
                                        .unwrap_or_else(|| "—".into()),
                                ),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_wrap()
                                .gap_2()
                                .when_some(task_id, |actions, task_id| {
                                    actions.child(
                                        Button::new(format!("batch-task-{task_id}"))
                                            .debug_selector(|| "batch-task-detail-button".into())
                                            .label("查看任务详情")
                                            .on_click(cx.listener(move |view, _, _, cx| {
                                                view.release_history_selected =
                                                    Some(task_id.clone());
                                                cx.notify();
                                            })),
                                    )
                                })
                                .when(can_skip, |actions| {
                                    actions.child(
                                        Button::new(format!("batch-skip-{batch_id}-{host_id}"))
                                            .debug_selector(|| "batch-skip-button".into())
                                            .label("跳过失败主机")
                                            .on_click(cx.listener(move |view, _, window, cx| {
                                                view.confirm_batch_resolution(
                                                    project_id,
                                                    batch_id.clone(),
                                                    Some(host_id),
                                                    window,
                                                    cx,
                                                )
                                            })),
                                    )
                                })
                                .when(task.is_none(), |actions| actions.child("—")),
                        ),
                );
            }
            let project_name = self
                .projects
                .iter()
                .find(|item| item.id == batch.project_id)
                .map_or_else(
                    || format!("项目 {}", batch.project_id),
                    |item| item.name.clone(),
                );
            content = content.child(div().border_1().border_color(self.palette().border)
                .debug_selector(|| "batch-history-card".into())
                .rounded_md().overflow_hidden()
                .child(div().px_4().py_3().flex().flex_wrap().justify_between().gap_3()
                    .child(div().flex().flex_col().items_start().gap_1()
                        .child(div().font_family(mono_font()).child(format!(
                            "批次 {} · {project_name} · {}",
                            batch.id.chars().take(8).collect::<String>(), batch.created_at)))
                        .child(div().text_color(self.palette().muted).child(format!(
                            "健康 {healthy}/{} · 待核实 {unknown} · 未开始 {waiting} · 共享镜像 {} 份",
                            batch.targets.len(), batch.images.len())))
                        .when(next_host_id.is_some(), |el| {
                            let host_id = next_host_id.unwrap_or_default();
                            let batch_id = batch.id.clone();
                            el.child(Button::new(format!("batch-continue-{batch_id}"))
                                .debug_selector(|| "batch-continue-button".into())
                                .primary().label("继续下一台")
                                .on_click(cx.listener(move |view, _, window, cx| {
                                    view.confirm_continue_batch(batch_id.clone(), host_id, window, cx)
                                })))
                        }))
                    .child(div().flex().flex_wrap().items_center().gap_3()
                        .child(div().text_color(color).child(status))
                        .when(can_terminate, |el| {
                            let batch_id = batch.id.clone();
                            el.child(Button::new(format!("batch-terminate-{batch_id}"))
                                .debug_selector(|| "batch-terminate-button".into())
                                .danger().label("终止批次")
                                .on_click(cx.listener(move |view, _, window, cx| {
                                    view.confirm_batch_resolution(project_id, batch_id.clone(), None, window, cx)
                                })))
                        })))
                .when_some(self.batch_operation_stage.get(&batch.id), |card, stage| {
                    card.child(div().px_4().pb_3().text_color(self.palette().warning)
                        .child(format!("批次操作中：{stage}")))
                })
                .child(div().id(format!("batch-history-scroll-{}", batch.id))
                    .overflow_x_scroll().child(table)));
        }
        content.into_any_element()
    }

    fn release_history_panel(&self, project_id: Option<i64>, cx: &mut Context<Self>) -> AnyElement {
        let mut rows = div().px_6().pb_6().flex().flex_col().gap_3();
        if let Some(id) = project_id {
            rows = rows.child(
                div()
                    .flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        Button::new(("release-retention", id as u64))
                            .label(if self.retention_loading.contains(&id) {
                                "检查中…"
                            } else {
                                "检查版本保留"
                            })
                            .disabled(self.retention_loading.contains(&id))
                            .on_click(cx.listener(move |view, _, _, cx| {
                                view.inspect_release_retention(id, cx)
                            })),
                    )
                    .child(
                        Button::new(("local-archive-preview", id as u64))
                            .label(if self.local_archive_preview_loading.contains(&id) {
                                "核验中…"
                            } else {
                                "预览本机旧版归档"
                            })
                            .disabled(self.local_archive_preview_loading.contains(&id))
                            .on_click(cx.listener(move |view, _, _, cx| {
                                view.inspect_local_archive_preview(id, cx)
                            })),
                    ),
            );
            if let Some(result) = self.retention_reports.get(&id) {
                let text = match result {
                    Ok(plan) if plan.unresolved => format!(
                        "存在待核实/阻断的发布或回滚；{} 条历史任务的材料均须保留，不自动清理。",
                        plan.entries.len()
                    ),
                    Ok(plan) => {
                        let count = |class| {
                            plan.entries
                                .iter()
                                .filter(|entry| entry.class == class)
                                .count()
                        };
                        let latest = plan
                            .entries
                            .iter()
                            .filter(|entry| {
                                entry.class == release_store::RetentionClass::LatestCompleted
                            })
                            .map(|entry| entry.task_id.chars().take(8).collect::<String>())
                            .collect::<Vec<_>>()
                            .join("、");
                        format!(
                            "保留快照：当前指针版本 {}（{}）、其前驱 {}、其他历史待人工核对 {}、身份/结果不确定 {}。当前容器仍须远端核实；此处不会删除材料。",
                            count(release_store::RetentionClass::LatestCompleted),
                            latest,
                            count(release_store::RetentionClass::PreviousCompleted),
                            count(release_store::RetentionClass::HistoricalReview),
                            count(release_store::RetentionClass::UncertainHistory)
                        )
                    }
                    Err(error) => format!("保留策略检查失败：{error}"),
                };
                rows = rows.child(div().text_color(self.palette().muted).child(text));
            }
            if let Some(result) = self.local_archive_previews.get(&id) {
                match result {
                    Err(error) => {
                        rows = rows.child(
                            div()
                                .text_color(self.palette().warning)
                                .child(format!("本机归档预览失败：{error}")),
                        )
                    }
                    Ok(preview) if preview.blocked => {
                        rows = rows.child(div().text_color(self.palette().warning).child(format!(
                            "存在待核实任务；{} 条发布任务的归档全部保留，不计算清理候选。",
                            preview.total_tasks
                        )))
                    }
                    Ok(preview) => {
                        rows = rows.child(div().text_color(self.palette().muted)
                            .child(format!("上次点击时只读核验：{} 条更早历史的本机 tar 齐全；文件可能已变化，清理前须重新核验。仅显示占用，不代表允许删除或远端可释放空间。", preview.candidates.len())));
                        for candidate in &preview.candidates {
                            rows = rows.child(div().font_family(mono_font()).child(format!(
                                "{} · {} 份镜像归档 · {:.1} MiB（{} 字节）",
                                candidate.task_id.chars().take(8).collect::<String>(),
                                candidate.services,
                                candidate.verified_bytes as f64 / (1024. * 1024.),
                                candidate.verified_bytes,
                            )));
                        }
                        if !preview.unavailable.is_empty() {
                            rows = rows.child(div().text_color(self.palette().warning).child(
                                format!(
                                    "{} 条历史的本机归档缺失或校验失败，不作为清理候选：{}",
                                    preview.unavailable.len(),
                                    preview
                                        .unavailable
                                        .iter()
                                        .map(|id| id.chars().take(8).collect::<String>())
                                        .collect::<Vec<_>>()
                                        .join("、"),
                                ),
                            ));
                        }
                    }
                }
            }
        }
        rows = rows.child(
            div()
                .flex()
                .flex_wrap()
                .justify_end()
                .gap_2()
                .child(
                    Button::new("release-recover-prepared")
                        .label(if self.release_recovery_busy {
                            "清理中…"
                        } else {
                            "重试构建前中断清理"
                        })
                        .disabled(self.release_recovery_busy)
                        .on_click(cx.listener(|view, _, window, cx| {
                            view.retry_prepared_recovery(window, cx)
                        })),
                )
                .child(
                    Button::new("release-recover-batches")
                        .label(if self.release_recovery_busy {
                            "核对中…"
                        } else {
                            "核对中断批次"
                        })
                        .disabled(self.release_recovery_busy)
                        .on_click(cx.listener(|view, _, window, cx| {
                            view.inspect_interrupted_batches(window, cx)
                        })),
                ),
        );
        if let Some(error) = &self.release_recovery_error {
            rows = rows.child(
                div()
                    .text_color(self.palette().warning)
                    .child(format!("中断恢复待处理：{error}")),
            );
        }
        let error = match project_id {
            Some(id) => self.release_project_errors.get(&id),
            None => self.release_tasks_error.as_ref(),
        };
        if let Some(error) = error {
            rows = rows.child(
                div()
                    .text_color(self.palette().destructive)
                    .child(error.clone()),
            );
        }
        let tasks = match project_id {
            Some(id) => self
                .release_project_tasks
                .get(&id)
                .map(Vec::as_slice)
                .unwrap_or(&[]),
            None => self.release_tasks.as_slice(),
        };
        rows = rows.child(self.batch_history_rows(project_id, tasks, cx));
        if tasks.is_empty() {
            rows = rows.child(div().text_color(self.palette().muted).child(
                if project_id.is_some_and(|id| self.release_project_loading.contains(&id))
                    || (project_id.is_none() && self.release_tasks_loading)
                {
                    "正在读取任务…"
                } else if error.is_some() {
                    "历史读取失败，请重试"
                } else if project_id.map_or(!self.release_batches.is_empty(), |id| {
                    self.release_project_batches
                        .get(&id)
                        .is_some_and(|batches| !batches.is_empty())
                }) {
                    "批次尚无逐主机发布任务"
                } else {
                    "暂无发布任务；版本回滚历史尚未接入"
                },
            ));
        }
        if !tasks.is_empty() {
            let mut table = div().min_w(px(900.)).flex().flex_col();
            let mut header = div()
                .h(px(42.))
                .px_4()
                .flex()
                .items_center()
                .bg(self.palette().accent)
                .text_color(self.palette().muted)
                .child(div().w(px(155.)).flex_shrink_0().child("时间"));
            if project_id.is_none() {
                header = header.child(div().w(px(135.)).flex_shrink_0().child("项目"));
            }
            header = header
                .child(div().w(px(175.)).flex_shrink_0().child("发布标识"))
                .child(
                    div()
                        .w(px(135.))
                        .flex_shrink_0()
                        .child(if project_id.is_some() {
                            "目标主机数"
                        } else {
                            "目标主机"
                        }),
                )
                .child(div().w(px(155.)).flex_shrink_0().child("结果"))
                .child(div().w(px(90.)).flex_shrink_0().child("耗时"))
                .child("操作");
            table = table.child(header);
            for (index, task) in tasks.iter().enumerate() {
                let (status, color) = match (task.outcome.as_str(), task.health_result.as_deref()) {
                    ("complete", Some("healthy")) => ("成功", self.palette().success),
                    ("blocked", Some("healthy")) => ("待确认", self.palette().warning),
                    ("blocked", Some("running_unverified")) => {
                        ("运行中，未验证就绪", self.palette().warning)
                    }
                    ("blocked", _) => ("失败待处理", self.palette().destructive),
                    ("unknown", _) => ("待核实", self.palette().warning),
                    ("failed", _) => ("失败", self.palette().destructive),
                    _ => ("进行中", self.palette().muted),
                };
                let can_rollback = task.phase == "checked"
                    && task.docker_daemon_id.is_some()
                    && matches!(
                        (task.outcome.as_str(), task.health_result.as_deref()),
                        ("complete", Some("healthy")) | ("blocked", Some("failed"))
                    );
                let mut row = div()
                    .h(px(54.))
                    .px_4()
                    .border_t_1()
                    .border_color(self.palette().border)
                    .flex()
                    .items_center()
                    .child(
                        div()
                            .w(px(155.))
                            .flex_shrink_0()
                            .child(task.started_at.clone()),
                    );
                if project_id.is_none() {
                    row = row.child(
                        div()
                            .w(px(135.))
                            .flex_shrink_0()
                            .truncate()
                            .child(task.project_name.clone()),
                    );
                }
                let selected = self.release_history_selected.as_deref() == Some(task.id.as_str());
                row = row
                    .child(
                        div()
                            .w(px(175.))
                            .flex_shrink_0()
                            .truncate()
                            .font_family(mono_font())
                            .child(history_tag(&task.image_reference)),
                    )
                    .child(div().w(px(135.)).flex_shrink_0().child("1 台"))
                    .child(
                        div()
                            .w(px(155.))
                            .flex_shrink_0()
                            .text_color(color)
                            .child(status),
                    )
                    .child(div().w(px(90.)).flex_shrink_0().child(history_duration(
                        &task.started_at,
                        task.execution_finished_at.as_deref(),
                    )))
                    .child(
                        div()
                            .flex()
                            .gap_1()
                            .child(
                                Button::new(("history-detail", index))
                                    .debug_selector(|| "history-detail-button".into())
                                    .label(if selected { "收起" } else { "详情" })
                                    .on_click(cx.listener({
                                        let id = task.id.clone();
                                        move |view, _, _, cx| {
                                            view.release_history_selected =
                                                if view.release_history_selected.as_deref()
                                                    == Some(&id)
                                                {
                                                    None
                                                } else {
                                                    Some(id.clone())
                                                };
                                            cx.notify();
                                        }
                                    })),
                            )
                            .when(can_rollback, |actions| {
                                actions.child(
                                    Button::new(("history-rollback", index))
                                        .debug_selector(|| "release-rollback-button".into())
                                        .label("回滚")
                                        .disabled(
                                            self.deploy_run_busy
                                                || self.rollback_running.is_some()
                                                || self.rollback_previewing.contains(&task.id),
                                        )
                                        .on_click(cx.listener({
                                            let id = task.id.clone();
                                            move |view, _, window, cx| {
                                                view.preview_rollback(id.clone(), window, cx)
                                            }
                                        })),
                                )
                            }),
                    );
                table = table.child(row);
            }
            rows = rows.child(
                div()
                    .id("release-history-table-scroll")
                    .overflow_x_scroll()
                    .border_1()
                    .border_color(self.palette().border)
                    .rounded_md()
                    .child(table),
            );
        }
        for (index, task) in tasks.iter().enumerate() {
            if self.release_history_selected.as_deref() != Some(task.id.as_str()) {
                continue;
            }
            let id = task.id.clone();
            let checking = self.release_checking.contains(&id);
            let resolving = self.release_resolving.contains(&id);
            let material_checking = self.release_material_checking.contains(&id);
            let image_checking = self.release_image_checking.contains(&id);
            let upload_inspecting = self.release_upload_inspecting.contains(&id);
            let resume_batch = self
                .release_batches
                .iter()
                .chain(
                    self.release_project_batches
                        .values()
                        .flat_map(|items| items.iter()),
                )
                .find(|batch| {
                    batch.state == "unknown"
                        && batch.targets.iter().any(|target| {
                            target.state == "unknown"
                                && target.release_task_id.as_deref() == Some(id.as_str())
                        })
                })
                .map(|batch| batch.id.as_str());
            let can_resume = resume_batch.is_some_and(|batch_id| {
                !self.deploy_run_busy
                    && !self.batch_resolving.contains(batch_id)
                    && self
                        .release_upload_inspections
                        .get(&id)
                        .is_some_and(|result| {
                            result.as_ref().is_ok_and(resumable_upload_inspection)
                        })
            });
            let previous_checking = self.previous_version_checking.contains(&id);
            let current_checking = self.current_version_loading.contains(&id);
            let observation = self.release_observations.get(&id);
            let absent = observation.is_some_and(|result| result.as_ref().is_ok_and(Vec::is_empty));
            let eligible = task.docker_endpoint.is_some()
                && task.docker_daemon_id.is_some()
                && matches!(
                    (task.phase.as_str(), task.outcome.as_str()),
                    ("checked", "blocked") | ("checking", "unknown")
                );
            let active = matches!(task.outcome.as_str(), "running" | "unknown" | "blocked");
            let can_complete = task.phase == "checked"
                && task.outcome == "blocked"
                && task.health_result.as_deref() == Some("healthy");
            let can_reconcile = task.health_result.is_none()
                && ((task.phase == "checking"
                    && matches!(task.outcome.as_str(), "unknown" | "running"))
                    || (task.phase == "deploying" && task.outcome == "unknown"));
            let detail = match observation {
                Some(Ok(containers)) if containers.is_empty() && eligible => {
                    "远端无该项目容器；可再次核实后关闭任务".into()
                }
                Some(Ok(containers)) if containers.is_empty() => {
                    "远端无该项目容器，但旧任务缺少 Docker daemon 身份快照或阶段不安全，不能自动解除阻断".into()
                }
                Some(Ok(containers)) => containers
                    .iter()
                    .map(|item| {
                        format!(
                            "{}：{} / {}",
                            item.service,
                            item.status,
                            item.health.as_deref().unwrap_or("无健康检查")
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("  ·  "),
                Some(Err(error)) => format!("核实失败：{error}"),
                None => task.detail.clone(),
            };
            rows =
                rows.child(
                    div()
                        .border_1()
                        .border_color(self.palette().border)
                        .rounded_md()
                        .p_4()
                        .flex()
                        .flex_col()
                        .gap_2()
                        .child(div().font_family(mono_font()).child(format!(
                            "{}  ·  {}  ·  {}",
                            id.chars().take(8).collect::<String>(),
                            task.project_name,
                            task.host.name
                        )))
                        .child(div().text_color(self.palette().muted).child(format!(
                            "{}  ·  {}  ·  {}",
                            task.started_at, task.phase, task.outcome
                        )))
                        .when_some(task.health_result.as_deref(), |row, health| {
                            let (label, color) = match health {
                                "healthy" => ("发布时核实：全部健康（仍未解除阻断）", self.palette().success),
                                "running_unverified" => ("发布时核实：运行中，未验证就绪", self.palette().warning),
                                _ => ("发布时核实：未健康", self.palette().destructive),
                            };
                            row.child(div().text_color(color).child(label))
                        })
                        .child(div().whitespace_normal().child(detail))
                        .when(self.rollback_running.as_deref() == Some(id.as_str()), |row| {
                            row.child(div().text_color(self.palette().warning)
                                .child(format!("回滚执行中：{}", self.rollback_stage)))
                        })
                        .when_some(self.release_material_checks.get(&id), |row, material| {
                            row.child(div().text_color(self.palette().muted).child(
                                match material {
                                    Ok(check) => format!(
                                        "Compose/环境快照可读：{} 份 Compose，环境快照{}；镜像归档 {}/{} 份可读（不代表可回滚）",
                                        check.compose_files,
                                        if check.has_env { "已保存" } else { "为空" },
                                        check.archives,
                                        check.planned_archives,
                                    ),
                                    Err(error) => format!("本机材料不可用：{error}"),
                                },
                            ))
                        })
                        .when_some(self.release_image_checks.get(&id), |row, result| {
                            row.child(div().text_color(self.palette().muted).child(match result {
                                Ok(images) => format!(
                                    "远端历史镜像：{}（只读，不代表可回滚）",
                                    images.iter().map(|(service, available)| format!(
                                        "{service} {}", if *available { "ID/架构匹配" } else { "缺失或已变化" }
                                    )).collect::<Vec<_>>().join("  ·  ")
                                ),
                                Err(error) => format!("远端镜像核实失败：{error}"),
                            }))
                        })
                        .when_some(self.release_upload_inspections.get(&id), |row, result| {
                            let mut details = div().flex().flex_col().gap_1().whitespace_normal();
                            match result {
                                Ok(check) => {
                                    details = details.child(format!(
                                        "上次只读核实：该项目容器 {} 个；镜像 tag 尚未核实。此结果不授权续传或解除阻断。",
                                        check.containers.len()
                                    ));
                                    if check.archives.is_empty() {
                                        details = details.child("尚无远端归档预留记录；确认后将重新核对并从首份开始上传。".to_owned());
                                    }
                                    for archive in &check.archives {
                                        let status = match (archive.actual_bytes, archive.digest_matches) {
                                            (None, _) if archive.state == "reserved" => "预留路径无文件；确认后从零上传".into(),
                                            (None, _) => "已确认上传的远端文件缺失，禁止续传".into(),
                                            (Some(actual), Some(true)) if actual == archive.expected_bytes => "完整摘要匹配".into(),
                                            (Some(actual), Some(false)) if actual == archive.expected_bytes => "完整摘要不符".into(),
                                            (Some(actual), _) if actual > archive.expected_bytes => "大小超出预期".into(),
                                            (Some(actual), _) => format!("部分文件 {actual}/{} 字节；前缀尚未核实", archive.expected_bytes),
                                        };
                                        details = details.child(format!("{} · {} · {status}", archive.service, archive.directory));
                                    }
                                }
                                Err(error) => details = details.child(format!("远端归档核实失败：{error}")),
                            }
                            row.child(div().text_color(self.palette().warning).child(details))
                        })
                        .when_some(resume_batch.and_then(|batch_id| self.batch_operation_stage.get(batch_id)), |row, stage| {
                            row.child(div().text_color(self.palette().warning).child(format!("批次操作中：{stage}")))
                        })
                        .when_some(self.previous_version_checks.get(&id), |row, result| {
                            row.child(div().text_color(self.palette().muted).child(match result {
                                Ok(check) => format!(
                                    "上一健康版本 {}：{} 份镜像及本机快照在核查时齐全；尚不可回滚",
                                    check.task_id.chars().take(8).collect::<String>(), check.images
                                ),
                                Err(error) => format!("上一版本预检不可用：{error}"),
                            }))
                        })
                        .when_some(self.current_version_checks.get(&id), |row, result| {
                            row.child(div().text_color(self.palette().muted).child(match result {
                                Ok(check) if check.containers == 0 => "远端当前无该项目容器；不能判定运行版本。".into(),
                                Ok(check) if check.running_containers != check.containers =>
                                    format!("项目有 {} 个容器，其中 {} 个运行；不能确认完整运行版本，全部材料继续保留。",
                                        check.containers, check.running_containers),
                                Ok(check) => match &check.matched_task_id {
                                    Some(version) => format!("检查时容器对应已完成版本 {}；当前{}。外部变化后须重新核实。",
                                        version.chars().take(8).collect::<String>(),
                                        if check.all_healthy { "全部健康" } else { "存在未健康服务" }),
                                    None => "当前容器与任何已完成版本均不完全匹配；可能是部分发布或外部漂移，全部材料继续保留。".into(),
                                },
                                Err(error) => format!("当前运行版本核实失败：{error}"),
                            }))
                        })
                        .child(
                            div()
                                .flex()
                                .flex_wrap()
                                .gap_2()
                                .child(
                                    Button::new(("release-inspect", index))
                                        .label(if checking {
                                            "核实中…"
                                        } else {
                                            "重新核实状态"
                                        })
                                        .disabled(checking || resolving || !active)
                                        .on_click(cx.listener({
                                            let id = id.clone();
                                            move |view, _, _, cx| {
                                                view.inspect_release_task(id.clone(), cx)
                                            }
                                        })),
                                )
                                .child(
                                    Button::new(("release-materials", index))
                                        .label(if material_checking {
                                            "检查中…"
                                        } else {
                                            "检查本机材料"
                                        })
                                        .disabled(material_checking)
                                        .on_click(cx.listener({
                                            let id = id.clone();
                                            move |view, _, _, cx| {
                                                view.inspect_release_materials(id.clone(), cx)
                                            }
                                        })),
                                )
                                .child(
                                    Button::new(("release-images", index))
                                        .label(if image_checking { "核实中…" } else { "核实远端镜像" })
                                        .disabled(image_checking)
                                        .on_click(cx.listener({
                                            let id = id.clone();
                                            move |view, _, _, cx| view.inspect_release_images(id.clone(), cx)
                                        })),
                                )
                                .when(task.phase == "uploading" && task.outcome == "unknown", |actions| {
                                    actions.child(Button::new(("release-upload-inspect", index))
                                        .debug_selector(|| "release-upload-inspect-button".into())
                                        .label(if upload_inspecting { "核对中…" } else { "只读核对临时 tar" })
                                        .disabled(upload_inspecting)
                                        .on_click(cx.listener({
                                            let id = id.clone();
                                            move |view, _, _, cx| view.inspect_release_upload(id.clone(), cx)
                                        })))
                                })
                                .when(task.phase == "uploading" && task.outcome == "unknown" && resume_batch.is_some(), |actions| {
                                    actions.child(Button::new(("release-upload-resume", index))
                                        .debug_selector(|| "release-upload-resume-button".into())
                                        .label("确认断点续传")
                                        .disabled(!can_resume)
                                        .on_click(cx.listener({
                                            let id = id.clone();
                                            move |view, _, window, cx| view.confirm_resume_unknown_upload(id.clone(), window, cx)
                                        })))
                                })
                                .child(
                                    Button::new(("previous-version", index))
                                        .label(if previous_checking { "预检中…" } else { "检查上一版本" })
                                        .disabled(previous_checking)
                                        .on_click(cx.listener({
                                            let id = id.clone();
                                            move |view, _, _, cx| view.inspect_previous_version(id.clone(), cx)
                                        })),
                                )
                                .child(Button::new(("current-version", index))
                                    .label(if current_checking { "核实中…" } else { "核实当前运行版本" })
                                    .disabled(current_checking || task.docker_daemon_id.is_none())
                                    .on_click(cx.listener({
                                        let id = id.clone();
                                        move |view, _, _, cx| view.inspect_current_version(id.clone(), cx)
                                    })))
                                .when(absent && eligible, |row| {
                                    row.child(
                                        Button::new(("release-confirm-absence", index))
                                            .danger()
                                            .label(if resolving {
                                                "处理中…"
                                            } else {
                                                "确认无容器，解除阻断"
                                            })
                                            .disabled(resolving || checking)
                                            .on_click(cx.listener({
                                                let id = id.clone();
                                                move |view, _, window, cx| {
                                                    view.confirm_close_absent_task(
                                                        id.clone(),
                                                        window,
                                                        cx,
                                                    )
                                                }
                                            })),
                                    )
                                })
                                .when(task.phase == "uploading" && task.outcome == "unknown", |row| {
                                    row.child(Button::new(("release-close-predeploy", index)).outline()
                                        .label(if resolving { "核实中…" } else { "核实旧版本并关闭上传中断" })
                                        .disabled(resolving || self.deploy_run_busy || self.rollback_running.is_some())
                                        .on_click(cx.listener({
                                            let id = id.clone();
                                            move |view, _, window, cx| view.confirm_close_predeploy_task(id.clone(), window, cx)
                                        })))
                                })
                                .when(can_complete, |row| {
                                    row.child(Button::new(("release-confirm-healthy", index))
                                        .debug_selector(|| "release-confirm-healthy-button".into())
                                        .label(if resolving { "处理中…" } else { "确认健康发布完成" })
                                        .disabled(resolving || checking)
                                        .on_click(cx.listener({
                                            let id = id.clone();
                                            move |view, _, window, cx| view.confirm_complete_task(id.clone(), window, cx)
                                        })))
                                })
                                .when(can_reconcile, |row| {
                                    row.child(Button::new(("release-reconcile-health", index))
                                        .debug_selector(|| "release-reconcile-health-button".into())
                                         .label(if resolving {
                                             "核实中…"
                                         } else if task.phase == "deploying" {
                                             "核实启动结果并恢复记录"
                                         } else {
                                             "恢复中断的健康核实"
                                         })
                                        .disabled(resolving || checking)
                                        .on_click(cx.listener({
                                            let id = id.clone();
                                            move |view, _, window, cx| view.confirm_reconcile_health(id.clone(), window, cx)
                                        })))
                                }),
                        ),
                );
        }
        let rollbacks = match project_id {
            Some(id) => self
                .rollback_project_tasks
                .get(&id)
                .map(Vec::as_slice)
                .unwrap_or(&[]),
            None => self.rollback_tasks.as_slice(),
        };
        for rollback in rollbacks {
            let reconcilable = matches!(
                (rollback.phase.as_str(), rollback.outcome.as_str()),
                ("checking", "unknown" | "running") | ("checked", "blocked")
            );
            let resolving = self.release_resolving.contains(&rollback.id);
            let status = match rollback.outcome.as_str() {
                "complete" => "已恢复健康版本",
                "unknown" => "结果待核实，不得重试",
                "blocked" => "恢复未健康，保持阻断",
                "failed" => "未执行恢复",
                _ => "处理中",
            };
            rows = rows.child(
                div()
                    .border_1()
                    .border_color(self.palette().border)
                    .rounded_md()
                    .p_4()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(div().font_family(mono_font()).child(format!(
                        "回滚 {} · 项目 {} · {} → {}",
                        rollback.id.chars().take(8).collect::<String>(),
                        rollback.project_id,
                        rollback.source_task_id.chars().take(8).collect::<String>(),
                        rollback.target_task_id.chars().take(8).collect::<String>()
                    )))
                    .child(div().text_color(self.palette().muted).child(format!(
                            "{} · {} · {} · {}",
                            rollback.started_at,
                            rollback.phase,
                            status,
                            rollback
                                .observed_containers
                                .map_or("服务待核实".into(), |count| format!(
                                    "已核对 {count} 个服务"
                                ))
                        )))
                    .child(div().whitespace_normal().child(rollback.detail.clone()))
                    .when_some(
                        rollback.pending_directories.as_deref(),
                        |row, directories| {
                            row.child(
                                div()
                                    .text_color(self.palette().warning)
                                    .child(format!("远端临时镜像归档待人工核对：{directories}")),
                            )
                        },
                    )
                    .when(reconcilable, |row| {
                        row.child(
                            Button::new(format!("rollback-reconcile-{}", rollback.id))
                                .label(if resolving {
                                    "核实中…"
                                } else {
                                    "只读核实回滚结果"
                                })
                                .disabled(resolving)
                                .on_click(cx.listener({
                                    let id = rollback.id.clone();
                                    move |view, _, window, cx| {
                                        view.confirm_reconcile_rollback(id.clone(), window, cx)
                                    }
                                })),
                        )
                    }),
            );
        }
        self.panel(
            "发布任务",
            "回滚前只读预检并二次确认；结果不确定不得自动重试。",
            rows,
        )
        .into_any_element()
    }

    fn project_detail(
        &self,
        project: &Project,
        narrow: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let tabs = [
            (ProjectTab::Build, "构建配置", "build"),
            (ProjectTab::Compose, "Compose", "compose"),
            (ProjectTab::Environment, "环境变量", "project-env"),
            (ProjectTab::Targets, "目标主机", "targets"),
            (ProjectTab::History, "发布历史", "project-history"),
        ];
        let tab_row = div().id("project-tabs-scroll").overflow_x_scroll().child(
            div().flex().child(
                div()
                    .flex()
                    .gap_1()
                    .p_1()
                    .rounded_md()
                    .bg(self.palette().accent)
                    .children(tabs.map(|(tab, title, id)| {
                        let project_id = project.id;
                        self.tab(
                            id,
                            title,
                            self.project_tab == tab,
                            cx,
                            move |view, window, cx| {
                                if view.project_tab == ProjectTab::Compose
                                    && tab != ProjectTab::Compose
                                {
                                    view.invalidate_compose_summary();
                                }
                                view.project_tab = tab;
                                if tab == ProjectTab::Compose {
                                    view.refresh_compose_status(project_id, cx);
                                } else if tab == ProjectTab::Build {
                                    view.refresh_services(project_id, cx);
                                } else if tab == ProjectTab::Targets {
                                    view.open_target_form(project_id);
                                } else if tab == ProjectTab::Environment {
                                    view.load_environment(project_id, None, window, cx);
                                } else if tab == ProjectTab::History {
                                    view.refresh_release_tasks(Some(project_id), cx);
                                }
                                cx.notify();
                            },
                        )
                    })),
            ),
        );
        let body = match self.project_tab {
            ProjectTab::Build | ProjectTab::Compose | ProjectTab::Targets
                if project.manifest_mode =>
            {
                self.manifest_panel(project)
            }
            ProjectTab::Build => self.build_panel(project, narrow, cx),
            ProjectTab::Compose => self.compose_panel(project, cx),
            ProjectTab::Environment => self.environment_panel(project, cx).into_any_element(),
            ProjectTab::Targets => self.targets_panel(project, cx).into_any_element(),
            ProjectTab::History => self.release_history_panel(Some(project.id), cx),
        };
        let release_project_id = project.id;
        div()
            .flex()
            .flex_col()
            .gap_6()
            .child(
                self.page_head(
                    project.name.clone(),
                    Button::new("project-release")
                        .primary()
                        .label("发布")
                        .disabled(project.config_error.is_some())
                        .on_click(cx.listener(move |view, _, window, cx| {
                            view.navigate(Page::Deploy, window, cx);
                            if view.deploy_project_id != Some(release_project_id) {
                                view.select_deploy_project(release_project_id, window, cx);
                            }
                        })),
                ),
            )
            .child(
                self.panel(
                    if project.manifest_mode {
                        "devingpanel.yaml"
                    } else {
                        "项目配置来源"
                    },
                    if project.manifest_mode {
                        "开发者维护的非秘密发布方案"
                    } else {
                        "当前为本机表单配置，可明确迁移到项目文件"
                    },
                    div()
                        .px_6()
                        .pb_6()
                        .flex()
                        .flex_col()
                        .items_start()
                        .gap_3()
                        .child(if let Some(error) = &project.config_error {
                            format!("配置待修复：{error}")
                        } else if project.manifest_mode {
                            format!(
                                "配置已读取 · {} 台目标主机 · 发布前仍需完整预检",
                                project.host_ids.len()
                            )
                        } else {
                            "开发者可以将项目配置交付为 devingpanel.yaml。".into()
                        })
                        .child(if project.manifest_mode {
                            Button::new("manifest-reload")
                                .label("重新读取配置")
                                .on_click(cx.listener(|view, _, window, cx| {
                                    view.reload_project_configs(window, cx)
                                }))
                        } else {
                            let id = project.id;
                            Button::new("manifest-adopt")
                                .label("改用 devingpanel.yaml")
                                .on_click(cx.listener(move |view, _, window, cx| {
                                    view.confirm_adopt_manifest(id, window, cx)
                                }))
                        })
                        .when(
                            project.manifest_mode && project.config_error.is_some(),
                            |el| {
                                let id = project.id;
                                el.child(
                                    Button::new("manifest-targets-confirm")
                                        .label("管理员确认目标绑定")
                                        .on_click(cx.listener(move |view, _, window, cx| {
                                            view.confirm_manifest_targets(id, window, cx)
                                        })),
                                )
                            },
                        ),
                ),
            )
            .child(tab_row)
            .child(body)
    }

    pub(super) fn clear_batch_preflight(&mut self) {
        self.deploy_batch_check_generation = self.deploy_batch_check_generation.wrapping_add(1);
        if let Some(cancel) = self.deploy_batch_check_cancel.take() {
            cancel.store(true, Ordering::Relaxed);
        }
        self.deploy_batch_check_loading = false;
        self.deploy_batch_check = None;
    }

    pub(super) fn select_deploy_project(
        &mut self,
        id: i64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.deploy_run_busy && self.deploy_run_project_id != Some(id) {
            window.push_notification(Notification::warning("发布进行中，暂不能切换项目。"), cx);
            return;
        }
        if let Some(project) = self
            .projects
            .iter()
            .find(|project| project.id == id)
            .cloned()
        {
            self.clear_batch_preflight();
            if self.deploy_run_project_id != Some(id) {
                self.deploy_run_project_id = None;
                self.deploy_run_host_id = None;
                self.deploy_run_stage.clear();
                self.deploy_run_log.clear();
                self.deploy_run_transfer = None;
                self.deploy_run_started = None;
                self.deploy_run_elapsed_secs = 0;
            }
            self.deploy_project_id = Some(id);
            self.deploy_host_ids = project.host_ids.clone();
            self.deploy_parallel = 1;
            self.deploy_tag_generation = self.deploy_tag_generation.wrapping_add(1);
            let generation = self.deploy_tag_generation;
            self.deploy_tag_loading = true;
            self.deploy_tag_error = None;
            self.deploy_tag_plan_generation = self.deploy_tag_plan_generation.wrapping_add(1);
            self.deploy_tag_plan = None;
            self.deploy_batch_plan = None;
            self.deploy_tag_plan_loading = false;
            self.deploy_tag
                .update(cx, |input, cx| input.set_value("", window, cx));
            cx.notify();
            cx.spawn_in(window, async move |view, cx| {
                let result = cx
                    .background_executor()
                    .spawn(async move { generate_release_tag(&project) })
                    .await;
                let _ = cx.update(|window, app| {
                    let _ = view.update(app, |view, cx| {
                        if view.deploy_project_id != Some(id)
                            || view.deploy_tag_generation != generation
                        {
                            return;
                        }
                        view.deploy_tag_loading = false;
                        match result {
                            Ok(tag) => {
                                view.deploy_tag
                                    .update(cx, |input, cx| input.set_value(tag, window, cx));
                                view.schedule_tag_plan(cx);
                            }
                            Err(error) => view.deploy_tag_error = Some(error),
                        }
                        cx.notify();
                    });
                });
            })
            .detach();
            self.refresh_deploy_environment(cx);
        }
    }

    pub(super) fn refresh_build_capabilities(&mut self, cx: &mut Context<Self>) {
        if self.deploy_build_loading {
            return;
        }
        self.deploy_build_generation = self.deploy_build_generation.wrapping_add(1);
        let generation = self.deploy_build_generation;
        self.deploy_build_loading = true;
        cx.notify();
        cx.spawn(async move |view, cx| {
            let result = cx
                .background_executor()
                .spawn(async { docker::build_platforms() })
                .await;
            let _ = view.update(cx, |view, cx| {
                if view.deploy_build_generation == generation {
                    view.deploy_build_loading = false;
                    view.deploy_build_platforms = Some(result);
                    view.schedule_tag_plan(cx);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    pub(super) fn refresh_deploy_environment(&mut self, cx: &mut Context<Self>) {
        self.clear_batch_preflight();
        self.refresh_deploy_config(cx);
        self.refresh_deploy_targets(cx);
        self.deploy_env_generation = self.deploy_env_generation.wrapping_add(1);
        let generation = self.deploy_env_generation;
        let Some(project_id) = self.deploy_project_id else {
            return;
        };
        let host_ids = self.deploy_host_ids.clone();
        self.deploy_env_result = None;
        self.deploy_env_loading = !host_ids.is_empty();
        if host_ids.is_empty() {
            self.deploy_env_result = Some(Err("请先选择目标主机。".into()));
            cx.notify();
            return;
        }
        cx.notify();
        cx.spawn(async move |view, cx| {
            let result = cx
                .background_executor()
                .spawn(
                    async move { env_store::check_deployment_environment(project_id, &host_ids) },
                )
                .await;
            let _ = view.update(cx, |view, cx| {
                if view.deploy_project_id != Some(project_id)
                    || view.deploy_env_generation != generation
                {
                    return;
                }
                view.deploy_env_loading = false;
                view.deploy_env_result = Some(result);
                cx.notify();
            });
        })
        .detach();
    }

    fn refresh_deploy_if_current(&mut self, project_id: i64, cx: &mut Context<Self>) {
        if self.page != Page::Deploy || self.deploy_project_id != Some(project_id) {
            return;
        }
        if let Some(project) = self
            .projects
            .iter()
            .find(|project| project.id == project_id)
        {
            if project.manifest_mode {
                self.deploy_host_ids = project.host_ids.clone();
            } else {
                self.deploy_host_ids
                    .retain(|id| project.host_ids.contains(id));
            }
            self.deploy_parallel = self.deploy_parallel.min(self.deploy_host_ids.len().max(1));
        }
        self.refresh_deploy_environment(cx);
    }

    fn refresh_deploy_targets(&mut self, cx: &mut Context<Self>) {
        self.clear_batch_preflight();
        self.deploy_tag_plan_generation = self.deploy_tag_plan_generation.wrapping_add(1);
        self.deploy_tag_plan = None;
        self.deploy_batch_plan = None;
        self.deploy_tag_plan_loading = false;
        if let Some(cancel) = self.deploy_target_cancel.take() {
            cancel.store(true, Ordering::Relaxed);
        }
        self.deploy_target_generation = self.deploy_target_generation.wrapping_add(1);
        let generation = self.deploy_target_generation;
        let Some(project) = self
            .deploy_project_id
            .and_then(|id| self.projects.iter().find(|project| project.id == id))
            .cloned()
        else {
            return;
        };
        let project_id = project.id;
        let host_ids = self.deploy_host_ids.clone();
        let hosts = self.hosts.clone();
        self.deploy_target_result = None;
        if host_ids.is_empty() {
            self.deploy_target_loading = false;
            self.deploy_target_result = Some(Err("请先选择目标主机。".into()));
            cx.notify();
            return;
        }
        if !self.host_loaded {
            self.deploy_target_loading = true;
            cx.notify();
            return;
        }
        let cancel = Arc::new(AtomicBool::new(false));
        self.deploy_target_cancel = Some(cancel.clone());
        self.deploy_target_loading = true;
        cx.notify();
        cx.spawn(async move |view, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { release::probe_targets(&project, &host_ids, &hosts, cancel) })
                .await;
            let _ = view.update(cx, |view, cx| {
                if view.deploy_project_id == Some(project_id)
                    && view.deploy_target_generation == generation
                {
                    view.deploy_target_loading = false;
                    view.deploy_target_cancel = None;
                    view.deploy_target_result = Some(result);
                    view.schedule_tag_plan(cx);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    pub(super) fn schedule_tag_plan(&mut self, cx: &mut Context<Self>) {
        self.clear_batch_preflight();
        self.deploy_tag_plan_generation = self.deploy_tag_plan_generation.wrapping_add(1);
        let generation = self.deploy_tag_plan_generation;
        self.deploy_tag_plan = None;
        self.deploy_batch_plan = None;
        self.deploy_tag_plan_loading = false;
        let Some(project) = self
            .deploy_project_id
            .and_then(|id| self.projects.iter().find(|project| project.id == id))
            .cloned()
        else {
            cx.notify();
            return;
        };
        let Some(Ok(targets)) = &self.deploy_target_result else {
            cx.notify();
            return;
        };
        let Some(Ok(supported)) = &self.deploy_build_platforms else {
            cx.notify();
            return;
        };
        if targets
            .iter()
            .any(|(_, platform)| !supported.contains(platform))
        {
            cx.notify();
            return;
        }
        let targets = targets.clone();
        self.deploy_tag_plan_loading = true;
        cx.notify();
        cx.spawn(async move |view, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(400))
                .await;
            let tag = view
                .update(cx, |view, cx| {
                    (view.page == Page::Deploy
                        && view.deploy_project_id == Some(project.id)
                        && view.deploy_tag_plan_generation == generation)
                        .then(|| view.deploy_tag.read(cx).value().to_string())
                })
                .ok()
                .flatten();
            let Some(tag) = tag else {
                return;
            };
            let (result, batch) = cx
                .background_executor()
                .spawn(async move {
                    if project.manifest_mode && targets.len() > 1 {
                        let batch = stack_release::plan_batch(&project, &tag, &targets);
                        let result = batch.as_ref().map_err(Clone::clone).and_then(|plan| {
                            let images = plan
                                .builds
                                .iter()
                                .map(|build| release::PlannedImage {
                                    platform: build.platform.clone(),
                                    reference: build.reference.clone(),
                                })
                                .collect::<Vec<_>>();
                            release::check_planned_tags(&images)?;
                            Ok(images)
                        });
                        (result, Some(batch))
                    } else {
                        (release::check_image_tags(&project, &tag, &targets), None)
                    }
                })
                .await;
            let _ = view.update(cx, |view, cx| {
                if view.deploy_tag_plan_generation == generation && view.page == Page::Deploy {
                    view.deploy_tag_plan_loading = false;
                    view.deploy_tag_plan = Some(result);
                    view.deploy_batch_plan = batch;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn start_batch_preflight(&mut self, cx: &mut Context<Self>) {
        let Some(project_id) = self.deploy_project_id else {
            return;
        };
        if self.deploy_host_ids.len() < 2
            || self.deploy_batch_check_loading
            || self.deploy_run_busy
            || self.deploy_config_loading
            || self.deploy_env_loading
            || self.deploy_target_loading
            || self.deploy_tag_plan_loading
            || !matches!(self.deploy_config_result, Some(Ok(())))
            || !matches!(self.deploy_env_result, Some(Ok(())))
            || !matches!(self.deploy_batch_plan, Some(Ok(_)))
            || !matches!(self.deploy_tag_plan, Some(Ok(_)))
        {
            return;
        }
        self.clear_batch_preflight();
        let generation = self.deploy_batch_check_generation;
        let cancel = Arc::new(AtomicBool::new(false));
        self.deploy_batch_check_cancel = Some(cancel.clone());
        self.deploy_batch_check_loading = true;
        let tag = self.deploy_tag.read(cx).value().to_string();
        cx.notify();
        cx.spawn(async move |view, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    stack_release::preflight_batch(project_id, &tag, cancel).map(Arc::new)
                })
                .await;
            let _ = view.update(cx, |view, cx| {
                if view.page == Page::Deploy
                    && view.deploy_project_id == Some(project_id)
                    && view.deploy_batch_check_generation == generation
                {
                    view.deploy_batch_check_loading = false;
                    view.deploy_batch_check_cancel = None;
                    view.deploy_batch_check = Some(result);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn refresh_deploy_config(&mut self, cx: &mut Context<Self>) {
        self.deploy_config_generation = self.deploy_config_generation.wrapping_add(1);
        let generation = self.deploy_config_generation;
        let Some(project) = self
            .deploy_project_id
            .and_then(|id| self.projects.iter().find(|project| project.id == id))
            .cloned()
        else {
            return;
        };
        let project_id = project.id;
        let host_ids = self.deploy_host_ids.clone();
        self.deploy_config_result = None;
        self.deploy_config_loading = true;
        cx.notify();
        cx.spawn(async move |view, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { release::check_project_config(&project, &host_ids) })
                .await;
            let _ = view.update(cx, |view, cx| {
                if view.deploy_project_id == Some(project_id)
                    && view.deploy_config_generation == generation
                {
                    view.deploy_config_loading = false;
                    view.deploy_config_result = Some(result);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    pub(super) fn deploy_ready(&self, project: &Project, ids: &[i64]) -> bool {
        project.manifest_mode
            && ids.len() == 1
            && self.deploy_project_id == Some(project.id)
            && !self.deploy_run_busy
            && self.batch_resolving.is_empty()
            && self.rollback_running.is_none()
            && matches!(self.deploy_config_result, Some(Ok(())))
            && matches!(self.deploy_env_result, Some(Ok(())))
            && matches!(self.deploy_tag_plan, Some(Ok(_)))
            && matches!(&self.deploy_target_result, Some(Ok(targets)) if targets.len() == 1 && targets[0].0 == ids[0])
            && !self.deploy_config_loading
            && !self.deploy_env_loading
            && !self.deploy_target_loading
            && !self.deploy_tag_plan_loading
    }

    pub(super) fn batch_start_ready(&self, project: &Project, ids: &[i64]) -> bool {
        project.manifest_mode
            && ids.len() > 1
            && project.host_ids == ids
            && self.deploy_project_id == Some(project.id)
            && self.deploy_parallel == 1
            && !self.deploy_run_busy
            && self.batch_resolving.is_empty()
            && self.rollback_running.is_none()
            && !self.deploy_batch_check_loading
            && !self.deploy_config_loading
            && !self.deploy_env_loading
            && !self.deploy_target_loading
            && !self.deploy_tag_plan_loading
            && matches!(self.deploy_config_result, Some(Ok(())))
            && matches!(self.deploy_env_result, Some(Ok(())))
            && matches!((&self.deploy_tag_plan, &self.deploy_batch_check), (Some(Ok(images)), Some(Ok(snapshot)))
                if images.iter().map(|image| image.reference.as_str()).collect::<Vec<_>>()
                    == snapshot.plan.builds.iter().map(|build| build.reference.as_str()).collect::<Vec<_>>()
                    && snapshot.targets.iter().map(|target| target.host_id).collect::<Vec<_>>() == ids
                    && snapshot.env_files.len() == ids.len())
    }

    pub(super) fn confirm_batch_deploy(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(project) = self
            .deploy_project_id
            .and_then(|id| self.projects.iter().find(|item| item.id == id))
        else {
            return;
        };
        let ids = self.deploy_host_ids.clone();
        if !self.batch_start_ready(project, &ids) {
            window.push_notification(
                Notification::warning("批次预检已失效，请重新核对目标。"),
                cx,
            );
            return;
        }
        let Some(Ok(snapshot)) = &self.deploy_batch_check else {
            return;
        };
        let snapshot = snapshot.clone();
        let generation = self.deploy_batch_check_generation;
        let project_id = project.id;
        let name = project.name.clone();
        let tag = self.deploy_tag.read(cx).value().to_string();
        let targets = snapshot
            .targets
            .iter()
            .map(|target| target.host.name.clone())
            .collect::<Vec<_>>()
            .join(" → ");
        let images = snapshot.plan.builds.len();
        let view = cx.weak_entity();
        window.open_dialog(cx, move |dialog, _, _| {
            let confirm_view = view.clone();
            let snapshot = snapshot.clone();
            let tag = tag.clone();
            dialog.title("确认多主机批次发布").w(px(560.))
                .child(div().flex().flex_col().gap_3()
                    .child(format!("项目：{name} · 发布标识：{tag}"))
                    .child(format!("顺序：{targets} · 共享镜像 {images} 份"))
                    .child("当前并行度为 1。执行前将重新核对完整预检快照；只构建一次共享镜像，再依次处理目标。")
                    .child("第一台全部服务健康后会暂停，须到历史页人工核实并确认，才可显式继续下一台。失败或结果未知不会自动重试或回滚。"))
                .footer(div().flex().justify_end().gap_2()
                    .child(Button::new("batch-start-cancel").debug_selector(|| "batch-start-cancel".into())
                        .label("取消").on_click(|_, window, cx| window.close_dialog(cx)))
                    .child(Button::new("batch-start-confirm").primary()
                        .debug_selector(|| "batch-start-confirm".into()).label("确认开始批次")
                        .on_click(move |_, window, cx| {
                            window.close_dialog(cx);
                            let _ = confirm_view.update(cx, |view, cx| {
                                view.start_batch_deploy(project_id, &tag, generation, snapshot.clone(), window, cx)
                            });
                        })))
        });
    }

    fn start_batch_deploy(
        &mut self,
        project_id: i64,
        tag: &str,
        generation: u64,
        snapshot: Arc<stack_release::BatchPreflight>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(project) = self.projects.iter().find(|item| item.id == project_id) else {
            return;
        };
        if !self.batch_start_ready(project, &self.deploy_host_ids)
            || self.deploy_batch_check_generation != generation
            || self.deploy_tag.read(cx).value() != tag
            || !matches!(&self.deploy_batch_check, Some(Ok(current)) if Arc::ptr_eq(current, &snapshot))
        {
            window.push_notification(
                Notification::warning("批次确认期间配置已变化，请重新预检。"),
                cx,
            );
            return;
        }
        self.deploy_run_busy = true;
        self.deploy_run_project_id = Some(project_id);
        self.deploy_run_host_id = snapshot.targets.first().map(|target| target.host_id);
        self.deploy_run_stage = "重新预检批次并准备共享构建".into();
        self.deploy_run_log.clear();
        self.deploy_run_log.push(self.deploy_run_stage.clone());
        self.deploy_run_started = Some(Instant::now());
        self.deploy_run_elapsed_secs = 0;
        self.deploy_run_transfer = None;
        cx.notify();
        let (tx, rx) = mpsc::channel::<BatchStartEvent>();
        cx.background_executor()
            .spawn(async move {
                let result = stack_release::run_batch_sequential(
                    project_id,
                    &snapshot,
                    Arc::new(AtomicBool::new(false)),
                    |event| {
                        if let Some((sent, total)) = transfer_progress(event) {
                            let _ = tx.send(BatchStartEvent::Transfer { sent, total });
                            return;
                        }
                        if let Some(stage) = safe_deploy_stage(event) {
                            let _ = tx.send(BatchStartEvent::Stage(stage));
                        }
                    },
                );
                let _ = tx.send(BatchStartEvent::Finished(result));
            })
            .detach();
        cx.spawn_in(window, async move |view, cx| {
            loop {
                match rx.try_recv() {
                    Ok(BatchStartEvent::Stage(stage)) => {
                        let _ = view.update(cx, |view, cx| {
                            view.deploy_run_stage = stage.clone();
                            view.deploy_run_transfer = None;
                            if view.deploy_run_log.last() != Some(&stage) {
                                view.deploy_run_log.push(stage);
                            }
                            cx.notify();
                        });
                    }
                    Ok(BatchStartEvent::Transfer { sent, total }) => {
                        let _ = view.update(cx, |view, cx| {
                            view.deploy_run_transfer = Some((sent, total));
                            cx.notify();
                        });
                    }
                    Ok(BatchStartEvent::Finished(result)) => {
                        let _ = cx.update(|window, app| {
                            let _ = view.update(app, |view, cx| {
                                view.deploy_run_busy = false;
                                view.deploy_run_elapsed_secs = view
                                    .deploy_run_started
                                    .map_or(0, |start| start.elapsed().as_secs());
                                view.deploy_run_transfer = None;
                                view.deploy_run_stage = if result.is_ok() {
                                    "当前主机健康，等待人工确认"
                                } else {
                                    "批次未完成，请核实历史"
                                }
                                .into();
                                view.deploy_run_log.push(view.deploy_run_stage.clone());
                                view.refresh_release_tasks(None, cx);
                                view.refresh_release_tasks(Some(project_id), cx);
                                cx.notify();
                            });
                            window.push_notification(
                                match result {
                                    Ok(stack_release::BatchProgress::AwaitingConfirmation {
                                        batch_id,
                                        ..
                                    }) => Notification::success(format!(
                                        "批次 {} 当前主机已健康；请在历史页人工确认",
                                        &batch_id[..8]
                                    )),
                                    Ok(stack_release::BatchProgress::Complete { batch_id }) => {
                                        Notification::success(format!(
                                            "批次 {} 全部完成",
                                            &batch_id[..8]
                                        ))
                                    }
                                    Err(error) => Notification::warning(format!(
                                        "批次 {}：{}；请核实历史，勿重复开始。",
                                        error.batch_id.as_deref().unwrap_or("未创建"),
                                        error.message
                                    )),
                                },
                                app,
                            );
                        });
                        break;
                    }
                    Err(TryRecvError::Disconnected) => {
                        let _ = cx.update(|window, app| {
                            let _ = view.update(app, |view, cx| {
                                view.deploy_run_busy = false;
                                view.deploy_run_transfer = None;
                                view.deploy_run_stage = "执行进程异常中断；请核实批次历史".into();
                                view.refresh_release_tasks(None, cx);
                                view.refresh_release_tasks(Some(project_id), cx);
                                cx.notify();
                            });
                            window.push_notification(
                                Notification::error("批次执行通道中断；结果待核实，不得重新开始。"),
                                app,
                            );
                        });
                        break;
                    }
                    Err(TryRecvError::Empty) => {
                        let _ = view.update(cx, |view, cx| {
                            if let Some(start) = view.deploy_run_started {
                                let seconds = start.elapsed().as_secs();
                                if view.deploy_run_elapsed_secs != seconds {
                                    view.deploy_run_elapsed_secs = seconds;
                                    cx.notify();
                                }
                            }
                        });
                        cx.background_executor()
                            .timer(Duration::from_millis(200))
                            .await;
                    }
                }
            }
        })
        .detach();
    }

    pub(super) fn confirm_deploy(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(project) = self
            .deploy_project_id
            .and_then(|id| self.projects.iter().find(|p| p.id == id))
        else {
            return;
        };
        let ids = self.deploy_host_ids.clone();
        if ids.len() > 1 && MULTI_HOST_START_ENABLED {
            self.confirm_batch_deploy(window, cx);
            return;
        }
        if !self.deploy_ready(project, &ids) {
            window.push_notification(
                Notification::warning("仅支持预检通过的 YAML v2 单主机发布。"),
                cx,
            );
            return;
        }
        let project_id = project.id;
        let host_id = ids[0];
        let name = project.name.clone();
        let Some(confirmed_host) = self.hosts.iter().find(|host| host.id == host_id).cloned()
        else {
            window.push_notification(Notification::warning("目标主机资料尚未读取。"), cx);
            return;
        };
        let host_name = confirmed_host.name.clone();
        let tag = self.deploy_tag.read(cx).value().to_string();
        let planned = self
            .deploy_tag_plan
            .as_ref()
            .and_then(|plan| plan.as_ref().ok())
            .map(|images| {
                images
                    .iter()
                    .map(|image| image.reference.clone())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let targets = self
            .deploy_target_result
            .as_ref()
            .and_then(|result| result.as_ref().ok())
            .cloned()
            .unwrap_or_default();
        let source_digest = match project_store::effective(project_id).and_then(|fresh| {
            let current = release::plan_images(&fresh, &tag, &targets)?;
            if current
                .iter()
                .map(|image| &image.reference)
                .ne(planned.iter())
            {
                return Err("镜像计划已变化，请重新检查。".into());
            }
            release::source_digest(&fresh)
        }) {
            Ok(digest) => digest,
            Err(error) => {
                window.push_notification(Notification::warning(error), cx);
                self.refresh_deploy_environment(cx);
                return;
            }
        };
        let images = planned.join("、");
        let view = cx.weak_entity();
        window.open_dialog(cx, move |dialog, _, _| {
            let confirm_view = view.clone();
            let tag = tag.clone();
            let source_digest = source_digest.clone();
            let confirmed_host = confirmed_host.clone();
            dialog.title("确认发布整个 Compose 项目").w(px(540.))
                .child(div().flex().flex_col().gap_3()
                    .child(format!("项目：{name} · 主机：{host_name}"))
                    .child(format!("本次镜像：{images}"))
                    .child("将逐个构建并传输自有镜像，在目标主机启动 Compose 中的全部服务。执行前会重新预检项目文件和主机；运行期间请勿关闭程序。")
                    .child("仅全部服务健康且材料及远端身份再次核实通过，才记录完成；失败或连接不确定会阻断后续发布，不自动重试。"))
                .footer(div().flex().justify_end().gap_2()
                    .child(Button::new("deploy-cancel").debug_selector(|| "deploy-cancel-button".into()).label("取消")
                        .on_click(|_, window, cx| window.close_dialog(cx)))
                    .child(Button::new("deploy-confirm").primary().debug_selector(|| "deploy-confirm-button".into()).label("确认开始发布")
                        .on_click(move |_, window, cx| {
                            window.close_dialog(cx);
                            let tag = tag.clone();
                            let digest = source_digest.clone();
                            let host = confirmed_host.clone();
                            let _ = confirm_view.update(cx, |view, cx| view.start_deploy(project_id, host, tag, digest, window, cx));
                        })))
        });
    }

    fn start_deploy(
        &mut self,
        project_id: i64,
        confirmed_host: Host,
        tag: String,
        confirmed_digest: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let host_id = confirmed_host.id;
        let Some(project) = self
            .projects
            .iter()
            .find(|project| project.id == project_id)
        else {
            return;
        };
        if self.deploy_host_ids != [host_id]
            || !self.deploy_ready(project, &[host_id])
            || self.deploy_tag.read(cx).value() != tag
        {
            window.push_notification(
                Notification::warning("发布配置已变化，请重新检查并确认。"),
                cx,
            );
            return;
        }
        let current_digest = project_store::effective(project_id)
            .and_then(|current| release::source_digest(&current));
        if current_digest.as_deref() != Ok(confirmed_digest.as_str()) {
            window.push_notification(
                Notification::warning("项目配置在确认期间变化；请重新检查并确认。"),
                cx,
            );
            self.refresh_deploy_environment(cx);
            return;
        }
        let host_unchanged = host_store::list().is_ok_and(|(hosts, _)| {
            hosts
                .iter()
                .any(|host| same_deploy_host(host, &confirmed_host))
        });
        if !host_unchanged {
            window.push_notification(
                Notification::warning("目标主机在确认期间变化，请重新检查。"),
                cx,
            );
            self.refresh_deploy_environment(cx);
            return;
        }
        self.deploy_run_busy = true;
        self.deploy_run_project_id = Some(project_id);
        self.deploy_run_host_id = Some(host_id);
        self.deploy_run_stage = "重新预检并准备材料".into();
        self.deploy_run_log.clear();
        self.deploy_run_log.push(self.deploy_run_stage.clone());
        self.deploy_run_transfer = None;
        self.deploy_run_started = Some(Instant::now());
        self.deploy_run_elapsed_secs = 0;
        cx.notify();
        let (tx, rx) = mpsc::channel::<DeployRunEvent>();
        cx.background_executor()
            .spawn(async move {
                let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                let mut last_percent = None;
                let result = release::run_single_host_confirmed(
                    project_id,
                    host_id,
                    &tag,
                    &confirmed_digest,
                    cancel,
                    |event| {
                        if event.starts_with("传输镜像：") {
                            last_percent = None;
                        }
                        if let Some((sent, total)) = transfer_progress(event) {
                            let percent = (u128::from(sent) * 100 / u128::from(total)) as u8;
                            if last_percent != Some(percent) {
                                let _ = tx.send(DeployRunEvent::Transfer { sent, total });
                                last_percent = Some(percent);
                            }
                            return;
                        }
                        if let Some(stage) = safe_deploy_stage(event) {
                            let _ = tx.send(DeployRunEvent::Stage(stage));
                        }
                    },
                )
                .and_then(|id| {
                    let task = release_store::get_task(&id)?;
                    if task.health_result.as_deref() != Some("healthy") {
                        return Err(format!(
                            "发布任务 {id} 未达到全部服务健康；请在发布历史核实，不会自动重试。"
                        ));
                    }
                    let _ = tx.send(DeployRunEvent::Stage(
                        "二次核实版本材料、镜像和服务健康".into(),
                    ));
                    release::confirm_healthy_and_complete(&id).map(|count| (id, count))
                });
                let _ = tx.send(DeployRunEvent::Finished(result));
            })
            .detach();
        cx.spawn_in(window, async move |view, cx| {
            loop {
                match rx.try_recv() {
                    Ok(DeployRunEvent::Stage(stage)) => {
                        let _ = view.update(cx, |view, cx| {
                            view.deploy_run_stage = stage.clone();
                            view.deploy_run_transfer = None;
                            if view.deploy_run_log.last() != Some(&stage) {
                                view.deploy_run_log.push(stage);
                            }
                            cx.notify();
                        });
                    }
                    Ok(DeployRunEvent::Transfer { sent, total }) => {
                        let _ = view.update(cx, |view, cx| {
                            view.deploy_run_transfer = Some((sent, total));
                            cx.notify();
                        });
                    }
                    Ok(DeployRunEvent::Finished(result)) => {
                        let _ = cx.update(|window, app| {
                            let _ = view.update(app, |view, cx| {
                                view.deploy_run_busy = false;
                                view.deploy_run_elapsed_secs = view
                                    .deploy_run_started
                                    .map_or(0, |started| started.elapsed().as_secs());
                                view.deploy_run_transfer = None;
                                view.deploy_run_stage = if result.is_ok() {
                                    "全部服务健康 · 发布完成".into()
                                } else {
                                    "发布未完成 · 请核实历史记录".into()
                                };
                                view.deploy_run_log.push(view.deploy_run_stage.clone());
                                view.refresh_release_tasks(None, cx);
                                view.refresh_release_tasks(Some(project_id), cx);
                                view.refresh_deploy_environment(cx);
                                cx.notify();
                            });
                            window.push_notification(
                                match result {
                                    Ok((id, count)) => Notification::success(format!(
                                        "发布 {id} 完成，{count} 个服务健康"
                                    )),
                                    Err(error) => Notification::error(format!(
                                        "{error}；请到发布历史核实，勿重复点击。"
                                    )),
                                },
                                app,
                            );
                        });
                        break;
                    }
                    Err(TryRecvError::Disconnected) => {
                        let _ = cx.update(|window, app| {
                            let _ = view.update(app, |view, cx| {
                                view.deploy_run_busy = false;
                                view.deploy_run_elapsed_secs = view
                                    .deploy_run_started
                                    .map_or(0, |started| started.elapsed().as_secs());
                                view.deploy_run_transfer = None;
                                view.deploy_run_stage =
                                    "执行进程异常中止；请在历史页人工核实".into();
                                view.refresh_release_tasks(None, cx);
                                view.refresh_release_tasks(Some(project_id), cx);
                                cx.notify();
                            });
                            window.push_notification(
                                Notification::error(
                                    "发布执行异常中断，请先在历史页核实，不得重试。",
                                ),
                                app,
                            );
                        });
                        break;
                    }
                    Err(TryRecvError::Empty) => {
                        let _ = view.update(cx, |view, cx| {
                            if let Some(started) = view.deploy_run_started {
                                let seconds = started.elapsed().as_secs();
                                if view.deploy_run_elapsed_secs != seconds {
                                    view.deploy_run_elapsed_secs = seconds;
                                    cx.notify();
                                }
                            }
                        });
                        cx.background_executor()
                            .timer(Duration::from_millis(200))
                            .await
                    }
                }
            }
        })
        .detach();
    }

    fn deploy_page(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .gap_6()
            .child(self.page_head("发布", div()))
            .child(
                div().flex().child(
                    div()
                        .flex()
                        .gap_1()
                        .p_1()
                        .rounded_md()
                        .bg(self.palette().accent)
                        .child(self.tab(
                            "execute",
                            "执行",
                            self.deploy_tab == DeployTab::Execute,
                            cx,
                            |this, _, cx| {
                                this.deploy_tab = DeployTab::Execute;
                                cx.notify();
                            },
                        ))
                        .child(self.tab(
                            "history",
                            "历史",
                            self.deploy_tab == DeployTab::History,
                            cx,
                            |this, _, cx| {
                                this.deploy_tab = DeployTab::History;
                                this.refresh_release_tasks(None, cx);
                                cx.notify();
                            },
                        )),
                ),
            )
            .child(if self.deploy_tab == DeployTab::Execute {
                if !self.project_loaded {
                    self.empty_card("正在读取项目…", assets::IconName::Folder)
                        .into_any_element()
                } else if let Some(project) = self
                    .deploy_project_id
                    .and_then(|id| self.projects.iter().find(|project| project.id == id))
                    .or_else(|| self.projects.first())
                {
                    self.deploy_idle(project, cx).into_any_element()
                } else if let Some(error) = &self.project_error {
                    self.panel(
                        "项目数据读取失败",
                        "",
                        div().px_6().pb_6().child(error.clone()),
                    )
                    .into_any_element()
                } else {
                    self.project_empty("先创建项目", cx).into_any_element()
                }
            } else {
                self.release_history_panel(None, cx)
            })
    }

    fn open_deploy_targets_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(project) = self
            .deploy_project_id
            .and_then(|id| self.projects.iter().find(|project| project.id == id))
        else {
            return;
        };
        if project.manifest_mode {
            window.push_notification(
                Notification::warning("YAML 项目的目标顺序由开发者预设。"),
                cx,
            );
            return;
        }
        let bound = project.host_ids.clone();
        let hosts = self.hosts.clone();
        let draft = Rc::new(RefCell::new(self.deploy_host_ids.clone()));
        let view = cx.weak_entity();
        let muted = self.palette().muted;
        let border = self.palette().border;
        window.open_dialog(cx, move |dialog, _, _| {
            let selected = draft.borrow().clone();
            let mut sorted = bound.clone();
            sorted.sort_by_key(|id| {
                selected
                    .iter()
                    .position(|value| value == id)
                    .unwrap_or(usize::MAX)
            });
            let mut rows = div().flex().flex_col();
            for (index, id) in sorted.iter().copied().enumerate() {
                let checked = selected.contains(&id);
                let up_draft = draft.clone();
                let down_draft = draft.clone();
                let check_draft = draft.clone();
                let host = hosts.iter().find(|host| host.id == id);
                rows = rows.child(
                    div()
                        .h(px(52.))
                        .min_w(px(690.))
                        .border_t_1()
                        .border_color(border)
                        .px_3()
                        .flex()
                        .items_center()
                        .child(
                            div()
                                .w(px(145.))
                                .flex_shrink_0()
                                .flex()
                                .items_center()
                                .gap_1()
                                .child((index + 1).to_string())
                                .child(
                                    Button::new(("deploy-target-up", id as usize))
                                        .icon(assets::IconName::ArrowUp)
                                        .accessibility_label("上移")
                                        .disabled(!checked || index == 0)
                                        .on_click(move |_, window, _| {
                                            let mut ids = up_draft.borrow_mut();
                                            if let Some(position) =
                                                ids.iter().position(|value| *value == id)
                                                && position > 0
                                            {
                                                ids.swap(position, position - 1);
                                                window.refresh();
                                            }
                                        }),
                                )
                                .child(
                                    Button::new(("deploy-target-down", id as usize))
                                        .icon(assets::IconName::ArrowDown)
                                        .accessibility_label("下移")
                                        .disabled(!checked || index + 1 >= selected.len())
                                        .on_click(move |_, window, _| {
                                            let mut ids = down_draft.borrow_mut();
                                            if let Some(position) =
                                                ids.iter().position(|value| *value == id)
                                                && position + 1 < ids.len()
                                            {
                                                ids.swap(position, position + 1);
                                                window.refresh();
                                            }
                                        }),
                                ),
                        )
                        .child(
                            div().w(px(70.)).flex_shrink_0().child(
                                base::Checkbox::new(("deploy-target-check", id as usize))
                                    .checked(checked)
                                    .accessibility_label(format!(
                                        "选择 {}",
                                        host.map_or("主机", |host| host.name.as_str())
                                    ))
                                    .on_change(move |_, _, window, _| {
                                        let mut ids = check_draft.borrow_mut();
                                        if let Some(position) =
                                            ids.iter().position(|value| *value == id)
                                        {
                                            ids.remove(position);
                                        } else {
                                            ids.push(id);
                                        }
                                        window.refresh();
                                    }),
                            ),
                        )
                        .child(
                            div()
                                .w(px(95.))
                                .flex_shrink_0()
                                .text_color(muted)
                                .child("未连接"),
                        )
                        .child(
                            div()
                                .w(px(130.))
                                .flex_shrink_0()
                                .child(host.map_or("已删除".into(), |host| host.name.clone())),
                        )
                        .child(
                            div()
                                .w(px(180.))
                                .flex_shrink_0()
                                .child(host.map_or("—".into(), |host| host.address.clone())),
                        )
                        .child(host.map_or("—".into(), |host| host.group.clone())),
                );
            }
            let save_view = view.clone();
            let save_draft = draft.clone();
            dialog
                .title("更改目标主机")
                .w(px(760.))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_4()
                        .child(
                            div()
                                .text_color(muted)
                                .child("仅可选择该项目绑定的主机，可调整发布顺序。"),
                        )
                        .child(
                            div()
                                .id("deploy-target-dialog-scroll")
                                .overflow_x_scroll()
                                .border_1()
                                .border_color(border)
                                .rounded_md()
                                .child(
                                    div()
                                        .min_w(px(690.))
                                        .h(px(40.))
                                        .px_3()
                                        .flex()
                                        .items_center()
                                        .text_color(muted)
                                        .child(div().w(px(145.)).flex_shrink_0().child("顺序"))
                                        .child(div().w(px(70.)).flex_shrink_0().child("选择"))
                                        .child(div().w(px(95.)).flex_shrink_0().child("状态"))
                                        .child(div().w(px(130.)).flex_shrink_0().child("主机名"))
                                        .child(div().w(px(180.)).flex_shrink_0().child("地址"))
                                        .child("分组"),
                                )
                                .child(rows),
                        ),
                )
                .footer(
                    div()
                        .flex()
                        .justify_end()
                        .gap_2()
                        .child(
                            Button::new("deploy-target-cancel")
                                .label("取消")
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            Button::new("deploy-target-confirm")
                                .primary()
                                .label("确认")
                                .on_click(move |_, window, cx| {
                                    let ids = save_draft.borrow().clone();
                                    let _ = save_view.update(cx, |view, cx| {
                                        view.deploy_host_ids = ids;
                                        view.deploy_parallel = view
                                            .deploy_parallel
                                            .min(view.deploy_host_ids.len().max(1));
                                        view.refresh_deploy_environment(cx);
                                        cx.notify();
                                    });
                                    window.close_dialog(cx);
                                }),
                        ),
                )
        });
    }

    fn deploy_idle(&self, project: &Project, cx: &mut Context<Self>) -> impl IntoElement {
        let ids = if self.deploy_project_id == Some(project.id) {
            self.deploy_host_ids.clone()
        } else {
            project.host_ids.clone()
        };
        let (env_status, env_color) = if self.deploy_env_loading {
            (
                "环境配置 · 正在检查所选主机…".to_owned(),
                self.palette().muted,
            )
        } else {
            match &self.deploy_env_result {
                Some(Ok(())) => (
                    format!("环境配置 · 已核对 {} 台（仅本机）", ids.len()),
                    self.palette().success,
                ),
                Some(Err(error)) => (format!("环境配置 · {error}"), self.palette().warning),
                None => ("环境配置 · 等待检查".into(), self.palette().muted),
            }
        };
        let (build_status, build_color) = if self.deploy_build_loading {
            (
                "本地构建能力 · 正在检测 Buildx…".to_owned(),
                self.palette().muted,
            )
        } else {
            match &self.deploy_build_platforms {
                Some(Ok(platforms)) => (
                    format!("本地构建能力 · {}（尚未实际构建）", platforms.join("、")),
                    self.palette().success,
                ),
                Some(Err(error)) => (format!("本地构建能力 · {error}"), self.palette().warning),
                None => ("本地构建能力 · 等待检查".into(), self.palette().muted),
            }
        };
        let (config_status, config_color) = if self.deploy_config_loading {
            (
                "项目配置 · 正在检查 Compose 与文件…".to_owned(),
                self.palette().muted,
            )
        } else {
            match &self.deploy_config_result {
                Some(Ok(())) => (
                    "项目配置 · 本地静态检查通过（发布时仍需重查）".into(),
                    self.palette().success,
                ),
                Some(Err(error)) => (format!("项目配置 · {error}"), self.palette().warning),
                None => ("项目配置 · 等待检查".into(), self.palette().muted),
            }
        };
        let (target_status, target_color) = if self.deploy_target_loading {
            (
                "目标架构 · 正在经 SSH 核对所选主机…".to_owned(),
                self.palette().muted,
            )
        } else {
            match &self.deploy_target_result {
                Some(Ok(targets)) => {
                    let names = targets
                        .iter()
                        .map(|(id, platform)| {
                            let name = self
                                .hosts
                                .iter()
                                .find(|host| host.id == *id)
                                .map_or_else(|| id.to_string(), |host| host.name.clone());
                            format!("{name} {platform}")
                        })
                        .collect::<Vec<_>>()
                        .join(" · ");
                    match &self.deploy_build_platforms {
                        Some(Ok(supported)) => {
                            if let Some((_, platform)) = targets
                                .iter()
                                .find(|(_, platform)| !supported.contains(platform))
                            {
                                (
                                    format!("目标架构 · 当前本机构建器未声明支持 {platform}"),
                                    self.palette().warning,
                                )
                            } else {
                                (
                                    format!("目标架构 · {names}（SFTP v3 仅握手，写入待核实）"),
                                    self.palette().success,
                                )
                            }
                        }
                        Some(Err(_)) => (
                            format!("目标架构 · {names}；本机构建能力不可用"),
                            self.palette().warning,
                        ),
                        None => (
                            format!("目标架构 · {names}；本机构建能力待核对"),
                            self.palette().muted,
                        ),
                    }
                }
                Some(Err(error)) => (format!("目标架构 · {error}"), self.palette().warning),
                None => ("目标架构 · 等待检查".into(), self.palette().muted),
            }
        };
        let (tag_status, tag_color) = if self.deploy_tag_plan_loading {
            (
                "镜像 tag · 正在核对本机冲突…".to_owned(),
                self.palette().muted,
            )
        } else {
            match &self.deploy_tag_plan {
                Some(Ok(images)) => (
                    format!(
                        "计划镜像 · {}（尚未构建）",
                        images
                            .iter()
                            .map(|image| format!("{} {}", image.platform, image.reference))
                            .collect::<Vec<_>>()
                            .join(" · ")
                    ),
                    self.palette().success,
                ),
                Some(Err(error)) => (format!("镜像 tag · {error}"), self.palette().warning),
                None => (
                    "镜像 tag · 等待目标架构与本机构建能力核对".into(),
                    self.palette().muted,
                ),
            }
        };
        let project_view = cx.weak_entity();
        let project_options: Vec<_> = self
            .projects
            .iter()
            .map(|p| (p.id, p.name.clone()))
            .collect();
        let project_selector = Popover::new("deploy-project-select")
            .trigger(
                Button::new("deploy-project-trigger")
                    .h(px(36.))
                    .w_full()
                    .outline()
                    .disabled(self.deploy_run_busy)
                    .label(format!("{}  ▾", project.name)),
            )
            .content(move |_, _, cx| {
                let popover = cx.weak_entity();
                let mut options = div().w(px(200.)).p_1().flex().flex_col();
                for (id, name) in &project_options {
                    let id = *id;
                    let view = project_view.clone();
                    let popover = popover.clone();
                    options = options.child(
                        Button::new(("deploy-project-option", id as usize))
                            .w_full()
                            .label(name.clone())
                            .on_click(move |_, window, cx| {
                                let _ = popover.update(cx, |state, cx| state.dismiss(window, cx));
                                let _ = view.update(cx, |view, cx| {
                                    view.select_deploy_project(id, window, cx)
                                });
                            }),
                    );
                }
                options
            });
        let parallel_view = cx.weak_entity();
        let can_start = self.deploy_ready(project, &ids)
            || (MULTI_HOST_START_ENABLED && self.batch_start_ready(project, &ids));
        let can_batch_check = project.manifest_mode
            && ids.len() > 1
            && !self.deploy_batch_check_loading
            && !self.deploy_run_busy
            && !self.deploy_config_loading
            && !self.deploy_env_loading
            && !self.deploy_target_loading
            && !self.deploy_tag_plan_loading
            && matches!(self.deploy_config_result, Some(Ok(())))
            && matches!(self.deploy_env_result, Some(Ok(())))
            && matches!(self.deploy_batch_plan, Some(Ok(_)))
            && matches!(self.deploy_tag_plan, Some(Ok(_)));
        let parallel_options = ids.len().max(1);
        let parallel_selector = Popover::new("deploy-parallel-select")
            .trigger(
                Button::new("deploy-parallel-trigger")
                    .h(px(36.))
                    .w_full()
                    .outline()
                    .disabled(project.manifest_mode)
                    .label(format!("{}  ▾", self.deploy_parallel)),
            )
            .content(move |_, _, cx| {
                let popover = cx.weak_entity();
                let mut options = div().w(px(96.)).p_1().flex().flex_col();
                for count in 1..=parallel_options {
                    let view = parallel_view.clone();
                    let popover = popover.clone();
                    options = options.child(
                        Button::new(("deploy-parallel-option", count))
                            .w_full()
                            .label(count.to_string())
                            .on_click(move |_, window, cx| {
                                let _ = popover.update(cx, |state, cx| state.dismiss(window, cx));
                                let _ = view.update(cx, |view, cx| {
                                    view.deploy_parallel = count;
                                    cx.notify();
                                });
                            }),
                    );
                }
                options
            });
        let mut host_rows = div().flex().flex_col();
        for (index, id) in ids.iter().enumerate() {
            let name = self
                .hosts
                .iter()
                .find(|host| host.id == *id)
                .map(|host| host.name.clone())
                .unwrap_or_else(|| "主机数据待读取".into());
            let current = self.deploy_run_project_id == Some(project.id)
                && self.deploy_run_host_id == Some(*id);
            let transfer_percent =
                if current && self.deploy_run_busy && self.deploy_run_stage == "传输镜像" {
                    self.deploy_run_transfer
                        .map(|(sent, total)| (u128::from(sent) * 100 / u128::from(total)) as f32)
                } else {
                    None
                };
            host_rows = host_rows.child(
                div()
                    .min_w(px(770.))
                    .h(px(52.))
                    .border_t_1()
                    .border_color(self.palette().border)
                    .px_4()
                    .flex()
                    .items_center()
                    .child(
                        div()
                            .w(px(70.))
                            .flex_shrink_0()
                            .child((index + 1).to_string()),
                    )
                    .child(div().w(px(155.)).flex_shrink_0().truncate().child(name))
                    .child(
                        div()
                            .w(px(115.))
                            .flex_shrink_0()
                            .text_color(self.palette().muted)
                            .child(
                                if self.deploy_run_project_id == Some(project.id)
                                    && self.deploy_run_host_id == Some(*id)
                                {
                                    if self.deploy_run_busy {
                                        "发布中"
                                    } else if self.deploy_run_stage.contains("发布完成") {
                                        "健康完成"
                                    } else {
                                        "待核实"
                                    }
                                } else {
                                    "等待"
                                },
                            ),
                    )
                    .child(
                        div()
                            .w(px(130.))
                            .flex_shrink_0()
                            .text_color(self.palette().muted)
                            .truncate()
                            .child(
                                if self.deploy_run_project_id == Some(project.id)
                                    && self.deploy_run_host_id == Some(*id)
                                {
                                    self.deploy_run_stage.clone()
                                } else {
                                    "—".into()
                                },
                            ),
                    )
                    .child(
                        div()
                            .w(px(90.))
                            .flex_shrink_0()
                            .flex()
                            .flex_col()
                            .when_some(transfer_percent, |el, percent| {
                                el.child(Progress::new(("deploy-transfer", index)).value(percent))
                                    .child(format!("{percent:.0}%"))
                            })
                            .when(transfer_percent.is_none(), |el| el.child("—")),
                    )
                    .child(div().w(px(85.)).flex_shrink_0().child(if current {
                        format!("{}s", self.deploy_run_elapsed_secs)
                    } else {
                        "—".into()
                    }))
                    .child("—"),
            );
        }
        div()
            .flex()
            .flex_col()
            .gap_6()
            .child(
                div()
                    .w_full()
                    .border_1()
                    .border_color(self.palette().border)
                    .rounded_lg()
                    .p_6()
                    .flex()
                    .flex_wrap()
                    .items_end()
                    .gap_6()
                    .child(div().w(px(180.)).child(Self::form_field("项目", project_selector)))
                    .child(div().w(px(200.)).flex().flex_col().gap_2()
                        .child("发布标识")
                        .child(Input::new(&self.deploy_tag).w_full().disabled(self.deploy_tag_loading || self.deploy_run_busy))
                        .when_some(self.deploy_tag_error.as_ref(), |el, error| el.child(
                            div().text_size(px(12.)).text_color(self.palette().warning).child(error.clone())
                        )))
                    .child(div().w(px(130.)).flex().flex_col().gap_2()
                        .child("目标主机")
                        .child(div().h(px(36.)).flex().items_center().gap_2()
                            .child(format!("已选 {} 台", ids.len()))
                            .when(project.manifest_mode, |el| el.child("YAML 预设"))
                            .when(!project.manifest_mode, |el| el.child(Button::new("deploy-targets-change").label("更改")
                                .disabled(project.host_ids.is_empty() || self.deploy_run_busy)
                                .on_click(cx.listener(|view, _, window, cx| view.open_deploy_targets_dialog(window, cx)))))))
                    .child(div().w(px(75.)).child(Self::form_field("并行度", parallel_selector)))
                    .child(Button::new("start-deploy").primary()
                        .debug_selector(|| "start-deploy-button".into())
                        .label(if self.deploy_run_busy { "发布中…" } else { "开始发布" })
                        .tooltip(if project.manifest_mode && ids.len() > 1 && !MULTI_HOST_START_ENABLED {
                            "批次发布仍在隔离验收，开始按钮保持禁用；不会只发布第一台"
                        } else if project.manifest_mode && ids.len() > 1 {
                            "将按 YAML 目标顺序逐台发布；每台健康后都须人工确认"
                        } else {
                            "当前支持 YAML v2 单主机整栈发布；开始前将再次预检"
                        })
                        .disabled(!can_start)
                        .on_click(cx.listener(|view, _, window, cx| view.confirm_deploy(window, cx)))),
            )
            .child(self.panel(
                "发布进度总览",
                "SSH 直连 · save → SFTP → load",
                div()
                    .px_6()
                    .pb_6()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(div().text_color(build_color).child(build_status))
                    .child(div().text_color(config_color).child(config_status))
                    .child(div().text_color(target_color).child(target_status))
                    .child(div().text_color(tag_color).child(tag_status))
                    .when_some(self.deploy_batch_plan.as_ref(), |el, batch| el.child(
                        div().whitespace_normal().text_color(self.palette().muted).child(match batch {
                            Ok(plan) => format!(
                                "只读批次矩阵 · {} 份唯一服务/架构镜像 → {} 台目标：{}。初始执行入口仍待隔离验收。",
                                plan.builds.len(), plan.hosts.len(),
                                plan.hosts.iter().map(|target| {
                                    let name = self.hosts.iter().find(|host| host.id == target.host_id)
                                        .map_or_else(|| target.host_id.to_string(), |host| host.name.clone());
                                    format!("{name} {} / {} 份镜像", target.platform, target.images.len())
                                }).collect::<Vec<_>>().join(" · "),
                            ),
                            Err(error) => format!("批次镜像矩阵不可用：{error}"),
                        })
                    ))
                    .child(div().text_color(env_color).child(env_status))
                    .when(project.manifest_mode && ids.len() > 1, |el| el.child(
                        div().flex().flex_col().items_start().gap_2()
                            .child(Button::new("batch-preflight-check")
                                .debug_selector(|| "batch-preflight-check-button".into())
                                .label(if self.deploy_batch_check_loading { "正在核对批次目标…" } else { "核对批次目标" })
                                .disabled(!can_batch_check)
                                .on_click(cx.listener(|view, _, _, cx| view.start_batch_preflight(cx))))
                            .child(div().whitespace_normal().text_color(match &self.deploy_batch_check {
                                Some(Ok(_)) => self.palette().success,
                                Some(Err(_)) => self.palette().warning,
                                None => self.palette().muted,
                            }).child(match &self.deploy_batch_check {
                                Some(Ok(snapshot)) => format!(
                                    "批次目标/材料预检通过 · {} 台可信目标 / {} 份共享镜像；尚未构建，HEALTHCHECK 将在构建后核对。仅核实，不创建任务或部署。",
                                    snapshot.targets.len(), snapshot.plan.builds.len(),
                                ),
                                Some(Err(error)) => format!("批次预检未通过：{error}"),
                                None => "批次目标尚未完整预检；当前仍不能开始多主机发布。".into(),
                            }))
                    ))
                    .when(project.manifest_mode && ids.len() > 1, |el| el.child(
                        div().debug_selector(|| "multi-host-unavailable".into())
                            .border_1().border_color(self.palette().warning).rounded_md().p_3()
                            .text_color(self.palette().warning)
                            .child(format!("YAML 预设 {} 台目标主机；批次初始执行仍在隔离验收。开始发布保持禁用，不会仅部署第一台。", ids.len()))
                    ))
                    .child(div().text_color(self.palette().muted).child(if self.deploy_run_busy && self.deploy_run_project_id == Some(project.id) {
                        format!("正在执行：{}；传输百分比按当前镜像计算，多镜像时会重置。离开页面不会取消任务，请勿关闭程序。", self.deploy_run_stage)
                    } else {
                        "当前仅支持 YAML v2 单主机整栈发布；传输百分比按当前镜像计算，多镜像时会重置。全部服务健康后再次核实材料与远端状态。".into()
                    })),
            ))
            .child(self.panel(
                "各主机进度",
                "顺序  ·  主机  ·  状态  ·  阶段  ·  进度  ·  耗时  ·  操作",
                div().px_6().pb_6()
                    .child(if ids.is_empty() {
                        div().text_color(self.palette().muted).child("尚未选择目标主机").into_any_element()
                    } else {
                        div().id("deploy-host-table-scroll").overflow_x_scroll().border_1().border_color(self.palette().border).rounded_md()
                            .child(div().min_w(px(770.)).h(px(40.)).px_4().flex().items_center().text_color(self.palette().muted)
                                .child(div().w(px(70.)).flex_shrink_0().child("顺序"))
                                .child(div().w(px(155.)).flex_shrink_0().child("主机"))
                                .child(div().w(px(115.)).flex_shrink_0().child("状态"))
                                .child(div().w(px(130.)).flex_shrink_0().child("阶段"))
                                .child(div().w(px(90.)).flex_shrink_0().child("进度"))
                                .child(div().w(px(85.)).flex_shrink_0().child("耗时"))
                                .child("操作"))
                            .child(host_rows).into_any_element()
                    }),
            ))
            .child(self.panel(
                "构建日志",
                "",
                div().px_6().pb_6().flex().flex_col().gap_2().text_color(self.palette().muted)
                    .children(if self.deploy_run_project_id != Some(project.id) || self.deploy_run_log.is_empty() {
                        vec!["等待开始发布…".to_owned()]
                    } else { self.deploy_run_log.clone() }),
            ))
    }

    pub(super) fn switch_container_host(
        &mut self,
        id: i64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.container_host_id != Some(id) {
            if let Some(cancel) = self.active_log_cancel.take() {
                cancel.store(true, Ordering::Relaxed);
            }
            if window.has_active_sheet(cx) {
                window.close_sheet(cx);
            }
        }
        self.select_container_host(id, cx);
    }

    pub(super) fn select_container_host(&mut self, id: i64, cx: &mut Context<Self>) {
        if !self.hosts.iter().any(|host| host.id == id) {
            return;
        }
        self.container_host_id = Some(id);
        self.container_inventory = None;
        self.container_error = None;
        self.container_loading = true;
        self.container_generation = self.container_generation.wrapping_add(1);
        let generation = self.container_generation;
        cx.notify();
        cx.spawn(async move |view, cx| {
            loop {
                let (still_active, busy) = view
                    .update(cx, |view, _| {
                        (
                            view.page == Page::Containers
                                && view.container_host_id == Some(id)
                                && view.container_generation == generation,
                            view.container_operations
                                .iter()
                                .any(|(host_id, _)| *host_id == id),
                        )
                    })
                    .unwrap_or((false, false));
                if !still_active {
                    break;
                }
                if busy {
                    cx.background_executor().timer(Duration::from_secs(1)).await;
                    continue;
                }
                let result = cx
                    .background_executor()
                    .spawn(async move { ssh::inventory(id) })
                    .await;
                let active = view
                    .update(cx, |view, cx| {
                        if view.page != Page::Containers
                            || view.container_host_id != Some(id)
                            || view.container_generation != generation
                        {
                            return false;
                        }
                        if view
                            .container_operations
                            .iter()
                            .any(|(host_id, _)| *host_id == id)
                        {
                            return true;
                        }
                        view.container_loading = false;
                        match result {
                            Ok(inventory) => {
                                view.container_inventory = Some(inventory);
                                view.container_error = None;
                            }
                            Err(error) => {
                                view.container_inventory = None;
                                view.container_error = Some(error);
                            }
                        }
                        cx.notify();
                        true
                    })
                    .unwrap_or(false);
                if !active {
                    break;
                }
                cx.background_executor()
                    .timer(Duration::from_secs(10))
                    .await;
            }
        })
        .detach();
    }

    fn run_container_action(
        &mut self,
        host: Host,
        container_id: String,
        action: ssh::ContainerAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let host_id = host.id;
        if self
            .container_operations
            .contains(&(host_id, container_id.clone()))
            || self.container_host_id != Some(host_id)
        {
            return;
        }
        self.container_operations
            .insert((host_id, container_id.clone()));
        cx.notify();
        cx.spawn_in(window, async move |view, cx| {
            let operation_id = container_id.clone();
            let result = cx
                .background_executor()
                .spawn(async move { ssh::container_action(host, &container_id, action) })
                .await;
            let _ = cx.update(|window, app| {
                let refresh = view
                    .update(app, |view, cx| {
                        view.container_operations
                            .remove(&(host_id, operation_id.clone()));
                        let refresh = view.page == Page::Containers
                            && view.container_host_id == Some(host_id)
                            && !view
                                .container_operations
                                .iter()
                                .any(|(active_host, _)| *active_host == host_id);
                        cx.notify();
                        refresh
                    })
                    .unwrap_or(false);
                if refresh {
                    let _ = view.update(app, |view, cx| view.select_container_host(host_id, cx));
                }
                window.push_notification(
                    match result {
                        Ok(()) => Notification::success(format!("容器已{}", action.label())),
                        Err(error) => Notification::error(error),
                    },
                    app,
                );
            });
        })
        .detach();
    }

    fn preview_disk_prune(&mut self, host: Host, window: &mut Window, cx: &mut Context<Self>) {
        if self.prune_busy || self.container_host_id != Some(host.id) {
            return;
        }
        self.prune_busy = true;
        cx.notify();
        cx.spawn_in(window, async move |view, cx| {
            let expected = host.clone();
            let result = cx
                .background_executor()
                .spawn(async move { ssh::disk_preview(expected) })
                .await;
            let _ = cx.update(|window, app| {
                let _ = view.update(app, |view, cx| {
                    view.prune_busy = false;
                    if view.page == Page::Containers && view.container_host_id == Some(host.id) {
                        match result {
                            Ok(preview) => view.show_disk_preview(host, preview, window, cx),
                            Err(error) => window.push_notification(Notification::error(error), cx),
                        }
                    }
                    cx.notify();
                });
            });
        })
        .detach();
    }

    fn show_disk_preview(
        &mut self,
        host: Host,
        preview: ssh::DiskPreview,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let palette = self.palette();
        window.open_dialog(cx, move |dialog, _, _| {
            let mut rows = div().flex().flex_col().gap_2();
            for (kind, label) in [
                ("Images", "悬空镜像"),
                ("Containers", "停止容器"),
                ("Build Cache", "构建缓存"),
            ] {
                if let Some(row) = preview.rows.iter().find(|row| row.kind == kind) {
                    rows = rows.child(
                        div()
                            .flex()
                            .justify_between()
                            .child(format!("{label} · 总数 {}", row.total))
                            .child(row.reclaimable.clone()),
                    );
                }
            }
            rows = rows.child(
                div()
                    .flex()
                    .justify_between()
                    .child("未使用网络")
                    .child(format!("{} 个 · 空间未知", preview.unused_networks)),
            );
            dialog
                .title("Docker 磁盘占用 · 只读")
                .w(px(520.))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_4()
                        .child(format!("目标主机：{} ({}:{})", host.name, host.address, host.port))
                        .child(rows)
                        .child(div().text_color(palette.muted).child("悬空镜像大小因共享层可能高于实际可释放量。为保护发布历史，全局 Docker 清理暂不可用；此处只读，不删除镜像、容器、网络或数据卷。")),
                )
                .footer(
                    div()
                        .flex()
                        .justify_end()
                        .gap_2()
                        .child(Button::new("disk-preview-close").label("关闭").on_click(
                            |_, window, cx| window.close_dialog(cx),
                        )),
                )
        });
    }

    fn confirm_container_action(
        &mut self,
        host: Host,
        container_id: String,
        name: String,
        action: ssh::ContainerAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let view = cx.weak_entity();
        window.open_dialog(cx, move |dialog, _, _| {
            let view = view.clone();
            let container_id = container_id.clone();
            let save_host = host.clone();
            let title = format!("{}容器？", action.label());
            let confirm =
                Button::new("container-action-confirm").label(format!("确认{}", action.label()));
            dialog
                .title(title)
                .w(px(480.))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_3()
                        .child(format!("目标容器：{name}"))
                        .child(format!(
                            "目标主机：{} ({}:{})",
                            host.name, host.address, host.port
                        ))
                        .child(
                            "这会直接操作远端 Docker，可能影响项目服务。删除容器不会清理其数据卷。",
                        ),
                )
                .footer(
                    div()
                        .flex()
                        .justify_end()
                        .gap_2()
                        .child(
                            Button::new("container-action-cancel")
                                .label("取消")
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            if matches!(action, ssh::ContainerAction::Delete) {
                                confirm.danger()
                            } else {
                                confirm.primary()
                            }
                            .on_click(move |_, window, cx| {
                                let _ = view.update(cx, |view, cx| {
                                    view.run_container_action(
                                        save_host.clone(),
                                        container_id.clone(),
                                        action,
                                        window,
                                        cx,
                                    );
                                });
                                window.close_dialog(cx);
                            }),
                        ),
                )
        });
    }

    fn container_actions_menu(
        &self,
        item: &ssh::ContainerInfo,
        index: usize,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let view = cx.weak_entity();
        let host = self
            .container_host_id
            .and_then(|id| self.hosts.iter().find(|host| host.id == id))
            .cloned();
        let container_id = item.id.clone();
        let name = item.name.clone();
        let running = item.state == "running";
        let busy = host.as_ref().is_some_and(|host| {
            self.container_operations
                .contains(&(host.id, container_id.clone()))
        });
        Popover::new(("container-actions", index))
            .trigger(
                Button::new(("container-more", index))
                    .icon(assets::IconName::Ellipsis)
                    .accessibility_label(format!("{} 的操作", item.name))
                    .disabled(busy),
            )
            .content(move |_, _, cx| {
                let popover = cx.weak_entity();
                let mut options = div().w(px(160.)).p_1().flex().flex_col();
                for (ordinal, action) in [
                    ssh::ContainerAction::Start,
                    ssh::ContainerAction::Stop,
                    ssh::ContainerAction::Restart,
                    ssh::ContainerAction::Delete,
                ]
                .into_iter()
                .enumerate()
                {
                    let disabled = busy
                        || match action {
                            ssh::ContainerAction::Start => running,
                            ssh::ContainerAction::Stop | ssh::ContainerAction::Restart => !running,
                            ssh::ContainerAction::Delete => running,
                        };
                    let view = view.clone();
                    let popover = popover.clone();
                    let container_id = container_id.clone();
                    let name = name.clone();
                    let host = host.clone();
                    options = options.child(
                        Button::new(("container-operation", index * 4 + ordinal))
                            .label(action.label())
                            .disabled(disabled)
                            .when(matches!(action, ssh::ContainerAction::Delete), |button| {
                                button.danger()
                            })
                            .on_click(move |_, window, cx| {
                                let _ = popover.update(cx, |state, cx| state.dismiss(window, cx));
                                if let Some(host) = host.as_ref() {
                                    let _ = view.update(cx, |view, cx| {
                                        if matches!(action, ssh::ContainerAction::Start) {
                                            view.run_container_action(
                                                host.clone(),
                                                container_id.clone(),
                                                action,
                                                window,
                                                cx,
                                            );
                                        } else {
                                            view.confirm_container_action(
                                                host.clone(),
                                                container_id.clone(),
                                                name.clone(),
                                                action,
                                                window,
                                                cx,
                                            );
                                        }
                                    });
                                }
                            }),
                    );
                }
                options
            })
    }

    fn open_container_logs(
        &mut self,
        container_id: String,
        name: String,
        running: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(host) = self
            .container_host_id
            .and_then(|id| self.hosts.iter().find(|host| host.id == id))
            .cloned()
        else {
            return;
        };
        if let Some(old) = self.active_log_cancel.take() {
            old.store(true, Ordering::Relaxed);
        }
        if window.has_active_sheet(cx) {
            window.close_sheet(cx);
        }
        let cancel = Arc::new(AtomicBool::new(false));
        self.active_log_cancel = Some(cancel.clone());
        let state = Rc::new(RefCell::new(LogSheetState::new()));
        let scroll = ScrollHandle::new();
        let (sender, receiver) = mpsc::sync_channel(2048);
        let view = cx.weak_entity();
        let sheet_cancel = cancel.clone();
        let sheet_state = state.clone();
        let sheet_scroll = scroll.clone();
        let action_host = host.clone();
        let action_id = container_id.clone();
        window.open_sheet(cx, move |sheet, window, cx| {
            let snapshot = sheet_state.borrow();
            let lines: Vec<_> = snapshot.lines.iter().cloned().collect();
            let follow = snapshot.follow;
            let message = snapshot.message.clone().unwrap_or_else(|| {
                if snapshot.running {
                    "正在跟随日志…".into()
                } else {
                    "日志流已结束".into()
                }
            });
            let (running_now, busy) = view
                .upgrade()
                .map(|entity| {
                    let app = entity.read(cx);
                    if app.container_host_id != Some(action_host.id) {
                        return (false, true);
                    }
                    let running = app.container_inventory.as_ref().is_some_and(|inventory| {
                        inventory
                            .containers
                            .iter()
                            .any(|item| item.id == action_id && item.state == "running")
                    });
                    let busy = app
                        .container_operations
                        .contains(&(action_host.id, action_id.clone()));
                    (running, busy)
                })
                .unwrap_or((running, false));
            let height = if window.viewport_size().height > px(420.) {
                window.viewport_size().height - px(210.)
            } else {
                px(210.)
            };
            let follow_state = sheet_state.clone();
            let follow_scroll = sheet_scroll.clone();
            let stop_view = view.clone();
            let restart_view = view.clone();
            let stop_host = action_host.clone();
            let restart_host = action_host.clone();
            let stop_id = action_id.clone();
            let restart_id = action_id.clone();
            let stop_name = name.clone();
            let restart_name = name.clone();
            let wheel_state = sheet_state.clone();
            let wheel_scroll = sheet_scroll.clone();
            let footer_state = sheet_state.clone();
            let footer_scroll = sheet_scroll.clone();
            let close_cancel = sheet_cancel.clone();
            sheet
                .size(if window.viewport_size().width < px(640.) {
                    window.viewport_size().width
                } else {
                    px(640.)
                })
                .on_close(move |_, _, _| close_cancel.store(true, Ordering::Relaxed))
                .title(
                    div()
                        .flex()
                        .flex_wrap()
                        .items_center()
                        .justify_between()
                        .gap_2()
                        .child(
                            div()
                                .font_family(mono_font())
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(name.clone()),
                        )
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
                                .child(
                                    base::Checkbox::new("container-log-follow")
                                        .checked(follow)
                                        .accessibility_label("跟随日志")
                                        .on_change(move |_, _, window, _| {
                                            let mut state = follow_state.borrow_mut();
                                            state.follow = !state.follow;
                                            if state.follow {
                                                follow_scroll.scroll_to_bottom();
                                            }
                                            window.refresh();
                                        }),
                                )
                                .child("跟随")
                                .child(
                                    Button::new("log-stop-container")
                                        .label("停止")
                                        .disabled(!running_now || busy)
                                        .on_click(move |_, window, cx| {
                                            let _ = stop_view.update(cx, |view, cx| {
                                                view.confirm_container_action(
                                                    stop_host.clone(),
                                                    stop_id.clone(),
                                                    stop_name.clone(),
                                                    ssh::ContainerAction::Stop,
                                                    window,
                                                    cx,
                                                )
                                            });
                                        }),
                                )
                                .child(
                                    Button::new("log-restart-container")
                                        .label("重启")
                                        .disabled(!running_now || busy)
                                        .on_click(move |_, window, cx| {
                                            let _ = restart_view.update(cx, |view, cx| {
                                                view.confirm_container_action(
                                                    restart_host.clone(),
                                                    restart_id.clone(),
                                                    restart_name.clone(),
                                                    ssh::ContainerAction::Restart,
                                                    window,
                                                    cx,
                                                )
                                            });
                                        }),
                                ),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_2()
                        .child(
                            div()
                                .id("container-log-scroll")
                                .h(height)
                                .overflow_y_scroll()
                                .overflow_x_scroll()
                                .track_scroll(&sheet_scroll)
                                .bg(hsl(240., 0.04, 0.13))
                                .rounded_md()
                                .p_3()
                                .font_family(mono_font())
                                .text_size(px(12.))
                                .text_color(rgb(0xf5f5f5))
                                .on_scroll_wheel(move |event, window, _| {
                                    let delta = event.delta.pixel_delta(px(16.)).y;
                                    let max = wheel_scroll.max_offset().y;
                                    if max > px(0.)
                                        && delta > px(0.)
                                        && wheel_scroll.offset().y + delta > -max + px(12.)
                                    {
                                        wheel_state.borrow_mut().follow = false;
                                        window.refresh();
                                    }
                                })
                                .children(
                                    lines
                                        .into_iter()
                                        .map(|line| div().whitespace_nowrap().child(line)),
                                ),
                        )
                        .child(div().text_color(hsl(240., 0.04, 0.6)).child(message)),
                )
                .footer(
                    div()
                        .w_full()
                        .flex()
                        .items_center()
                        .justify_between()
                        .child("仅显示最近 1000 行 (docker logs)")
                        .child(
                            Button::new("log-bottom")
                                .label("回到底部")
                                .disabled(follow)
                                .on_click(move |_, window, _| {
                                    footer_state.borrow_mut().follow = true;
                                    footer_scroll.scroll_to_bottom();
                                    window.refresh();
                                }),
                        ),
                )
        });
        let worker_cancel = cancel.clone();
        cx.background_executor()
            .spawn(async move {
                ssh::stream_logs(host, container_id, worker_cancel, sender);
            })
            .detach();
        cx.spawn_in(window, async move |_, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(200))
                    .await;
                let mut changed = false;
                let mut ended = false;
                {
                    let mut state = state.borrow_mut();
                    for _ in 0..500 {
                        match receiver.try_recv() {
                            Ok(ssh::LogEvent::Line(line)) => {
                                state.push_line(line);
                                changed = true;
                            }
                            Ok(ssh::LogEvent::Error(error)) => {
                                state.message = Some(error);
                                state.running = false;
                                ended = true;
                                changed = true;
                            }
                            Ok(ssh::LogEvent::Ended) => {
                                state.running = false;
                                ended = true;
                                changed = true;
                            }
                            Err(TryRecvError::Empty) => break,
                            Err(TryRecvError::Disconnected) => {
                                state.running = false;
                                ended = true;
                                changed = true;
                                break;
                            }
                        }
                    }
                    if changed && state.follow {
                        scroll.scroll_to_bottom();
                    }
                }
                if changed {
                    let _ = cx.update(|window, _| window.refresh());
                }
                if cancel.load(Ordering::Relaxed) || ended {
                    break;
                }
            }
        })
        .detach();
    }

    fn container_table(
        &self,
        inventory: &ssh::Inventory,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let mut rows = div().flex().flex_col();
        for (index, item) in inventory.containers.iter().enumerate() {
            let running = item.state == "running";
            let log_id = item.id.clone();
            let log_name = item.name.clone();
            let operating = self.container_host_id.is_some_and(|host_id| {
                self.container_operations
                    .contains(&(host_id, item.id.clone()))
            });
            rows = rows.child(
                div()
                    .id(("container-row", index))
                    .min_w(px(940.))
                    .h(px(58.))
                    .border_t_1()
                    .border_color(self.palette().border)
                    .px_4()
                    .flex()
                    .items_center()
                    .child(
                        div()
                            .w(px(125.))
                            .flex_shrink_0()
                            .text_color(if operating {
                                self.palette().warning
                            } else if running {
                                self.palette().success
                            } else {
                                self.palette().muted
                            })
                            .child(if operating {
                                "操作中…".into()
                            } else {
                                item.status.clone()
                            }),
                    )
                    .child(
                        div()
                            .w(px(155.))
                            .flex_shrink_0()
                            .truncate()
                            .font_family(mono_font())
                            .child(item.name.clone()),
                    )
                    .child(
                        div()
                            .w(px(170.))
                            .flex_shrink_0()
                            .truncate()
                            .font_family(mono_font())
                            .child(item.image.clone()),
                    )
                    .child(
                        div()
                            .w(px(205.))
                            .flex_shrink_0()
                            .truncate()
                            .font_family(mono_font())
                            .child(item.ports.clone()),
                    )
                    .child(div().w(px(75.)).flex_shrink_0().child(item.cpu.clone()))
                    .child(
                        div()
                            .w(px(135.))
                            .flex_shrink_0()
                            .truncate()
                            .child(item.memory.clone()),
                    )
                    .child(
                        div()
                            .flex()
                            .gap_1()
                            .child(
                                Button::new(("container-logs", index))
                                    .label("日志")
                                    .on_click(cx.listener(move |view, _, window, cx| {
                                        view.open_container_logs(
                                            log_id.clone(),
                                            log_name.clone(),
                                            running,
                                            window,
                                            cx,
                                        );
                                    })),
                            )
                            .child(self.container_actions_menu(item, index, cx)),
                    ),
            );
        }
        div()
            .id("containers-table-scroll")
            .overflow_x_scroll()
            .border_1()
            .border_color(self.palette().border)
            .rounded_md()
            .child(
                div()
                    .min_w(px(940.))
                    .h(px(42.))
                    .px_4()
                    .flex()
                    .items_center()
                    .text_color(self.palette().muted)
                    .child(div().w(px(125.)).flex_shrink_0().child("状态"))
                    .child(div().w(px(155.)).flex_shrink_0().child("名称"))
                    .child(div().w(px(170.)).flex_shrink_0().child("镜像"))
                    .child(div().w(px(205.)).flex_shrink_0().child("端口"))
                    .child(div().w(px(75.)).flex_shrink_0().child("CPU"))
                    .child(div().w(px(135.)).flex_shrink_0().child("内存"))
                    .child("操作"),
            )
            .child(rows)
    }

    fn image_table(&self, inventory: &ssh::Inventory) -> impl IntoElement {
        let mut rows = div().flex().flex_col();
        for (index, item) in inventory.images.iter().enumerate() {
            rows = rows.child(
                div()
                    .min_w(px(760.))
                    .h(px(58.))
                    .border_t_1()
                    .border_color(self.palette().border)
                    .px_4()
                    .flex()
                    .items_center()
                    .child(
                        div()
                            .w(px(260.))
                            .flex_shrink_0()
                            .truncate()
                            .font_family(mono_font())
                            .child(item.reference.clone()),
                    )
                    .child(div().w(px(115.)).flex_shrink_0().child(item.size.clone()))
                    .child(
                        div()
                            .w(px(125.))
                            .flex_shrink_0()
                            .child(item.created.clone()),
                    )
                    .child(
                        div()
                            .w(px(145.))
                            .flex_shrink_0()
                            .text_color(self.palette().muted)
                            .child(match item.container_refs {
                                Some(0) if item.dangling => "悬空，未被引用",
                                Some(0) => "未被容器引用",
                                Some(_) => "容器引用，受保护",
                                None if item.dangling => "悬空，引用待核实",
                                None => "使用情况待核实",
                            }),
                    )
                    .child(
                        Button::new(("image-delete", index))
                            .label("删除")
                            .disabled(true)
                            .tooltip("发布保留保护与删除确认尚未接入"),
                    ),
            );
        }
        div()
            .id("images-table-scroll")
            .overflow_x_scroll()
            .child(
                div()
                    .min_w(px(760.))
                    .h(px(42.))
                    .px_4()
                    .flex()
                    .items_center()
                    .text_color(self.palette().muted)
                    .child(div().w(px(260.)).flex_shrink_0().child("仓库名:tag"))
                    .child(div().w(px(115.)).flex_shrink_0().child("大小"))
                    .child(div().w(px(125.)).flex_shrink_0().child("创建时间"))
                    .child(div().w(px(145.)).flex_shrink_0().child("未使用标记"))
                    .child("操作"),
            )
            .child(rows)
    }

    fn containers_page(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let selected = self
            .container_host_id
            .and_then(|id| self.hosts.iter().find(|host| host.id == id));
        let host_name = selected
            .map(|host| host.name.clone())
            .unwrap_or_else(|| "暂无主机".into());
        let view = cx.weak_entity();
        let options: Vec<_> = self
            .hosts
            .iter()
            .map(|host| (host.id, host.name.clone()))
            .collect();
        let selector = Popover::new("container-host-select")
            .trigger(
                Button::new("container-host")
                    .outline()
                    .label(format!("主机：{host_name}  ▾"))
                    .disabled(self.hosts.is_empty()),
            )
            .content(move |_, _, cx| {
                let popover = cx.weak_entity();
                let mut items = div().w(px(200.)).p_1().flex().flex_col();
                for (id, name) in &options {
                    let id = *id;
                    let view = view.clone();
                    let popover = popover.clone();
                    items = items.child(
                        Button::new(("container-host-option", id as usize))
                            .w_full()
                            .label(name.clone())
                            .on_click(move |_, window, cx| {
                                let _ = popover.update(cx, |state, cx| state.dismiss(window, cx));
                                let _ = view.update(cx, |view, cx| {
                                    view.switch_container_host(id, window, cx)
                                });
                            }),
                    );
                }
                items
            });
        let body = if self.hosts.is_empty() {
            self.empty_card("暂无容器或镜像", assets::IconName::Box)
                .into_any_element()
        } else if self.container_loading {
            self.empty_card("正在通过 SSH 读取 Docker 列表…", assets::IconName::Box)
                .into_any_element()
        } else if let Some(error) = &self.container_error {
            self.panel(
                "容器列表读取失败",
                "可能是 SSH 连接、凭据、Docker 权限或输出格式问题；按具体提示处理。",
                div()
                    .px_6()
                    .pb_6()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(
                        div()
                            .text_color(self.palette().destructive)
                            .child(error.clone()),
                    )
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .child(Button::new("container-retry").label("重试").on_click(
                                cx.listener(|view, _, _, cx| {
                                    if let Some(id) = view.container_host_id {
                                        view.select_container_host(id, cx);
                                    }
                                }),
                            ))
                            .child(
                                Button::new("container-go-host")
                                    .label("前往主机核对")
                                    .on_click(cx.listener(|view, _, window, cx| {
                                        let id = view.container_host_id;
                                        view.navigate(Page::Hosts, window, cx);
                                        if let Some(id) = id {
                                            view.open_host_detail(id, cx);
                                        }
                                    })),
                            ),
                    ),
            )
            .into_any_element()
        } else if let Some(inventory) = &self.container_inventory {
            if self.container_tab == ContainerTab::Containers {
                self.panel(
                    "容器",
                    "",
                    if inventory.containers.is_empty() {
                        div()
                            .px_6()
                            .pb_6()
                            .text_color(self.palette().muted)
                            .child("暂无容器")
                            .into_any_element()
                    } else {
                        div()
                            .px_6()
                            .pb_6()
                            .child(self.container_table(inventory, cx))
                            .into_any_element()
                    },
                )
                .into_any_element()
            } else {
                div()
                    .w_full()
                    .min_w_0()
                    .border_1()
                    .border_color(self.palette().border)
                    .rounded_lg()
                    .child(
                        div()
                            .p_6()
                            .flex()
                            .items_center()
                            .justify_between()
                            .gap_3()
                            .child(
                                div()
                                    .text_size(px(18.))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child("镜像"),
                            )
                            .child(
                                Button::new("image-prune")
                                    .label("清理悬空镜像")
                                    .disabled(true)
                                    .tooltip("待接入发布历史保护，避免删除需回滚的悬空镜像"),
                            ),
                    )
                    .child(if inventory.images.is_empty() {
                        div()
                            .px_6()
                            .pb_6()
                            .text_color(self.palette().muted)
                            .child("暂无镜像")
                            .into_any_element()
                    } else {
                        self.image_table(inventory).into_any_element()
                    })
                    .into_any_element()
            }
        } else {
            self.empty_card("请选择主机", assets::IconName::Box)
                .into_any_element()
        };
        div()
            .flex()
            .flex_col()
            .gap_6()
            .child(
                self.page_head(
                    "容器",
                    Button::new("disk-prune")
                        .outline()
                        .label(if self.prune_busy {
                            "正在读取…"
                        } else {
                            "磁盘占用"
                        })
                        .disabled(self.prune_busy || self.container_host_id.is_none())
                        .on_click(cx.listener(|view, _, window, cx| {
                            if let Some(host) = view
                                .container_host_id
                                .and_then(|id| view.hosts.iter().find(|host| host.id == id))
                                .cloned()
                            {
                                view.preview_disk_prune(host, window, cx);
                            }
                        })),
                ),
            )
            .child(
                div().flex().items_center().gap_4().child(selector).child(
                    div()
                        .text_color(self.palette().muted)
                        .child("每 10 秒刷新 · SSH 直连"),
                ),
            )
            .child(
                div().flex().child(
                    div()
                        .flex()
                        .gap_1()
                        .p_1()
                        .rounded_md()
                        .bg(self.palette().accent)
                        .child(self.tab(
                            "containers",
                            "容器",
                            self.container_tab == ContainerTab::Containers,
                            cx,
                            |this, _, cx| {
                                this.container_tab = ContainerTab::Containers;
                                cx.notify();
                            },
                        ))
                        .child(self.tab(
                            "images",
                            "镜像",
                            self.container_tab == ContainerTab::Images,
                            cx,
                            |this, _, cx| {
                                this.container_tab = ContainerTab::Images;
                                cx.notify();
                            },
                        )),
                ),
            )
            .child(body)
    }

    fn settings_nav(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut nav = div().w(px(184.)).flex_shrink_0().flex().flex_col().gap_1();
        for section in SettingsSection::ALL {
            nav = nav.child(
                div()
                    .id(section.title())
                    .h(px(40.))
                    .px_3()
                    .flex()
                    .items_center()
                    .gap_3()
                    .rounded_md()
                    .text_size(px(14.))
                    .text_color(if self.settings_section == section {
                        self.palette().foreground
                    } else {
                        self.palette().muted
                    })
                    .when(self.settings_section == section, |el| {
                        el.bg(self.palette().accent)
                            .font_weight(FontWeight::SEMIBOLD)
                    })
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.settings_section = section;
                        this.settings_menu_open = false;
                        cx.notify();
                    }))
                    .child(Icon::new(section.icon()).size(px(18.)))
                    .child(section.title()),
            );
        }
        nav
    }

    fn open_data_folder(&self, window: &mut Window, cx: &mut Context<Self>) {
        let result = (|| -> Result<(), String> {
            let path = preferences::data_dir().ok_or("无法确定本机数据目录。")?;
            std::fs::create_dir_all(&path).map_err(|err| format!("无法创建数据目录：{err}"))?;
            let program = if cfg!(target_os = "windows") {
                "explorer.exe"
            } else if cfg!(target_os = "macos") {
                "open"
            } else {
                "xdg-open"
            };
            Command::new(program)
                .arg(path)
                .spawn()
                .map_err(|err| format!("无法打开数据目录：{err}"))?;
            Ok(())
        })();
        if let Err(error) = result {
            window.push_notification(Notification::error(error), cx);
        }
    }

    fn settings_content(&self, narrow: bool, cx: &mut Context<Self>) -> impl IntoElement {
        match self.settings_section {
            SettingsSection::Retention => self
                .panel(
                    "发布保留策略",
                    "按每项目 / 每主机计算已完成版本，保护当前运行与可回滚版本。",
                    div()
                        .w_full()
                        .min_w_0()
                        .px_6()
                        .pb_6()
                        .flex()
                        .flex_col()
                        .items_start()
                        .gap_4()
                        .child("已完成版本保留数（尚未启用自动清理）")
                        .child(
                            div()
                                .w_full()
                                .h(px(36.))
                                .border_1()
                                .border_color(self.palette().border)
                                .rounded_md()
                                .px_3()
                                .flex()
                                .items_center()
                                .justify_between()
                                .text_color(self.palette().muted)
                                .child("尚未生效")
                                .child("⌄"),
                        )
                        .child("自动清理未启用")
                        .child(
                            div()
                                .w_full()
                                .min_w_0()
                                .whitespace_normal()
                                .text_color(self.palette().muted)
                                .child("可在项目发布历史只读预览本机旧版 tar 占用；当前不会删除本机归档、远端镜像或数据卷。"),
                        )
                        .child(
                            Button::new("cleanup-now")
                                .outline()
                                .label("立即清理托管的历史镜像")
                                .disabled(true),
                        ),
                )
                .into_any_element(),
            SettingsSection::Appearance => self
                .panel(
                    "外观",
                    "与顶部栏主题切换同步。",
                    div()
                        .px_6()
                        .pb_6()
                        .flex()
                        .flex_col()
                        .gap_4()
                        .child(
                            div()
                                .flex()
                                .flex_wrap()
                                .gap_4()
                                .children(ThemeChoice::ALL.map(|choice| {
                                    let selected = self.theme == choice;
                                    div()
                                        .id(choice.saved())
                                        .flex()
                                        .items_center()
                                        .gap_2()
                                        .cursor_pointer()
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            this.set_theme(choice, window, cx)
                                        }))
                                        .child(
                                            div()
                                                .size(px(16.))
                                                .rounded_full()
                                                .border_1()
                                                .border_color(self.palette().muted)
                                                .flex()
                                                .items_center()
                                                .justify_center()
                                                .when(selected, |el| {
                                                    el.child(
                                                        div()
                                                            .size(px(8.))
                                                            .rounded_full()
                                                            .bg(self.palette().foreground),
                                                    )
                                                }),
                                        )
                                        .child(choice.label())
                                })),
                        )
                        .child("语言")
                        .child(
                            div()
                                .w_full()
                                .h(px(36.))
                                .border_1()
                                .border_color(self.palette().border)
                                .rounded_md()
                                .px_3()
                                .flex()
                                .items_center()
                                .justify_between()
                                .text_color(self.palette().muted)
                                .child("简体中文")
                                .child("⌄"),
                        )
                        .when_some(self.theme_error.as_ref(), |el, error| {
                            el.child(
                                div()
                                    .text_color(self.palette().destructive)
                                    .child(error.clone()),
                            )
                        }),
                )
                .into_any_element(),
            SettingsSection::Registry => self
                .panel(
                    "镜像仓库",
                    "默认通过 SSH 直连传输，配置后可选用外部 Registry。",
                    div()
                        .px_6()
                        .pb_6()
                        .text_color(self.palette().muted)
                        .child("尚未配置镜像仓库；凭据只能保存至操作系统凭据管理器。"),
                )
                .into_any_element(),
            SettingsSection::Backup => self
                .panel(
                    "备份与恢复",
                    "配置备份必须先设置加密密码，敏感凭据不包含在备份中。",
                    div()
                        .px_6()
                        .pb_6()
                        .text_color(self.palette().muted)
                        .child("本地加密备份及 S3 连接尚未接入桌面端。"),
                )
                .into_any_element(),
            SettingsSection::Environment => self
                .panel(
                    "本地环境",
                    "以下路径及检测结果均为本机实际状态。",
                    div()
                        .px_6()
                        .pb_6()
                        .flex()
                        .flex_col()
                        .gap_4()
                        .child(
                            div()
                                .flex()
                                .flex_wrap()
                                .items_center()
                                .justify_between()
                                .gap_3()
                                .child(
                                    div()
                                        .flex()
                                        .flex_wrap()
                                        .items_center()
                                        .gap_2()
                                        .child(
                                            div()
                                                .min_w_0()
                                                .whitespace_normal()
                                                .child(format!(
                                                    "Docker：{}",
                                                    docker::executable_path()
                                                        .as_deref()
                                                        .map(|path| display_path(path, narrow, 22))
                                                        .unwrap_or_else(|| "未找到 Docker CLI".into())
                                                )),
                                        )
                                        .child(
                                            div()
                                                .size(px(8.))
                                                .rounded_full()
                                                .bg(self.docker_color()),
                                        )
                                        .child(self.docker.label()),
                                )
                                .child(
                                    Button::new("environment-recheck")
                                        .outline()
                                        .label("重新检测")
                                        .disabled(matches!(self.docker, DockerStatus::Checking))
                                        .on_click(cx.listener(|view, _, _, cx| view.refresh_docker(cx))),
                                ),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_wrap()
                                .items_center()
                                .justify_between()
                                .gap_3()
                                .child(
                                    div()
                                        .min_w_0()
                                        .whitespace_normal()
                                        .child(
                                            preferences::data_dir()
                                                .map(|path| display_path(&path.join("state.sqlite3"), narrow, 30))
                                                .unwrap_or_else(|| "无法确定 SQLite 路径".into()),
                                        ),
                                )
                                .child(
                                    Button::new("environment-open-folder")
                                        .outline()
                                        .label("打开所在文件夹")
                                        .disabled(preferences::data_dir().is_none())
                                        .on_click(cx.listener(|view, _, window, cx| view.open_data_folder(window, cx))),
                                ),
                        ),
                )
                .into_any_element(),
            SettingsSection::Security => self
                .panel(
                    "安全",
                    "凭据、锁屏与审计日志。",
                    div().px_6().pb_6().text_color(self.palette().muted).child(
                        "SSH 密码和私钥仅写入系统凭据管理器；私钥口令、本机锁屏与审计日志尚未接入。",
                    ),
                )
                .into_any_element(),
        }
    }

    fn settings_page(&self, narrow: bool, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .gap_6()
            .child(self.page_head("设置", div()))
            .when(narrow, |el| {
                el.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_2()
                        .w_full()
                        .child(div().text_size(px(14.)).child("设置分类"))
                        .child(
                            div()
                                .id("settings-category-toggle")
                                .w_full()
                                .border_1()
                                .border_color(self.palette().border)
                                .rounded_md()
                                .px_3()
                                .py_2()
                                .cursor_pointer()
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.settings_menu_open = !this.settings_menu_open;
                                    cx.notify();
                                }))
                                .flex()
                                .justify_between()
                                .child(self.settings_section.title())
                                .child("⌄"),
                        ),
                )
                .when(self.settings_menu_open, |el| {
                    el.child(self.settings_nav(cx))
                })
            })
            .child(
                div()
                    .flex()
                    .w_full()
                    .min_w_0()
                    .gap_6()
                    .when(!narrow, |el| el.child(self.settings_nav(cx)))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(self.settings_content(narrow, cx)),
                    ),
            )
    }

    fn dashboard(&self, viewport_width: Pixels, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .gap_6()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_size(px(24.))
                            .font_weight(FontWeight::BOLD)
                            .child("总览"),
                    )
                    .child(Button::new("go-deploy").primary().label("去发布").on_click(
                        cx.listener(|this, _, window, cx| this.navigate(Page::Deploy, window, cx)),
                    )),
            )
            .child(
                div()
                    .h(px(56.))
                    .border_1()
                    .border_color(self.palette().border)
                    .rounded_lg()
                    .px_4()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(div().size(px(8.)).rounded_full().bg(self.docker_color()))
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(self.docker.label()),
                    )
                    .when_some(
                        match &self.docker {
                            DockerStatus::Available(version) => Some(version.clone()),
                            _ => None,
                        },
                        |el, version| {
                            el.child(
                                div()
                                    .text_color(self.palette().muted)
                                    .child(format!("Docker Engine {version}")),
                            )
                        },
                    ),
            )
            .child(self.dashboard_hosts(viewport_width, cx))
            .child(
                self.panel(
                    "最近发布",
                    "最近 5 条发布记录",
                    div()
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .border_t_1()
                                .border_color(self.palette().border)
                                .p_4()
                                .text_color(self.palette().muted)
                                .child("暂无发布记录"),
                        )
                        .child(
                            div()
                                .id("all-deployments")
                                .border_t_1()
                                .border_color(self.palette().border)
                                .px_6()
                                .py_3()
                                .text_size(px(14.))
                                .cursor_pointer()
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.navigate(Page::Deploy, window, cx);
                                    this.deploy_tab = DeployTab::History;
                                    cx.notify();
                                }))
                                .child("查看全部"),
                        ),
                ),
            )
    }

    pub(super) fn start_dashboard_monitor(&mut self, cx: &mut Context<Self>) {
        if let Some(cancel) = self.dashboard_cancel.take() {
            cancel.store(true, Ordering::Relaxed);
        }
        self.dashboard_generation = self.dashboard_generation.wrapping_add(1);
        self.dashboard_metrics
            .retain(|id, _| self.hosts.iter().any(|host| host.id == *id));
        if self.hosts.is_empty() {
            cx.notify();
            return;
        }
        let generation = self.dashboard_generation;
        let hosts = self.hosts.clone();
        let cancel = Arc::new(AtomicBool::new(false));
        self.dashboard_cancel = Some(cancel.clone());
        cx.notify();
        cx.spawn(async move |view, cx| {
            loop {
                let active = view
                    .update(cx, |view, _| {
                        view.page == Page::Dashboard && view.dashboard_generation == generation
                    })
                    .unwrap_or(false);
                if !active || cancel.load(Ordering::Relaxed) {
                    break;
                }
                let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
                let mut remaining = hosts.iter().cloned();
                let mut in_flight = 0;
                for host in remaining.by_ref().take(3) {
                    let sender = sender.clone();
                    let cancel = cancel.clone();
                    cx.background_executor()
                        .spawn(async move {
                            let result = ssh::monitor_host(host.clone(), cancel);
                            let _ = sender.send((host.id, result));
                        })
                        .detach();
                    in_flight += 1;
                }
                while in_flight > 0 {
                    let Some((id, result)) = receiver.recv().await else {
                        break;
                    };
                    in_flight -= 1;
                    let active = view
                        .update(cx, |view, cx| {
                            if view.page != Page::Dashboard
                                || view.dashboard_generation != generation
                            {
                                return false;
                            }
                            view.dashboard_metrics.insert(id, (Instant::now(), result));
                            cx.notify();
                            true
                        })
                        .unwrap_or(false);
                    if !active || cancel.load(Ordering::Relaxed) {
                        cancel.store(true, Ordering::Relaxed);
                        break;
                    }
                    if let Some(host) = remaining.next() {
                        let sender = sender.clone();
                        let cancel = cancel.clone();
                        cx.background_executor()
                            .spawn(async move {
                                let result = ssh::monitor_host(host.clone(), cancel);
                                let _ = sender.send((host.id, result));
                            })
                            .detach();
                        in_flight += 1;
                    }
                }
                if cancel.load(Ordering::Relaxed) {
                    break;
                }
                cx.background_executor()
                    .timer(Duration::from_secs(30))
                    .await;
            }
        })
        .detach();
    }

    fn retry_dashboard_hosts(&mut self, cx: &mut Context<Self>) {
        if !self.host_loaded {
            return;
        }
        self.host_loaded = false;
        self.host_error = None;
        cx.notify();
        cx.spawn(async move |view, cx| {
            let result = cx
                .background_executor()
                .spawn(async { host_store::list() })
                .await;
            let _ = view.update(cx, |view, cx| {
                view.host_loaded = true;
                match result {
                    Ok((hosts, warning)) => {
                        view.hosts = hosts;
                        view.credential_warning = warning;
                        view.host_error = None;
                        if view.page == Page::Dashboard {
                            view.start_dashboard_monitor(cx);
                        }
                    }
                    Err(error) => view.host_error = Some(error),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn dashboard_hosts(&self, viewport_width: Pixels, cx: &mut Context<Self>) -> AnyElement {
        if !self.host_loaded {
            return div()
                .w_full()
                .flex()
                .flex_wrap()
                .gap_4()
                .children((0..3).map(|_| {
                    div()
                        .w(px(260.))
                        .h(px(140.))
                        .border_1()
                        .border_color(self.palette().border)
                        .rounded_lg()
                        .bg(self.palette().accent)
                }))
                .into_any_element();
        }
        if let Some(error) = &self.host_error {
            return self
                .panel(
                    "主机读取失败",
                    "本机配置暂不可用。",
                    div()
                        .px_6()
                        .pb_6()
                        .flex()
                        .flex_col()
                        .items_start()
                        .gap_3()
                        .child(
                            div()
                                .text_color(self.palette().destructive)
                                .child(error.clone()),
                        )
                        .child(Button::new("dashboard-hosts-retry").label("重试").on_click(
                            cx.listener(|view, _, _, cx| view.retry_dashboard_hosts(cx)),
                        )),
                )
                .into_any_element();
        }
        if self.hosts.is_empty() {
            return div()
                .h(px(164.))
                .w_full()
                .border_1()
                .border_color(self.palette().border)
                .rounded_lg()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap_4()
                .child(Icon::new(assets::IconName::Server).size(px(24.)))
                .child(if self.host_loaded {
                    "添加你的第一台 VPS"
                } else {
                    "正在读取主机…"
                })
                .when(self.host_loaded, |el| {
                    el.child(
                        Button::new("add-host")
                            .primary()
                            .label("添加主机")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.navigate(Page::Hosts, window, cx)
                            })),
                    )
                })
                .into_any_element();
        }
        let card_width = if viewport_width < px(900.) {
            1.
        } else if viewport_width < px(1360.) {
            0.48
        } else {
            0.31
        };
        let mut cards = div().w_full().flex().flex_wrap().gap_4();
        for host in &self.hosts {
            let id = host.id;
            let sample = self
                .dashboard_metrics
                .get(&id)
                .filter(|(captured, _)| captured.elapsed() < Duration::from_secs(60))
                .map(|(_, result)| result);
            let metrics = sample.and_then(|result| result.as_ref().ok());
            let color = match sample {
                Some(Ok(_)) => self.palette().success,
                Some(Err(ssh::MonitorError::Connection(_))) => self.palette().destructive,
                Some(Err(ssh::MonitorError::Configuration(_))) => self.palette().warning,
                Some(Err(ssh::MonitorError::Sampling(_))) => self.palette().warning,
                None => self.host_status(id).1,
            };
            cards = cards.child(
                div()
                    .id(("dashboard-host", id as usize))
                    .w(relative(card_width))
                    .min_w_0()
                    .flex_shrink_0()
                    .border_1()
                    .border_color(self.palette().border)
                    .rounded_lg()
                    .p_4()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .cursor_pointer()
                    .on_click(cx.listener(move |view, _, _, cx| {
                        view.open_host_detail(id, cx);
                    }))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .child(div().size(px(8.)).rounded_full().bg(color))
                                    .child(
                                        div()
                                            .min_w_0()
                                            .truncate()
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .child(host.name.clone()),
                                    ),
                            )
                            .child(
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .font_family(mono_font())
                                    .text_color(self.palette().muted)
                                    .child(host.address.clone()),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .gap_3()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .flex()
                                    .flex_col()
                                    .gap_2()
                                    .child(
                                        div().flex().justify_between().child("CPU").child(
                                            metrics
                                                .map(|m| format!("{:.0}%", m.cpu_percent))
                                                .unwrap_or_else(|| "--".into()),
                                        ),
                                    )
                                    .when_some(metrics, |el, m| {
                                        el.child(
                                            Progress::new(("dashboard-cpu", id as usize))
                                                .value(m.cpu_percent.clamp(0., 100.) as f32),
                                        )
                                    }),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .flex()
                                    .flex_col()
                                    .gap_2()
                                    .child(
                                        div().flex().justify_between().child("内存").child(
                                            metrics
                                                .map(|m| {
                                                    format!(
                                                        "{:.0}%",
                                                        m.memory_used_kib as f64 * 100.
                                                            / m.memory_total_kib as f64
                                                    )
                                                })
                                                .unwrap_or_else(|| "--".into()),
                                        ),
                                    )
                                    .when_some(metrics, |el, m| {
                                        el.child(
                                            Progress::new(("dashboard-memory", id as usize)).value(
                                                (m.memory_used_kib as f64 * 100.
                                                    / m.memory_total_kib as f64)
                                                    .clamp(0., 100.)
                                                    as f32,
                                            ),
                                        )
                                    }),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .justify_between()
                            .text_color(self.palette().muted)
                            .child(
                                metrics
                                    .and_then(|m| {
                                        m.containers.as_ref().map(|containers| {
                                            format!("容器 {} 运行", containers.len())
                                        })
                                    })
                                    .unwrap_or_else(|| "容器 --".into()),
                            )
                            .child(
                                metrics
                                    .map(|m| format!("磁盘 {:.0}%", m.disk_percent))
                                    .unwrap_or_else(|| "磁盘 --".into()),
                            ),
                    ),
            );
        }
        if self.hosts.len() > 8 {
            cards
                .max_h(px(600.))
                .overflow_y_scrollbar()
                .into_any_element()
        } else {
            cards.into_any_element()
        }
    }

    pub(super) fn content(
        &self,
        narrow_settings: bool,
        viewport_width: Pixels,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        match self.page {
            Page::Dashboard => self.dashboard(viewport_width, cx).into_any_element(),
            Page::Hosts => self.hosts_page(cx).into_any_element(),
            Page::Projects => self.projects_page(narrow_settings, cx).into_any_element(),
            Page::Deploy => self.deploy_page(cx).into_any_element(),
            Page::Containers => self.containers_page(cx).into_any_element(),
            Page::Settings => self.settings_page(narrow_settings, cx).into_any_element(),
        }
    }
}

#[cfg(test)]
mod release_tag_tests {
    use super::{
        batch_phase_label, display_path, generate_release_tag, history_duration, history_tag,
        resumable_upload_inspection, safe_deploy_stage, same_deploy_host, transfer_progress,
    };
    use crate::host_store::Host;
    use crate::project_store::Project;
    use crate::release::UnknownUploadInspection;
    use crate::ssh::RemoteArchiveObservation;
    use std::path::Path;

    #[test]
    fn history_table_uses_base_tag_and_recorded_elapsed_time() {
        assert_eq!(history_tag("registry.example/app:v1-amd64"), "v1");
        assert_eq!(history_tag("app:release-arm64"), "release");
        assert_eq!(
            history_duration("2026-10-27 10:00:00", Some("2026-10-27 10:01:30")),
            "1分30秒"
        );
        assert_eq!(history_duration("2026-10-27 10:00:00", None), "—");
        assert_eq!(batch_phase_label("loading"), "镜像加载");
        assert_eq!(batch_phase_label("checking"), "健康检查");
        assert_eq!(batch_phase_label("other"), "阶段待核实");
    }

    #[test]
    fn resume_action_requires_a_present_safe_length_and_matching_full_digest() {
        let mut check = UnknownUploadInspection {
            containers: vec![],
            archives: vec![RemoteArchiveObservation {
                service: "web".into(),
                directory: "/tmp/devingpanel.test".into(),
                state: "reserved".into(),
                expected_bytes: 100,
                actual_bytes: Some(40),
                digest_matches: None,
            }],
        };
        assert!(resumable_upload_inspection(&check));
        let empty = UnknownUploadInspection {
            containers: vec![],
            archives: vec![],
        };
        assert!(resumable_upload_inspection(&empty));
        check.archives[0].actual_bytes = Some(100);
        assert!(!resumable_upload_inspection(&check));
        check.archives[0].digest_matches = Some(true);
        assert!(resumable_upload_inspection(&check));
        check.archives[0].actual_bytes = Some(101);
        assert!(!resumable_upload_inspection(&check));
        check.archives[0].actual_bytes = None;
        assert!(resumable_upload_inspection(&check));
        check.archives[0].state = "uploaded".into();
        assert!(!resumable_upload_inspection(&check));
        check.archives[0].actual_bytes = Some(40);
        assert!(!resumable_upload_inspection(&check));
    }

    #[test]
    fn deploy_log_only_shows_coordinator_stage_not_build_output() {
        assert_eq!(safe_deploy_stage("构建镜像：web"), Some("构建镜像".into()));
        assert_eq!(
            safe_deploy_stage("启动全部 Compose 服务"),
            Some("启动全部 Compose 服务".into())
        );
        assert_eq!(safe_deploy_stage("RUN echo secret-token"), None);
        assert_eq!(
            safe_deploy_stage("构建镜像：secret-token"),
            Some("构建镜像".into())
        );
    }

    #[test]
    fn transfer_progress_accepts_only_bounded_coordinator_counts() {
        assert_eq!(transfer_progress("镜像传输 50/100 字节"), Some((50, 100)));
        assert_eq!(transfer_progress("镜像传输 0/100 字节"), Some((0, 100)));
        assert_eq!(transfer_progress("镜像传输 101/100 字节"), None);
        assert_eq!(transfer_progress("镜像传输 1/0 字节"), None);
        assert_eq!(transfer_progress("secret 镜像传输 1/2 字节"), None);
    }

    #[test]
    fn deploy_confirmation_rejects_changed_host_identity() {
        let host = Host {
            id: 1,
            name: "test".into(),
            address: "127.0.0.1".into(),
            port: 22,
            username: "user".into(),
            group: "test".into(),
            auth: "key".into(),
            trusted_fingerprint: Some("fingerprint".into()),
        };
        let mut changed = host.clone();
        assert!(same_deploy_host(&host, &changed));
        changed.port = 2222;
        assert!(!same_deploy_host(&host, &changed));
        changed = host.clone();
        changed.trusted_fingerprint = None;
        assert!(!same_deploy_host(&host, &changed));
    }

    #[test]
    fn narrow_paths_wrap_at_directory_boundaries() {
        let path = Path::new("C:\\Users\\wangkaihui\\AppData\\Roaming\\devingPanel\\state.sqlite3");
        let text = display_path(path, true, 30);
        assert!(text.contains("\n"));
        assert!(text.lines().all(|line| line.chars().count() <= 30));
        assert!(text.contains("state.sqlite3"));
        assert_eq!(display_path(path, false, 30), path.display().to_string());
    }

    #[test]
    fn expands_timestamp_and_preserves_literal_rule() {
        let mut project = Project {
            id: 1,
            name: "app".into(),
            path: ".".into(),
            dockerfile: "Dockerfile".into(),
            image: "app".into(),
            tag_rule: "v{timestamp}".into(),
            platform: "auto".into(),
            service: String::new(),
            compose_files: Vec::new(),
            missing_compose_files: Vec::new(),
            build_args: Vec::new(),
            host_ids: Vec::new(),
            manifest_mode: false,
            config_error: None,
        };
        let tag = generate_release_tag(&project).unwrap();
        assert_eq!(tag.len(), 16);
        assert!(tag.starts_with('v'));
        assert_eq!(tag.as_bytes()[9], b'-');
        project.tag_rule = "v1.4.2".into();
        assert_eq!(generate_release_tag(&project).unwrap(), "v1.4.2");
        project.tag_rule = "{git-sha}".into();
        project.path = std::env::temp_dir()
            .join("deving-panel-missing-git-dir")
            .to_string_lossy()
            .into();
        assert!(generate_release_tag(&project).is_err());
    }
}
