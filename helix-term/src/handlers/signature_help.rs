use std::sync::Arc;
use std::time::Duration;

use helix_core::syntax::config::LanguageServerFeature;
use helix_event::{cancelable_future, register_hook, send_blocking, TaskController, TaskHandle};
use helix_lsp::lsp::{self, SignatureInformation};
use helix_stdx::rope::RopeSliceExt;
use helix_view::document::Mode;
use helix_view::events::{DocumentDidChange, SelectionDidChange};
use helix_view::handlers::lsp::{SignatureHelpEvent, SignatureHelpInvoked};
use helix_view::Editor;
use tokio::sync::mpsc::Sender;
use tokio::time::Instant;

use crate::commands::Open;
use crate::compositor::Compositor;
use crate::events::{OnModeSwitch, PostInsertChar};
use crate::handlers::Handlers;
use crate::ui::lsp::signature_help::{Signature, SignatureHelp};
use crate::ui::Popup;
use crate::{job, ui};

#[derive(Debug, PartialEq, Eq)]
enum State {
    Open,
    Closed,
    Pending,
}

/// debounce timeout in ms, value taken from VSCode
/// TODO: make this configurable?
const TIMEOUT: u64 = 120;

#[derive(Debug)]
pub(super) struct SignatureHelpHandler {
    trigger: Option<SignatureHelpInvoked>,
    state: State,
    task_controller: TaskController,
}

impl SignatureHelpHandler {
    pub fn new() -> SignatureHelpHandler {
        SignatureHelpHandler {
            trigger: None,
            state: State::Closed,
            task_controller: TaskController::new(),
        }
    }
}

impl helix_event::AsyncHook for SignatureHelpHandler {
    type Event = SignatureHelpEvent;

    fn handle_event(
        &mut self,
        event: Self::Event,
        timeout: Option<tokio::time::Instant>,
    ) -> Option<Instant> {
        match event {
            SignatureHelpEvent::Invoked => {
                self.trigger = Some(SignatureHelpInvoked::Manual);
                self.state = State::Closed;
                self.finish_debounce();
                return None;
            }
            SignatureHelpEvent::Trigger => {}
            SignatureHelpEvent::ReTrigger => {
                // don't retrigger if we aren't open/pending yet
                if matches!(self.state, State::Closed) {
                    return timeout;
                }
            }
            SignatureHelpEvent::Cancel => {
                self.state = State::Closed;
                return None;
            }
            SignatureHelpEvent::RequestComplete { open } => {
                // don't cancel rerequest that was already triggered
                if self.state == State::Pending && self.task_controller.is_running() {
                    return timeout;
                }
                self.state = if open { State::Open } else { State::Closed };
                self.task_controller.cancel();

                return timeout;
            }
        }
        if self.trigger.is_none() {
            self.trigger = Some(SignatureHelpInvoked::Automatic)
        }
        Some(Instant::now() + Duration::from_millis(TIMEOUT))
    }

    fn finish_debounce(&mut self) {
        let invocation = self.trigger.take().unwrap();
        self.state = State::Pending;
        let handle = self.task_controller.restart();
        job::dispatch_blocking(move |editor, _| request_signature_help(editor, invocation, handle))
    }
}

pub fn request_signature_help(
    editor: &mut Editor,
    invoked: SignatureHelpInvoked,
    cancel: TaskHandle,
) {
    let (view, doc) = current!(editor);

    // TODO merge multiple language server signature help into one instead of just taking the first language server that supports it
    let future = doc
        .language_servers_with_feature(LanguageServerFeature::SignatureHelp)
        .find_map(|language_server| {
            let pos = doc.position(view.id, language_server.offset_encoding());
            language_server.text_document_signature_help(doc.identifier(), pos, None)
        });

    let Some(future) = future else {
        // Do not show the message if signature help was invoked
        // automatically on backspace, trigger characters, etc.
        if invoked == SignatureHelpInvoked::Manual {
            editor.set_error("No configured language server supports signature-help");
        }
        return;
    };

    tokio::spawn(async move {
        match cancelable_future(future, cancel).await {
            Some(Ok(res)) => {
                job::dispatch(move |editor, compositor| {
                    show_signature_help(editor, compositor, invoked, res)
                })
                .await
            }
            Some(Err(err)) => log::error!("signature help request failed: {err}"),
            None => (),
        }
    });
}

fn active_param_range(
    signature: &SignatureInformation,
    response_active_parameter: Option<u32>,
) -> Option<(usize, usize)> {
    let param_idx = signature
        .active_parameter
        .or(response_active_parameter)
        .unwrap_or(0) as usize;
    let param = signature.parameters.as_ref()?.get(param_idx)?;
    match &param.label {
        lsp::ParameterLabel::Simple(string) => {
            let start = signature.label.find(string.as_str())?;
            Some((start, start + string.len()))
        }
        lsp::ParameterLabel::LabelOffsets([start, end]) => {
            // LS sends offsets based on utf-16 based string representation
            // but highlighting in helix is done using byte offset.
            use helix_core::str_utils::char_to_byte_idx;
            let from = char_to_byte_idx(&signature.label, *start as usize);
            let to = char_to_byte_idx(&signature.label, *end as usize);
            Some((from, to))
        }
    }
}

pub fn show_signature_help(
    editor: &mut Editor,
    compositor: &mut Compositor,
    invoked: SignatureHelpInvoked,
    response: Option<lsp::SignatureHelp>,
) {
    let config = &editor.config();

    if !(config.lsp.auto_signature_help
        || SignatureHelp::visible_popup(compositor).is_some()
        || invoked == SignatureHelpInvoked::Manual)
    {
        return;
    }

    // If the signature help invocation is automatic, don't show it outside of Insert Mode:
    // it very probably means the server was a little slow to respond and the user has
    // already moved on to something else, making a signature help popup will just be an
    // annoyance, see https://github.com/helix-editor/helix/issues/3112
    // For the most part this should not be needed as the request gets canceled automatically now
    // but it's technically possible for the mode change to just preempt this callback so better safe than sorry
    if invoked == SignatureHelpInvoked::Automatic && editor.mode != Mode::Insert {
        return;
    }

    let response = match response {
        // According to the spec the response should be None if there
        // are no signatures, but some servers don't follow this.
        Some(s) if !s.signatures.is_empty() => s,
        _ => {
            send_blocking(
                &editor.handlers.signature_hints,
                SignatureHelpEvent::RequestComplete { open: false },
            );
            compositor.remove(SignatureHelp::ID);
            return;
        }
    };
    send_blocking(
        &editor.handlers.signature_hints,
        SignatureHelpEvent::RequestComplete { open: true },
    );

    let doc = doc!(editor);
    let language = doc.language_name().unwrap_or("");

    if response.signatures.is_empty() {
        return;
    }

    let signatures: Vec<Signature> = response
        .signatures
        .into_iter()
        .map(|s| {
            let active_param_range = active_param_range(&s, response.active_parameter);

            let signature_doc = if config.lsp.display_signature_help_docs {
                s.documentation.map(|doc| match doc {
                    lsp::Documentation::String(s) => s,
                    lsp::Documentation::MarkupContent(markup) => markup.value,
                })
            } else {
                None
            };

            Signature {
                signature: s.label,
                signature_doc,
                active_param_range,
            }
        })
        .collect();

    let old_popup = compositor.find_id::<Popup<SignatureHelp>>(SignatureHelp::ID);
    let lsp_signature = response.active_signature.map(|s| s as usize);

    // take the new suggested lsp signature if changed
    // otherwise take the old signature if possible
    // otherwise the last one (in case there is less signatures than before)
    let active_signature = old_popup
        .as_ref()
        .map(|popup| {
            let old_lsp_sig = popup.contents().lsp_signature();
            let old_sig = popup
                .contents()
                .active_signature()
                .min(signatures.len() - 1);

            if old_lsp_sig != lsp_signature {
                lsp_signature.unwrap_or(old_sig)
            } else {
                old_sig
            }
        })
        .unwrap_or(lsp_signature.unwrap_or_default());

    let contents = SignatureHelp::new(
        language.to_string(),
        Arc::clone(&editor.syn_loader),
        active_signature,
        lsp_signature,
        signatures,
    );

    let mut popup = Popup::new(SignatureHelp::ID, contents)
        .position(old_popup.and_then(|p| p.get_position()))
        .position_bias(Open::Above)
        .ignore_escape_key(true);

    // Don't create a popup if it intersects the auto-complete menu.
    let size = compositor.size();
    if compositor
        .find::<ui::EditorView>()
        .unwrap()
        .completion
        .as_mut()
        .map(|completion| completion.area(size, editor))
        .filter(|area| area.intersects(popup.area(size, editor)))
        .is_some()
    {
        return;
    }

    compositor.replace_or_push(SignatureHelp::ID, popup);
}

fn signature_help_post_insert_char_hook(
    tx: &Sender<SignatureHelpEvent>,
    PostInsertChar { cx, .. }: &mut PostInsertChar<'_, '_>,
) -> anyhow::Result<()> {
    if !cx.editor.config().lsp.auto_signature_help {
        return Ok(());
    }
    let (view, doc) = current!(cx.editor);
    // TODO support multiple language servers (not just the first that is found), likely by merging UI somehow
    let Some(language_server) = doc
        .language_servers_with_feature(LanguageServerFeature::SignatureHelp)
        .next()
    else {
        return Ok(());
    };

    let capabilities = language_server.capabilities();

    if let lsp::ServerCapabilities {
        signature_help_provider:
            Some(lsp::SignatureHelpOptions {
                trigger_characters: Some(triggers),
                // TODO: retrigger_characters
                ..
            }),
        ..
    } = capabilities
    {
        let mut text = doc.text().slice(..);
        let cursor = doc.selection(view.id).primary().cursor(text);
        text = text.slice(..cursor);
        if triggers.iter().any(|trigger| text.ends_with(trigger)) {
            send_blocking(tx, SignatureHelpEvent::Trigger)
        }
    }
    Ok(())
}

pub(super) fn register_hooks(handlers: &Handlers) {
    let tx = handlers.signature_hints.clone();
    register_hook!(move |event: &mut OnModeSwitch<'_, '_>| {
        match (event.old_mode, event.new_mode) {
            (Mode::Insert, _) => {
                send_blocking(&tx, SignatureHelpEvent::Cancel);
                event.cx.callback.push(Box::new(|compositor, _| {
                    compositor.remove(SignatureHelp::ID);
                }));
            }
            (_, Mode::Insert) => {
                if event.cx.editor.config().lsp.auto_signature_help {
                    send_blocking(&tx, SignatureHelpEvent::Trigger);
                }
            }
            _ => (),
        }
        Ok(())
    });

    let tx = handlers.signature_hints.clone();
    register_hook!(
        move |event: &mut PostInsertChar<'_, '_>| signature_help_post_insert_char_hook(&tx, event)
    );

    let tx = handlers.signature_hints.clone();
    register_hook!(move |event: &mut DocumentDidChange<'_>| {
        if event.doc.config.load().lsp.auto_signature_help && !event.ghost_transaction {
            send_blocking(&tx, SignatureHelpEvent::ReTrigger);
        }
        Ok(())
    });

    let tx = handlers.signature_hints.clone();
    register_hook!(move |event: &mut SelectionDidChange<'_>| {
        if event.doc.config.load().lsp.auto_signature_help {
            send_blocking(&tx, SignatureHelpEvent::ReTrigger);
        }
        Ok(())
    });
}
