use super::*;
use crate::host_store::{HostUpdate, NewHost};
use crate::project_store::{BuildConfig, NewProject};
use chrono::Local;
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
        if self.host_testing.is_some() {
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
            _ => "尚未完成 SSH 与 Docker 检查。".into(),
        };
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
                Button::new("project-empty-add")
                    .primary()
                    .label("新建项目")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.open_project_dialog(None, window, cx)
                    })),
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
                    Button::new("project-new")
                        .primary()
                        .label("＋ 新建项目")
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.open_project_dialog(None, window, cx)
                        })),
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
                            .text_color(self.palette().warning)
                            .child(div().size(px(7.)).rounded_full().bg(self.palette().warning))
                            .child("待完善"),
                    )
                    .child(
                        Button::new(("project-deploy", id as usize))
                            .primary()
                            .label("发布")
                            .tooltip("请先完善 Compose、部署服务和目标主机")
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
                                }
                                cx.notify();
                            },
                        )
                    })),
            ),
        );
        let body = match self.project_tab {
            ProjectTab::Build => self.build_panel(project, narrow, cx),
            ProjectTab::Compose => self.compose_panel(project, cx),
            ProjectTab::Environment => self.environment_panel(project, cx).into_any_element(),
            ProjectTab::Targets => self.targets_panel(project, cx).into_any_element(),
            ProjectTab::History => self
                .panel(
                    "发布历史",
                    "",
                    div()
                        .px_6()
                        .pb_6()
                        .text_color(self.palette().muted)
                        .child("暂无发布记录"),
                )
                .into_any_element(),
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
                        .on_click(cx.listener(move |view, _, window, cx| {
                            view.navigate(Page::Deploy, window, cx);
                            if view.deploy_project_id != Some(release_project_id) {
                                view.select_deploy_project(release_project_id, window, cx);
                            }
                        })),
                ),
            )
            .child(tab_row)
            .child(body)
    }

    pub(super) fn select_deploy_project(
        &mut self,
        id: i64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(project) = self
            .projects
            .iter()
            .find(|project| project.id == id)
            .cloned()
        {
            self.deploy_project_id = Some(id);
            self.deploy_host_ids = project.host_ids.clone();
            self.deploy_parallel = 1;
            self.deploy_tag_generation = self.deploy_tag_generation.wrapping_add(1);
            let generation = self.deploy_tag_generation;
            self.deploy_tag_loading = true;
            self.deploy_tag_error = None;
            self.deploy_tag_plan_generation = self.deploy_tag_plan_generation.wrapping_add(1);
            self.deploy_tag_plan = None;
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
            self.deploy_host_ids
                .retain(|id| project.host_ids.contains(id));
            self.deploy_parallel = self.deploy_parallel.min(self.deploy_host_ids.len().max(1));
        }
        self.refresh_deploy_environment(cx);
    }

    fn refresh_deploy_targets(&mut self, cx: &mut Context<Self>) {
        self.deploy_tag_plan_generation = self.deploy_tag_plan_generation.wrapping_add(1);
        self.deploy_tag_plan = None;
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
        self.deploy_tag_plan_generation = self.deploy_tag_plan_generation.wrapping_add(1);
        let generation = self.deploy_tag_plan_generation;
        self.deploy_tag_plan = None;
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
            let result = cx
                .background_executor()
                .spawn(async move { release::check_image_tags(&project, &tag, &targets) })
                .await;
            let _ = view.update(cx, |view, cx| {
                if view.deploy_tag_plan_generation == generation && view.page == Page::Deploy {
                    view.deploy_tag_plan_loading = false;
                    view.deploy_tag_plan = Some(result);
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
                self.panel(
                    "发布历史",
                    "",
                    div()
                        .border_t_1()
                        .border_color(self.palette().border)
                        .p_4()
                        .text_color(self.palette().muted)
                        .child("暂无发布记录"),
                )
                .into_any_element()
            })
    }

    fn open_deploy_targets_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(project) = self
            .deploy_project_id
            .and_then(|id| self.projects.iter().find(|project| project.id == id))
        else {
            return;
        };
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
        let parallel_options = ids.len().max(1);
        let parallel_selector = Popover::new("deploy-parallel-select")
            .trigger(
                Button::new("deploy-parallel-trigger")
                    .h(px(36.))
                    .w_full()
                    .outline()
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
                            .child("等待"),
                    )
                    .child(
                        div()
                            .w(px(130.))
                            .flex_shrink_0()
                            .text_color(self.palette().muted)
                            .child("—"),
                    )
                    .child(div().w(px(90.)).flex_shrink_0().child("—"))
                    .child(div().w(px(85.)).flex_shrink_0().child("—"))
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
                        .child(Input::new(&self.deploy_tag).w_full().disabled(self.deploy_tag_loading))
                        .when_some(self.deploy_tag_error.as_ref(), |el, error| el.child(
                            div().text_size(px(12.)).text_color(self.palette().warning).child(error.clone())
                        )))
                    .child(div().w(px(130.)).flex().flex_col().gap_2()
                        .child("目标主机")
                        .child(div().h(px(36.)).flex().items_center().gap_2()
                            .child(format!("已选 {} 台", ids.len()))
                            .child(Button::new("deploy-targets-change").label("更改")
                                .disabled(project.host_ids.is_empty())
                                .on_click(cx.listener(|view, _, window, cx| view.open_deploy_targets_dialog(window, cx))))))
                    .child(div().w(px(75.)).child(Self::form_field("并行度", parallel_selector)))
                    .child(Button::new("start-deploy").primary().label("开始发布").tooltip("SSH 发布链路与执行尚未接入").disabled(true)),
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
                    .child(div().text_color(env_color).child(env_status))
                    .child(div().text_color(self.palette().muted).child("构建前将检查 Compose、目标主机架构及本地能力；构建后检查 HEALTHCHECK 与目标镜像。发布执行尚未接入。")),
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
                div().px_6().pb_6().text_color(self.palette().muted).child("等待开始发布…"),
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
        let view = cx.weak_entity();
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
            let host = host.clone();
            let view = view.clone();
            dialog
                .title("清理磁盘 · 预检")
                .w(px(520.))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_4()
                        .child(format!("目标主机：{} ({}:{})", host.name, host.address, host.port))
                        .child(rows)
                        .child(div().text_color(palette.muted).child("悬空镜像大小合计可能因共享层而高于实际释放量；其他估值也不等于本次实际释放量，且资源可能属于其他项目。不清理数据卷。")),
                )
                .footer(
                    div()
                        .flex()
                        .justify_end()
                        .gap_2()
                        .child(Button::new("disk-preview-cancel").label("取消").on_click(
                            |_, window, cx| window.close_dialog(cx),
                        ))
                        .child(Button::new("disk-preview-next").danger().label("继续").on_click(
                            move |_, window, cx| {
                                window.close_dialog(cx);
                                let _ = view.update(cx, |view, cx| {
                                    if view.container_host_id == Some(host.id) {
                                        view.confirm_disk_prune(host.clone(), window, cx);
                                    }
                                });
                            },
                        )),
                )
        });
    }

    fn confirm_disk_prune(&mut self, host: Host, window: &mut Window, cx: &mut Context<Self>) {
        let view = cx.weak_entity();
        window.open_dialog(cx, move |dialog, _, _| {
            let host = host.clone();
            let view = view.clone();
            dialog
                .title("确认全局 Docker 清理")
                .w(px(480.))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_3()
                        .child(format!("目标主机：{} ({}:{})", host.name, host.address, host.port))
                        .child("这是全局 Docker 清理，可能影响其他项目：将删除停止容器、未使用网络、悬空镜像及构建缓存。未被容器引用的无标签回滚镜像也可能被删；不会删除数据卷。这不是受保护的发布历史清理。"),
                )
                .footer(
                    div()
                        .flex()
                        .justify_end()
                        .gap_2()
                        .child(Button::new("disk-confirm-cancel").label("取消").on_click(
                            |_, window, cx| window.close_dialog(cx),
                        ))
                        .child(Button::new("disk-confirm-run").danger().label("确认清理").on_click(
                            move |_, window, cx| {
                                let _ = view.update(cx, |view, cx| {
                                    view.run_disk_prune(host.clone(), window, cx);
                                });
                                window.close_dialog(cx);
                            },
                        )),
                )
        });
    }

    fn run_disk_prune(&mut self, host: Host, window: &mut Window, cx: &mut Context<Self>) {
        if self.prune_busy || self.container_host_id != Some(host.id) {
            return;
        }
        self.prune_busy = true;
        cx.notify();
        cx.spawn_in(window, async move |view, cx| {
            let host_id = host.id;
            let result = cx
                .background_executor()
                .spawn(async move { ssh::disk_prune(host) })
                .await;
            let _ = cx.update(|window, app| {
                let _ = view.update(app, |view, cx| {
                    view.prune_busy = false;
                    if view.page == Page::Containers && view.container_host_id == Some(host_id) {
                        view.select_container_host(host_id, cx);
                    }
                    cx.notify();
                });
                window.push_notification(
                    match result {
                        Ok(Some(actual)) => {
                            Notification::success(format!("清理完成，实际释放 {actual}"))
                        }
                        Ok(None) => Notification::success("清理完成；远端未报告实际释放量"),
                        Err(error) => Notification::error(error),
                    },
                    app,
                );
            });
        })
        .detach();
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
                            "正在清理/预检…"
                        } else {
                            "清理磁盘"
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
                        .child("已完成版本保留数")
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
                                .child("3")
                                .child("⌄"),
                        )
                        .child("自动清理  客户端在线时每日检查")
                        .child(
                            div()
                                .w_full()
                                .min_w_0()
                                .whitespace_normal()
                                .text_color(self.palette().muted)
                                .child("离线期间将在下次启动 / 重连时补查，不安装 VPS 定时任务。"),
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
    use super::{display_path, generate_release_tag};
    use crate::project_store::Project;
    use std::path::Path;

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
