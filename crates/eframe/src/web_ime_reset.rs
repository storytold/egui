//! Local DOM routing admission, not a browser/OS composition cancellation acknowledgement.

use egui::{IMEPurpose, Id, ImeResetId, ViewportId, output::IMEOutput};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ContextKey(Id, u64, IMEPurpose);

/// Retained outside `AppRunner` across destroy/start. Already observed output cannot revive a fence.
#[derive(Default)]
pub(crate) struct ResetRequests {
    latest: Option<ImeResetId>,
    pending: Option<(egui::ImeResetId, ContextKey)>,
}

impl ResetRequests {
    pub fn invalidate(&mut self) {
        self.pending = None;
    }

    /// Admit only a new explicit ROOT request from settled output. Failure consumes admission,
    /// never acknowledges it: a later actual application interruption must allocate a new id.
    pub fn admit(&mut self, ime: Option<&IMEOutput>) -> Option<ImeResetId> {
        self.pending = None;
        let request = ime?.reset_request?;
        if request.viewport_id() != ViewportId::ROOT
            || self
                .latest
                .is_some_and(|latest| latest.as_u64() >= request.as_u64())
        {
            return None;
        }
        self.latest = Some(request);
        let ime = ime?;
        self.pending = Some((
            request,
            ContextKey(ime.owner, ime.context_generation, ime.purpose),
        ));
        Some(request)
    }

    /// Completion is permitted only for the exact descriptor whose fresh source was configured.
    pub fn complete(
        &mut self,
        request: ImeResetId,
        ime: &IMEOutput,
        events: &mut Vec<egui::Event>,
    ) -> bool {
        if self.pending.take()
            != Some((
                request,
                ContextKey(ime.owner, ime.context_generation, ime.purpose),
            ))
            || ime.reset_request != Some(request)
        {
            return false;
        }
        events.push(egui::Event::ImeResetCompleted {
            request_id: request,
        });
        true
    }
}

/// Only replacement agents require their own compositionstart. The ordinary startup/untokened
/// path retains its existing policy. Rejected DOM values must also be scrubbed by the caller.
#[derive(Default)]
pub(crate) struct CompositionSession {
    require_start: bool,
    started: bool,
}

impl CompositionSession {
    pub fn replacement() -> Self {
        Self {
            require_start: true,
            started: false,
        }
    }

    pub fn start(&mut self) {
        self.started = true;
    }

    pub fn input(&self, composing: bool) -> bool {
        !composing || !self.require_start || self.started
    }

    pub fn end(&mut self) -> bool {
        let accepted = !self.require_start || self.started;
        self.started = false;
        accepted
    }

    pub fn clear(&mut self) {
        self.started = false;
    }
}

/// Private receipt for the currently held press in one shared `KeyboardKeys` instance.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PressId(u64);

#[derive(Default)]
pub(crate) struct HeldPresses {
    next: u64,
    held: std::collections::HashMap<String, (PressId, Option<egui::Key>)>,
}

impl HeldPresses {
    fn identity(key: &str, code: &str) -> String {
        if code.is_empty() {
            format!("key:{key}")
        } else {
            format!("code:{code}")
        }
    }

    pub fn press(&mut self, key: &str, code: &str, logical: Option<egui::Key>) {
        let identity = Self::identity(key, code);
        // Remove the old ownership even if checked allocation is exhausted.
        self.held.remove(&identity);
        if let Some(next) = self.next.checked_add(1) {
            self.next = next;
            self.held.insert(identity, (PressId(next), logical));
        }
    }

    pub fn current(&self, key: &str, code: &str) -> Option<PressId> {
        self.held.get(&Self::identity(key, code)).map(|(id, _)| *id)
    }

    pub fn release(&mut self, key: &str, code: &str) {
        self.held.remove(&Self::identity(key, code));
    }

    pub fn release_matching(&mut self, key: &str, code: &str, receipt: PressId) -> bool {
        if self.current(key, code) != Some(receipt) {
            return false;
        }
        self.release(key, code);
        true
    }

    /// One queued egui logical release affects all physical aliases for that key.
    pub fn release_logical(&mut self, key: egui::Key) {
        self.held.retain(|_, (_, logical)| *logical != Some(key));
    }

    pub fn clear(&mut self) {
        self.held.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::{Context, Rect};

    fn request(ctx: &Context, viewport: ViewportId) -> IMEOutput {
        let mut ime = IMEOutput::new(
            Id::unique("browser editor"),
            1,
            IMEPurpose::Normal,
            Rect::ZERO,
            Rect::ZERO,
        );
        ime.reset_request = ctx.request_ime_reset(viewport);
        assert!(ime.reset_request.is_some());
        ime
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn settled_browser_requests_are_exact_latest_root_and_one_shot_across_restart() {
        let ctx = Context::default();
        let mut requests = ResetRequests::default();
        let first = request(&ctx, ViewportId::ROOT);
        let next = request(&ctx, ViewportId::ROOT);
        assert_eq!(requests.admit(Some(&first)), first.reset_request);
        assert_eq!(requests.admit(Some(&next)), next.reset_request);
        assert_eq!(requests.admit(Some(&first)), None);
        requests.invalidate(); // destroy, then start uses the same external admission owner
        assert_eq!(requests.admit(Some(&next)), None);
        assert_eq!(requests.admit(None), None);
        assert_eq!(
            requests.admit(Some(&request(&ctx, ViewportId::from_hash_of("foreign")))),
            None
        );
        let fresh = request(&Context::default(), ViewportId::ROOT);
        assert_eq!(requests.admit(Some(&fresh)), fresh.reset_request);
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn setup_failure_has_no_implicit_retry_or_completion() {
        let ctx = Context::default();
        let ime = request(&ctx, ViewportId::ROOT);
        let mut requests = ResetRequests::default();
        assert_eq!(requests.admit(Some(&ime)), ime.reset_request);
        // Failed setup does not turn replayed output into another attempt/acknowledgement.
        for _ in 0..100 {
            assert_eq!(requests.admit(Some(&ime)), None);
        }
        let fresh = request(&ctx, ViewportId::ROOT);
        assert_eq!(requests.admit(Some(&fresh)), fresh.reset_request);
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn replacement_composition_requires_its_own_start_but_direct_corrections_remain_live() {
        let mut session = CompositionSession::replacement();
        assert!(!session.input(true));
        assert!(!session.end());
        assert!(
            session.input(false),
            "insertText/replacement/correction remain direct input"
        );
        session.start();
        assert!(session.input(true));
        assert!(
            session.input(false),
            "terminal input before compositionend remains accepted"
        );
        assert!(session.end());
        assert!(!session.end());
        assert!(!session.input(true));
        session.start();
        assert!(session.input(true));
        session.clear();
        assert!(!session.end());
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn ordinary_agent_retains_untokened_lifecycle_policy() {
        let mut session = CompositionSession::default();
        assert!(session.input(true));
        assert!(session.end());
        assert!(session.input(false));
    }
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn successful_local_fence_preserves_hidden_old_prefix_new_suffix_and_deduplicates() {
        let ctx = Context::default();
        let ime = request(&ctx, ViewportId::ROOT);
        let id = ime.reset_request.unwrap();
        let mut requests = ResetRequests::default();
        let mut queued = egui::RawInput::default();
        // The real host appends unconsumed hidden input before handling settled output.
        let mut hidden = egui::RawInput::default();
        hidden
            .events
            .push(egui::Event::Ime(egui::ImeEvent::Commit("old".into())));
        queued.append(hidden);
        assert_eq!(requests.admit(Some(&ime)), Some(id));
        assert!(requests.complete(id, &ime, &mut queued.events));
        assert!(!requests.complete(id, &ime, &mut queued.events));
        // No visible pass consumed these events; take/append mirrors WebInput's host boundary.
        let next_hidden = queued.take();
        queued.append(next_hidden);
        queued
            .events
            .push(egui::Event::Ime(egui::ImeEvent::Preedit {
                text: "新".into(),
                active_range_chars: Some(0..1),
            }));
        assert!(
            matches!(&queued.events[..], [egui::Event::Ime(egui::ImeEvent::Commit(old)), egui::Event::ImeResetCompleted { request_id }, egui::Event::Ime(egui::ImeEvent::Preedit { text, .. })] if old == "old" && *request_id == id && text == "新")
        );
        assert_eq!(requests.admit(Some(&ime)), None);
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn failed_retirement_or_changed_descriptor_cannot_complete() {
        let ctx = Context::default();
        for change in 0..5 {
            let ime = request(&ctx, ViewportId::ROOT);
            let id = ime.reset_request.unwrap();
            let mut requests = ResetRequests::default();
            let mut events = Vec::new();
            requests.admit(Some(&ime));
            let mut replacement = ime.clone();
            match change {
                0 => requests.invalidate(), // setup/unsubscribe failure or runner teardown
                1 => replacement.owner = Id::unique("different owner"),
                2 => replacement.context_generation += 1,
                3 => replacement.purpose = IMEPurpose::Password,
                _ => replacement.reset_request = None,
            }
            assert!(!requests.complete(id, &replacement, &mut events));
            assert!(events.is_empty());
            assert_eq!(requests.admit(Some(&ime)), None);
        }
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn shared_press_receipts_reject_released_repressed_and_focus_cleared_keys() {
        let mut held = HeldPresses::default();
        held.press("a", "KeyA", Some(egui::Key::A));
        let agent = held.current("a", "KeyA").unwrap();
        held.release("a", "KeyA"); // canvas keyup uses the same shared bookkeeping
        held.press("a", "KeyA", Some(egui::Key::A));
        let canvas = held.current("a", "KeyA").unwrap();
        assert_ne!(agent, canvas);
        assert!(!held.release_matching("a", "KeyA", agent));
        assert_eq!(held.current("a", "KeyA"), Some(canvas));
        held.clear();
        held.press("a", "KeyA", Some(egui::Key::A));
        assert!(!held.release_matching("a", "KeyA", canvas));
        let current = held.current("a", "KeyA").unwrap();
        held.press("b", "KeyB", Some(egui::Key::B));
        assert!(held.release_matching("a", "KeyA", current));
        assert!(held.current("b", "KeyB").is_some());
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn press_receipt_exhaustion_retires_old_ownership_without_wrap_or_panic() {
        let mut held = HeldPresses::default();
        held.press("a", "KeyA", Some(egui::Key::A));
        let old = held.current("a", "KeyA").unwrap();
        held.next = u64::MAX;
        held.press("a", "KeyA", Some(egui::Key::A));
        assert_eq!(held.current("a", "KeyA"), None);
        assert!(!held.release_matching("a", "KeyA", old));
        held.clear();
        held.press("a", "KeyA", Some(egui::Key::A));
        assert_eq!(held.current("a", "KeyA"), None);
    }
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn queued_logical_release_retires_all_aliases_but_preserves_unreleased_keys() {
        let mut held = HeldPresses::default();
        held.press("a", "KeyA", Some(egui::Key::A));
        let old = held.current("a", "KeyA").unwrap();
        held.press("a", "KeyQ", Some(egui::Key::A));
        held.press("a", "", Some(egui::Key::A));
        held.press("b", "KeyB", Some(egui::Key::B));
        let unrelated = held.current("b", "KeyB").unwrap();
        held.press("Unidentified", "", None);
        held.release_logical(egui::Key::A);
        for code in ["KeyA", "KeyQ", ""] {
            assert_eq!(held.current("a", code), None);
        }
        assert_eq!(held.current("b", "KeyB"), Some(unrelated));
        assert!(held.current("Unidentified", "").is_some());
        held.press("a", "KeyA", Some(egui::Key::A));
        let newer = held.current("a", "KeyA").unwrap();
        assert!(!held.release_matching("a", "KeyA", old));
        assert_eq!(held.current("a", "KeyA"), Some(newer));
    }
}
