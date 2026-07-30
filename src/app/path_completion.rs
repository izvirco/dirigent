use super::*;

pub(super) fn start_fuzzy_index(
    path: &std::path::Path,
    watch: bool,
    ready: FuzzyIndexReady,
    ready_events: Sender<FuzzyIndexReady>,
) -> Result<SharedFilePicker, String> {
    let picker = SharedFilePicker::default();
    FilePicker::new_with_shared_state(
        picker.clone(),
        SharedFrecency::default(),
        FilePickerOptions {
            base_path: path.to_string_lossy().into_owned(),
            mode: FFFMode::Ai,
            watch,
            enable_fs_root_scanning: true,
            enable_home_dir_scanning: true,
            ..Default::default()
        },
    )
    .map_err(|error| format!("could not index {}: {error}", path.display()))?;

    let waiting_picker = picker.clone();
    std::thread::spawn(move || {
        waiting_picker.wait_for_scan(Duration::from_secs(60));
        let _ = ready_events.send_blocking(ready);
    });
    Ok(picker)
}

pub(super) fn composer_path_query(text: &str, cursor: usize) -> Option<(Range<usize>, String)> {
    let prefix = text.get(..cursor)?;
    let at = prefix.rfind('@')?;
    if at > 0 {
        let preceding = prefix[..at].chars().next_back()?;
        if !preceding.is_whitespace() && !"([{<".contains(preceding) {
            return None;
        }
    }
    let query = &prefix[at + 1..];
    if query.chars().any(char::is_whitespace) {
        return None;
    }
    Some((at + 1..cursor, query.to_string()))
}

pub(super) fn resolve_tilde_path(raw: &str, home: &std::path::Path) -> Option<PathBuf> {
    if raw == "~" {
        return Some(home.to_path_buf());
    }
    if let Some(relative) = raw.strip_prefix("~/").or_else(|| raw.strip_prefix(r"~\")) {
        return Some(home.join(relative));
    }
    (!raw.starts_with('~')).then(|| PathBuf::from(raw))
}

pub(super) fn directory_path_query(raw: &str) -> Option<(PathBuf, String)> {
    if raw.is_empty() {
        return None;
    }
    let path = if raw.starts_with('~') {
        let home = platform::home_dir()?;
        resolve_tilde_path(raw, &home)?
    } else {
        PathBuf::from(raw)
    };
    if !path.is_absolute() {
        return None;
    }
    if path.is_dir() {
        path.parent()?;
        let has_trailing_separator = raw.ends_with('/') || raw.ends_with(std::path::MAIN_SEPARATOR);
        return has_trailing_separator.then(|| (path, String::new()));
    }
    let parent = path.parent()?.to_path_buf();
    parent.parent()?;
    if !parent.is_dir() {
        return None;
    }
    let query = path.file_name()?.to_string_lossy().into_owned();
    (!query.is_empty()).then_some((parent, query))
}

pub(super) fn fuzzy_file_results(picker: &SharedFilePicker, query: &str) -> Vec<String> {
    let Ok(guard) = picker.read() else {
        return Vec::new();
    };
    let Some(picker) = guard.as_ref() else {
        return Vec::new();
    };
    let parser = QueryParser::default();
    let query = parser.parse(query);
    picker
        .fuzzy_search(
            &query,
            None,
            FuzzySearchOptions {
                pagination: PaginationArgs {
                    offset: 0,
                    limit: 8,
                },
                ..Default::default()
            },
        )
        .items
        .into_iter()
        .map(|item| item.relative_path(picker))
        .collect()
}

pub(super) fn directory_child_results(root: &std::path::Path, query: &str) -> Vec<String> {
    let query = query.to_lowercase();
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };
    let mut children = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            if !path.is_dir() {
                return None;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            let normalized = name.to_lowercase();
            (query.is_empty() || normalized.contains(&query)).then_some((
                !normalized.starts_with(&query),
                normalized,
                path.to_string_lossy().into_owned(),
            ))
        })
        .collect::<Vec<_>>();
    children
        .sort_unstable_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));
    children
        .into_iter()
        .take(8)
        .map(|(_, _, path)| path)
        .collect()
}

impl Dirigent {
    pub(super) fn path_completion_input(
        &self,
        target: PathCompletionTarget,
    ) -> Option<Entity<TextInput>> {
        match target {
            PathCompletionTarget::Project => Some(self.project_input.clone()),
            PathCompletionTarget::NewHarness => Some(self.harness_input.clone()),
            PathCompletionTarget::Harness(id) => self.composer_inputs.get(&id).cloned(),
        }
    }
    pub(super) fn sync_path_completion_input(&self, cx: &mut Context<Self>) {
        let target = self
            .path_completion
            .as_ref()
            .map(|completion| completion.target);
        let visible = if self.adding_project {
            Some(PathCompletionTarget::Project)
        } else if self.creating_harness {
            Some(PathCompletionTarget::NewHarness)
        } else {
            self.selected_harness.map(PathCompletionTarget::Harness)
        };
        if let Some(visible) = visible
            && let Some(input) = self.path_completion_input(visible)
        {
            input.update(cx, |input, _| {
                input.set_completion_active(target == Some(visible))
            });
        }
    }
    pub(super) fn refresh_composer_path_completion(
        &mut self,
        target: PathCompletionTarget,
        cx: &mut Context<Self>,
    ) {
        let Some(input) = self.path_completion_input(target) else {
            self.path_completion = None;
            return;
        };
        let query = {
            let input = input.read(cx);
            composer_path_query(input.text(), input.cursor())
        };
        let Some((replacement, query)) = query else {
            self.path_completion = None;
            input.update(cx, |input, _| input.set_completion_active(false));
            return;
        };
        let project_id = match target {
            PathCompletionTarget::NewHarness => self.selected_project,
            PathCompletionTarget::Harness(id) => self
                .harnesses
                .iter()
                .find(|harness| harness.id == id)
                .map(|harness| harness.project_id),
            PathCompletionTarget::Project => None,
        };
        let items = project_id
            .and_then(|id| self.project_file_pickers.get(&id))
            .map_or_else(Vec::new, |picker| fuzzy_file_results(picker, &query));
        let active = !items.is_empty();
        self.path_completion = active.then_some(PathCompletion {
            target,
            items,
            selected: 0,
            replacement,
        });
        input.update(cx, |input, _| input.set_completion_active(active));
        self.composer_dropdown = None;
        cx.notify();
    }
    pub(super) fn refresh_project_path_completion(&mut self, cx: &mut Context<Self>) {
        let raw = self.project_input.read(cx).text().to_string();
        let Some((root, query)) = directory_path_query(raw.trim()) else {
            self.path_completion = None;
            self.project_input
                .update(cx, |input, _| input.set_completion_active(false));
            return;
        };
        let items = directory_child_results(&root, &query);
        let active = !items.is_empty();
        self.path_completion = active.then_some(PathCompletion {
            target: PathCompletionTarget::Project,
            items,
            selected: 0,
            replacement: 0..raw.len(),
        });
        self.project_input
            .update(cx, |input, _| input.set_completion_active(active));
        cx.notify();
    }
    pub(super) fn handle_fuzzy_index_ready(
        &mut self,
        event: FuzzyIndexReady,
        cx: &mut Context<Self>,
    ) {
        match event {
            FuzzyIndexReady::Project(project_id) => {
                let target = if self.creating_harness && self.selected_project == Some(project_id) {
                    Some(PathCompletionTarget::NewHarness)
                } else {
                    self.selected_harness.and_then(|id| {
                        self.harnesses
                            .iter()
                            .find(|harness| harness.id == id && harness.project_id == project_id)
                            .map(|_| PathCompletionTarget::Harness(id))
                    })
                };
                if let Some(target) = target {
                    self.refresh_composer_path_completion(target, cx);
                }
            }
        }
    }
    pub(super) fn move_path_completion(&mut self, delta: isize, cx: &mut Context<Self>) {
        let Some(completion) = self.path_completion.as_mut() else {
            return;
        };
        completion.selected = (completion.selected as isize + delta)
            .rem_euclid(completion.items.len() as isize) as usize;
        cx.notify();
    }
    pub(crate) fn choose_path_completion(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(completion) = self.path_completion.as_mut() else {
            return;
        };
        if index >= completion.items.len() {
            return;
        }
        completion.selected = index;
        self.accept_path_completion(cx);
    }
    pub(super) fn accept_path_completion(&mut self, cx: &mut Context<Self>) {
        let Some(completion) = self.path_completion.take() else {
            return;
        };
        let Some(value) = completion.items.get(completion.selected).cloned() else {
            return;
        };
        let Some(input) = self.path_completion_input(completion.target) else {
            return;
        };
        let mut replacement = completion.replacement;
        let value = if completion.target == PathCompletionTarget::Project {
            value
        } else {
            if let Some(separator) = input.read(cx).text()[replacement.end..]
                .chars()
                .next()
                .filter(|character| matches!(character, ' ' | '\t'))
            {
                replacement.end += separator.len_utf8();
            }
            format!("{value} ")
        };
        input.update(cx, |input, cx| {
            input.set_completion_active(false);
            input.replace_range(replacement, &value, cx);
        });
        cx.notify();
    }
}
