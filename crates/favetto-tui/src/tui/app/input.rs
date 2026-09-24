//! Keyboard, mouse, and list-navigation input handling for [`App`](super::App).

use super::*;

impl App {
    /// Route a keypress.
    ///
    /// Ctrl+Y toggles keyboard focus between the embedded agent and favetto. With
    /// the agent focused (the default when the panel is opened), every other key is
    /// forwarded to the agent PTY. With favetto focused, an open popup owns the
    /// keyboard, Ctrl+P toggles the menu, and the Agent tab handles its own keys.
    pub fn handle_key(&mut self, key: KeyEvent) -> UiAction {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

        if is_focus_toggle(&key) {
            // A read-only PTY never takes the keyboard, so focus cannot move to it.
            if !self.agent_read_only {
                self.agent_capture = !self.agent_capture;
            }
            return UiAction::None;
        }

        // Ctrl+P closes an open popup, or opens the menu under favetto focus. While
        // the agent captures keys it is forwarded to the agent instead.
        if ctrl && matches!(key.code, KeyCode::Char('p') | KeyCode::Char('P')) {
            if matches!(self.popup, Popup::None) {
                if self.tab == Tab::Agent && self.agent_capture {
                    return self.forward_agent_key(&key);
                }
                self.popup = Popup::Menu { selected: 0 };
            } else {
                self.popup = Popup::None;
            }
            return UiAction::None;
        }

        // Ctrl+O opens the session picker (or closes it again). It never clobbers
        // an unrelated popup, and is forwarded while the agent captures keys.
        if ctrl && matches!(key.code, KeyCode::Char('o') | KeyCode::Char('O')) {
            return match self.popup {
                Popup::None => {
                    if self.tab == Tab::Agent && self.agent_capture {
                        self.forward_agent_key(&key)
                    } else {
                        UiAction::OpenSessions
                    }
                }
                Popup::Sessions { .. } => {
                    self.popup = Popup::None;
                    UiAction::None
                }
                _ => UiAction::None,
            };
        }

        // Ctrl+R answers a precise input request on the attached session. It is
        // intercepted only while such a request is pending, so sessions without a
        // state channel (and the agent's own Ctrl+R) keep their existing handling.
        if ctrl
            && matches!(key.code, KeyCode::Char('r') | KeyCode::Char('R'))
            && matches!(self.popup, Popup::None)
        {
            if let Some((session_id, request)) = self.pending_request() {
                self.popup = Popup::Reply(ReplyPrompt::new(session_id, request));
                return UiAction::None;
            }
        }

        // A popup owns the keyboard while it is open.
        match &self.popup {
            Popup::Form(_) => return self.handle_form_key(key),
            Popup::Menu { .. } => return self.handle_menu_key(key.code),
            Popup::Wizard(_) => return self.handle_wizard_key(key),
            Popup::TaskVars(_) => return self.handle_task_vars_key(key),
            Popup::Help { .. } => return self.handle_help_key(key.code),
            Popup::Workflow { .. } => return self.handle_workflow_key(key.code),
            Popup::Sessions { .. } => return self.handle_sessions_key(key.code),
            Popup::Reply(_) => return self.handle_reply_key(key),
            Popup::None => {}
        }

        // Agent keyboard focus: everything else is forwarded to the PTY.
        if self.tab == Tab::Agent && self.agent_capture && !self.agent_read_only {
            return self.forward_agent_key(&key);
        }

        // `?` opens help when no popup owns the keyboard and the embedded agent is
        // not capturing. Popup routing and agent forwarding above take precedence.
        if key.code == KeyCode::Char('?') {
            self.popup = Popup::Help { scroll: 0 };
            return UiAction::None;
        }

        // `M` mutes/unmutes sound. Like `?`, it is literal inside a popup and is
        // forwarded to the agent while the embedded agent captures the keyboard.
        if key.code == KeyCode::Char('m') || key.code == KeyCode::Char('M') {
            self.toggle_sound_muted();
            return UiAction::None;
        }

        // `w` toggles the floating workflow graph, like `?`/`M`.
        if key.code == KeyCode::Char('w') || key.code == KeyCode::Char('W') {
            self.popup = Popup::Workflow {
                scroll: 0,
                hscroll: 0,
            };
            return UiAction::OpenWorkflow;
        }

        // `p` toggles the Catalog preview pane, but only on the Catalog tab.
        if self.tab == Tab::Catalog
            && (key.code == KeyCode::Char('p') || key.code == KeyCode::Char('P'))
        {
            self.toggle_catalog_preview();
            return UiAction::None;
        }

        if self.tab == Tab::Agent {
            return self.handle_agent_favetto_key(&key);
        }

        self.handle_normal_key(&key)
    }

    /// Keys for the Agent tab while favetto (not the agent) has the keyboard.
    pub(super) fn handle_agent_favetto_key(&mut self, key: &KeyEvent) -> UiAction {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && matches!(key.code, KeyCode::Char('q') | KeyCode::Char('Q')) {
            self.tab = Tab::Tasks;
            return UiAction::None;
        }
        if ctrl && matches!(key.code, KeyCode::Char('n') | KeyCode::Char('N')) {
            if let Some(task_id) = self.agent_task_id.clone() {
                return UiAction::NewAgent(task_id);
            }
            return UiAction::None;
        }
        match key.code {
            KeyCode::Tab | KeyCode::Right => {
                self.next_tab();
                UiAction::None
            }
            KeyCode::Left | KeyCode::BackTab => {
                self.prev_tab();
                UiAction::None
            }
            KeyCode::Esc => {
                self.tab = Tab::Tasks;
                UiAction::None
            }
            _ => UiAction::None,
        }
    }

    /// Encode a keypress and forward it to the active agent session.
    pub(super) fn forward_agent_key(&self, key: &KeyEvent) -> UiAction {
        let bytes = encode_key(key);
        if bytes.is_empty() {
            UiAction::None
        } else {
            UiAction::AgentInput(bytes)
        }
    }

    /// Handle mouse input.
    ///
    /// Favetto's own regions (the tab bar) are handled first so a click switches
    /// tabs. A click on a list row selects it, or runs the row's primary action
    /// when it was already selected. Any other click on the Agent tab is forwarded
    /// to the embedded agent as a terminal mouse report when the agent has enabled
    /// mouse reporting.
    pub fn handle_mouse(&mut self, mouse: MouseEvent) -> UiAction {
        // A popup owns the screen; ignore clicks so we don't select under it.
        if !matches!(self.popup, Popup::None) {
            return UiAction::None;
        }

        if let MouseEventKind::Down(MouseButton::Left) = mouse.kind {
            for region in &self.click_regions {
                if mouse.row == region.row
                    && mouse.column >= region.col_start
                    && mouse.column < region.col_end
                {
                    let ClickAction::Tab(tab) = region.action;
                    self.tab = tab;
                    return UiAction::None;
                }
            }
            if let Some(action) = self.handle_list_click(mouse.column, mouse.row) {
                return action;
            }
        }

        // The wheel scrolls the Catalog preview when the pointer is over it.
        // This must run before `scroll_list`: on narrow terminals the preview is
        // a floating overlay drawn inside the tree's full-width hit-test rect, so
        // offering the wheel to the list first would always steal it.
        if self.tab == Tab::Catalog && self.catalog_preview_visible {
            if let Some(area) = self.catalog_preview_area {
                let over = mouse.column >= area.x
                    && mouse.column < area.x + area.width
                    && mouse.row >= area.y
                    && mouse.row < area.y + area.height;
                if over {
                    match mouse.kind {
                        MouseEventKind::ScrollUp => {
                            self.scroll_catalog_preview(-3);
                            return UiAction::None;
                        }
                        MouseEventKind::ScrollDown => {
                            self.scroll_catalog_preview(3);
                            return UiAction::None;
                        }
                        _ => {}
                    }
                }
            }
        }

        // The wheel over a list scrolls that list's selection.
        let wheel_delta = match mouse.kind {
            MouseEventKind::ScrollUp => Some(-3),
            MouseEventKind::ScrollDown => Some(3),
            _ => None,
        };
        if let Some(delta) = wheel_delta {
            if self.scroll_list(mouse.column, mouse.row, delta) {
                return UiAction::None;
            }
        }

        if self.tab == Tab::Agent
            && self.agent_running
            && self.agent_session_id.is_some()
            && !self.agent_read_only
        {
            if let Some(area) = self.agent_area {
                if let Some(bytes) = encode_mouse(mouse, area, self.term.screen()) {
                    return UiAction::AgentInput(bytes);
                }
            }
        }
        UiAction::None
    }

    /// The list geometry of the active tab, if that tab has a list.
    pub(super) fn active_list_geometry(&self) -> Option<(Tab, ListGeometry)> {
        match self.tab {
            Tab::Tasks => Some((Tab::Tasks, self.tasks_geom)),
            Tab::Catalog => Some((Tab::Catalog, self.catalog_geom)),
            Tab::Events => Some((Tab::Events, self.events_geom)),
            Tab::Scheduler => Some((Tab::Scheduler, self.schedules_geom)),
            Tab::Notifications => Some((Tab::Notifications, self.notifications_geom)),
            Tab::Agent => None,
        }
    }

    /// Handle a left click on a list row. Returns `Some` when the click hit a row
    /// (the inner action may be `UiAction::None` for selection-only panels).
    pub(super) fn handle_list_click(&mut self, col: u16, row: u16) -> Option<UiAction> {
        let (tab, geom) = self.active_list_geometry()?;
        let index = geom.row_at(row, col)?;
        Some(self.select_row(tab, index))
    }

    /// Select a data row and run the tab's primary action when it was already
    /// selected (mouse equivalent of Enter).
    pub(super) fn select_row(&mut self, tab: Tab, index: usize) -> UiAction {
        match tab {
            Tab::Tasks => {
                if self.tasks.is_empty() {
                    return UiAction::None;
                }
                let already = self.tasks_selected == index;
                self.tasks_selected = index.min(self.tasks.len() - 1);
                if already {
                    if let Some(t) = self.tasks.get(index) {
                        return UiAction::OpenAgent(t.id.to_string());
                    }
                }
                UiAction::None
            }
            Tab::Catalog => {
                let row = self.catalog_rows().get(index).cloned();
                match row {
                    // Clicking a folder selects it and folds/unfolds it.
                    Some(CatalogRow::Folder { .. }) => {
                        self.select_catalog_row(index);
                        self.toggle_catalog_folder();
                    }
                    Some(CatalogRow::Task {
                        index: task_index, ..
                    }) => {
                        let already = self.catalog_selected == index;
                        self.select_catalog_row(index);
                        if already {
                            if let Some(entry) = self.catalog.get(task_index).cloned() {
                                return self.begin_catalog_task(&entry);
                            }
                        }
                    }
                    None => {}
                }
                UiAction::None
            }
            Tab::Events => {
                if !self.events.is_empty() {
                    self.events_selected = index.min(self.events.len() - 1);
                }
                UiAction::None
            }
            Tab::Scheduler => {
                if !self.schedules.is_empty() {
                    self.schedules_selected = index.min(self.schedules.len() - 1);
                }
                UiAction::None
            }
            Tab::Notifications => {
                if !self.notifications.is_empty() {
                    self.notifications_selected = index.min(self.notifications.len() - 1);
                }
                UiAction::None
            }
            Tab::Agent => UiAction::None,
        }
    }

    /// Scroll the active list's selection by `delta` rows when the pointer is over
    /// its rectangle. Returns `true` when the wheel was consumed.
    pub(super) fn scroll_list(&mut self, col: u16, row: u16, delta: i32) -> bool {
        let Some((tab, geom)) = self.active_list_geometry() else {
            return false;
        };
        let over = col >= geom.inner.x
            && col < geom.inner.x + geom.inner.width
            && row >= geom.inner.y
            && row < geom.inner.y + geom.inner.height;
        if !over {
            return false;
        }
        match tab {
            Tab::Tasks => {
                self.tasks_selected = shift_index(self.tasks_selected, delta, self.tasks.len());
            }
            Tab::Catalog => {
                let len = self.catalog_rows().len();
                let next = shift_index(self.catalog_selected, delta, len);
                self.select_catalog_row(next);
            }
            Tab::Events => {
                self.events_selected = shift_index(self.events_selected, delta, self.events.len());
            }
            Tab::Scheduler => {
                self.schedules_selected =
                    shift_index(self.schedules_selected, delta, self.schedules.len());
            }
            Tab::Notifications => {
                self.notifications_selected =
                    shift_index(self.notifications_selected, delta, self.notifications.len());
            }
            Tab::Agent => {}
        }
        true
    }

    /// Keys while the Help overlay is open: `?`/`Esc` close, arrows and page keys
    /// scroll the content.
    pub(super) fn handle_help_key(&mut self, code: KeyCode) -> UiAction {
        match code {
            KeyCode::Esc | KeyCode::Char('?') => self.popup = Popup::None,
            KeyCode::Up => {
                if let Popup::Help { scroll } = &mut self.popup {
                    *scroll = scroll.saturating_sub(1);
                }
            }
            KeyCode::PageUp => {
                if let Popup::Help { scroll } = &mut self.popup {
                    *scroll = scroll.saturating_sub(10);
                }
            }
            KeyCode::Down => {
                if let Popup::Help { scroll } = &mut self.popup {
                    *scroll = scroll.saturating_add(1);
                }
            }
            KeyCode::PageDown => {
                if let Popup::Help { scroll } = &mut self.popup {
                    *scroll = scroll.saturating_add(10);
                }
            }
            _ => {}
        }
        UiAction::None
    }

    /// Keys while the workflow overlay is open: `w`/`Esc` close, arrows and page
    /// keys scroll the cached graph (or DOT fallback) in both axes.
    pub(super) fn handle_workflow_key(&mut self, code: KeyCode) -> UiAction {
        match code {
            KeyCode::Esc | KeyCode::Char('w') | KeyCode::Char('W') => self.popup = Popup::None,
            KeyCode::Up => {
                if let Popup::Workflow { scroll, .. } = &mut self.popup {
                    *scroll = scroll.saturating_sub(1);
                }
            }
            KeyCode::PageUp => {
                if let Popup::Workflow { scroll, .. } = &mut self.popup {
                    *scroll = scroll.saturating_sub(10);
                }
            }
            KeyCode::Down => {
                if let Popup::Workflow { scroll, .. } = &mut self.popup {
                    *scroll = scroll.saturating_add(1);
                }
            }
            KeyCode::PageDown => {
                if let Popup::Workflow { scroll, .. } = &mut self.popup {
                    *scroll = scroll.saturating_add(10);
                }
            }
            KeyCode::Left => {
                if let Popup::Workflow { hscroll, .. } = &mut self.popup {
                    *hscroll = hscroll.saturating_sub(1);
                }
            }
            KeyCode::Right => {
                if let Popup::Workflow { hscroll, .. } = &mut self.popup {
                    *hscroll = hscroll.saturating_add(1);
                }
            }
            _ => {}
        }
        UiAction::None
    }

    /// Keys while the Ctrl+O session picker is open: arrows move, Enter attaches,
    /// Esc (or Ctrl+O) closes.
    pub(super) fn handle_sessions_key(&mut self, code: KeyCode) -> UiAction {
        match code {
            KeyCode::Esc => {
                self.popup = Popup::None;
                UiAction::None
            }
            KeyCode::Up => {
                if let Popup::Sessions { selected, .. } = &mut self.popup {
                    *selected = selected.saturating_sub(1);
                }
                UiAction::None
            }
            KeyCode::Down => {
                if let Popup::Sessions { selected, sessions } = &mut self.popup {
                    *selected = (*selected + 1).min(sessions.len().saturating_sub(1));
                }
                UiAction::None
            }
            KeyCode::Enter => {
                let sid = match &self.popup {
                    Popup::Sessions { selected, sessions } => {
                        sessions.get(*selected).map(|s| s.id.clone())
                    }
                    _ => None,
                };
                match sid {
                    Some(sid) => {
                        self.popup = Popup::None;
                        UiAction::AttachSession(sid)
                    }
                    None => UiAction::None,
                }
            }
            _ => UiAction::None,
        }
    }

    /// Keys while the Ctrl+R reply popup is open: arrows pick an option, Enter
    /// sends `agents.reply`, and a free-form prompt edits its value buffer.
    pub(super) fn handle_reply_key(&mut self, key: KeyEvent) -> UiAction {
        if key.code == KeyCode::Esc {
            self.popup = Popup::None;
            return UiAction::None;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let has_options = match &self.popup {
            Popup::Reply(prompt) => prompt.has_options(),
            _ => return UiAction::None,
        };
        if key.code == KeyCode::Enter {
            let (session_id, request_id, reply) = match &self.popup {
                Popup::Reply(prompt) => (
                    prompt.session_id.clone(),
                    prompt.request.id.clone(),
                    prompt.reply(),
                ),
                _ => return UiAction::None,
            };
            self.popup = Popup::None;
            return UiAction::Reply {
                session_id,
                request_id,
                reply,
            };
        }
        let Popup::Reply(prompt) = &mut self.popup else {
            return UiAction::None;
        };
        match key.code {
            KeyCode::Up if has_options => {
                prompt.selected = prompt.selected.saturating_sub(1);
            }
            KeyCode::Down if has_options => {
                prompt.selected =
                    (prompt.selected + 1).min(prompt.request.options.len().saturating_sub(1));
            }
            KeyCode::Backspace if !has_options => prompt.input.backspace(),
            KeyCode::Delete if !has_options => prompt.input.delete(),
            KeyCode::Left if !has_options => prompt.input.move_left(),
            KeyCode::Right if !has_options => prompt.input.move_right(),
            KeyCode::Home if !has_options => prompt.input.home(),
            KeyCode::End if !has_options => prompt.input.end(),
            KeyCode::Char(c) if !has_options && !ctrl && !alt => prompt.input.insert_char(c),
            _ => {}
        }
        UiAction::None
    }

    pub(super) fn handle_menu_key(&mut self, code: KeyCode) -> UiAction {
        match code {
            KeyCode::Esc => {
                self.popup = Popup::None;
                UiAction::None
            }
            KeyCode::Up => {
                if let Popup::Menu { selected } = &mut self.popup {
                    *selected = selected.saturating_sub(1);
                }
                UiAction::None
            }
            KeyCode::Down => {
                if let Popup::Menu { selected } = &mut self.popup {
                    *selected = (*selected + 1).min(MENU_OPTIONS.len() - 1);
                }
                UiAction::None
            }
            KeyCode::Enter => {
                let selected = match self.popup {
                    Popup::Menu { selected } => selected,
                    _ => 0,
                };
                self.open_form(selected)
            }
            _ => UiAction::None,
        }
    }

    pub(super) fn handle_normal_key(&mut self, key: &KeyEvent) -> UiAction {
        let code = key.code;

        match code {
            KeyCode::Char('q') | KeyCode::Esc => UiAction::Quit,
            KeyCode::Tab | KeyCode::Right => {
                self.next_tab();
                UiAction::None
            }
            KeyCode::Left | KeyCode::BackTab => {
                self.prev_tab();
                UiAction::None
            }
            KeyCode::Up => {
                match self.tab {
                    Tab::Tasks => self.select_prev(),
                    Tab::Catalog => {
                        let prev = self.catalog_selected.saturating_sub(1);
                        self.select_catalog_row(prev);
                    }
                    Tab::Events => self.events_selected = self.events_selected.saturating_sub(1),
                    Tab::Scheduler => {
                        self.schedules_selected = self.schedules_selected.saturating_sub(1)
                    }
                    Tab::Notifications => {
                        self.notifications_selected = self.notifications_selected.saturating_sub(1)
                    }
                    _ => {}
                }
                UiAction::None
            }
            KeyCode::Down => {
                match self.tab {
                    Tab::Tasks => self.select_next(),
                    Tab::Catalog if !self.catalog.is_empty() => {
                        let next = self.catalog_selected + 1;
                        self.select_catalog_row(next);
                    }
                    Tab::Events if !self.events.is_empty() => {
                        self.events_selected =
                            (self.events_selected + 1).min(self.events.len() - 1);
                    }
                    Tab::Scheduler if !self.schedules.is_empty() => {
                        self.schedules_selected =
                            (self.schedules_selected + 1).min(self.schedules.len() - 1);
                    }
                    Tab::Notifications if !self.notifications.is_empty() => {
                        self.notifications_selected =
                            (self.notifications_selected + 1).min(self.notifications.len() - 1);
                    }
                    _ => {}
                }
                UiAction::None
            }
            KeyCode::PageUp => {
                match self.tab {
                    Tab::Events => {
                        self.events_selected = self.events_selected.saturating_sub(10);
                    }
                    Tab::Catalog => {
                        let page = self.catalog_preview_page();
                        self.scroll_catalog_preview(-page);
                    }
                    _ => {}
                }
                UiAction::None
            }
            KeyCode::PageDown => {
                match self.tab {
                    Tab::Events => {
                        if !self.events.is_empty() {
                            self.events_selected =
                                (self.events_selected + 10).min(self.events.len() - 1);
                        }
                    }
                    Tab::Catalog => {
                        let page = self.catalog_preview_page();
                        self.scroll_catalog_preview(page);
                    }
                    _ => {}
                }
                UiAction::None
            }
            KeyCode::Enter => {
                if self.tab == Tab::Tasks {
                    if let Some(t) = self.tasks.get(self.tasks_selected) {
                        return UiAction::OpenAgent(t.id.to_string());
                    }
                }
                if self.tab == Tab::Catalog {
                    // Enter on a folder folds/unfolds it; only a task row starts.
                    if !self.toggle_catalog_folder() {
                        if let Some(index) = self.selected_catalog_task() {
                            if let Some(entry) = self.catalog.get(index).cloned() {
                                return self.begin_catalog_task(&entry);
                            }
                        }
                    }
                }
                UiAction::None
            }
            // Space folds/unfolds the selected Catalog folder (Left/Right already
            // switch tabs, so they would conflict with tree navigation).
            KeyCode::Char(' ') if self.tab == Tab::Catalog => {
                self.toggle_catalog_folder();
                UiAction::None
            }
            // `e` edits the selected Catalog task; a folder row is a no-op.
            KeyCode::Char('e') if self.tab == Tab::Catalog => {
                match self
                    .selected_catalog_task()
                    .and_then(|i| self.catalog.get(i))
                {
                    Some(entry) => UiAction::EditCatalog(entry.name.clone()),
                    None => UiAction::None,
                }
            }
            _ => UiAction::None,
        }
    }
}
