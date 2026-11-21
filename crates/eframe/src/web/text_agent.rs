//! Hidden DOM input with a locally retireable callback/session boundary.
//!
//! Replacing an agent fences its own DOM callbacks. It does not prove cancellation of a
//! browser/OS composition or the semantic freshness of input retargeted to the new element.

use core::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};

use wasm_bindgen::prelude::*;

use crate::web_caret_position::{
    CanvasMetrics, CaretGeometry, CaretPositionKey, CaretPositionTracker,
};

use super::{AppRunner, NeedRepaint, PanicHandler, WebRunner};

#[derive(Clone)]
pub(super) struct TextAgentContext {
    pub runner: Weak<RefCell<Option<AppRunner>>>,
    pub panic_handler: PanicHandler,
    pub listeners: Rc<RefCell<TextAgentListeners>>,
}

// A JS dispatch snapshot may retain a callback even after successful removeEventListener.
// Gate before entering the wasm Closure, so a captured retired wrapper remains a safe no-op
// after Rust releases the Closure. Static module code requires no unsafe-eval/CSP exception.
#[wasm_bindgen(
    inline_js = "export function egui_text_agent_session() { return { active: false }; }\nexport function egui_text_agent_activate(session, active) { session.active = active; }\nexport function egui_text_agent_listener(session, callback) { return event => { if (session.active) callback(event); }; }"
)]
extern "C" {
    fn egui_text_agent_session() -> JsValue;

    fn egui_text_agent_activate(session: &JsValue, active: bool);

    fn egui_text_agent_listener(session: &JsValue, callback: &js_sys::Function)
    -> js_sys::Function;
}

struct Session {
    active: Cell<bool>,
    gate: JsValue,

    /// Only keys actually forwarded by this agent; do not reset unrelated keyboard state.
    forwarded_keys: RefCell<Vec<ForwardedKey>>,
}

pub(super) struct ForwardedKey {
    pub event: web_sys::KeyboardEvent,
    pub press: crate::web_ime_reset::PressId,
}

impl Default for Session {
    fn default() -> Self {
        Self {
            active: Cell::new(false),
            gate: egui_text_agent_session(),
            forwarded_keys: Default::default(),
        }
    }
}

impl Session {
    fn set_active(&self, active: bool) {
        self.active.set(active);
        egui_text_agent_activate(&self.gate, active);
    }
}

struct Listener {
    target: web_sys::EventTarget,
    name: &'static str,
    callback: js_sys::Function,
    _closure: Closure<dyn FnMut(web_sys::Event)>,
    capture: bool,
}

impl Listener {
    fn new<E: JsCast>(
        target: &web_sys::EventTarget,
        name: &'static str,
        session: Weak<Session>,
        mut callback: impl FnMut(E) + 'static,
    ) -> Self {
        let callback_session = Weak::clone(&session);
        let closure = Closure::wrap(Box::new(move |event: web_sys::Event| {
            let Some(session) = session.upgrade() else {
                return;
            };
            // Guard outside every InputState borrow, including password/key early paths.
            if session.active.get() {
                callback(event.unchecked_into::<E>());
            }
        }) as Box<dyn FnMut(web_sys::Event)>);
        let callback = if let Some(session) = callback_session.upgrade() {
            egui_text_agent_listener(&session.gate, closure.as_ref().unchecked_ref())
        } else {
            // Construction always owns its session. A retired/missing session is never active.
            let gate = egui_text_agent_session();
            egui_text_agent_listener(&gate, closure.as_ref().unchecked_ref())
        };
        Self {
            target: target.clone(),
            name,
            callback,
            _closure: closure,
            capture: false,
        }
    }

    fn subscribe(&self) -> Result<(), JsValue> {
        self.target.add_event_listener_with_callback_and_bool(
            self.name,
            &self.callback,
            self.capture,
        )
    }

    fn unsubscribe(&self) -> Result<(), JsValue> {
        self.target.remove_event_listener_with_callback_and_bool(
            self.name,
            &self.callback,
            self.capture,
        )
    }
}

struct ListenerGroup {
    session: Rc<Session>,
    input: web_sys::HtmlInputElement,
    handles: Vec<Listener>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct FocusContext(egui::Id, u64, egui::IMEPurpose);

impl From<&egui::output::IMEOutput> for FocusContext {
    fn from(ime: &egui::output::IMEOutput) -> Self {
        Self(ime.owner, ime.context_generation, ime.purpose)
    }
}

struct FocusIntent {
    context: FocusContext,
    outside_epoch: u64,
}

/// Independent of the `AppRunner` borrow, including synchronous focus effects during removal.
struct FocusState {
    canvas: web_sys::HtmlCanvasElement,
    input: RefCell<Option<web_sys::HtmlInputElement>>,
    pending: RefCell<Option<FocusIntent>>,
    outside_epoch: Cell<u64>,
    exhausted: Cell<bool>,
    replacing: Cell<bool>,
}

impl FocusState {
    fn owns_dom_focus(&self) -> bool {
        if !super::has_focus(&self.canvas)
            && !self.input.borrow().as_ref().is_some_and(super::has_focus)
        {
            return false;
        }
        let mut root = self.canvas.get_root_node();
        while let Some(shadow) = root.dyn_ref::<web_sys::ShadowRoot>() {
            let host = shadow.host();
            let parent = host.get_root_node();
            if super::focused_element(&parent).as_ref() != Some(&host) {
                return false;
            }
            root = parent;
        }
        true
    }

    fn neutral_dom_focus(&self) -> bool {
        // Follow every shadow host to the document. BODY/HTML or an empty root are neutral;
        // an active host is neutral only when the nested root was itself neutral.
        let mut root = self.canvas.get_root_node();
        let mut allowed_host = None;
        loop {
            if super::focused_element(&root)
                .as_ref()
                .is_some_and(|element| {
                    !matches!(element.tag_name().as_str(), "BODY" | "HTML")
                        && Some(element) != allowed_host.as_ref()
                })
            {
                return false;
            }
            let Some(shadow) = root.dyn_ref::<web_sys::ShadowRoot>() else {
                return true;
            };
            let host = shadow.host();
            root = host.get_root_node();
            allowed_host = Some(host);
        }
    }

    fn observe_focus(&self) {
        if self.owns_dom_focus() || (self.replacing.get() && self.neutral_dom_focus()) {
            return;
        }
        // Do not restore after any observed external choice, even if it later blurs to BODY.
        self.pending.borrow_mut().take();
        if let Some(next) = self.outside_epoch.get().checked_add(1) {
            self.outside_epoch.set(next);
        } else {
            self.exhausted.set(true);
        }
    }

    fn capture(&self, ime: &egui::output::IMEOutput) {
        let context = FocusContext::from(ime);
        let keep_pending = self.pending.borrow().as_ref().is_some_and(|intent| {
            intent.context == context && intent.outside_epoch == self.outside_epoch.get()
        });
        let owns = self.owns_dom_focus(); // deliberately independent of document.hidden()
        *self.pending.borrow_mut() =
            (!self.exhausted.get() && (owns || keep_pending)).then_some(FocusIntent {
                context,
                outside_epoch: self.outside_epoch.get(),
            });
    }

    fn update_context(&self, ime: Option<&egui::output::IMEOutput>) {
        if self.pending.borrow().as_ref().is_some_and(|intent| {
            Some(intent.context) != ime.map(FocusContext::from)
                || intent.outside_epoch != self.outside_epoch.get()
        }) {
            self.pending.borrow_mut().take();
        }
    }
}

struct FocusWatch {
    state: Rc<FocusState>,
    session: Rc<Session>,
    handles: Vec<Listener>,
}

impl FocusWatch {
    fn retire(&mut self) -> bool {
        self.state.pending.borrow_mut().take();
        self.session.set_active(false);
        self.handles.retain(|handle| match handle.unsubscribe() {
            Ok(()) => false,
            Err(err) => {
                log::warn!(
                    "failed to remove retired focus observer: {}",
                    super::string_from_js_value(&err)
                );
                true
            }
        });
        self.handles.is_empty()
    }
}

/// `WebRunner` owns this separately from its possibly poisoned `AppRunner`. Callbacks never own
/// either runner or this controller, so normal teardown has no Rust reference cycle.
#[derive(Default)]
pub(super) struct TextAgentListeners {
    group: Option<ListenerGroup>,
    focus_watch: Option<FocusWatch>,
    pub requests: crate::web_ime_reset::ResetRequests,
}

impl TextAgentListeners {
    #[cfg(test)]
    pub(super) fn test_listener_counts(&self) -> (usize, usize) {
        (
            self.group.as_ref().map_or(0, |group| group.handles.len()),
            self.focus_watch
                .as_ref()
                .map_or(0, |watch| watch.handles.len()),
        )
    }

    fn focus_state(
        &mut self,
        canvas: &web_sys::HtmlCanvasElement,
    ) -> Result<Rc<FocusState>, JsValue> {
        if let Some(watch) = &mut self.focus_watch {
            if watch.state.canvas == *canvas && watch.session.active.get() {
                return Ok(Rc::clone(&watch.state));
            }
            if !watch.retire() {
                return Err(JsValue::from_str("previous focus observer removal failed"));
            }
            self.focus_watch = None;
        }
        let state = Rc::new(FocusState {
            canvas: canvas.clone(),
            input: RefCell::new(None),
            pending: RefCell::new(None),
            outside_epoch: Cell::new(0),
            exhausted: Cell::new(false),
            replacing: Cell::new(false),
        });
        let session = Rc::new(Session::default());
        let document = web_sys::window().unwrap().document().unwrap();
        let root = canvas.get_root_node();
        let mut targets = vec![document.clone().unchecked_into::<web_sys::EventTarget>()];
        if root != document.unchecked_into::<web_sys::Node>() {
            targets.push(root.unchecked_into::<web_sys::EventTarget>());
        }
        let mut watch = FocusWatch {
            state: Rc::clone(&state),
            session,
            handles: Vec::new(),
        };
        for target in targets {
            let state = Rc::clone(&state);
            let mut handle = Listener::new(
                &target,
                "focusin",
                Rc::downgrade(&watch.session),
                move |_: web_sys::FocusEvent| state.observe_focus(),
            );
            handle.capture = true;
            watch.handles.push(handle);
            if let Err(err) = watch.handles.last().unwrap().subscribe() {
                if !watch.retire() {
                    self.focus_watch = Some(watch);
                }
                return Err(err);
            }
        }
        watch.session.set_active(true);
        self.focus_watch = Some(watch);
        Ok(state)
    }

    /// Teardown also invalidates the independent observer without borrowing `AppRunner`.
    pub fn shutdown(&mut self) {
        self.requests.invalidate();
        if let Some(watch) = &mut self.focus_watch
            && watch.retire()
        {
            self.focus_watch = None;
        }
        self.retire();
    }

    /// Inactivate before remove/blur can synchronously dispatch anything. A removal failure
    /// retains its exact Closure and blocks attaching another group until cleanup succeeds.
    pub fn retire(&mut self) -> bool {
        let Some(group) = &mut self.group else {
            return true;
        };
        group.session.set_active(false);
        group.handles.retain(|handle| match handle.unsubscribe() {
            Ok(()) => false,
            Err(err) => {
                log::warn!(
                    "failed to remove retired text agent listener: {}",
                    super::string_from_js_value(&err)
                );
                true
            }
        });
        group.input.remove();
        if group.handles.is_empty() {
            self.group = None;
            true
        } else {
            false
        }
    }
}

impl Drop for TextAgentListeners {
    fn drop(&mut self) {
        // Failed handles were retained while alive. Even if final JS removal fails, the
        // private gate is now permanently closed before Rust releases their callback captures.
        self.shutdown();
    }
}

pub struct TextAgent {
    input: web_sys::HtmlInputElement,
    input_state: Rc<RefCell<InputState>>,
    session: Rc<Session>,
    context: TextAgentContext,
    focus: Rc<FocusState>,

    /// The `lang` attribute of `input`.
    lang: RefCell<Option<String>>,
}

impl TextAgent {
    /// Attach the initial agent, preserving startup focus without page scrolling.
    pub fn attach(
        runner_ref: &WebRunner,
        canvas: &web_sys::HtmlCanvasElement,
    ) -> Result<Self, JsValue> {
        let agent = Self::attach_unfocused(runner_ref.text_agent_context(), canvas, false)?;
        agent.focus.input.replace(Some(agent.input.clone()));
        agent.session.set_active(true);
        agent.focus();
        Ok(agent)
    }

    fn attach_unfocused(
        context: TextAgentContext,
        canvas: &web_sys::HtmlCanvasElement,
        replacement: bool,
    ) -> Result<Self, JsValue> {
        if context.listeners.borrow().group.is_some() {
            return Err(JsValue::from_str(
                "previous text-agent listeners could not be removed",
            ));
        }
        let focus = context.listeners.borrow_mut().focus_state(canvas)?;
        let document = web_sys::window().unwrap().document().unwrap();
        let input = document
            .create_element("input")?
            .dyn_into::<web_sys::HtmlInputElement>()?;
        input.set_type("text");
        input.set_attribute("autocapitalize", "off")?;
        // Keep a sibling of the canvas, including inside shadow DOM. No focus during setup.
        let style = input.style();
        for (key, value) in [
            ("background-color", "transparent"),
            ("color", "transparent"),
            ("opacity", "0"),
            ("pointer-events", "none"),
            ("border", "none"),
            ("outline", "none"),
            ("width", "1px"),
            ("height", "1px"),
            ("caret-color", "transparent"),
            ("position", "absolute"),
            ("font-size", "16px"),
        ] {
            style.set_property(key, value)?;
        }
        style.set_property("top", &format!("{}px", canvas.offset_top()))?;
        style.set_property("left", &format!("{}px", canvas.offset_left()))?;
        let input_state = Rc::new(RefCell::new(InputState::new(input.clone())));
        if replacement {
            input_state.borrow_mut().composition =
                crate::web_ime_reset::CompositionSession::replacement();
        }
        let session = Rc::new(Session::default());
        context.listeners.borrow_mut().group = Some(ListenerGroup {
            session: Rc::clone(&session),
            input: input.clone(),
            handles: Vec::new(),
        });
        let agent = Self {
            input,
            input_state,
            session,
            context,
            focus,
            lang: RefCell::new(None),
        };
        let setup = (|| {
            if let Some(parent) = canvas.parent_node() {
                parent.insert_before(&agent.input, canvas.next_sibling().as_ref())?;
            } else if let Some(body) = document.body() {
                log::warn!("Canvas has no parent element - appending text agent to document body");
                body.append_child(&agent.input)?;
            } else {
                return Err(JsValue::from_str("no parent for text agent"));
            }
            agent.listen("compositionstart", {
                let state = Rc::clone(&agent.input_state);
                move |_: web_sys::CompositionEvent, runner| {
                    state.borrow_mut().composition.start();
                    runner.needs_repaint.repaint_asap();
                }
            })?;
            agent.listen("input", {
                let state = Rc::clone(&agent.input_state);
                move |event: web_sys::InputEvent, runner| {
                    state.borrow_mut().handle_input_event(
                        &event,
                        &mut runner.input.raw.events,
                        &runner.needs_repaint,
                    );
                }
            })?;
            agent.listen("compositionend", {
                let state = Rc::clone(&agent.input_state);
                move |_: web_sys::CompositionEvent, runner| {
                    state.borrow_mut().handle_composition_end_event(
                        &mut runner.input.raw.events,
                        &runner.needs_repaint,
                    );
                }
            })?;
            agent.listen("keydown", {
                let state = Rc::clone(&agent.input_state);
                let session = Rc::clone(&agent.session);
                move |event: web_sys::KeyboardEvent, runner| {
                    if !InputState::handle_keydown_event(&state, &event) {
                        let start = runner.input.raw.events.len();
                        super::events::on_keydown(event.clone(), runner);
                        if runner.input.raw.events[start..]
                            .iter()
                            .any(|event| matches!(event, egui::Event::Key { pressed: true, .. }))
                        {
                            let mut keys = session.forwarded_keys.borrow_mut();
                            keys.retain(|old| !same_key(&old.event, &event));
                            if let Some(press) = runner.input.keys.press_id(&event) {
                                keys.push(ForwardedKey { event, press });
                            }
                        }
                    }
                }
            })?;
            agent.listen("keyup", {
                let state = Rc::clone(&agent.input_state);
                let session = Rc::clone(&agent.session);
                move |event: web_sys::KeyboardEvent, runner| {
                    if !InputState::handle_keyup_event(&state, &event) {
                        session
                            .forwarded_keys
                            .borrow_mut()
                            .retain(|old| !same_key(&old.event, &event));
                        super::events::on_keyup(event, runner);
                    }
                }
            })?;
            Ok(())
        })();
        if let Err(err) = setup {
            agent.retire();
            return Err(err);
        }
        Ok(agent)
    }

    fn listen<E: JsCast>(
        &self,
        name: &'static str,
        mut callback: impl FnMut(E, &mut AppRunner) + 'static,
    ) -> Result<(), JsValue> {
        let runner = Weak::clone(&self.context.runner);
        let panic_handler = self.context.panic_handler.clone();
        let session = Rc::downgrade(&self.session);
        let handle = Listener::new(&self.input, name, session, move |event: E| {
            if panic_handler.has_panicked() {
                return;
            }
            let Some(runner) = runner.upgrade() else {
                return;
            };
            let Ok(mut lock) = runner.try_borrow_mut() else {
                return;
            };
            let Some(runner) = lock.as_mut() else { return };
            callback(event, runner);
        });
        // Retain even if registration throws, until removal has safely succeeded.
        let mut listeners = self.context.listeners.borrow_mut();
        let group = listeners
            .group
            .as_mut()
            .ok_or_else(|| JsValue::from_str("text agent retired during setup"))?;
        group.handles.push(handle);
        group.handles.last().unwrap().subscribe()
    }

    fn retire(&self) -> bool {
        self.session.set_active(false);
        let mut listeners = self.context.listeners.borrow_mut();
        if listeners
            .group
            .as_ref()
            .is_some_and(|group| Rc::ptr_eq(&group.session, &self.session))
        {
            listeners.retire()
        } else {
            true
        }
    }

    /// Release only keys this old source actually forwarded. A discarded old-node keyup then
    /// cannot leave those keys stuck, and unrelated canvas keys/modifiers remain untouched.
    pub(super) fn take_forwarded_keys(&self) -> Vec<ForwardedKey> {
        core::mem::take(&mut *self.session.forwarded_keys.borrow_mut())
    }

    pub fn admit_reset(&self, ime: Option<&egui::output::IMEOutput>) -> Option<egui::ImeResetId> {
        self.context.listeners.borrow_mut().requests.admit(ime)
    }

    pub(crate) fn complete_reset(
        &self,
        request: egui::ImeResetId,
        ime: &egui::output::IMEOutput,
        events: &mut Vec<egui::Event>,
        repaint: &NeedRepaint,
    ) -> bool {
        if !self
            .context
            .listeners
            .borrow_mut()
            .requests
            .complete(request, ime, events)
        {
            return false;
        }
        // Queuing control alone does not schedule a subsequent raw batch, especially hidden.
        repaint.repaint_asap();
        true
    }

    pub fn fail_reset(&self) {
        self.context.listeners.borrow_mut().requests.invalidate();
        self.focus.pending.borrow_mut().take();
    }

    pub fn capture_reset_focus(&self, ime: &egui::output::IMEOutput) {
        self.focus.capture(ime);
    }

    /// Restore only a retained, still-owned context after visibility returns. This does not
    /// pretend a failed browser focus call provides a virtual keyboard or platform IME proof.
    pub fn restore_pending_focus(&self) {
        if self.focus.exhausted.get() {
            return;
        }
        let valid = self.focus.pending.borrow().as_ref().is_some_and(|intent| {
            intent.outside_epoch == self.focus.outside_epoch.get()
                && self
                    .input_state
                    .borrow()
                    .ime_output
                    .as_ref()
                    .map(FocusContext::from)
                    == Some(intent.context)
        });
        if !valid {
            self.focus.pending.borrow_mut().take();
            return;
        }
        if web_sys::window().unwrap().document().unwrap().hidden() {
            return;
        }
        if !self.focus.owns_dom_focus() && !self.focus.neutral_dom_focus() {
            self.focus.pending.borrow_mut().take();
            return;
        }
        self.focus();
        if self.has_focus() {
            self.focus.pending.borrow_mut().take();
        }
    }

    /// Configure a new inactive source completely before activation/focus or completion.
    pub fn replace_for_reset(
        &mut self,
        ime: egui::output::IMEOutput,
        canvas: &web_sys::HtmlCanvasElement,
        zoom_factor: f32,
        lang: Option<&str>,
    ) -> Result<(), JsValue> {
        self.focus.replacing.set(true);
        let result = (|| {
            if !self.retire() {
                return Err(JsValue::from_str(
                    "retired text-agent listener removal failed",
                ));
            }
            let replacement = Self::attach_unfocused(self.context.clone(), canvas, true)?;
            replacement.try_set_lang(lang)?;
            replacement.update(Some(ime), canvas, zoom_factor)?;
            replacement
                .focus
                .input
                .replace(Some(replacement.input.clone()));
            replacement.session.set_active(true);
            *self = replacement;
            Ok(())
        })();
        self.focus.replacing.set(false);
        if result.is_err() {
            self.focus.pending.borrow_mut().take();
        }
        result
    }

    pub fn update(
        &self,
        ime: Option<egui::output::IMEOutput>,
        canvas: &web_sys::HtmlCanvasElement,
        zoom_factor: f32,
    ) -> Result<(), JsValue> {
        self.focus.update_context(ime.as_ref());
        self.input_state
            .borrow_mut()
            .update(ime, canvas, zoom_factor)
    }

    fn try_set_lang(&self, lang: Option<&str>) -> Result<(), JsValue> {
        let mut current = self.lang.borrow_mut();
        if current.as_deref() == lang {
            return Ok(());
        }
        match lang {
            Some(lang) => self.input.set_attribute("lang", lang)?,
            None => self.input.remove_attribute("lang")?,
        }
        *current = lang.map(ToOwned::to_owned);
        Ok(())
    }

    pub fn set_lang(&self, lang: Option<&str>) {
        if let Err(err) = self.try_set_lang(lang) {
            log::error!(
                "failed to set text agent lang: {}",
                super::string_from_js_value(&err)
            );
        }
    }

    #[cfg(test)]
    pub(super) fn test_keyup_callback(&self) -> js_sys::Function {
        self.context
            .listeners
            .borrow()
            .group
            .as_ref()
            .unwrap()
            .handles
            .iter()
            .find(|handle| handle.name == "keyup")
            .unwrap()
            .callback
            .clone()
    }

    #[cfg(test)]
    pub(super) fn test_input(&self) -> web_sys::HtmlInputElement {
        self.input.clone()
    }

    #[cfg(test)]
    pub(super) fn test_has_restore_intent(&self) -> bool {
        self.focus.pending.borrow().is_some()
    }

    pub fn set_focus(&self, on: bool) {
        if on {
            self.focus();
        } else {
            self.blur();
        }
    }

    pub fn has_focus(&self) -> bool {
        super::has_focus(&self.input)
    }

    pub fn focus(&self) {
        if !self.session.active.get() || self.has_focus() {
            return;
        }
        if let Err(err) = super::focus_without_scroll(&self.input) {
            log::error!("failed to set focus: {}", super::string_from_js_value(&err));
        }
    }

    pub fn blur(&self) {
        if !self.has_focus() {
            return;
        }
        if let Err(err) = self.input.blur() {
            log::error!(
                "failed to remove focus: {}",
                super::string_from_js_value(&err)
            );
        }
        self.input_state.borrow_mut().clear();
    }

    pub(crate) fn interrupt_ime_composition(&self) {
        self.input_state.borrow_mut().clear();
    }
}

impl Drop for TextAgent {
    fn drop(&mut self) {
        self.retire();
    }
}

fn same_key(a: &web_sys::KeyboardEvent, b: &web_sys::KeyboardEvent) -> bool {
    if a.code().is_empty() || b.code().is_empty() {
        a.key() == b.key()
    } else {
        a.code() == b.code()
    }
}

struct InputState {
    input: web_sys::HtmlInputElement,
    last_text: String,
    ime_output: Option<egui::output::IMEOutput>,

    /// The last caret position written to the DOM, independent of text/owner/selection.
    caret_position: CaretPositionTracker,
    keydown_special_case: KeydownSpecialCase,
    composition: crate::web_ime_reset::CompositionSession,
}

#[derive(Clone, Copy)]
enum KeydownSpecialCase {
    None,

    /// On Android Gboard 14.7.09, when suggestions remain visible while typing
    /// letters without IME composition (e.g., Latin or Cyrillic), pressing
    /// Backspace produces key code 229 instead of the expected Backspace key
    /// code.
    /// Without the workaround, users have to press Backspace twice before text
    /// starts being deleted.
    ///
    /// This workaround is also required for Android Gboard corrections and
    /// completions (e.g., `tex|` -> `Texas`) to work correctly. In these
    /// cases, a `deleteContentBackward` input event fires first (e.g., to
    /// delete `tex`), followed by an `insertText` input event (e.g., to insert
    /// `Texas`).
    ///
    /// Since it is difficult to distinguish between a Backspace press and a
    /// correction or completion (e.g., when the state is `t|`, it is unclear
    /// whether the user wants to delete `t` or replace it with `Texas`), we
    /// send a `DeleteSurrounding` IME event in all cases instead of
    /// synthetically generating Backspace press and release events.
    AndroidKeycode229,

    /// iOS (18.6)'s built-in Korean keyboard uses `deleteContentBackward` to
    /// compose Hangul characters. In these cases, the key code is 0.
    IosKeycode0,
}

impl InputState {
    fn new(input: web_sys::HtmlInputElement) -> Self {
        Self {
            input,
            last_text: String::new(),
            ime_output: None,
            caret_position: CaretPositionTracker::default(),
            keydown_special_case: KeydownSpecialCase::None,
            composition: Default::default(),
        }
    }

    fn update(
        &mut self,
        ime: Option<egui::output::IMEOutput>,
        canvas: &web_sys::HtmlCanvasElement,
        zoom_factor: f32,
    ) -> Result<(), JsValue> {
        let native_ppp = super::native_pixels_per_point();
        let canvas_metrics = CanvasMetrics {
            width: canvas.width(),
            height: canvas.height(),
            offset_left: canvas.offset_left(),
            offset_top: canvas.offset_top(),
            native_pixels_per_point: native_ppp,
        };
        let geometry = ime
            .as_ref()
            .map(|ime| CaretGeometry::from_ime(ime, canvas_metrics, zoom_factor));
        let position_to_write = self
            .caret_position
            .update(geometry.map(CaretPositionKey::new));

        // Keep the complete descriptor for IME lifecycle decisions, independently of whether
        // this pass needs to move the hidden input. A text/selection/owner change alone does
        // not touch the DOM style.
        self.ime_output = ime;
        if position_to_write.is_none() {
            return Ok(());
        }
        let Some(ime) = self.ime_output.as_ref() else {
            return Ok(());
        };

        // NOTE: we don't set the input's `type` to `password` based on
        // `ime.purpose`, because that would confuse some password managers.
        // For example, Chrome's password manager will always think the last
        // letter typed in the password field is the password.

        let style = self.input.style();

        // The input is a sibling of the canvas (see `attach`), so we position
        // it relative to the same containing block using the canvas offset.
        // Unlike `get_bounding_client_rect`, the offset is unaffected by page
        // scrolling, and doesn't flap when the virtual keyboard is shown on
        // mobile Safari.

        // Clamp the input position within the canvas width to prevent unwanted horizontal scrolling.
        let logical_canvas_width = canvas.width() as f32 / native_ppp;
        let visible_x = ime.cursor_rect.center().x * zoom_factor;
        let clamped_x = visible_x.clamp(0.0, logical_canvas_width);

        // Clamp the input position within the canvas height to prevent unwanted vertical scrolling.
        let logical_canvas_height = canvas.height() as f32 / native_ppp;
        let visible_y = ime.cursor_rect.center().y * zoom_factor;
        let clamped_y = visible_y.clamp(0.0, logical_canvas_height);

        // This is where the IME input will point to:
        style.set_property(
            "left",
            &format!("{}px", canvas.offset_left() as f32 + clamped_x),
        )?;
        style.set_property(
            "top",
            &format!("{}px", canvas.offset_top() as f32 + clamped_y),
        )?;

        Ok(())
    }

    fn clear(&mut self) {
        self.input.set_value("");
        self.last_text.clear();
        self.composition.clear();
    }

    fn handle_input_event(
        &mut self,
        event: &web_sys::InputEvent,
        events: &mut Vec<egui::Event>,
        repaint: &NeedRepaint,
    ) {
        // A replacement agent must observe its own compositionstart. Scrub the rejected DOM
        // value as well as the diff baseline, including before the password early branch.
        if !self.composition.input(event.is_composing()) {
            self.clear();
            return;
        }
        if self
            .ime_output
            .as_ref()
            .is_some_and(|ime| ime.purpose == egui::IMEPurpose::Password)
        {
            self.handle_input_event_password(event, events);
            return;
        }

        let input_type = event.input_type();

        if !event.is_composing()
            && input_type != "insertText"
            // iOS uses this for corrections and completions (e.g., `tex|` ->
            // `Texas`).
            && input_type != "insertReplacementText"
            && (matches!(self.keydown_special_case, KeydownSpecialCase::None)
                || input_type != "deleteContentBackward")
        {
            self.clear();

            return;
        }

        let text = self.input.value();

        let prefix_len = longest_common_prefix_length(&text, &self.last_text);
        let last_text_len = self.last_text.chars().count();
        if prefix_len < last_text_len {
            let out_event = egui::Event::Ime(egui::ImeEvent::DeleteSurrounding {
                before_chars: last_text_len - prefix_len,
                after_chars: 0,
            });
            events.push(out_event);
        }

        let preedit_text: String = text.chars().skip(prefix_len).collect();
        let out_event = if event.is_composing() {
            // We handle the composition update here instead of in a
            // `compositionupdate` event because the selection range
            // has not yet been updated when `compositionupdate` fires.
            let active_range_chars = self.active_range_chars(&text, prefix_len);
            egui::Event::Ime(egui::ImeEvent::Preedit {
                text: preedit_text,
                active_range_chars,
            })
        } else {
            egui::Event::Text(preedit_text)
        };
        events.push(out_event);

        if event.is_composing() {
            self.last_text = text.chars().take(prefix_len).collect();
        } else {
            self.last_text = text;
        }

        repaint.repaint_asap();
    }

    fn handle_input_event_password(
        &mut self,
        event: &web_sys::InputEvent,
        events: &mut Vec<egui::Event>,
    ) {
        let input_type = event.input_type();

        if input_type != "insertText" {
            return;
        }

        let text = self.input.value();

        events.push(egui::Event::Text(text));
        self.clear();
    }

    /// Compute the active range (cursor or conversion segment) within the
    /// preedit text, based on the selection in the input element.
    ///
    /// `text` is the full `input.value()`, and `prefix_len_chars` is the
    /// number of chars at the start of `text` that are committed (not part
    /// of the preedit). `selectionStart`/`selectionEnd` are UTF-16 offsets
    /// within the full `input.value()`, so they are adjusted to be relative
    /// to the preedit text.
    fn active_range_chars(
        &self,
        text: &str,
        prefix_len_chars: usize,
    ) -> Option<core::ops::Range<usize>> {
        let selection_start = self.input.selection_start().unwrap_or(None)? as usize;
        let selection_end = self.input.selection_end().unwrap_or(None)? as usize;

        let text_utf16 = text.encode_utf16().collect::<Vec<u16>>();
        if selection_start > text_utf16.len() || selection_end > text_utf16.len() {
            // This can occur on Android Chrome. see discussion in:
            // <https://github.com/emilk/egui/pull/8045>.
            return None;
        }

        let text_before_selection = String::from_utf16_lossy(&text_utf16[..selection_start]);
        let text_in_selection =
            String::from_utf16_lossy(&text_utf16[selection_start..selection_end]);
        let count_before_selection = text_before_selection.chars().count();
        let count_in_selection = text_in_selection.chars().count();

        // Adjust for the committed prefix to get the range within the preedit text.
        let start = count_before_selection.saturating_sub(prefix_len_chars);
        let end = start + count_in_selection;
        Some(start..end)
    }

    fn handle_composition_end_event(
        &mut self,
        events: &mut Vec<egui::Event>,
        repaint: &NeedRepaint,
    ) {
        if !self.composition.end() {
            self.clear();
            return;
        }
        let text = self.input.value();

        let commit_text = {
            let prefix_len = self.last_text.chars().count();
            text.chars().skip(prefix_len).collect::<String>()
        };
        let out_event = egui::Event::Ime(egui::ImeEvent::Commit(commit_text));
        events.push(out_event);

        self.last_text = text;

        repaint.repaint_asap();
    }

    /// ## Returns
    /// Whether the event is consumed. If `true`, the caller should not do
    /// further processing for this event.
    fn handle_keydown_event(input_state: &RefCell<Self>, event: &web_sys::KeyboardEvent) -> bool {
        // Platform-sniffing methods are unreliable, so they are not used as
        // guards here.
        let special_case = match event.key_code() {
            229 => KeydownSpecialCase::AndroidKeycode229,
            0 => KeydownSpecialCase::IosKeycode0,
            _ => KeydownSpecialCase::None,
        };
        input_state.borrow_mut().keydown_special_case = special_case;

        // https://web.archive.org/web/20200526195704/https://www.fxsitecompat.dev/en-CA/docs/2018/keydown-and-keyup-events-are-now-fired-during-ime-composition/
        if event.is_composing() || !matches!(special_case, KeydownSpecialCase::None) {
            true
        } else {
            if crate::web_keys::is_named_key(&event.key())
                || event.ctrl_key()
                || event.alt_key()
                || event.meta_key()
            {
                input_state.borrow_mut().clear();
            }
            false
        }
    }

    /// ## Returns
    /// Whether the event is consumed. If `true`, the caller should not do
    /// further processing for this event.
    fn handle_keyup_event(input_state: &RefCell<Self>, event: &web_sys::KeyboardEvent) -> bool {
        input_state.borrow_mut().keydown_special_case = KeydownSpecialCase::None;

        // https://web.archive.org/web/20200526195704/https://www.fxsitecompat.dev/en-CA/docs/2018/keydown-and-keyup-events-are-now-fired-during-ime-composition/
        event.is_composing() || event.key_code() == 229
    }
}

fn longest_common_prefix_length(a: &str, b: &str) -> usize {
    core::iter::zip(a.chars(), b.chars())
        .take_while(|(a, b)| a == b)
        .count()
}

#[cfg(test)]
mod browser_tests {
    use super::*;

    // Actual DOM events and injected DOM API failures, without changing production features or
    // production dependencies. The standard wasm-bindgen-test runner discovers these
    // individual tests; none is part of an ordinary production build.
    #[wasm_bindgen(
        inline_js = "export function egui_probe_input(kind, composing) { return new InputEvent('input', { inputType: kind, isComposing: composing }); }\nexport function egui_probe_remove_failure(input, name) { if (name) input.removeEventListener = function(type, callback) { if (type === name) throw new Error('injected remove failure'); return EventTarget.prototype.removeEventListener.call(this, type, callback); }; else delete input.removeEventListener; }\nlet eguiProbeOriginalAdd; export function egui_probe_add_failure(n) { if (eguiProbeOriginalAdd) { HTMLInputElement.prototype.addEventListener = eguiProbeOriginalAdd; eguiProbeOriginalAdd = undefined; } if (n) { eguiProbeOriginalAdd = HTMLInputElement.prototype.addEventListener; let count = 0; HTMLInputElement.prototype.addEventListener = function(...args) { if (++count === n) throw new Error('injected add failure'); return eguiProbeOriginalAdd.apply(this, args); }; } }"
    )]
    extern "C" {
        fn egui_probe_input(kind: &str, composing: bool) -> web_sys::InputEvent;

        fn egui_probe_remove_failure(input: &web_sys::EventTarget, name: Option<&str>);

        fn egui_probe_add_failure(n: u32);
    }

    struct Fixture {
        input: web_sys::HtmlInputElement,
        state: Rc<RefCell<InputState>>,
        events: Rc<RefCell<Vec<egui::Event>>>,
        owner: TextAgentListeners,
    }

    impl Fixture {
        fn new(password: bool) -> Self {
            let document = web_sys::window().unwrap().document().unwrap();
            let input = document
                .create_element("input")
                .unwrap()
                .dyn_into::<web_sys::HtmlInputElement>()
                .unwrap();
            document.body().unwrap().append_child(&input).unwrap();
            let state = Rc::new(RefCell::new(InputState::new(input.clone())));
            state.borrow_mut().composition =
                crate::web_ime_reset::CompositionSession::replacement();
            state.borrow_mut().ime_output = Some(egui::output::IMEOutput::new(
                egui::Id::unique("DOM test"),
                1,
                if password {
                    egui::IMEPurpose::Password
                } else {
                    egui::IMEPurpose::Normal
                },
                egui::Rect::ZERO,
                egui::Rect::ZERO,
            ));
            let session = Rc::new(Session::default());
            let events = Rc::new(RefCell::new(Vec::new()));
            let repaint = Rc::new(NeedRepaint::new(None));
            let mut handles = Vec::new();
            handles.push(Listener::new(
                &input,
                "compositionstart",
                Rc::downgrade(&session),
                {
                    let state = Rc::clone(&state);
                    move |_: web_sys::CompositionEvent| state.borrow_mut().composition.start()
                },
            ));
            handles.push(Listener::new(&input, "input", Rc::downgrade(&session), {
                let (state, events, repaint) =
                    (Rc::clone(&state), Rc::clone(&events), Rc::clone(&repaint));
                move |event: web_sys::InputEvent| {
                    state.borrow_mut().handle_input_event(
                        &event,
                        &mut events.borrow_mut(),
                        &repaint,
                    );
                }
            }));
            handles.push(Listener::new(
                &input,
                "compositionend",
                Rc::downgrade(&session),
                {
                    let (state, events, repaint) =
                        (Rc::clone(&state), Rc::clone(&events), Rc::clone(&repaint));
                    move |_: web_sys::CompositionEvent| {
                        state
                            .borrow_mut()
                            .handle_composition_end_event(&mut events.borrow_mut(), &repaint);
                    }
                },
            ));
            for handle in &handles {
                handle.subscribe().unwrap();
            }
            session.set_active(true);
            let owner = TextAgentListeners {
                group: Some(ListenerGroup {
                    session,
                    input: input.clone(),
                    handles,
                }),
                focus_watch: None,
                requests: Default::default(),
            };
            Self {
                input,
                state,
                events,
                owner,
            }
        }

        fn input(&self, text: &str, composing: bool, kind: &str) {
            self.input.set_value(text);
            self.input
                .dispatch_event(&egui_probe_input(kind, composing))
                .unwrap();
        }

        fn composition(&self, name: &str) {
            self.input
                .dispatch_event(&web_sys::CompositionEvent::new(name).unwrap())
                .unwrap();
        }
    }

    #[wasm_bindgen_test::wasm_bindgen_test]
    fn orphan_dom_value_is_scrubbed_before_direct_input_and_fresh_multibyte_composition() {
        for password in [false, true] {
            let fixture = Fixture::new(password);
            fixture.input("obsolete", true, "insertCompositionText");
            assert!(fixture.events.borrow().is_empty());
            assert_eq!(fixture.input.value(), "");
            assert_eq!(fixture.state.borrow().last_text, "");
            fixture.input("n", false, "insertText");
            assert_eq!(&*fixture.events.borrow(), &[egui::Event::Text("n".into())]);
            fixture.events.borrow_mut().clear();
            fixture.input.set_value("orphan end");
            fixture.composition("compositionend");
            assert!(fixture.events.borrow().is_empty());
            assert_eq!(fixture.input.value(), "");
            fixture.composition("compositionstart");
            fixture.input("😀語", true, "insertCompositionText");
            fixture.input.set_selection_range(2, 3).unwrap();
            fixture
                .input
                .dispatch_event(&egui_probe_input("insertCompositionText", true))
                .unwrap();
            if !password {
                assert!(fixture.events.borrow().iter().any(|event| matches!(event, egui::Event::Ime(egui::ImeEvent::Preedit { text, active_range_chars: Some(range) }) if text == "😀語" && *range == (1..2))));
            }
            fixture.composition("compositionend");
            let committed = fixture
                .events
                .borrow()
                .iter()
                .filter_map(|event| match event {
                    egui::Event::Ime(egui::ImeEvent::Commit(text)) | egui::Event::Text(text)
                        if !text.is_empty() =>
                    {
                        Some(text.clone())
                    }
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(committed, ["😀語"]);
        }
    }

    #[wasm_bindgen_test::wasm_bindgen_test]
    fn captured_retired_js_callback_is_safe_after_successful_or_failed_rust_teardown() {
        for fail in [false, true] {
            let mut fixture = Fixture::new(false);
            fixture.input("accepted", false, "insertText");
            let before = fixture.events.borrow().clone();
            let callback = fixture.owner.group.as_ref().unwrap().handles[1]
                .callback
                .clone();
            let input = fixture.input.clone();
            let events = Rc::clone(&fixture.events);
            if fail {
                egui_probe_remove_failure(&input, Some("input"));
            }
            assert_eq!(fixture.owner.retire(), !fail);
            if fail {
                assert_eq!(
                    fixture.owner.group.as_ref().unwrap().handles.len(),
                    1,
                    "retain exactly the failed handle"
                );
            }
            drop(fixture); // Releases the actual Rust Closure, including on removal failure.
            input.set_value("late");
            callback
                .call1(
                    &JsValue::NULL,
                    egui_probe_input("insertText", false).as_ref(),
                )
                .unwrap();
            input
                .dispatch_event(&egui_probe_input("insertText", false))
                .unwrap();
            assert_eq!(
                &*events.borrow(),
                &before,
                "retired wrappers never enter a released Rust callback"
            );
            egui_probe_remove_failure(&input, None);
        }
    }

    #[wasm_bindgen_test::wasm_bindgen_test]
    fn direct_replacement_and_mobile_delete_correction_remain_valid_on_fresh_source() {
        let fixture = Fixture::new(false);
        fixture.input("tex", false, "insertText");
        for special in [
            KeydownSpecialCase::AndroidKeycode229,
            KeydownSpecialCase::IosKeycode0,
        ] {
            fixture.state.borrow_mut().keydown_special_case = special;
            fixture.input("", false, "deleteContentBackward");
            fixture.input("Texas", false, "insertReplacementText");
            assert!(fixture.events.borrow().iter().any(|event| matches!(event, egui::Event::Ime(egui::ImeEvent::DeleteSurrounding { before_chars, after_chars: 0 }) if *before_chars > 0)));
            assert!(
                matches!(fixture.events.borrow().last(), Some(egui::Event::Text(text)) if text == "Texas")
            );
            fixture.events.borrow_mut().clear();
        }
    }

    #[wasm_bindgen_test::wasm_bindgen_test]
    fn partial_registration_rolls_back_and_real_replacements_keep_one_group() {
        let document = web_sys::window().unwrap().document().unwrap();
        let canvas = document
            .create_element("canvas")
            .unwrap()
            .dyn_into::<web_sys::HtmlCanvasElement>()
            .unwrap();
        document.body().unwrap().append_child(&canvas).unwrap();
        let listeners = Rc::new(RefCell::new(TextAgentListeners::default()));
        let context = TextAgentContext {
            runner: Weak::new(),
            panic_handler: PanicHandler::install(),
            listeners: Rc::clone(&listeners),
        };
        for nth in 1..=5 {
            egui_probe_add_failure(nth);
            let result = TextAgent::attach_unfocused(context.clone(), &canvas, true);
            egui_probe_add_failure(0); // Restore before assertions, including expected failure.
            assert!(result.is_err());
            assert!(listeners.borrow().group.is_none());
        }
        let mut agent = TextAgent::attach_unfocused(context.clone(), &canvas, true).unwrap();
        let ctx = egui::Context::default();
        let mut events = Vec::new();
        let repaint = NeedRepaint::new(None);
        for name in [
            "compositionstart",
            "input",
            "compositionend",
            "keydown",
            "keyup",
        ] {
            let old = agent.input.clone();
            let callback = listeners
                .borrow()
                .group
                .as_ref()
                .unwrap()
                .handles
                .iter()
                .find(|handle| handle.name == name)
                .unwrap()
                .callback
                .clone();
            let mut ime = egui::output::IMEOutput::new(
                egui::Id::unique("failure editor"),
                1,
                egui::IMEPurpose::Normal,
                egui::Rect::ZERO,
                egui::Rect::ZERO,
            );
            let request = ctx.request_ime_reset(egui::ViewportId::ROOT).unwrap();
            ime.reset_request = Some(request);
            assert_eq!(agent.admit_reset(Some(&ime)), Some(request));
            egui_probe_remove_failure(&old, Some(name));
            let result = agent.replace_for_reset(ime.clone(), &canvas, 1.0, Some("ja"));
            assert!(result.is_err());
            assert_eq!(listeners.borrow().group.as_ref().unwrap().handles.len(), 1);
            assert!(
                TextAgent::attach_unfocused(context.clone(), &canvas, true).is_err(),
                "failed removal blocks a second group"
            );
            agent.fail_reset(); // same failure branch as AppRunner
            assert!(!agent.complete_reset(request, &ime, &mut events, &repaint));
            assert!(events.is_empty());
            egui_probe_remove_failure(&old, None);
            assert!(listeners.borrow_mut().retire());
            callback
                .call1(&JsValue::NULL, web_sys::Event::new(name).unwrap().as_ref())
                .unwrap();
            agent
                .replace_for_reset(ime, &canvas, 1.0, Some("ja"))
                .unwrap();
        }
        for _ in 0..100 {
            let old = agent.input.clone();
            let callback = listeners.borrow().group.as_ref().unwrap().handles[1]
                .callback
                .clone();
            let mut ime = egui::output::IMEOutput::new(
                egui::Id::unique("DOM editor"),
                1,
                egui::IMEPurpose::Password,
                egui::Rect::ZERO,
                egui::Rect::ZERO,
            );
            let request = ctx.request_ime_reset(egui::ViewportId::ROOT).unwrap();
            ime.reset_request = Some(request);
            assert_eq!(agent.admit_reset(Some(&ime)), Some(request));
            agent
                .replace_for_reset(ime.clone(), &canvas, 1.0, Some("ja"))
                .unwrap();
            repaint.clear();
            assert!(agent.complete_reset(request, &ime, &mut events, &repaint));
            assert_eq!(
                repaint.when_to_repaint(),
                f64::NEG_INFINITY,
                "even no-composition reset explicitly wakes the host"
            );
            repaint.clear();
            assert!(!agent.complete_reset(request, &ime, &mut events, &repaint));
            assert_eq!(repaint.when_to_repaint(), f64::INFINITY);
            assert_eq!(agent.input.get_attribute("lang").as_deref(), Some("ja"));
            assert_eq!(agent.input.type_(), "text");
            assert_eq!(
                agent
                    .input_state
                    .borrow()
                    .ime_output
                    .as_ref()
                    .unwrap()
                    .purpose,
                egui::IMEPurpose::Password
            );
            assert!(old.parent_node().is_none());
            assert_eq!(listeners.borrow().group.as_ref().unwrap().handles.len(), 5);
            callback
                .call1(
                    &JsValue::NULL,
                    egui_probe_input("insertText", false).as_ref(),
                )
                .unwrap();
        }
        assert_eq!(events.len(), 100);
        assert!(
            events
                .iter()
                .all(|event| matches!(event, egui::Event::ImeResetCompleted { .. }))
        );
        drop(agent);
        assert!(listeners.borrow().group.is_none());
        canvas.remove();
    }

    #[wasm_bindgen_test::wasm_bindgen_test]
    fn fresh_terminal_input_and_end_orders_commit_exactly_once() {
        for end_first in [false, true] {
            let fixture = Fixture::new(false);
            fixture.composition("compositionstart");
            fixture.input("に", true, "insertCompositionText");
            if end_first {
                fixture.composition("compositionend");
                fixture.input("に", false, "insertText");
            } else {
                fixture.input("に", false, "insertText");
                fixture.composition("compositionend");
            }
            fixture.input("にx", false, "insertText");
            let committed = fixture
                .events
                .borrow()
                .iter()
                .filter_map(|event| match event {
                    egui::Event::Text(text) | egui::Event::Ime(egui::ImeEvent::Commit(text))
                        if !text.is_empty() =>
                    {
                        Some(text.clone())
                    }
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(committed, ["に", "x"]);
        }
    }

    #[wasm_bindgen_test::wasm_bindgen_test]
    fn focus_observer_retirement_preserves_safe_captured_js_callback_on_remove_failure() {
        for fail in [false, true] {
            let document = web_sys::window().unwrap().document().unwrap();
            let canvas = document
                .create_element("canvas")
                .unwrap()
                .dyn_into::<web_sys::HtmlCanvasElement>()
                .unwrap();
            document.body().unwrap().append_child(&canvas).unwrap();
            let listeners = Rc::new(RefCell::new(TextAgentListeners::default()));
            let context = TextAgentContext {
                runner: Weak::new(),
                panic_handler: PanicHandler::install(),
                listeners: Rc::clone(&listeners),
            };
            let agent = TextAgent::attach_unfocused(context.clone(), &canvas, true).unwrap();
            let callback = listeners.borrow().focus_watch.as_ref().unwrap().handles[0]
                .callback
                .clone();
            let target = listeners.borrow().focus_watch.as_ref().unwrap().handles[0]
                .target
                .clone();
            if fail {
                egui_probe_remove_failure(&target, Some("focusin"));
            }
            listeners.borrow_mut().shutdown();
            assert!(!agent.test_has_restore_intent());
            if fail {
                assert_eq!(
                    listeners
                        .borrow()
                        .focus_watch
                        .as_ref()
                        .unwrap()
                        .handles
                        .len(),
                    1
                );
                assert!(TextAgent::attach_unfocused(context.clone(), &canvas, true).is_err());
            } else {
                assert!(listeners.borrow().focus_watch.is_none());
            }
            drop(agent);
            drop(context);
            drop(listeners); // Actual Rust Closure release, including failed final JS removal.
            callback
                .call1(
                    &JsValue::NULL,
                    web_sys::Event::new("focusin").unwrap().as_ref(),
                )
                .unwrap();
            egui_probe_remove_failure(&target, None);
            target
                .remove_event_listener_with_callback_and_bool("focusin", &callback, true)
                .unwrap();
            canvas.remove();
        }
    }
}
