use egui::{Context, IMEPurpose, Id, RawInput, Rect, ViewportId, output::IMEOutput};

fn descriptor(owner: Id, generation: u64) -> IMEOutput {
    IMEOutput::new(
        owner,
        generation,
        IMEPurpose::Normal,
        Rect::ZERO,
        Rect::ZERO,
    )
}

fn frame(ctx: &Context, ime: Option<&IMEOutput>) -> Option<IMEOutput> {
    let mut output = ctx.run_ui(RawInput::default(), |ui| {
        ui.output_mut(|out| out.ime = ime.cloned());
    });
    output.textures_delta.clear();
    output.platform_output.ime
}

#[test]
fn requests_are_viewport_bound_process_unique_and_supersede_same_owner_caret() {
    let ctx = Context::default();
    let other_ctx = Context::default();
    let viewport = ViewportId::ROOT;
    let first = ctx.request_ime_reset(viewport).unwrap();
    let next = ctx.request_ime_reset(viewport).unwrap();
    let other = other_ctx
        .request_ime_reset(ViewportId::from_hash_of("other"))
        .unwrap();
    assert_eq!(next.viewport_id(), viewport);
    assert_ne!(first, next);
    assert_ne!(other.as_u64(), first.as_u64());
    assert_ne!(other.as_u64(), next.as_u64());
    let owner = Id::unique("same owner after caret move");
    let ime = frame(&ctx, Some(&descriptor(owner, 7))).unwrap();
    assert_eq!(
        ime.reset_request,
        Some(next),
        "latest actual interruption wins"
    );
    assert!(ime.should_interrupt_composition);
    assert_eq!(
        frame(&ctx, Some(&descriptor(owner, 7)))
            .unwrap()
            .reset_request,
        None,
        "output publication is one-shot"
    );
}

#[test]
fn hidden_and_no_descriptor_iterations_do_not_lose_or_acknowledge_request() {
    let ctx = Context::default();
    let owner = Id::unique("hidden editor");
    let mut requested = None;
    let logic = ctx.run_logic(&RawInput::default(), |ctx| {
        requested = ctx.request_ime_reset(ViewportId::ROOT);
    });
    assert!(requested.is_some());
    assert!(
        logic.platform_output.ime.is_none(),
        "no descriptor means no native reset call"
    );
    assert!(frame(&ctx, None).is_none());
    let ime = frame(&ctx, Some(&descriptor(owner, 1))).unwrap();
    assert_eq!(
        ime.reset_request, requested,
        "a real visible descriptor publishes the hidden request"
    );
    assert_eq!(
        frame(&ctx, Some(&descriptor(owner, 1)))
            .unwrap()
            .reset_request,
        None
    );
}

#[test]
fn discarded_pass_retains_request_only_for_same_final_context() {
    let ctx = Context::default();
    let owner = Id::unique("discard editor");
    let mut count = 0;
    let mut token = None;
    let mut output = ctx.run_ui(RawInput::default(), |ui| {
        count += 1;
        if count == 1 {
            token = ui.ctx().request_ime_reset(ViewportId::ROOT);
            ui.ctx()
                .request_discard("same request must survive settled replay");
        }
        ui.output_mut(|out| out.ime = Some(descriptor(owner, 3)));
    });
    output.textures_delta.clear();
    assert_eq!(count, 2);
    assert_eq!(output.platform_output.ime.unwrap().reset_request, token);
    assert_eq!(
        frame(&ctx, Some(&descriptor(owner, 3)))
            .unwrap()
            .reset_request,
        None
    );

    let mut count = 0;
    let mut output = ctx.run_ui(RawInput::default(), |ui| {
        count += 1;
        if count == 1 {
            ui.ctx().request_ime_reset(ViewportId::ROOT).unwrap();
            ui.ctx().request_discard("retire obsolete owner");
        }
        ui.output_mut(|out| {
            out.ime = Some(descriptor(
                if count == 1 {
                    owner
                } else {
                    Id::unique("replacement")
                },
                3,
            ));
        });
    });
    output.textures_delta.clear();
    assert_eq!(count, 2);
    assert_eq!(
        output.platform_output.ime.unwrap().reset_request,
        None,
        "old owner reset must not leak to another editor"
    );
}

#[test]
fn discarded_pass_newer_request_wins_and_final_absence_cancels_publication() {
    let ctx = Context::default();
    let owner = Id::unique("caret replay editor");
    let mut tokens = Vec::new();
    let mut output = ctx.run_ui(RawInput::default(), |ui| {
        tokens.push(ui.ctx().request_ime_reset(ViewportId::ROOT).unwrap());
        ui.output_mut(|out| out.ime = Some(descriptor(owner, 1)));
        if tokens.len() == 1 {
            ui.ctx()
                .request_discard("another actual caret interruption");
        }
    });
    output.textures_delta.clear();
    assert_eq!(tokens.len(), 2);
    assert_ne!(tokens[0], tokens[1]);
    assert_eq!(
        output.platform_output.ime.unwrap().reset_request,
        Some(tokens[1])
    );
    let mut pass = 0;
    let mut output = ctx.run_ui(RawInput::default(), |ui| {
        pass += 1;
        if pass == 1 {
            ui.ctx().request_ime_reset(ViewportId::ROOT).unwrap();
            ui.output_mut(|out| out.ime = Some(descriptor(owner, 1)));
            ui.ctx().request_discard("editor disappeared");
        }
    });
    output.textures_delta.clear();
    assert!(output.platform_output.ime.is_none());
    assert_eq!(
        frame(&ctx, Some(&descriptor(owner, 1)))
            .unwrap()
            .reset_request,
        None
    );
}

#[test]
fn stock_interrupt_api_remains_untokened() {
    let ctx = Context::default();
    let mut output = ctx.run_ui(RawInput::default(), |ui| {
        ui.memory_mut(|memory| memory.interrupt_ime());
        ui.output_mut(|out| out.ime = Some(descriptor(Id::unique("stock"), 1)));
    });
    output.textures_delta.clear();
    let ime = output.platform_output.ime.unwrap();
    assert!(ime.should_interrupt_composition);
    assert_eq!(ime.reset_request, None);
}

#[test]
fn discarded_obsolete_purpose_drops_token_and_its_interrupt_without_changing_stock_policy() {
    for tokened in [false, true] {
        let ctx = Context::default();
        let owner = Id::unique("purpose boundary");
        let mut passes = 0;
        let mut output = ctx.run_ui(RawInput::default(), |ui| {
            passes += 1;
            let mut ime = descriptor(owner, 1);
            if passes == 1 {
                if tokened {
                    ui.ctx().request_ime_reset(ViewportId::ROOT).unwrap();
                } else {
                    ui.memory_mut(|memory| memory.interrupt_ime());
                }
                ui.ctx().request_discard("normal -> password context");
            } else {
                ime.purpose = IMEPurpose::Password;
            }
            ui.output_mut(|out| out.ime = Some(ime));
        });
        output.textures_delta.clear();
        assert_eq!(passes, 2);
        let ime = output.platform_output.ime.unwrap();
        assert_eq!(ime.purpose, IMEPurpose::Password);
        assert_eq!(ime.reset_request, None);
        assert_eq!(
            ime.should_interrupt_composition, !tokened,
            "retired token cannot create a stock reset; untokened stock append policy stays unchanged"
        );
    }
}
