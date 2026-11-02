use egui::{
    Context, Event, Id, ImeEvent, Key, Modifiers, RawInput, Rect, TextEdit, ViewportBuilder,
    ViewportCommand, ViewportId,
    output::{IMESurroundingText, ImeChangeCause},
    pos2, vec2,
};

struct Field {
    ctx: Context,
    id: Id,
    text: String,
    password: bool,
    time: f64,
    discard_once: bool,
}

impl Field {
    fn new(text: &str) -> Self {
        Self {
            ctx: Context::default(),
            id: Id::unique("surrounding-text-field"),
            text: text.to_owned(),
            password: false,
            time: 0.0,
            discard_once: false,
        }
    }

    fn frame(&mut self, events: Vec<Event>) -> egui::output::IMEOutput {
        self.time += 0.1;
        let id = self.id;
        let password = self.password;
        let text = &mut self.text;
        let mut discard_once = core::mem::take(&mut self.discard_once);
        let mut output = self.ctx.run_ui(
            RawInput {
                screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(500.0, 300.0))),
                time: Some(self.time),
                events,
                ..Default::default()
            },
            |ui| {
                ui.memory_mut(|mem| mem.request_focus(id));
                let mut edit = TextEdit::singleline(text).id(id).cursor_at_end(true);
                if password {
                    edit = edit.password(true);
                }
                ui.add(edit);
                if core::mem::take(&mut discard_once) {
                    ui.ctx()
                        .request_discard("retain an IME interrupt across a discarded pass");
                }
            },
        );
        output.textures_delta.clear();
        output
            .platform_output
            .ime
            .expect("the focused editable field emits an IME descriptor")
    }

    fn payload(ime: &egui::output::IMEOutput) -> &egui::output::IMESurroundingTextPayload {
        ime.surrounding_text
            .available()
            .expect("this field provides surrounding text")
    }
}

fn commit(text: &str) -> Event {
    Event::Ime(ImeEvent::Commit(text.to_owned()))
}

fn preedit(text: &str) -> Event {
    let end = text.chars().count();
    Event::Ime(ImeEvent::Preedit {
        text: text.to_owned(),
        active_range_chars: (end > 0).then_some(end..end),
    })
}

#[test]
fn accessible_whole_value_replacement_discards_the_old_composition_range() {
    let mut field = Field::new("prefix");
    field.frame(vec![]);
    field.frame(vec![preedit("compose")]);
    assert_eq!(field.text, "prefixcompose");
    field.discard_once = true;
    let output = field.frame(vec![Event::AccessKitActionRequest(
        egui::accesskit::ActionRequest {
            action: egui::accesskit::Action::SetValue,
            target_node: field.id.accesskit_id(),
            target_tree: egui::accesskit::TreeId::ROOT,
            data: Some(egui::accesskit::ActionData::Value(
                "replacement text".into(),
            )),
        },
    )]);
    assert_eq!(field.text, "replacement text");
    assert_eq!(Field::payload(&output).text, field.text);
    assert_eq!(Field::payload(&output).cursor, field.text.len());
    assert!(output.should_interrupt_composition);
    let next = field.frame(vec![]);
    assert_eq!(Field::payload(&next).text, "replacement text");
}

#[test]
fn external_buffer_edit_invalidates_a_shifted_preedit_range() {
    let mut field = Field::new("ab");
    field.frame(vec![]);
    field.frame(vec![preedit("に")]);
    assert_eq!(field.text, "abに");
    field.text.insert_str(0, "00");
    let output = field.frame(vec![]);
    assert_eq!(Field::payload(&output).text, "00abに");
    assert_eq!(output.change_cause, ImeChangeCause::Other);
    assert!(output.should_interrupt_composition);
}

#[test]
fn hidden_editor_with_stale_preedit_does_not_interrupt_the_focused_editor() {
    let ctx = Context::default();
    let a = Id::unique("hidden-editor-a");
    let b = Id::unique("focused-editor-b");
    let mut text_a = "a".to_owned();
    let mut text_b = "b".to_owned();

    let input = |time, events| RawInput {
        screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(500.0, 300.0))),
        time: Some(time),
        events,
        ..Default::default()
    };

    // A owns an active composition.
    let mut first = ctx.run_ui(input(0.0, vec![preedit("に")]), |ui| {
        ui.memory_mut(|mem| mem.request_focus(a));
        ui.add(TextEdit::singleline(&mut text_a).id(a).cursor_at_end(true));
    });
    first.textures_delta.clear();
    assert_eq!(text_a, "aに");

    // Hide A before its focus-loss reset can run, then give B focus and a composition.
    let mut hidden = ctx.run_ui(input(0.1, vec![]), |ui| {
        ui.memory_mut(|mem| mem.request_focus(b));
        ui.add(TextEdit::singleline(&mut text_b).id(b).cursor_at_end(true));
    });
    hidden.textures_delta.clear();
    let mut composed = ctx.run_ui(input(0.2, vec![preedit("x")]), |ui| {
        ui.memory_mut(|mem| mem.request_focus(b));
        ui.add(TextEdit::singleline(&mut text_b).id(b).cursor_at_end(true));
    });
    composed.textures_delta.clear();
    assert_eq!(
        composed
            .platform_output
            .ime
            .as_ref()
            .expect("B owns the focused IME")
            .owner,
        b
    );

    // Re-show A with a changed buffer. This `TextEdit` no longer owns IME, so its stale
    // pre-edit must be discarded without interrupting B's viewport-global composition.
    text_a.insert_str(0, "00");
    let mut reshown = ctx.run_ui(input(0.3, vec![]), |ui| {
        ui.memory_mut(|mem| mem.request_focus(b));
        ui.add(TextEdit::singleline(&mut text_a).id(a).cursor_at_end(true));
        ui.add(TextEdit::singleline(&mut text_b).id(b).cursor_at_end(true));
    });
    reshown.textures_delta.clear();

    let ime = reshown
        .platform_output
        .ime
        .expect("B still owns the focused IME");
    assert_eq!(ime.owner, b);
    assert!(
        !ime.should_interrupt_composition,
        "a non-owning hidden editor must not cancel the focused editor's composition"
    );
}

#[test]
fn immediate_viewport_receives_its_own_ime_command_before_platform_output() {
    let ctx = Context::default();
    ctx.set_embed_viewports(false);
    let child_viewport = ViewportId::from_hash_of("ime-immediate-child");
    let recorded = std::sync::Arc::new(egui::epaint::mutex::Mutex::new(None));

    Context::set_immediate_viewport_renderer({
        let recorded = std::sync::Arc::clone(&recorded);
        move |ctx, immediate| {
            let ids = immediate.ids;
            let mut viewport_ui_cb = immediate.viewport_ui_cb;
            let child_editor = Id::unique("ime-immediate-editor");
            let mut child_text = "child text".to_owned();
            let mut output = ctx.run_ui(
                RawInput {
                    viewport_id: ids.this,
                    viewports: core::iter::once((ids.this, egui::ViewportInfo::default()))
                        .collect(),
                    screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(500.0, 300.0))),
                    ..Default::default()
                },
                |ui| {
                    ui.memory_mut(|mem| mem.request_focus(child_editor));
                    ui.ctx()
                        .send_viewport_cmd(ViewportCommand::IMEAllowed(false));
                    ui.add(
                        TextEdit::singleline(&mut child_text)
                            .id(child_editor)
                            .cursor_at_end(true),
                    );
                    viewport_ui_cb(ui);
                },
            );
            output.textures_delta.clear();

            let child_commands = output
                .viewport_output
                .get(&ids.this)
                .map(|viewport| viewport.commands.clone())
                .unwrap_or_default();
            let mut order = Vec::new();
            if child_commands
                .iter()
                .any(|command| matches!(command, ViewportCommand::IMEAllowed(false)))
            {
                order.push("child WindowDisable");
            }
            if output.platform_output.ime.is_some() {
                order.push("child platform IME output");
            }
            *recorded.lock() = Some((child_commands, order));
        }
    });

    let mut parent_output = ctx.run_ui(
        RawInput {
            screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(500.0, 300.0))),
            ..Default::default()
        },
        |ui| {
            ui.ctx()
                .send_viewport_cmd(ViewportCommand::IMEAllowed(true));
            ui.show_viewport_immediate(child_viewport, ViewportBuilder::default(), |ui, _class| {
                ui.label("immediate child");
            });
        },
    );
    parent_output.textures_delta.clear();

    let (child_commands, order) = recorded
        .lock()
        .take()
        .expect("the real show_viewport_immediate renderer ran");
    assert_eq!(
        child_commands
            .iter()
            .filter(|command| matches!(command, ViewportCommand::IMEAllowed(_)))
            .collect::<Vec<_>>(),
        vec![&ViewportCommand::IMEAllowed(false)],
        "the immediate child must receive its own command from its child pass"
    );
    assert_eq!(
        order,
        ["child WindowDisable", "child platform IME output"],
        "the child command is available to the integration before its platform output"
    );

    let child_output = parent_output
        .viewport_output
        .get(&child_viewport)
        .expect("child viewport output");
    assert!(
        child_output.commands.is_empty(),
        "the child command must not be duplicated in the parent pass"
    );
    let root_output = parent_output
        .viewport_output
        .get(&ViewportId::ROOT)
        .expect("root viewport output");
    assert!(
        root_output
            .commands
            .contains(&ViewportCommand::IMEAllowed(true)),
        "an unrelated parent command stays in the parent's queue"
    );
    assert!(
        !root_output
            .commands
            .contains(&ViewportCommand::IMEAllowed(false)),
        "the child command must not be stolen into the parent's queue"
    );
}

#[test]
fn accessible_selection_replacement_interrupts_the_old_composition() {
    let mut field = Field::new("prefix");
    field.frame(vec![]);
    field.frame(vec![preedit("compose")]);
    let output = field.frame(vec![Event::AccessKitActionRequest(
        egui::accesskit::ActionRequest {
            action: egui::accesskit::Action::ReplaceSelectedText,
            target_node: field.id.accesskit_id(),
            target_tree: egui::accesskit::TreeId::ROOT,
            data: Some(egui::accesskit::ActionData::Value("replacement".into())),
        },
    )]);
    assert_eq!(field.text, "prefixreplacement");
    assert_eq!(Field::payload(&output).text, field.text);
    assert_eq!(output.change_cause, ImeChangeCause::Other);
    assert!(output.should_interrupt_composition);
}

#[test]
fn replacing_text_edit_state_cannot_reuse_a_live_ime_context() {
    let mut field = Field::new("same text");
    let first = field.frame(vec![]);
    egui::text_edit::TextEditState::default().store(&field.ctx, field.id);
    let recreated = field.frame(vec![]);
    assert_eq!(recreated.owner, first.owner);
    assert_ne!(recreated.context_generation, first.context_generation);
    assert_eq!(recreated.change_cause, ImeChangeCause::Other);
}

#[test]
fn password_to_normal_starts_a_new_available_context() {
    let mut field = Field::new("private value");
    field.password = true;
    let password = field.frame(vec![]);
    assert!(password.surrounding_text.available().is_none());
    field.password = false;
    let normal = field.frame(vec![]);
    assert!(normal.context_generation > password.context_generation);
    assert_eq!(Field::payload(&normal).text, "private value");
    assert_eq!(
        field.frame(vec![]).context_generation,
        normal.context_generation
    );
}

#[test]
fn discarded_layout_passes_keep_one_focused_context_generation() {
    let mut field = Field::new("same focused editor");
    let first = field.frame(vec![]);
    let mut generations = Vec::new();
    let mut output = field.ctx.run_ui(RawInput::default(), |ui| {
        ui.add(TextEdit::singleline(&mut field.text).id(field.id));
        generations.push(ui.output(|output| {
            output
                .ime
                .as_ref()
                .expect("focused field")
                .context_generation
        }));
        if generations.len() == 1 {
            ui.ctx()
                .request_discard("exercise the same editor in another layout pass");
        }
    });
    output.textures_delta.clear();
    assert_eq!(
        generations,
        [first.context_generation, first.context_generation]
    );
    assert_eq!(
        output
            .platform_output
            .ime
            .expect("final descriptor")
            .context_generation,
        first.context_generation
    );
}

#[test]
fn multibyte_window_uses_the_full_bound_without_splitting_code_points() {
    let text = "é".repeat(3000);
    let surrounding = IMESurroundingText::from_text(&text, 3000, 3000, None);
    let payload = surrounding.available().expect("valid UTF-8 selection");
    assert_eq!(payload.text, "é".repeat(2000));
    assert_eq!(
        payload.text.len(),
        egui::output::MAX_IME_SURROUNDING_TEXT_BYTES
    );
    assert_eq!((payload.cursor, payload.anchor), (2000, 2000));
    assert_eq!(&payload.text[..payload.cursor], "é".repeat(1000));
}

#[test]
fn empty_editor_emits_available_empty_text_with_stable_identity() {
    let mut field = Field::new("");
    let first = field.frame(vec![]);
    assert_eq!(first.owner, field.id);
    assert!(first.context_generation > 0);
    let payload = Field::payload(&first);
    assert_eq!(payload.text, "");
    assert_eq!((payload.cursor, payload.anchor), (0, 0));

    let second = field.frame(vec![]);
    assert_eq!(second.owner, first.owner);
    assert_eq!(second.context_generation, first.context_generation);
}

#[test]
fn hiding_a_focused_editor_then_refocusing_starts_a_new_context() {
    let mut field = Field::new("retained document");
    let first = field.frame(vec![]);
    let mut hidden = field.ctx.run_ui(RawInput::default(), |ui| {
        ui.label("The editor is in an inactive tab");
    });
    hidden.textures_delta.clear();
    assert!(hidden.platform_output.ime.is_none());
    assert!(!field.ctx.memory(|memory| memory.has_focus(field.id)));
    let refocused = field.frame(vec![]);
    assert_eq!(refocused.owner, first.owner);
    assert!(refocused.context_generation > first.context_generation);
    assert_eq!(Field::payload(&refocused).text, "retained document");
}

#[test]
fn stopping_text_input_while_the_editor_is_absent_starts_a_new_context() {
    let mut field = Field::new("document");
    let first = field.frame(vec![]);
    field.ctx.memory_mut(|memory| memory.stop_text_input());
    let mut hidden = field.ctx.run_ui(RawInput::default(), |ui| {
        ui.label("No editor");
    });
    hidden.textures_delta.clear();
    assert!(hidden.platform_output.ime.is_none());
    let refocused = field.frame(vec![]);
    assert!(refocused.context_generation > first.context_generation);
}

#[test]
fn final_discard_pass_without_an_editor_clears_ime_output() {
    let mut field = Field::new("transient");
    field.frame(vec![]);
    let mut calls = 0;
    let mut output = field.ctx.run_ui(RawInput::default(), |ui| {
        calls += 1;
        if calls == 1 {
            ui.add(TextEdit::singleline(&mut field.text).id(field.id));
            assert!(ui.output(|output| output.ime.is_some()));
            ui.ctx()
                .request_discard("exercise a later pass without an editor");
        } else {
            ui.label("The final pass has no editor");
        }
    });
    output.textures_delta.clear();
    assert_eq!(calls, 2);
    assert!(output.platform_output.ime.is_none());
}

#[test]
fn payload_is_emitted_after_commit_and_unicode_offsets_are_bytes() {
    let mut field = Field::new("");
    let first = field.frame(vec![]);

    let after_commit = field.frame(vec![commit("é")]);
    assert_eq!(field.text, "é");
    assert_eq!(after_commit.owner, first.owner);
    assert_eq!(after_commit.context_generation, first.context_generation);
    let payload = Field::payload(&after_commit);
    assert_eq!(payload.text, "é");
    assert_eq!((payload.cursor, payload.anchor), (2, 2));
}

#[test]
fn delete_and_commit_are_reflected_in_the_same_pass() {
    let mut field = Field::new("éx");
    field.frame(vec![]);

    let output = field.frame(vec![
        Event::Ime(ImeEvent::DeleteSurroundingBytes {
            before_bytes: 1,
            after_bytes: 0,
        }),
        commit("y"),
    ]);
    assert_eq!(field.text, "éy");
    let payload = Field::payload(&output);
    assert_eq!(payload.text, "éy");
    assert_eq!((payload.cursor, payload.anchor), (3, 3));
}

#[test]
fn preedit_is_excluded_from_surrounding_text() {
    let mut field = Field::new("ab");
    field.frame(vec![]);

    let output = field.frame(vec![preedit("に")]);
    assert_eq!(field.text, "abに");
    let payload = Field::payload(&output);
    assert_eq!(payload.text, "ab");
    assert_eq!((payload.cursor, payload.anchor), (2, 2));
}

#[test]
fn committing_over_a_preedit_replaces_the_composition_in_the_payload() {
    let mut field = Field::new("ab");
    field.frame(vec![]);
    field.frame(vec![preedit("に")]);
    assert_eq!(field.text, "abに");

    let output = field.frame(vec![commit("日本")]);
    assert_eq!(field.text, "ab日本");
    let payload = Field::payload(&output);
    assert_eq!(payload.text, "ab日本");
    assert_eq!((payload.cursor, payload.anchor), (8, 8));
}

#[test]
fn reversed_selection_preserves_primary_and_anchor_bytes() {
    let mut field = Field::new("aé");
    field.frame(vec![]);

    let output = field.frame(vec![Event::Key {
        key: Key::ArrowLeft,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: Modifiers::SHIFT,
    }]);
    let payload = Field::payload(&output);
    assert_eq!(payload.text, "aé");
    // Shift+Left keeps the anchor at the end and moves the primary cursor left.
    assert_eq!((payload.cursor, payload.anchor), (1, 3));
    assert!(payload.cursor < payload.anchor, "the selection is reversed");
    assert_eq!(output.change_cause, ImeChangeCause::Other);
}

#[test]
fn cursor_movement_is_reflected_in_the_payload() {
    let mut field = Field::new("éx");
    field.frame(vec![]);

    let output = field.frame(vec![Event::Key {
        key: Key::ArrowLeft,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: Modifiers::NONE,
    }]);
    assert_eq!(field.text, "éx", "moving the cursor does not edit the text");
    let payload = Field::payload(&output);
    assert_eq!(payload.text, "éx");
    assert_eq!((payload.cursor, payload.anchor), (2, 2));
    assert_eq!(output.change_cause, ImeChangeCause::Other);
}

#[test]
fn deletion_only_batch_is_reflected_immediately() {
    let mut field = Field::new("éx");
    field.frame(vec![]);

    let output = field.frame(vec![Event::Ime(ImeEvent::DeleteSurroundingBytes {
        before_bytes: 1,
        after_bytes: 0,
    })]);
    assert_eq!(field.text, "é");
    let payload = Field::payload(&output);
    assert_eq!(payload.text, "é");
    assert_eq!((payload.cursor, payload.anchor), (2, 2));
    assert_eq!(output.change_cause, ImeChangeCause::InputMethod);
}

#[test]
fn a_selection_wider_than_the_byte_limit_is_unavailable_but_keeps_identity() {
    let mut field = Field::new(&"a".repeat(5000));
    let first = field.frame(vec![]);
    assert!(first.context_generation > 0);

    let output = field.frame(vec![Event::Key {
        key: Key::A,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: Modifiers::COMMAND,
    }]);
    assert_eq!(output.owner, field.id);
    assert_eq!(output.context_generation, first.context_generation);
    assert!(
        matches!(output.surrounding_text, IMESurroundingText::Unavailable),
        "a selection wider than the payload limit must not be truncated"
    );

    // Collapsing the selection makes the context available again.
    let output = field.frame(vec![Event::Key {
        key: Key::ArrowRight,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: Modifiers::NONE,
    }]);
    let payload = Field::payload(&output);
    assert!(payload.text.len() <= egui::output::MAX_IME_SURROUNDING_TEXT_BYTES);
    assert_eq!(payload.cursor, payload.anchor);
}

#[test]
fn changing_purpose_while_focused_starts_a_new_context_generation() {
    let mut field = Field::new("x");
    let first = field.frame(vec![]);
    let generation = first.context_generation;

    field.password = true;
    let password = field.frame(vec![]);
    assert!(password.context_generation > generation);
    assert_eq!(password.purpose, egui::IMEPurpose::Password);
    assert!(matches!(
        password.surrounding_text,
        IMESurroundingText::Unavailable
    ));
}

#[test]
fn password_keeps_identity_but_never_reports_text() {
    let mut field = Field::new("secret");
    field.password = true;

    let output = field.frame(vec![]);
    assert_eq!(output.owner, field.id);
    assert!(output.context_generation > 0);
    assert_eq!(output.purpose, egui::IMEPurpose::Password);
    assert!(matches!(
        output.surrounding_text,
        IMESurroundingText::Unavailable
    ));
    assert!(!format!("{output:?}").contains("secret"));
}

fn two_field_frame(
    ctx: &Context,
    [a, b]: [Id; 2],
    focus: Id,
    text_a: &mut String,
    text_b: &mut String,
    password_b: bool,
) -> egui::output::IMEOutput {
    let mut output = ctx.run_ui(
        RawInput {
            screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(500.0, 300.0))),
            ..Default::default()
        },
        |ui| {
            ui.memory_mut(|mem| mem.request_focus(focus));
            ui.add(TextEdit::singleline(text_a).id(a));
            let mut edit = TextEdit::singleline(text_b).id(b);
            if password_b {
                edit = edit.password(true);
            }
            ui.add(edit);
        },
    );
    output.textures_delta.clear();
    output
        .platform_output
        .ime
        .expect("the focused editable field emits an IME descriptor")
}

#[test]
fn switching_fields_does_not_keep_a_stale_payload() {
    let ctx = Context::default();
    let ids @ [a, b] = [Id::unique("field-a"), Id::unique("field-b")];
    let mut text_a = "hello".to_owned();
    let mut text_b = "secret".to_owned();

    let first_a = two_field_frame(&ctx, ids, a, &mut text_a, &mut text_b, false);
    assert_eq!(first_a.owner, a);
    let payload = first_a.surrounding_text.available().unwrap();
    assert_eq!(payload.text, "hello");

    let second_b = two_field_frame(&ctx, ids, b, &mut text_a, &mut text_b, true);
    assert_eq!(second_b.owner, b);
    assert_eq!(second_b.purpose, egui::IMEPurpose::Password);
    assert!(
        matches!(second_b.surrounding_text, IMESurroundingText::Unavailable),
        "the password field must not inherit the previous field's payload"
    );

    let third_a = two_field_frame(&ctx, ids, a, &mut text_a, &mut text_b, true);
    assert_eq!(third_a.owner, a);
    let payload = third_a.surrounding_text.available().unwrap();
    assert_eq!(payload.text, "hello");
    assert!(
        third_a.context_generation > first_a.context_generation,
        "refocusing the same owner starts a new context generation"
    );
}

#[test]
fn ime_commit_reports_the_input_method_cause() {
    let mut field = Field::new("ab");
    let baseline = field.frame(vec![]);
    assert_eq!(
        baseline.change_cause,
        ImeChangeCause::Other,
        "the first focused context is a transition"
    );

    let output = field.frame(vec![commit("c")]);
    assert_eq!(field.text, "abc");
    assert_eq!(output.change_cause, ImeChangeCause::InputMethod);
}

#[test]
fn ime_delete_and_commit_report_input_method_and_undo_in_one_step() {
    let mut field = Field::new("éx");
    field.frame(vec![]);

    let output = field.frame(vec![
        Event::Ime(ImeEvent::DeleteSurroundingBytes {
            before_bytes: 1,
            after_bytes: 0,
        }),
        commit("y"),
    ]);
    assert_eq!(field.text, "éy");
    assert_eq!(output.change_cause, ImeChangeCause::InputMethod);

    // The deletion and the commit are one undo transaction.
    let undo = field.frame(vec![Event::Key {
        key: Key::Z,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: Modifiers::COMMAND,
    }]);
    assert_eq!(field.text, "éx");
    assert_eq!(undo.change_cause, ImeChangeCause::Other);
}

#[test]
fn application_edit_before_or_after_ime_commit_forces_other() {
    let mut app_first = Field::new("base");
    app_first.frame(vec![]);
    let output = app_first.frame(vec![Event::Text("x".to_owned()), commit("y")]);
    assert_eq!(app_first.text, "basexy");
    assert_eq!(output.change_cause, ImeChangeCause::Other);

    let mut ime_first = Field::new("base");
    ime_first.frame(vec![]);
    let output = ime_first.frame(vec![commit("x"), Event::Text("y".to_owned())]);
    assert_eq!(ime_first.text, "basexy");
    assert_eq!(output.change_cause, ImeChangeCause::Other);
}

#[test]
fn application_mutation_between_frames_invalidates_a_later_ime_receipt() {
    let mut field = Field::new("base");
    field.frame(vec![]);

    // The application changed the document after the last observation but before this frame.
    field.text.push('x');
    let output = field.frame(vec![commit("y")]);
    // The application appended after the existing caret; the IME inserts at that caret.
    assert_eq!(field.text, "baseyx");
    assert_eq!(output.change_cause, ImeChangeCause::Other);
}

#[test]
fn discarded_pass_retains_the_frames_input_method_cause() {
    let mut field = Field::new("base");
    field.frame(vec![]);

    let mut causes = Vec::new();
    let mut calls = 0;
    let mut output = field.ctx.run_ui(
        RawInput {
            screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(500.0, 300.0))),
            time: Some(1.0),
            events: vec![commit("x")],
            ..Default::default()
        },
        |ui| {
            calls += 1;
            ui.memory_mut(|mem| mem.request_focus(field.id));
            ui.add(
                TextEdit::singleline(&mut field.text)
                    .id(field.id)
                    .cursor_at_end(true),
            );
            causes.push(
                ui.output(|output| output.ime.as_ref().expect("focused editor").change_cause),
            );
            if calls == 1 {
                ui.ctx().request_discard("exercise a discarded IME pass");
            }
        },
    );
    output.textures_delta.clear();

    assert_eq!(calls, 2, "the frame ran two real passes");
    assert_eq!(
        field.text, "basex",
        "raw input is consumed by the first pass"
    );
    assert_eq!(
        causes,
        [ImeChangeCause::InputMethod, ImeChangeCause::InputMethod],
        "an unchanged discarded pass keeps the frame's cause"
    );
    assert_eq!(
        output
            .platform_output
            .ime
            .expect("final descriptor")
            .change_cause,
        ImeChangeCause::InputMethod
    );
}

#[test]
fn application_mutation_in_a_discarded_pass_dominates_the_frame() {
    let mut field = Field::new("base");
    field.frame(vec![]);

    let mut causes = Vec::new();
    let mut calls = 0;
    let mut output = field.ctx.run_ui(
        RawInput {
            screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(500.0, 300.0))),
            time: Some(2.0),
            events: vec![commit("x")],
            ..Default::default()
        },
        |ui| {
            calls += 1;
            if calls == 2 {
                field.text.push('y');
            }
            ui.memory_mut(|mem| mem.request_focus(field.id));
            ui.add(
                TextEdit::singleline(&mut field.text)
                    .id(field.id)
                    .cursor_at_end(true),
            );
            causes.push(
                ui.output(|output| output.ime.as_ref().expect("focused editor").change_cause),
            );
            if calls == 1 {
                ui.ctx()
                    .request_discard("exercise a mixed application/IME frame");
            }
        },
    );
    output.textures_delta.clear();

    assert_eq!(calls, 2);
    assert_eq!(field.text, "basexy");
    assert_eq!(
        causes,
        [ImeChangeCause::InputMethod, ImeChangeCause::Other],
        "an application change anywhere in the frame dominates"
    );
}

#[test]
fn preedit_only_and_preedit_clear_do_not_poison_a_committed_ime_cause() {
    let mut field = Field::new("ab");
    field.frame(vec![]);

    // A pre-edit changes the buffer but not the committed snapshot.
    let preedit_output = field.frame(vec![preedit("に")]);
    assert_eq!(field.text, "abに");
    assert_eq!(preedit_output.change_cause, ImeChangeCause::Other);

    // Clearing the pre-edit before committing is part of the same IME transaction and does
    // not count as an application edit.
    let output = field.frame(vec![commit("x")]);
    assert_eq!(field.text, "abx");
    assert_eq!(output.change_cause, ImeChangeCause::InputMethod);
}

#[test]
fn preedit_clear_then_commit_in_one_pass_is_input_method() {
    let mut field = Field::new("ab");
    field.frame(vec![]);
    field.frame(vec![preedit("に")]);
    assert_eq!(field.text, "abに");

    let output = field.frame(vec![preedit("x")]);
    assert_eq!(field.text, "abx");
    assert_eq!(output.change_cause, ImeChangeCause::Other);

    // The next pass clears the composition and commits in the same batch.
    let output = field.frame(vec![preedit(""), commit("y")]);
    assert_eq!(field.text, "aby");
    assert_eq!(output.change_cause, ImeChangeCause::InputMethod);
}

#[test]
fn preedit_created_and_committed_in_one_pass_is_input_method() {
    let mut field = Field::new("ab");
    field.frame(vec![]);

    let output = field.frame(vec![preedit("x"), commit("y")]);
    assert_eq!(field.text, "aby");
    assert_eq!(output.change_cause, ImeChangeCause::InputMethod);
}

#[test]
fn noop_ime_events_do_not_earn_input_method() {
    let mut field = Field::new("clean");
    field.frame(vec![]);

    let empty_commit = field.frame(vec![commit("")]);
    assert_eq!(field.text, "clean");
    assert_eq!(empty_commit.change_cause, ImeChangeCause::Other);

    let zero_delete = field.frame(vec![Event::Ime(ImeEvent::DeleteSurrounding {
        before_chars: 0,
        after_chars: 0,
    })]);
    assert_eq!(field.text, "clean");
    assert_eq!(zero_delete.change_cause, ImeChangeCause::Other);

    // An ignored newline-only commit is also a failure, not an IME text change.
    let newline_commit = field.frame(vec![commit("\n")]);
    assert_eq!(field.text, "clean");
    assert_eq!(newline_commit.change_cause, ImeChangeCause::Other);

    // A byte count that ends inside a scalar deletes nothing and is not an accepted mutation.
    let mut scalar = Field::new("ab👧");
    scalar.frame(vec![]);
    let inside_scalar = scalar.frame(vec![Event::Ime(ImeEvent::DeleteSurroundingBytes {
        before_bytes: 3,
        after_bytes: 0,
    })]);
    assert_eq!(scalar.text, "ab👧");
    assert_eq!(inside_scalar.change_cause, ImeChangeCause::Other);
}

#[test]
fn a_noop_after_an_accepted_commit_does_not_erase_input_method() {
    let mut field = Field::new("ab");
    field.frame(vec![]);

    let output = field.frame(vec![commit("x"), commit("")]);
    assert_eq!(field.text, "abx");
    assert_eq!(output.change_cause, ImeChangeCause::InputMethod);
}

#[test]
fn password_ime_commit_is_never_input_method() {
    let mut field = Field::new("secret");
    field.password = true;
    field.frame(vec![]);

    let output = field.frame(vec![commit("x")]);
    assert_eq!(field.text, "secretx");
    assert_eq!(output.change_cause, ImeChangeCause::Other);
    assert!(matches!(
        output.surrounding_text,
        IMESurroundingText::Unavailable
    ));
}

#[test]
fn reset_completion_is_control_only_for_stock_text_edit_composition_and_undo() {
    for bytes in [false, true] {
        let mut field = Field::new("éx");
        field.frame(vec![]);
        let request_id = field.ctx.request_ime_reset(ViewportId::ROOT).unwrap();
        let marker = Event::ImeResetCompleted { request_id };
        let unchanged = field.frame(vec![marker.clone()]);
        assert_eq!(
            field.text, "éx",
            "completion itself must not edit the buffer"
        );
        assert_eq!(Field::payload(&unchanged).cursor, "éx".len());
        assert_eq!(unchanged.reset_request, Some(request_id));

        let deletion = if bytes {
            ImeEvent::DeleteSurroundingBytes {
                before_bytes: 1,
                after_bytes: 0,
            }
        } else {
            ImeEvent::DeleteSurrounding {
                before_chars: 1,
                after_chars: 0,
            }
        };
        let changed = field.frame(vec![
            Event::Ime(deletion),
            marker.clone(),
            commit("y"),
            marker,
        ]);
        assert_eq!(
            field.text, "éy",
            "a valid delete→commit remains one mutation chain"
        );
        assert_eq!(changed.change_cause, ImeChangeCause::InputMethod);
        let undo = field.frame(vec![Event::Key {
            key: Key::Z,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::COMMAND,
        }]);
        assert_eq!(
            field.text, "éx",
            "the interspersed marker creates no extra undo step"
        );
        assert_eq!(undo.change_cause, ImeChangeCause::Other);
        field.frame(vec![commit("!")]);
        assert_eq!(
            field.text, "éx!",
            "direct commit without old composition stays valid"
        );
    }
}

#[test]
fn reset_completion_does_not_end_stock_preedit_or_move_its_selection() {
    let mut with_control = Field::new("prefix");
    let mut baseline = Field::new("prefix");
    with_control.frame(vec![]);
    baseline.frame(vec![]);
    let a = with_control.frame(vec![preedit("かな")]);
    let b = baseline.frame(vec![preedit("かな")]);
    assert_eq!(with_control.text, baseline.text);
    assert_eq!(Field::payload(&a), Field::payload(&b));
    let request_id = with_control
        .ctx
        .request_ime_reset(ViewportId::ROOT)
        .unwrap();
    let a = with_control.frame(vec![Event::ImeResetCompleted { request_id }]);
    let b = baseline.frame(vec![]);
    assert_eq!(
        with_control.text, baseline.text,
        "control is ignored by ordinary TextEdit"
    );
    assert_eq!(
        Field::payload(&a),
        Field::payload(&b),
        "committed snapshot and caret remain identical"
    );
    let a = with_control.frame(vec![
        preedit(""),
        Event::ImeResetCompleted { request_id },
        commit("語"),
    ]);
    let b = baseline.frame(vec![preedit(""), commit("語")]);
    assert_eq!(with_control.text, "prefix語");
    assert_eq!(with_control.text, baseline.text);
    assert_eq!(Field::payload(&a), Field::payload(&b));
}

#[test]
fn empty_text_edit_with_hint_text_renders_first_hangul_jamo_galley_on_same_frame() {
    for with_prefix in [false, true] {
        let ctx = Context::default();
        let id = Id::unique(("hint-first-jamo", with_prefix));
        let mut text = String::new();
        let screen_rect = Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(500.0, 300.0)));

        let mut empty_galley_text = String::new();
        let mut init_out = ctx.run_ui(
            RawInput {
                screen_rect,
                time: Some(0.1),
                ..Default::default()
            },
            |ui| {
                ui.memory_mut(|mem| mem.request_focus(id));
                let mut edit = TextEdit::singleline(&mut text)
                    .id(id)
                    .hint_text("Search...");
                if with_prefix {
                    edit = edit.prefix("Q: ");
                }
                let out = edit.show(ui);
                empty_galley_text = out.galley.text().to_owned();
            },
        );
        init_out.textures_delta.clear();
        let empty_ime = init_out
            .platform_output
            .ime
            .expect("focused empty field emits IMEOutput");
        assert_eq!(empty_galley_text, "");

        let mut first_jamo_galley_text = String::new();
        let mut is_composing = false;
        let mut jamo_out = ctx.run_ui(
            RawInput {
                screen_rect,
                time: Some(0.2),
                events: vec![preedit("ㅎ")],
                ..Default::default()
            },
            |ui| {
                let mut edit = TextEdit::singleline(&mut text)
                    .id(id)
                    .hint_text("Search...");
                if with_prefix {
                    edit = edit.prefix("Q: ");
                }
                let out = edit.show(ui);
                first_jamo_galley_text = out.galley.text().to_owned();
                is_composing = out.state.is_ime_composition();
            },
        );
        jamo_out.textures_delta.clear();
        let jamo_ime = jamo_out
            .platform_output
            .ime
            .expect("focused composing field emits IMEOutput");

        assert_eq!(text, "ㅎ");
        assert_eq!(
            first_jamo_galley_text, "ㅎ",
            "first Hangul jamo must appear in output.galley on the exact frame it arrives"
        );
        assert!(is_composing, "TextEditState reports active IME composition");
        assert!(
            jamo_ime.cursor_rect.min.x > empty_ime.cursor_rect.min.x,
            "IME cursor_rect must advance past the first jamo on the same frame"
        );
        assert_eq!(Field::payload(&jamo_ime).text, "");
        assert_eq!(Field::payload(&jamo_ime).cursor, 0);
    }
}

#[test]
fn text_edit_state_is_ime_composition_tracks_preedit_and_commit() {
    let mut field = Field::new("가");
    field.frame(vec![]);
    let loaded = egui::text_edit::TextEditState::load(&field.ctx, field.id)
        .expect("persisted TextEditState exists");
    assert!(
        !loaded.is_ime_composition(),
        "initial selection state is not composing"
    );

    field.frame(vec![preedit("ㅎ")]);
    assert!(
        egui::text_edit::TextEditState::load(&field.ctx, field.id)
            .expect("persisted TextEditState exists")
            .is_ime_composition(),
        "Preedit activates is_ime_composition()"
    );

    field.frame(vec![preedit("하")]);
    assert!(
        egui::text_edit::TextEditState::load(&field.ctx, field.id)
            .expect("persisted TextEditState exists")
            .is_ime_composition(),
        "updated Preedit keeps is_ime_composition() active"
    );

    field.frame(vec![commit("한")]);
    assert_eq!(field.text, "가한");
    assert!(
        !egui::text_edit::TextEditState::load(&field.ctx, field.id)
            .expect("persisted TextEditState exists")
            .is_ime_composition(),
        "Commit clears is_ime_composition()"
    );
}

#[test]
fn clear_preedit_uses_clamped_preedit_range_when_cursor_moved_inside_composition() {
    let ctx = Context::default();
    let id = Id::unique("clear-preedit-range");
    let mut text = "가나".to_owned();
    let screen_rect = Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(500.0, 300.0)));

    let run = |ctx: &Context, text: &mut String, time: f64, events: Vec<Event>| {
        let mut out = ctx.run_ui(
            RawInput {
                screen_rect,
                time: Some(time),
                events,
                ..Default::default()
            },
            |ui| {
                ui.add(TextEdit::singleline(text).id(id).cursor_at_end(true));
            },
        );
        out.textures_delta.clear();
        out.platform_output
            .ime
            .expect("focused field emits IMEOutput")
    };

    ctx.memory_mut(|mem| mem.request_focus(id));
    run(&ctx, &mut text, 0.1, vec![]);

    // Move caret between "가" and "나" (index 1), then start a 2-syllable preedit "한글" (1..3).
    let left_key = Event::Key {
        key: Key::ArrowLeft,
        physical_key: Some(Key::ArrowLeft),
        pressed: true,
        repeat: false,
        modifiers: Modifiers::NONE,
    };
    run(&ctx, &mut text, 0.2, vec![left_key.clone()]);
    run(&ctx, &mut text, 0.3, vec![preedit("한글")]);
    assert_eq!(text, "가한글나");

    // Collapse `cursor_range` to `1..1` while `state.cursor_purpose` still holds `preedit_range: 1..3`,
    // then update preedit to "함": the full `1..3` preedit must be replaced cleanly.
    run(&ctx, &mut text, 0.4, vec![left_key.clone(), preedit("함")]);
    assert_eq!(
        text, "가함나",
        "updated Preedit must clear the full active preedit_range rather than the collapsed cursor"
    );

    // Collapse `cursor_range` again and commit "합": `Commit` must read `preedit_range` BEFORE
    // resetting `state.cursor_purpose = Selection`.
    let committed = run(&ctx, &mut text, 0.5, vec![left_key, commit("합")]);
    assert_eq!(
        text, "가합나",
        "Commit must clear the active preedit_range before resetting cursor_purpose"
    );
    assert_eq!(Field::payload(&committed).text, "가합나");
    assert_eq!(committed.change_cause, ImeChangeCause::InputMethod);
}

#[test]
fn escape_during_ime_composition_cancels_preedit_interrupts_ime_and_retains_focus() {
    let ctx = Context::default();
    let id = Id::unique("escape-composition-focus-lock");
    let mut text = "가".to_owned();
    let screen_rect = Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(500.0, 300.0)));

    let run = |ctx: &Context, text: &mut String, time: f64, events: Vec<Event>| {
        let mut out = ctx.run_ui(
            RawInput {
                screen_rect,
                time: Some(time),
                events,
                ..Default::default()
            },
            |ui| {
                // Intentionally do NOT call `request_focus` here so `Focus::begin_pass` governs focus!
                ui.add(TextEdit::singleline(text).id(id).cursor_at_end(true));
            },
        );
        out.textures_delta.clear();
        out.platform_output.ime
    };

    ctx.memory_mut(|mem| mem.request_focus(id));
    assert!(run(&ctx, &mut text, 0.1, vec![]).is_some());
    assert!(ctx.memory(|mem| mem.has_focus(id)));

    assert!(run(&ctx, &mut text, 0.2, vec![preedit("ㅎ")]).is_some());
    assert!(run(&ctx, &mut text, 0.3, vec![preedit("하")]).is_some());
    assert_eq!(text, "가하");
    assert!(
        egui::text_edit::TextEditState::load(&ctx, id)
            .expect("persisted TextEditState exists")
            .is_ime_composition()
    );

    let escape = Event::Key {
        key: Key::Escape,
        physical_key: Some(Key::Escape),
        pressed: true,
        repeat: false,
        modifiers: Modifiers::NONE,
    };

    let after_first_escape = run(&ctx, &mut text, 0.4, vec![escape.clone()])
        .expect("Escape during active composition must retain focus and emit IMEOutput");
    assert!(
        ctx.memory(|mem| mem.has_focus(id)),
        "Escape during active Hangul composition must NOT drop TextEdit focus"
    );
    assert_eq!(text, "가", "Escape deletes the active preedit text");
    assert!(
        !egui::text_edit::TextEditState::load(&ctx, id)
            .expect("persisted TextEditState exists")
            .is_ime_composition(),
        "Escape exits ImeComposition"
    );
    assert!(
        after_first_escape.should_interrupt_composition,
        "Escape notifies platform integration to interrupt IME composition"
    );
    assert_eq!(Field::payload(&after_first_escape).text, "가");

    // Once composition has ended, pressing Escape again surrenders focus normally.
    let after_second_escape = run(&ctx, &mut text, 0.5, vec![escape]);
    assert!(
        !ctx.memory(|mem| mem.has_focus(id)),
        "Escape outside composition surrenders focus as usual"
    );
    assert!(after_second_escape.is_none());
}

#[test]
fn hangul_preedit_jamo_steps_are_not_recorded_in_undo_history() {
    let mut field = Field::new("");
    // Focus and settle the initial empty checkpoint.
    field.frame(vec![]);
    field.time += 2.0;
    field.frame(vec![]);

    // Commit "가" and let the undoer settle a checkpoint for "가".
    field.frame(vec![preedit("ㄱ")]);
    field.time += 2.0;
    field.frame(vec![preedit("가")]);
    field.time += 2.0;
    field.frame(vec![preedit(""), commit("가")]);
    field.time += 2.0;
    field.frame(vec![]);
    assert_eq!(field.text, "가");

    // Compose "한" slowly across multiple frames exceeding the undoer's auto-save interval.
    field.time += 2.0;
    field.frame(vec![preedit("ㅎ")]);
    assert_eq!(field.text, "가ㅎ");
    field.time += 2.0;
    field.frame(vec![]);

    field.time += 2.0;
    field.frame(vec![preedit("하")]);
    assert_eq!(field.text, "가하");
    field.time += 2.0;
    field.frame(vec![]);

    field.time += 2.0;
    field.frame(vec![preedit("한")]);
    assert_eq!(field.text, "가한");

    field.time += 2.0;
    field.frame(vec![preedit(""), commit("한")]);
    assert_eq!(field.text, "가한");

    let undo = Event::Key {
        key: Key::Z,
        physical_key: Some(Key::Z),
        pressed: true,
        repeat: false,
        modifiers: Modifiers::COMMAND,
    };

    // First Cmd+Z must step directly back to "가", never "가하" or "가ㅎ".
    field.frame(vec![undo.clone()]);
    assert_eq!(
        field.text, "가",
        "Cmd+Z must skip intermediate preedit jamo states"
    );

    // Second Cmd+Z steps back to initial "".
    field.frame(vec![undo]);
    assert_eq!(field.text, "");
}
