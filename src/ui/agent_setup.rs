use super::*;
use ratatui::widgets::Wrap;

const FIELDS: [&str; 4] = ["工具 ID", "名称", "图标", "颜色"];
const HELP: [&str; 4] = [
    "例如 studio",
    "留空使用工具 ID",
    "留空使用 ◈",
    "留空使用 #D29FFF",
];

pub(super) struct AgentSetup {
    draft: agent::Focus,
    from_sessions: bool,
    form: CustomTool,
    copy_result: Option<mpsc::Receiver<bool>>,
}

#[derive(Default)]
struct CustomTool {
    values: [String; 4],
    field: usize,
    error: Option<&'static str>,
}

impl CustomTool {
    fn build(&self) -> std::result::Result<(String, Brand), (usize, &'static str)> {
        let [id, name, icon, color] = self.values.each_ref().map(|value| value.trim());
        if id.is_empty()
            || id.len() > 40
            || !id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        {
            return Err((0, "工具 ID 需要 1–40 个英文字母、数字、点、短横线或下划线"));
        }
        if name.chars().any(char::is_control) || name.width() > 40 {
            return Err((1, "名称需要在 40 列以内"));
        }
        if icon.chars().any(char::is_control) || icon.width() > 3 {
            return Err((2, "图标需要在 3 列以内"));
        }
        let hex = color.strip_prefix('#').unwrap_or(color);
        if !hex.is_empty() && (hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit())) {
            return Err((3, "颜色需要 6 位十六进制，例如 #D29FFF"));
        }
        Ok((
            id.into(),
            Brand {
                name: if name.is_empty() {
                    id.into()
                } else {
                    name.into()
                },
                icon: if icon.is_empty() {
                    "◈".into()
                } else {
                    icon.into()
                },
                color: if hex.is_empty() {
                    "#D29FFF".into()
                } else {
                    format!("#{hex}")
                },
                ascii: id
                    .chars()
                    .next()
                    .map(|ch| ch.to_ascii_uppercase().to_string()),
            },
        ))
    }
}

impl App {
    pub(super) fn open_agent_sessions(&mut self) {
        self.panel = Panel::Sessions;
        self.choice = 0;
        self.options = vec![String::new(); self.activity.sessions.len()];
        self.activity.refresh = true;
    }

    pub(super) fn open_agent_setup(&mut self, from_sessions: bool) {
        self.activity.setup = Some(AgentSetup {
            draft: self.activity.focus.clone().unwrap_or_default(),
            from_sessions,
            form: CustomTool::default(),
            copy_result: None,
        });
        self.panel = Panel::AgentSetup;
        self.setup_options(None);
    }

    pub(super) fn setup_options(&mut self, selected: Option<&str>) {
        let setup = self.activity.setup.as_ref().expect("setup panel");
        self.options = self
            .activity
            .brands
            .keys()
            .chain(setup.draft.custom.keys())
            .chain(setup.draft.tools.iter())
            .chain(
                self.activity
                    .sessions
                    .iter()
                    .map(|session| &session.identity.tool),
            )
            .cloned()
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        self.choice = selected
            .and_then(|id| self.options.iter().position(|key| key == id))
            .unwrap_or(0);
    }

    pub(super) fn toggle_followed_tool(&mut self) {
        if let Some(tool) = self.options.get(self.choice)
            && let Some(setup) = self.activity.setup.as_mut()
            && !setup.draft.tools.remove(tool)
        {
            setup.draft.tools.insert(tool.clone());
        }
    }

    pub(super) fn setup_click(&mut self, pointer: ratatui::layout::Position, area: Rect) -> bool {
        if !area.contains(pointer) || pointer.y == area.y {
            return false;
        }
        let index = list_start(self.choice, area) + (pointer.y - area.y - 1) as usize;
        if index >= self.options.len() {
            return false;
        }
        self.choice = index;
        self.toggle_followed_tool();
        true
    }

    pub(super) fn setup_key(&mut self, key: K) -> Result<()> {
        match self.panel {
            Panel::AgentSetup => match key {
                K::Esc => {
                    let from_sessions = self
                        .activity
                        .setup
                        .take()
                        .is_some_and(|setup| setup.from_sessions);
                    if from_sessions {
                        self.open_agent_sessions();
                    } else {
                        self.back()?;
                    }
                }
                K::Up | K::Down => {
                    self.move_choice(key == K::Down);
                }
                K::Char(' ') => self.toggle_followed_tool(),
                K::Char('n') => {
                    self.activity.setup.as_mut().expect("setup panel").form = CustomTool::default();
                    self.panel = Panel::AgentCustom;
                }
                K::Char('h') if self.choice < self.options.len() => self.panel = Panel::AgentHook,
                K::Enter => {
                    let focus = &self.activity.setup.as_ref().expect("setup panel").draft;
                    if let Err(error) = agent::save_focus(&self.db, &self.root, focus) {
                        self.status = format!("关注设置保存失败 · {error}");
                        return Ok(());
                    }
                    let setup = self.activity.setup.take().expect("setup panel");
                    self.activity
                        .sessions
                        .retain(|session| setup.draft.follows(&session.identity.tool));
                    self.activity.focus = Some(setup.draft);
                    self.activity.task = None;
                    self.open_agent_sessions();
                }
                _ => {}
            },
            Panel::AgentCustom => {
                let setup = self.activity.setup.as_mut().expect("custom tool panel");
                let form = &mut setup.form;
                form.error = None;
                match key {
                    K::Esc => self.panel = Panel::AgentSetup,
                    K::Up | K::BackTab => form.field = form.field.saturating_sub(1),
                    K::Down | K::Tab => form.field = (form.field + 1).min(FIELDS.len() - 1),
                    K::Backspace => {
                        form.values[form.field].pop();
                    }
                    K::Char(ch) if !ch.is_control() && form.values[form.field].len() < 160 => {
                        form.values[form.field].push(ch);
                    }
                    K::Enter if form.field + 1 < FIELDS.len() => form.field += 1,
                    K::Enter => match form.build() {
                        Ok((id, brand)) => {
                            if self.activity.brands.contains_key(&id)
                                || setup.draft.custom.contains_key(&id)
                            {
                                form.field = 0;
                                form.error = Some("该工具 ID 已存在，请返回列表勾选");
                            } else {
                                setup.draft.custom.insert(id.clone(), brand);
                                setup.draft.tools.insert(id.clone());
                                self.setup_options(Some(&id));
                                self.panel = Panel::AgentHook;
                            }
                        }
                        Err((field, error)) => {
                            form.field = field;
                            form.error = Some(error);
                        }
                    },
                    _ => {}
                }
            }
            Panel::AgentHook => match key {
                K::Char('c') => self.copy_hook(),
                K::Enter | K::Esc => {
                    self.activity
                        .setup
                        .as_mut()
                        .expect("hook panel")
                        .copy_result = None;
                    self.panel = Panel::AgentSetup;
                }
                _ => {}
            },
            _ => {}
        }
        Ok(())
    }

    fn copy_hook(&mut self) {
        let setup = self.activity.setup.as_mut().expect("hook panel");
        if setup.copy_result.is_none()
            && let Some(tool) = self.options.get(self.choice)
        {
            let text = hook_sample(tool, &self.root);
            let (send, receive) = mpsc::channel();
            setup.copy_result = Some(receive);
            std::thread::spawn(move || {
                let _ = send.send(clipboard::copy(&text));
            });
        }
        self.notice = Some("复制中…");
    }

    pub(super) fn hook_copy_pending(&self) -> bool {
        self.activity
            .setup
            .as_ref()
            .is_some_and(|setup| setup.copy_result.is_some())
    }

    pub(super) fn poll_hook_copy(&mut self) {
        let Some(setup) = &mut self.activity.setup else {
            return;
        };
        let Some(result) = &setup.copy_result else {
            return;
        };
        let copied = match result.try_recv() {
            Ok(copied) => copied,
            Err(mpsc::TryRecvError::Empty) => return,
            Err(mpsc::TryRecvError::Disconnected) => false,
        };
        setup.copy_result = None;
        self.notice = Some(if copied {
            "已复制到剪贴板"
        } else {
            "无法写入剪贴板"
        });
        self.dirty = true;
    }

    pub(super) fn draw_agent_setup(&self, f: &mut Frame, panes: [Rect; 3]) {
        let Some(setup) = &self.activity.setup else {
            return;
        };
        let area = panes[1];
        let (title, lines, controls) = match self.panel {
            Panel::AgentSetup => {
                let lines = self
                    .options
                    .iter()
                    .enumerate()
                    .skip(list_start(self.choice, area))
                    .take(area.height.saturating_sub(1) as usize)
                    .map(|(i, tool)| {
                        let brand = setup
                            .draft
                            .custom
                            .get(tool)
                            .or_else(|| self.activity.brand(tool));
                        let name = brand.map_or(tool.as_str(), |brand| &brand.name);
                        let (icon, style) = self.activity.tool_badge(brand);
                        let check = if setup.draft.follows(tool) {
                            "[x]"
                        } else {
                            "[ ]"
                        };
                        let anchor = if self.choice == i { "›" } else { " " };
                        Line::from(vec![
                            Span::styled(format!("{anchor} {check} "), Style::new().fg(ACC)),
                            Span::styled(format!("{icon} {}", terminal_text(name)), style),
                            Span::styled(format!("  {tool}"), Style::new().fg(DIM)),
                        ])
                    })
                    .collect();
                (
                    "关注 AI · 当前目录",
                    lines,
                    "Space 勾选  n 新增  h Hook  Enter 保存  Esc 返回",
                )
            }
            Panel::AgentCustom => {
                let form = &setup.form;
                let mut lines: Vec<_> = FIELDS
                    .iter()
                    .enumerate()
                    .map(|(i, field)| {
                        let selected = form.field == i;
                        let value = terminal_text(&form.values[i]);
                        Line::from(vec![
                            Span::styled(
                                format!("{} {:<6}  ", if selected { "›" } else { " " }, field),
                                Style::new().fg(ACC),
                            ),
                            Span::raw(value),
                            Span::styled(if selected { "▏" } else { "" }, Style::new().fg(ACC)),
                            Span::styled(format!("   {}", HELP[i]), Style::new().fg(DIM)),
                        ])
                    })
                    .collect();
                if let Some(error) = form.error {
                    lines.push(Line::styled(error, Style::new().fg(Color::LightRed)));
                }
                (
                    "新增自定义工具",
                    lines,
                    "↑↓ 或 Tab 换项  Enter 下一项或确认  Esc 返回",
                )
            }
            Panel::AgentHook => {
                let tool = self.options.get(self.choice).map_or("", String::as_str);
                let sample = Line::styled(
                    terminal_text(&hook_sample(tool, &self.root)),
                    Style::new().fg(ACC),
                );
                let lines = match tool {
                    "claude-code" => vec![
                        sample,
                        Line::styled(
                            "把生成的片段合并到 .claude/settings.local.json。",
                            Style::new().fg(DIM),
                        ),
                    ],
                    "codex" => vec![
                        sample,
                        Line::styled(
                            "把生成的片段合并到 .codex/hooks.json，在 Codex 中确认。",
                            Style::new().fg(DIM),
                        ),
                    ],
                    _ => vec![
                        Line::from("让自己的 Hook 把一条 JSON 写到 tt ai-event 的 stdin。"),
                        sample,
                        Line::from(
                            "文件操作发送 write_start / write_finish，并带同一 call_id 和 files。",
                        ),
                        Line::styled("完整字段见 README 的「自定义工具」。", Style::new().fg(DIM)),
                    ],
                };
                ("Hook 接入", lines, "c 复制  Enter 或 Esc 返回")
            }
            _ => return,
        };
        let panel =
            Paragraph::new(lines).block(Block::default().title(title).borders(Borders::TOP));
        let panel = if matches!(self.panel, Panel::AgentSetup) {
            panel
        } else {
            panel.wrap(Wrap { trim: false })
        };
        f.render_widget(panel, area);
        let footer = if matches!(self.panel, Panel::AgentHook)
            && let Some(notice) = self.notice
        {
            format!("{notice}  ·  {controls}")
        } else {
            controls.into()
        };
        f.render_widget(Paragraph::new(footer).style(Style::new().fg(ACC)), panes[2]);
    }
}

fn hook_sample(tool: &str, root: &str) -> String {
    match tool {
        "claude-code" | "codex" => format!("tt ai-hook-config {tool}"),
        _ => serde_json::json!({
            "version": 1,
            "tool": tool,
            "session_id": "session-1",
            "cwd": root,
            "phase": "session_start",
        })
        .to_string(),
    }
}

fn list_start(choice: usize, area: Rect) -> usize {
    choice.saturating_sub(area.height.saturating_sub(1) as usize / 2)
}
