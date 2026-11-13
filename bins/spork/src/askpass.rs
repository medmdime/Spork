//! Git / ssh prompts (password, username, key passphrase) answered in a dialog (docs/specs/M10).

use std::net::TcpListener;

use gpui_kit::component::input::Input;
use gpui_kit::component::{ActiveTheme as _, WindowExt as _, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use smol::channel::{Receiver, Sender};
use spork_git::askpass;

use crate::RepoView;

/// A prompt from git plus where to send the answer (`None` cancels the git command).
pub type Prompt = (String, Sender<Option<String>>);

/// Starts the prompt server and routes every later git command's prompts to it.
pub fn start() -> Option<Receiver<Prompt>> {
    let listener = TcpListener::bind("127.0.0.1:0").ok()?;
    let addr = listener.local_addr().ok()?.to_string();
    let token = askpass::random_token();
    askpass::install(askpass::Config {
        program: std::env::current_exe().ok()?,
        addr,
        token: token.clone(),
    });
    let (tx, rx) = smol::channel::unbounded::<Prompt>();
    std::thread::spawn(move || {
        askpass::serve(listener, &token, |prompt| {
            let (reply, answer) = smol::channel::bounded(1);
            tx.send_blocking((prompt, reply)).ok()?;
            // a dialog closed without OK drops the sender, which cancels
            answer.recv_blocking().ok().flatten()
        })
    });
    Some(rx)
}

impl RepoView {
    /// Shows one prompt from git. Secrets (passwords, tokens, passphrases) are masked.
    pub fn ask(
        &mut self,
        prompt: String,
        reply: Sender<Option<String>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let lower = prompt.to_lowercase();
        let secret = ["password", "passphrase", "token"]
            .iter()
            .any(|w| lower.contains(w));
        let title = match () {
            _ if lower.contains("passphrase") => "SSH key passphrase",
            _ if lower.contains("password") => "Password",
            _ if lower.contains("username") => "Username",
            _ => "Git needs your input",
        };
        self.prompt_input.update(cx, |input, cx| {
            input.set_masked(secret, window, cx);
            input.set_value("", window, cx);
        });
        let input = self.prompt_input.clone();
        let muted = cx.theme().muted_foreground;
        window.open_alert_dialog(cx, move |dialog, _, _| {
            let (ok_reply, cancel_reply, input) = (reply.clone(), reply.clone(), input.clone());
            dialog
                .title(title)
                .description(
                    v_flex()
                        .gap_2()
                        .child(
                            div()
                                .text_sm()
                                .text_color(muted)
                                .child(prompt.trim().to_owned()),
                        )
                        .child(Input::new(&input).when(secret, |i| i.mask_toggle())),
                )
                .confirm()
                .ok_text("Continue")
                .on_ok(move |_, _, cx| {
                    ok_reply
                        .try_send(Some(input.read(cx).value().to_string()))
                        .ok();
                    true
                })
                .on_cancel(move |_, _, _| {
                    cancel_reply.try_send(None).ok();
                    true
                })
        });
    }
}
