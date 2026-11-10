//! The forge account as the desktop shows and drives it. `dirigent_service::forge` has the
//! account itself (one per data dir, shared with the embedded service); this is the UI's
//! copy of its status, read again every few seconds (`dirigent login` may change it), the
//! browser sign-in, and Settings › Forge providers, the one place that signs in and out.
//! `auth = "forge"` hosts connect with it and the leaderboard (`leaderboard.rs`) uploads as
//! it; where they need it while nobody is signed in, they link to the tab
//! (`open_forge_settings`).
//!
//! The tab lists the one forge there is (Vir's, or `DIRIGENT_FORGE_URL`): who is signed in
//! (picture and username), Sign in (in the browser, with Cancel while it waits) or Sign
//! out, and the forge having ended the sign-in (`forge::ENDED`). Under it, while the
//! account is on the leaderboard, the way off it (joining is on the board itself).

use crate::app::Dirigent;
use crate::settings::{self, saved_to, section};
use crate::theme::{self, text, u};
use dirigent_service::forge::{self, BrowserLogin, Status};
use gpui::{AnyElement, Context, Div, FontWeight, SharedString, div, prelude::*, rgba};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tracing::info;

/// How often (seconds, on `Dirigent::tick`) the status is read again.
pub const CHECK_EVERY: u64 = 5;

/// The forge's name in the tab.
const NAME: &str = "Vir forge";

#[derive(Default)]
pub struct State {
    pub status: Status,
    /// A browser sign-in waiting: set to cancel it.
    pub signing_in: Option<Arc<AtomicBool>>,
}

impl State {
    pub fn new() -> Self {
        Self { status: forge::status(), ..Self::default() }
    }
}

impl Dirigent {
    /// Settings on the Forge providers tab: where whatever needs the forge sign-in sends you.
    pub fn open_forge_settings(&mut self, cx: &mut Context<Self>) {
        self.settings.tab = settings::Tab::Forge;
        self.open_settings(cx);
    }

    /// Sign in in the browser.
    pub fn forge_sign_in(&mut self, cx: &mut Context<Self>) {
        self.menu = None;
        if self.forge.signing_in.is_some() {
            return;
        }
        let (login, url) = match BrowserLogin::start() {
            Ok(started) => started,
            Err(e) => return self.toast(format!("Couldn't start the sign-in: {e:#}"), cx),
        };
        // Without the query: its `state` is the sign-in's secret.
        info!(page = url.split('?').next().unwrap_or_default(), "signing in to the forge");
        cx.open_url(&url);
        let cancel = Arc::new(AtomicBool::new(false));
        self.forge.signing_in = Some(cancel.clone());
        crate::leaderboard::http(
            cx,
            move || login.finish(&cancel),
            |this, signed_in, cx| {
                this.forge.signing_in = None;
                match signed_in {
                    Ok(Some(account)) => {
                        info!(user = %account.username, "signed in to the forge");
                        this.sync_forge(cx);
                    }
                    Ok(None) => {}
                    Err(e) => this.toast(format!("Couldn't sign in to the forge: {e:#}"), cx),
                }
            },
        );
        cx.notify();
    }

    pub fn forge_cancel_sign_in(&mut self, cx: &mut Context<Self>) {
        if let Some(cancel) = self.forge.signing_in.take() {
            cancel.store(true, Ordering::Relaxed);
        }
        cx.notify();
    }

    /// Forget the sign-in here. The forge still lets Dirigent in until it's revoked on the
    /// forge's account page (the tab links it).
    pub fn forge_sign_out(&mut self, cx: &mut Context<Self>) {
        if let Err(e) = forge::logout() {
            self.toast(format!("Couldn't sign out: {e:#}"), cx);
        }
        self.sync_forge(cx);
    }

    /// Take the account's status as `forge.json` has it now.
    pub fn sync_forge(&mut self, cx: &mut Context<Self>) {
        let status = forge::status();
        if status != self.forge.status {
            self.forge.status = status;
            self.fetch_forge_picture(cx);
            cx.notify();
        }
    }

    /// The signed-in account's picture, for the tab (once; `leaderboard.rs` keeps them).
    pub fn fetch_forge_picture(&mut self, cx: &mut Context<Self>) {
        if let Some(a) = self.forge.status.signed_in() {
            let picture = a.picture.clone();
            self.fetch_pictures([picture], cx);
        }
    }
}

/// Settings › Forge providers: the forge's row, what it's for, and where it's kept.
pub fn render(this: &Dirigent, cx: &mut Context<Dirigent>) -> Vec<Div> {
    let t = theme::t();
    let ended = matches!(this.forge.status, Status::Ended(_)).then(|| {
        // Under the name, past the dot.
        div().pl(u(27.)).pb_1().text_size(text::sm()).text_color(rgba(t.error)).child(forge::ENDED)
    });
    let page = forge::forge().account_page();
    let link = div()
        .id("forge-account-page")
        .cursor_pointer()
        .text_size(text::sm())
        .text_color(rgba(t.accent_ink))
        .hover(|d| d.underline())
        .on_click(move |_, _, cx| cx.open_url(&page))
        .child("Open the forge account page");
    let body = div()
        .flex()
        .flex_col()
        .child(row(this, cx))
        .children(ended)
        .children(board_row(this, cx))
        .child(div().flex().flex_row().pt_3().child(link));
    vec![section("Forge providers", body), saved_to(&forge::path())]
}

/// `  Leaderboard  on the board as seb        [Leave and delete my usage]`, while the
/// signed-in account is on the leaderboard: off the board, usage and all, on every desktop.
/// Under the forge's row, past its dot.
fn board_row(this: &Dirigent, cx: &mut Context<Dirigent>) -> Option<Div> {
    let t = theme::t();
    let player = this.leaderboard.active(&this.forge.status)?;
    let leave = settings::text_button("board-leave", "Leave and delete my usage")
        .text_color(rgba(t.error))
        .on_click(cx.listener(|this, _, _, cx| this.leave_board(cx)));
    Some(
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_3()
            .h(u(38.))
            .px_2()
            .child(theme::dot(theme::alpha(t.fg_2, 0x00)))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(div().flex_shrink_0().font_weight(FontWeight::MEDIUM).child("Leaderboard"))
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_size(text::sm())
                            .text_color(rgba(t.fg_2))
                            .child(SharedString::from(format!("on the board as {}", player.name))),
                    ),
            )
            .child(leave),
    )
}

/// `● Vir forge  forge.sebba.dev        (picture) seb  [Sign out]`, or Sign in, or the
/// sign-in waiting for the browser with Cancel. The dot: green signed in, red ended, dim
/// signed out.
fn row(this: &Dirigent, cx: &mut Context<Dirigent>) -> Div {
    let t = theme::t();
    let url = forge::forge().url.clone();
    let host = url.split_once("://").map_or(url.as_str(), |(_, h)| h).to_owned();
    let dot = match &this.forge.status {
        Status::SignedIn(_) => t.done,
        Status::Ended(_) => t.error,
        Status::SignedOut => theme::alpha(t.fg_2, 0x60),
    };
    let quiet = |text: String| div().text_size(text::sm()).text_color(rgba(t.fg_1)).child(text);
    let mut right: Vec<AnyElement> = vec![];
    match (&this.forge.status, this.forge.signing_in.is_some()) {
        (_, true) => {
            right.push(quiet("Waiting for your browser…".into()).into_any_element());
            right.push(
                settings::text_button("forge-cancel", "Cancel")
                    .on_click(cx.listener(|this, _, _, cx| this.forge_cancel_sign_in(cx)))
                    .into_any_element(),
            );
        }
        (Status::SignedIn(a), _) => {
            let picture = this.leaderboard.avatars.get(&a.picture).cloned().flatten();
            right.push(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(crate::usage::avatar(picture, &a.username, t.accent_ink, 18.))
                    .child(quiet(a.username.clone()).text_color(rgba(t.fg)))
                    .into_any_element(),
            );
            right.push(
                settings::text_button("forge-sign-out", "Sign out")
                    .on_click(cx.listener(|this, _, _, cx| this.forge_sign_out(cx)))
                    .into_any_element(),
            );
        }
        (Status::SignedOut | Status::Ended(_), _) => right.push(
            settings::text_button("forge-sign-in", "Sign in")
                .on_click(cx.listener(|this, _, _, cx| this.forge_sign_in(cx)))
                .into_any_element(),
        ),
    }
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap_3()
        .h(u(38.))
        .px_2()
        .child(theme::dot(dot))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .child(div().flex_shrink_0().font_weight(FontWeight::MEDIUM).child(NAME))
                .child(div().min_w_0().truncate().text_size(text::sm()).text_color(rgba(t.fg_2)).child(SharedString::from(host))),
        )
        .child(div().flex().flex_row().items_center().gap_3().flex_shrink_0().children(right))
}
