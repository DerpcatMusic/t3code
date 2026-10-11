//! Native provider settings for the canonical local T3 backend.
use gpui::{
    AnyElement, Context, Entity, IntoElement, Render, SharedString, Subscription, Task, Window,
    div, prelude::*, px,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::rc::Rc;

use crate::{composer::ComposerInput, popover, settings::widgets, state::AppState, theme::Theme};

#[derive(Clone, Deserialize)]
struct Snapshot {
    instances: Vec<Instance>,
    drivers: Vec<Driver>,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Instance {
    instance_id: String,
    instance: Value,
    enabled: bool,
    removable: bool,
    live: Option<Value>,
}

#[derive(Clone, Deserialize)]
struct Driver {
    id: String,
    label: String,
    fields: Vec<Field>,
    known: bool,
}

#[derive(Clone, Deserialize)]
struct Field {
    key: String,
    label: String,
    control: String,
    options: Vec<FieldOption>,
}

#[derive(Clone, Deserialize)]
struct FieldOption {
    value: String,
    label: String,
}

struct EditorField {
    field: Field,
    input: Option<Entity<ComposerInput>>,
    select: widgets::SelectState,
}

struct Editor {
    original: Option<Instance>,
    instance: Value,
    driver: Driver,
    id: Entity<ComposerInput>,
    name: Entity<ComposerInput>,
    fields: Vec<EditorField>,
    config: Option<Entity<ComposerInput>>,
    model: Entity<ComposerInput>,
    editing_model: Option<usize>,
    confirm_remove: bool,
    show_models: bool,
}

pub struct ProvidersPage {
    state: Entity<AppState>,
    scroll: widgets::PageScroll,
    snapshot: Option<Snapshot>,
    busy: bool,
    error: Option<String>,
    editor: Option<Editor>,
    driver_select: widgets::SelectState,
    selected_driver: usize,
    task: Option<Task<()>>,
    _observe: Subscription,
}

fn text_input(
    label: &str,
    value: &str,
    multiline: bool,
    cx: &mut Context<ProvidersPage>,
) -> Entity<ComposerInput> {
    cx.new(|cx| {
        let input =
            ComposerInput::with_context(SharedString::from(label.to_owned()), "PaletteSearch", cx)
                .with_text_metrics(13.0, 20.0);
        let mut input = if multiline {
            input
        } else {
            input
                .with_single_line()
                .with_accessibility_role(gpui::Role::TextInput)
        };
        input.set_text(value.to_owned(), cx);
        input
    })
}

fn config(instance: &mut Value) -> &mut serde_json::Map<String, Value> {
    if !instance["config"].is_object() {
        instance["config"] = json!({});
    }
    instance["config"].as_object_mut().unwrap()
}

fn set_enabled(instance: &Value, enabled: bool) -> Value {
    let mut instance = instance.clone();
    instance["enabled"] = json!(enabled);
    if instance["config"].get("enabled").is_some() {
        config(&mut instance).insert("enabled".into(), json!(enabled));
    }
    instance
}

fn model_slug(model: &Value) -> &str {
    model
        .as_str()
        .or_else(|| model["slug"].as_str())
        .unwrap_or("")
}

fn replace_model(models: &mut Vec<Value>, index: Option<usize>, slug: &str) -> Result<(), String> {
    let slug = slug.trim();
    if slug.is_empty() {
        return Err("Enter a model slug.".into());
    }
    if models
        .iter()
        .enumerate()
        .any(|(i, model)| Some(i) != index && model_slug(model) == slug)
    {
        return Err("That custom model is already configured.".into());
    }
    match index {
        Some(index) => {
            let model = models
                .get_mut(index)
                .ok_or("The model changed. Reopen the editor.")?;
            if model.is_object() {
                model["slug"] = json!(slug);
            } else {
                *model = json!(slug);
            }
        }
        None => models.push(json!(slug)),
    }
    Ok(())
}

fn secret_configured(instance: &Value, field: &Field) -> bool {
    if field.control == "environmentPassword" {
        return instance["environment"].as_array().is_some_and(|variables| {
            variables.iter().any(|variable| {
                variable["name"] == field.key
                    && (variable["valueRedacted"] == true
                        || variable["value"]
                            .as_str()
                            .is_some_and(|value| !value.is_empty()))
            })
        });
    }
    instance["config"][&field.key]
        .as_str()
        .is_some_and(|value| !value.is_empty())
}

fn set_secret(instance: &mut Value, field: &Field, value: Option<&str>) {
    if field.control == "environmentPassword" {
        let mut variables = instance["environment"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        variables.retain(|variable| variable["name"] != field.key);
        if let Some(value) = value {
            variables.push(json!({"name":field.key,"value":value,"sensitive":true}));
        }
        instance["environment"] = json!(variables);
    } else if let Some(value) = value {
        config(instance).insert(field.key.clone(), json!(value));
    } else {
        config(instance).remove(&field.key);
    }
}

fn status_line(row: &Instance) -> String {
    let Some(live) = &row.live else {
        return "Not checked · Refresh to check health and authentication".into();
    };
    let health = live["status"].as_str().unwrap_or("unknown");
    let version = live["version"].as_str().unwrap_or("Version unknown");
    let auth = live["auth"]["status"].as_str().unwrap_or("unknown");
    let identity = live["auth"]["email"]
        .as_str()
        .or_else(|| live["auth"]["label"].as_str());
    let mut line = if row.enabled {
        format!("{health} · {version} · Auth: {auth}")
    } else {
        format!("Disabled · Last check: {version} · Auth: {auth}")
    };
    if let Some(identity) = identity {
        line.push_str(&format!(" · {identity}"));
    }
    line
}

fn brand_icon(driver: &str) -> &'static str {
    match driver {
        "codex" => crate::icons::OPENAI_MARK,
        "claudeAgent" => crate::icons::CLAUDE_MARK,
        "cursor" => crate::icons::CURSOR_MARK,
        "grok" => crate::icons::GROK_MARK,
        "pi" => crate::icons::PI_MARK,
        "opencode" => crate::icons::OPENCODE_MARK,
        "antigravity" => crate::icons::ANTIGRAVITY_MARK,
        _ => crate::icons::REMOTE_SERVER,
    }
}

fn button(
    id: impl Into<SharedString>,
    label: impl Into<SharedString>,
    active: bool,
    theme: &Theme,
    cx: &mut Context<ProvidersPage>,
    action: impl Fn(&mut ProvidersPage, &mut Context<ProvidersPage>) + 'static,
) -> AnyElement {
    let action = Rc::new(action);
    let click = action.clone();
    let id: SharedString = id.into();
    let label: SharedString = label.into();
    widgets::text_action(theme, widgets::ActionTone::Quiet, label.clone())
        .id(id)
        .tab_index(if active { 0 } else { -1 })
        .role(gpui::Role::Button)
        .aria_label(label)
        .when(!active, |element| {
            element.aria_description("Unavailable while another provider action or editor is open")
        })
        .opacity(if active { 1.0 } else { 0.4 })
        .focus_visible(|style| style.border_2().border_color(theme.accent))
        .when(active, |element| {
            element
                .on_click(cx.listener(move |page, _, _, cx| click(page, cx)))
                .on_key_down(cx.listener(move |page, event: &gpui::KeyDownEvent, _, cx| {
                    if !event.is_held && matches!(event.keystroke.key.as_str(), "enter" | "space") {
                        cx.stop_propagation();
                        action(page, cx);
                    }
                }))
        })
        .into_any_element()
}

impl ProvidersPage {
    pub fn dismiss_on_escape(&mut self, cx: &mut Context<Self>) -> bool {
        if self.driver_select.is_open() {
            widgets::close_select(self, |page| &mut page.driver_select, cx);
            return true;
        }
        if self.busy {
            return true;
        }
        if self.editor.take().is_some() {
            self.error = None;
            cx.notify();
            return true;
        }
        false
    }

    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let observe = cx.observe(&state, |_, _, cx| cx.notify());
        let mut page = Self {
            state,
            scroll: Default::default(),
            snapshot: None,
            busy: false,
            error: None,
            editor: None,
            driver_select: Default::default(),
            selected_driver: 0,
            task: None,
            _observe: observe,
        };
        page.request("T3ProvidersGet", json!({}), false, cx);
        page
    }

    fn request(
        &mut self,
        method: &'static str,
        params: Value,
        close_editor: bool,
        cx: &mut Context<Self>,
    ) {
        if self.busy {
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.error =
                Some("The local T3 backend is disconnected. Reconnect, then retry.".into());
            cx.notify();
            return;
        };
        self.busy = true;
        self.error = None;
        self.freeze_editor(true, cx);
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(method, params)
                .await
                .map_err(|error| error.to_string())
                .and_then(|value| {
                    serde_json::from_value::<Snapshot>(value).map_err(|error| error.to_string())
                });
            this.update(cx, |page, cx| {
                page.busy = false;
                page.freeze_editor(false, cx);
                match result {
                    Ok(snapshot) => {
                        page.selected_driver = page
                            .selected_driver
                            .min(snapshot.drivers.len().saturating_sub(1));
                        page.snapshot = Some(snapshot);
                        if close_editor {
                            page.editor = None;
                        }
                        crate::pickers::bump_harness_catalog(cx);
                    }
                    Err(error) => page.error = Some(if matches!(method, "T3ProviderUpsert" | "T3ProviderRemove" | "T3ProviderUpdate") {
                        format!("{error} Check the provider state before retrying; the request may have reached T3.")
                    } else { error }),
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn freeze_editor(&mut self, busy: bool, cx: &mut Context<Self>) {
        if let Some(editor) = &self.editor {
            for input in std::iter::once(&editor.id)
                .chain(std::iter::once(&editor.name))
                .chain(std::iter::once(&editor.model))
                .chain(
                    editor
                        .fields
                        .iter()
                        .filter_map(|field| field.input.as_ref()),
                )
                .chain(editor.config.iter())
            {
                input.update(cx, |input, cx| {
                    input.read_only = busy;
                    cx.notify();
                });
            }
            editor.id.update(cx, |input, cx| {
                input.read_only = busy || editor.original.is_some();
                cx.notify();
            });
        }
    }

    fn edit(&mut self, row: Option<Instance>, cx: &mut Context<Self>) {
        if self.busy || self.editor.is_some() {
            return;
        }
        let Some(snapshot) = &self.snapshot else {
            return;
        };
        let driver = if let Some(row) = &row {
            snapshot
                .drivers
                .iter()
                .find(|driver| row.instance["driver"] == driver.id)
        } else {
            snapshot.drivers.get(self.selected_driver)
        };
        let Some(driver) = driver.cloned() else {
            return;
        };
        let instance = row
            .as_ref()
            .map(|row| row.instance.clone())
            .unwrap_or_else(|| json!({"driver":driver.id,"enabled":false,"config":{}}));
        let id = text_input(
            "Instance ID",
            row.as_ref()
                .map(|row| row.instance_id.as_str())
                .unwrap_or(""),
            false,
            cx,
        );
        if row.is_some() {
            id.update(cx, |input, _| input.read_only = true);
        }
        let name = text_input(
            "Display name",
            instance["displayName"].as_str().unwrap_or(""),
            false,
            cx,
        );
        let fields = driver
            .fields
            .iter()
            .map(|field| EditorField {
                input: (field.control == "text").then(|| {
                    text_input(
                        &field.label,
                        instance["config"][&field.key].as_str().unwrap_or(""),
                        false,
                        cx,
                    )
                }),
                field: field.clone(),
                select: Default::default(),
            })
            .collect();
        let advanced = (!driver.known).then(|| {
            text_input(
                "Driver configuration (JSON)",
                &serde_json::to_string_pretty(&instance["config"]).unwrap_or_else(|_| "{}".into()),
                true,
                cx,
            )
        });
        let model = text_input("Model slug", "", false, cx);
        self.editor = Some(Editor {
            original: row,
            instance,
            driver,
            id,
            name,
            fields,
            config: advanced,
            model,
            editing_model: None,
            confirm_remove: false,
            show_models: false,
        });
        self.error = None;
        cx.notify();
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let result = self.editor_params(cx);
        match result {
            Ok(params) => self.request("T3ProviderUpsert", params, true, cx),
            Err(error) => {
                self.error = Some(error);
                cx.notify();
            }
        }
    }

    fn editor_params(&self, cx: &Context<Self>) -> Result<Value, String> {
        let editor = self.editor.as_ref().ok_or("No provider is being edited")?;
        let id = editor.id.read(cx).text().trim();
        let mut instance = editor.instance.clone();
        let name = editor.name.read(cx).text().trim();
        if name.is_empty() {
            instance.as_object_mut().unwrap().remove("displayName");
        } else {
            instance["displayName"] = json!(name);
        }
        if let Some(input) = &editor.config {
            let parsed: Value = serde_json::from_str(input.read(cx).text())
                .map_err(|error| format!("Invalid driver configuration: {error}"))?;
            if parsed != editor.instance["config"] {
                instance["config"] = parsed;
            }
        }
        let local_acp =
            instance["driver"] == "acpRegistry" && instance["config"]["source"] == "local";
        for field in &editor.fields {
            if local_acp && field.field.key != "source" && field.field.key != "commandPath" {
                continue;
            }
            if let Some(input) = &field.input {
                let value = input.read(cx).text().trim();
                // Unchanged empty/default values must not clear unseen driver state.
                if value
                    == editor.instance["config"][&field.field.key]
                        .as_str()
                        .unwrap_or("")
                {
                    continue;
                }
                if value.is_empty()
                    && !matches!(
                        (editor.driver.id.as_str(), field.field.key.as_str()),
                        ("antigravity", "binaryPath") | ("acpRegistry", "agentId")
                    )
                {
                    config(&mut instance).remove(&field.field.key);
                } else {
                    config(&mut instance).insert(field.field.key.clone(), json!(value));
                }
            }
        }
        if !editor.model.read(cx).text().trim().is_empty() {
            return Err("Add or update the model draft before saving the provider.".into());
        }
        Ok(
            json!({"instanceId":id,"instance":instance,"expectedInstance":editor.original.as_ref().map(|row| &row.instance)}),
        )
    }

    fn save_model(&mut self, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let Some(editor) = &mut self.editor else {
            return;
        };
        let slug = editor.model.read(cx).text().trim().to_owned();
        let mut models = editor.instance["config"]["customModels"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        match replace_model(&mut models, editor.editing_model, &slug) {
            Ok(()) => {
                config(&mut editor.instance).insert("customModels".into(), json!(models));
                editor.editing_model = None;
                editor.model.update(cx, |input, cx| input.set_text("", cx));
                self.error = None;
            }
            Err(error) => self.error = Some(error),
        }
        cx.notify();
    }

    fn remove(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = &self.editor else {
            return;
        };
        let Some(row) = &editor.original else {
            return;
        };
        let params = json!({"instanceId":row.instance_id,"expectedInstance":row.instance});
        self.request("T3ProviderRemove", params, true, cx);
    }

    fn render_editor(&mut self, theme: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        let editor = self.editor.as_ref()?;
        let active = !self.busy;
        let mut fields = vec![
            input_row(theme, "Instance ID", &editor.id),
            input_row(theme, "Display name", &editor.name),
        ];
        for (index, field) in editor.fields.iter().enumerate() {
            let local_acp =
                editor.driver.id == "acpRegistry" && editor.instance["config"]["source"] == "local";
            if local_acp && field.field.key != "source" && field.field.key != "commandPath" {
                continue;
            }
            let key = field.field.key.clone();
            let control = if let Some(input) = &field.input {
                input_row(theme, &field.field.label, input)
            } else if field.field.control == "select" {
                let value = editor.instance["config"][&key].as_str().unwrap_or_else(|| {
                    if key == "source" {
                        "registry"
                    } else {
                        "oauth-personal"
                    }
                });
                let selected = field
                    .field
                    .options
                    .iter()
                    .position(|option| option.value == value)
                    .unwrap_or(0);
                let options = field.field.options.clone();
                let select = widgets::select(
                    format!("t3-provider-field-{key}"),
                    field.field.label.clone(),
                    theme,
                    move |page: &mut Self| &mut page.editor.as_mut().unwrap().fields[index].select,
                )
                .options(
                    options
                        .iter()
                        .map(|option| widgets::SelectOption::new(option.label.clone())),
                    selected,
                )
                .on_select(move |page, selected, _, cx| {
                    if page.busy {
                        return;
                    }
                    if let Some(editor) = &mut page.editor {
                        if let Some(option) = options.get(selected) {
                            config(&mut editor.instance).insert(key.clone(), json!(option.value));
                        }
                    }
                    cx.notify();
                })
                .width(240.0)
                .render(&field.select, cx);
                div()
                    .flex()
                    .flex_col()
                    .gap(px(6.0))
                    .mt(px(12.0))
                    .child(widgets::field_label(theme, field.field.label.clone()))
                    .child(select)
                    .into_any_element()
            } else {
                let paste_field = field.field.clone();
                let clear_field = field.field.clone();
                let configured = secret_configured(&editor.instance, &field.field);
                div()
                    .mt(px(12.0))
                    .child(widgets::field_label(theme, field.field.label.clone()))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .flex_wrap()
                            .gap(px(8.0))
                            .child(widgets::badge(
                                theme,
                                if configured {
                                    "Configured"
                                } else {
                                    "Not configured"
                                },
                            ))
                            .child(button(
                                format!("t3-provider-paste-{key}"),
                                "Paste replacement",
                                active,
                                theme,
                                cx,
                                move |page, cx| {
                                    let Some(value) =
                                        cx.read_from_clipboard().and_then(|item| item.text())
                                    else {
                                        page.error =
                                            Some("Copy a value to the clipboard first.".into());
                                        cx.notify();
                                        return;
                                    };
                                    if value.trim().is_empty() {
                                        page.error = Some("The clipboard value is empty.".into());
                                        cx.notify();
                                        return;
                                    }
                                    if let Some(editor) = &mut page.editor {
                                        set_secret(
                                            &mut editor.instance,
                                            &paste_field,
                                            Some(value.trim()),
                                        );
                                    }
                                    cx.notify();
                                },
                            ))
                            .child(button(
                                format!("t3-provider-clear-{key}"),
                                "Clear",
                                active && configured,
                                theme,
                                cx,
                                move |page, cx| {
                                    if let Some(editor) = &mut page.editor {
                                        set_secret(&mut editor.instance, &clear_field, None);
                                    }
                                    cx.notify();
                                },
                            )),
                    )
                    .into_any_element()
            };
            fields.push(control);
        }
        if let Some(input) = &editor.config {
            fields.push(input_row(theme, "Driver configuration (JSON)", input));
        }
        let enabled = editor.instance["enabled"]
            .as_bool()
            .unwrap_or_else(|| editor.original.as_ref().is_some_and(|row| row.enabled));
        let enabled_control = widgets::toggle_switch(theme, enabled, "t3-provider-editor-enabled");
        let enable = button(
            "t3-provider-editor-toggle",
            if enabled {
                "Disable provider"
            } else {
                "Enable provider"
            },
            active,
            theme,
            cx,
            |page, cx| {
                if let Some(editor) = &mut page.editor {
                    let enabled = editor.instance["enabled"]
                        .as_bool()
                        .unwrap_or_else(|| editor.original.as_ref().is_some_and(|row| row.enabled));
                    editor.instance = set_enabled(&editor.instance, !enabled);
                }
                cx.notify();
            },
        );
        let mut content = div()
            .mt(px(24.0))
            .child(widgets::section_label(
                theme,
                format!("{} instance", editor.driver.label),
            ))
            .child(
                div().px(px(8.0)).children(fields).child(
                    div()
                        .mt(px(16.0))
                        .flex()
                        .items_center()
                        .gap(px(8.0))
                        .child(enabled_control)
                        .child(enable),
                ),
            );
        if let Some(models) = editor
            .original
            .as_ref()
            .and_then(|row| row.live.as_ref())
            .and_then(|live| live["models"].as_array())
        {
            content = content.child(button(
                "t3-provider-model-catalog",
                format!(
                    "{} available models · {}",
                    models.len(),
                    if editor.show_models { "Hide" } else { "Show" }
                ),
                active,
                theme,
                cx,
                |page, cx| {
                    if let Some(editor) = &mut page.editor {
                        editor.show_models = !editor.show_models;
                    }
                    cx.notify();
                },
            ));
            if editor.show_models {
                content = content.child(widgets::section_card(theme).children(
                    models.iter().enumerate().map(|(index, model)| {
                        widgets::card_row(theme, index == 0).child(
                            div()
                                .min_w_0()
                                .child(widgets::row_title(
                                    theme,
                                    model["name"].as_str().unwrap_or("Unnamed model").to_owned(),
                                ))
                                .child(widgets::meta_line(
                                    theme,
                                    vec![
                                        div()
                                            .child(SharedString::from(
                                                model["slug"].as_str().unwrap_or("").to_owned(),
                                            ))
                                            .into_any_element(),
                                    ],
                                )),
                        )
                    }),
                ));
            }
        }
        if editor.driver.known && editor.driver.id != "antigravity" {
            let models = editor.instance["config"]["customModels"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            let mut model_rows = Vec::new();
            for (index, model) in models.iter().enumerate() {
                let slug = model_slug(model).to_owned();
                let label = model["name"].as_str().unwrap_or(&slug).to_owned();
                model_rows.push(
                    widgets::card_row(theme, index == 0)
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .child(widgets::row_title(theme, label))
                                .child(widgets::meta_line(
                                    theme,
                                    vec![
                                        div()
                                            .child(SharedString::from(slug.clone()))
                                            .into_any_element(),
                                    ],
                                )),
                        )
                        .child(button(
                            format!("t3-model-edit-{index}"),
                            "Edit",
                            active && editor.editing_model.is_none(),
                            theme,
                            cx,
                            move |page, cx| {
                                if let Some(editor) = &mut page.editor {
                                    editor.editing_model = Some(index);
                                    editor
                                        .model
                                        .update(cx, |input, cx| input.set_text(slug.clone(), cx));
                                }
                                cx.notify();
                            },
                        ))
                        .child(button(
                            format!("t3-model-remove-{index}"),
                            "Remove",
                            active && editor.editing_model.is_none(),
                            theme,
                            cx,
                            move |page, cx| {
                                if let Some(editor) = &mut page.editor {
                                    if let Some(models) = config(&mut editor.instance)
                                        .get_mut("customModels")
                                        .and_then(Value::as_array_mut)
                                    {
                                        if index < models.len() {
                                            models.remove(index);
                                        }
                                    }
                                }
                                cx.notify();
                            },
                        ))
                        .into_any_element(),
                );
            }
            content = content
                .child(widgets::section_label(theme, "Custom models"))
                .when(!model_rows.is_empty(), |el| {
                    el.child(widgets::section_card(theme).children(model_rows))
                })
                .child(
                    div()
                        .px(px(8.0))
                        .child(input_row(theme, "Model slug", &editor.model))
                        .child(
                            div()
                                .mt(px(8.0))
                                .flex()
                                .gap(px(8.0))
                                .child(button(
                                    "t3-model-save",
                                    if editor.editing_model.is_some() {
                                        "Update model"
                                    } else {
                                        "Add model"
                                    },
                                    active,
                                    theme,
                                    cx,
                                    |page, cx| page.save_model(cx),
                                ))
                                .when(editor.editing_model.is_some(), |el| {
                                    el.child(button(
                                        "t3-model-cancel",
                                        "Cancel model edit",
                                        active,
                                        theme,
                                        cx,
                                        |page, cx| {
                                            if let Some(editor) = &mut page.editor {
                                                editor.editing_model = None;
                                                editor
                                                    .model
                                                    .update(cx, |input, cx| input.set_text("", cx));
                                            }
                                            cx.notify();
                                        },
                                    ))
                                }),
                        ),
                );
        }
        let removable = editor.original.as_ref().is_some_and(|row| row.removable);
        let confirm_remove = editor.confirm_remove;
        content = content.child(
            div()
                .px(px(8.0))
                .mt(px(20.0))
                .flex()
                .items_center()
                .flex_wrap()
                .gap(px(8.0))
                .child(button(
                    "t3-provider-save",
                    if self.busy {
                        "Working…"
                    } else {
                        "Save provider"
                    },
                    active,
                    theme,
                    cx,
                    |page, cx| page.save(cx),
                ))
                .child(button(
                    "t3-provider-cancel",
                    "Cancel",
                    active,
                    theme,
                    cx,
                    |page, cx| {
                        page.editor = None;
                        page.error = None;
                        cx.notify();
                    },
                ))
                .when(self.error.is_some(), |el| {
                    el.child(button(
                        "t3-provider-reload",
                        "Discard draft and reload",
                        active,
                        theme,
                        cx,
                        |page, cx| {
                            page.editor = None;
                            page.request("T3ProvidersGet", json!({}), false, cx);
                        },
                    ))
                })
                .when(removable && !confirm_remove, |el| {
                    el.child(button(
                        "t3-provider-remove",
                        "Remove instance",
                        active,
                        theme,
                        cx,
                        |page, cx| {
                            if let Some(editor) = &mut page.editor {
                                editor.confirm_remove = true;
                            }
                            cx.notify();
                        },
                    ))
                }),
        );
        if confirm_remove {
            content = content.child(widgets::warning_strip(theme, "Remove this provider instance? Existing threads keep their history; this instance will no longer be available."))
                .child(div().flex().gap(px(8.0))
                    .child(button("t3-provider-confirm-remove", "Remove instance", active, theme, cx, |page, cx| page.remove(cx)))
                    .child(button("t3-provider-keep", "Keep instance", active, theme, cx, |page, cx| {
                        if let Some(editor) = &mut page.editor { editor.confirm_remove = false; }
                        cx.notify();
                    })));
        }
        Some(content.into_any_element())
    }
}

fn input_row(theme: &Theme, label: &str, input: &Entity<ComposerInput>) -> AnyElement {
    div()
        .id(("t3-provider-input-group", input.entity_id()))
        .role(gpui::Role::Group)
        .aria_label(SharedString::from(label.to_owned()))
        .mt(px(12.0))
        .flex()
        .flex_col()
        .gap(px(6.0))
        .child(widgets::field_label(theme, label.to_owned()))
        .child(popover::dialog_field(input.clone().into_any_element()))
        .into_any_element()
}

impl Render for ProvidersPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).for_settings_surface();
        let active = !self.busy && self.editor.is_none();
        let rows: Vec<AnyElement> = self.snapshot.as_ref().map(|snapshot| snapshot.instances.iter().enumerate().map(|(index, row)| {
            let driver = row.instance["driver"].as_str().unwrap_or("Unknown provider");
            let name = row.instance["displayName"].as_str()
                .or_else(|| snapshot.drivers.iter().find(|item| item.id == driver).map(|item| item.label.as_str()))
                .unwrap_or(driver);
            let edit = row.clone();
            let toggle = row.clone();
            let update_id = row.instance_id.clone();
            let live = row.live.as_ref();
            let mut detail = div().flex_1().min_w_0()
                .child(widgets::row_title(&theme, name.to_owned()))
                .child(widgets::meta_line(&theme, vec![div().child(SharedString::from(row.instance_id.clone())).into_any_element()]))
                .child(div().text_size(crate::typography::ui_rems(12.0)).text_color(theme.text_muted).child(SharedString::from(status_line(row))));
            for key in ["message", "unavailableReason"] {
                if let Some(message) = live.and_then(|live| live[key].as_str()) { detail = detail.child(div().text_size(crate::typography::ui_rems(12.0)).text_color(theme.text_muted).child(SharedString::from(message.to_owned()))); }
            }
            if let Some(checked) = live.and_then(|live| live["checkedAt"].as_str()) { detail = detail.child(div().text_size(crate::typography::ui_rems(11.0)).text_color(theme.text_muted).child(SharedString::from(format!("Checked {checked}")))); }
            if let Some(update) = live.and_then(|live| live["updateState"]["message"].as_str()) { detail = detail.child(div().text_size(crate::typography::ui_rems(12.0)).text_color(theme.text_muted).child(SharedString::from(update.to_owned()))); }
            let update_busy = live.is_some_and(|live| matches!(live["updateState"]["status"].as_str(), Some("queued" | "running")));
            let can_update = live.and_then(|live| live["versionAdvisory"]["updateCommand"].as_str()).is_some();
            widgets::card_row(&theme, index == 0).flex_wrap()
                .child(widgets::row_tile(&theme, brand_icon(driver))).child(detail)
                .child(div().flex().items_center().gap(px(4.0)).flex_wrap()
                    .child(button(format!("t3-provider-edit-{}", row.instance_id), "Configure", active, &theme, cx, move |page, cx| page.edit(Some(edit.clone()), cx)))
                    .when(can_update, |el| el.child(button(format!("t3-provider-update-{}", row.instance_id), if update_busy {"Updating…"} else {"Update"}, active && !update_busy, &theme, cx, move |page, cx| page.request("T3ProviderUpdate", json!({"instanceId":update_id}), false, cx))))
                    .child(widgets::toggle_switch(&theme, row.enabled, row.instance_id.clone())
                        .id(SharedString::from(format!("t3-provider-toggle-{}", row.instance_id)))
                        .role(gpui::Role::Switch).tab_index(if active { 0 } else { -1 })
                        .aria_label(SharedString::from(format!("Enable {name}")))
                        .aria_toggled(if row.enabled { gpui::Toggled::True } else { gpui::Toggled::False })
                        .when(!active, |element| element.aria_description("Unavailable while another provider action or editor is open"))
                        .opacity(if active { 1.0 } else { 0.4 })
                        .focus_visible(|style| style.border_2().border_color(theme.accent))
                        .when(active, |el| {
                            let click = toggle.clone();
                            el.on_click(cx.listener(move |page, _, _, cx| page.request("T3ProviderUpsert", json!({"instanceId":click.instance_id,"instance":set_enabled(&click.instance,!click.enabled),"expectedInstance":click.instance}), false, cx)))
                                .on_key_down(cx.listener(move |page, event: &gpui::KeyDownEvent, _, cx| {
                                    if !event.is_held && matches!(event.keystroke.key.as_str(), "enter" | "space") { cx.stop_propagation(); page.request("T3ProviderUpsert", json!({"instanceId":toggle.instance_id,"instance":set_enabled(&toggle.instance,!toggle.enabled),"expectedInstance":toggle.instance}), false, cx); }
                                }))
                        })))
                .into_any_element()
        }).collect()).unwrap_or_default();
        let mut body = widgets::page_column()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .flex_wrap()
                    .gap(px(8.0))
                    .child(widgets::page_header(
                        &theme,
                        "Providers",
                        self.snapshot
                            .as_ref()
                            .map(|snapshot| snapshot.instances.len()),
                    ))
                    .child(button(
                        "t3-providers-refresh",
                        if self.busy {
                            "Working…"
                        } else if self.error.is_some() {
                            "Retry / refresh"
                        } else {
                            "Refresh"
                        },
                        active,
                        &theme,
                        cx,
                        |page, cx| page.request("T3ProvidersRefresh", json!({}), false, cx),
                    )),
            )
            .child(widgets::page_subtitle(
                &theme,
                "Agent runtimes and models on this T3 environment.",
            ));
        if let Some(error) = &self.error {
            body = body.child(widgets::error_strip(&theme, error.clone()));
        }
        if self.snapshot.is_none() && self.busy {
            body = body.child(widgets::section_card(&theme).p(px(16.0)).child(
                popover::skeleton_rows("t3-providers-loading", &theme, 4, cx.entity_id(), cx),
            ));
        }
        if let Some(snapshot) = &self.snapshot {
            body = body
                .child(widgets::section_label(&theme, "Provider instances"))
                .child(widgets::section_card(&theme).children(rows));
            if snapshot.instances.is_empty() {
                body = body.child(widgets::page_subtitle(
                    &theme,
                    "No provider instances are configured. Add one below.",
                ));
            }
            if self.editor.is_none() && !snapshot.drivers.is_empty() {
                let options: Vec<_> = snapshot
                    .drivers
                    .iter()
                    .map(|driver| widgets::SelectOption::new(driver.label.clone()))
                    .collect();
                let select = widgets::select(
                    "t3-provider-driver",
                    "Provider driver",
                    &theme,
                    |page: &mut Self| &mut page.driver_select,
                )
                .options(options, self.selected_driver)
                .on_select(|page, index, _, cx| {
                    if !page.busy {
                        page.selected_driver = index;
                        cx.notify();
                    }
                })
                .width(220.0)
                .render(&self.driver_select, cx);
                body = body.child(
                    div()
                        .mt(px(20.0))
                        .px(px(8.0))
                        .flex()
                        .items_center()
                        .flex_wrap()
                        .gap(px(8.0))
                        .child(select)
                        .child(button(
                            "t3-provider-add",
                            "Add instance",
                            active,
                            &theme,
                            cx,
                            |page, cx| page.edit(None, cx),
                        )),
                );
            }
        }
        if let Some(editor) = self.render_editor(&theme, cx) {
            body = body.child(editor);
        }
        let rail = widgets::rail(
            &mut self.scroll,
            "t3-providers-scrollbar",
            &theme,
            cx,
            |page| &mut page.scroll,
        );
        div()
            .id("t3-providers-page-host")
            .relative()
            .size_full()
            .on_hover(cx.listener(|page, hovered: &bool, _, cx| {
                if page.scroll.set_list_hovered(*hovered) {
                    cx.notify();
                }
            }))
            .child(
                crate::edge_fade::edge_faded(
                    16.0,
                    true,
                    true,
                    div()
                        .id("t3-providers-page")
                        .size_full()
                        .overflow_y_scroll()
                        .track_scroll(&self.scroll.scroll)
                        .child(body),
                )
                .fade_overflow_y(&self.scroll.scroll),
            )
            .children(rail)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toggles_update_both_flags_without_losing_custom_config_or_environment() {
        let original = json!({"driver":"pi","config":{"enabled":false,"launchArgs":"--provider custom","future":[1,2]},"environment":[{"name":"TOKEN","value":"","valueRedacted":true,"sensitive":true}]});
        let changed = set_enabled(&original, true);
        assert_eq!(changed["enabled"], true);
        assert_eq!(changed["config"]["enabled"], true);
        assert_eq!(changed["config"]["future"], original["config"]["future"]);
        assert_eq!(changed["environment"], original["environment"]);
        assert_eq!(original["config"]["enabled"], false);
    }

    #[test]
    fn model_edits_keep_capabilities_and_reject_duplicate_slugs() {
        let mut models = vec![
            json!({"slug":"old","name":"My model","capabilities":{"optionDescriptors":[{"key":"thinking"}]}}),
            json!("other"),
        ];
        let metadata = models[0]["capabilities"].clone();
        replace_model(&mut models, Some(0), " new ").unwrap();
        assert_eq!(models[0]["slug"], "new");
        assert_eq!(models[0]["capabilities"], metadata);
        assert_eq!(models[0]["name"], "My model");
        assert!(replace_model(&mut models, Some(0), "other").is_err());
        assert!(replace_model(&mut models, None, "").is_err());
        assert!(replace_model(&mut models, Some(9), "newer").is_err());
        replace_model(&mut models, None, "third").unwrap();
        assert_eq!(models[2], "third");
    }

    #[test]
    fn cursor_secret_edits_preserve_other_redacted_environment_variables() {
        let field = Field {
            key: "CURSOR_API_KEY".into(),
            label: "Cursor API key".into(),
            control: "environmentPassword".into(),
            options: vec![],
        };
        let other = json!({"name":"OTHER_TOKEN","value":"","sensitive":true,"valueRedacted":true});
        let mut instance = json!({"driver":"cursor","environment":[other,{"name":"CURSOR_API_KEY","value":"","sensitive":true,"valueRedacted":true}]});
        assert!(secret_configured(&instance, &field));
        set_secret(&mut instance, &field, Some("replacement"));
        assert_eq!(instance["environment"][0], other);
        assert_eq!(
            instance["environment"][1],
            json!({"name":"CURSOR_API_KEY","value":"replacement","sensitive":true})
        );
        set_secret(&mut instance, &field, None);
        assert!(!secret_configured(&instance, &field));
        assert_eq!(instance["environment"], json!([other]));
    }
}
