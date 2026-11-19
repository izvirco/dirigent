use crate::app::Dirigent;
use crate::settings::{self, saved_to, section};
use crate::theme::{self, text, u};
use dirigent_service::forge::{self, BrowserLogin, Status};
use gpui::{AnyElement, Context, Div, FontWeight, SharedString, div, prelude::*, rgba};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tracing::info;

pub const CHECK_SECS: u64 = 5;

const NAME: &str = "Vir forge";

const NO_DOT: u32 = 0x00000000;

#[derive(Default)]
pub struct State {
    pub status: Status,
    pub signing_in: Option<Arc<AtomicBool>>,
}

impl State {
    pub fn new() -> Self {
        Self { status: forge::status(), ..Self::default() }
    }
}

impl Dirigent {
    pub fn open_forge_settings(&mut self, cx: &mut Context<Self>) {
        self.settings.tab = settings::Tab::Forge;
        self.open_settings(cx);
    }

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

    pub fn sync_forge(&mut self, cx: &mut Context<Self>) {
        let status = forge::status();
        if status != self.forge.status {
            self.forge.status = status;
            self.fetch_forge_picture(cx);
            cx.notify();
        }
    }

    pub fn fetch_forge_picture(&mut self, cx: &mut Context<Self>) {
        if let Some(a) = self.forge.status.signed_in() {
            let picture = a.picture.clone();
            self.fetch_pictures([picture], cx);
        }
    }
}

pub fn render(this: &Dirigent, cx: &mut Context<Dirigent>) -> Vec<Div> {
    let t = theme::t();
    let ended = matches!(this.forge.status, Status::Ended(_))
        .then(|| line(NO_DOT).pb_1().child(div().flex_1().min_w_0().text_size(text::sm()).text_color(rgba(t.error)).child(forge::ENDED)));
    let page = forge::forge().account_page();
    let link = div()
        .id("forge-account-page")
        .cursor_pointer()
        .text_size(text::sm())
        .text_color(rgba(t.accent_ink))
        .hover(|d| d.underline())
        .on_click(move |_, _, cx| cx.open_url(&page))
        .child("Open the forge account page");
    let body =
        theme::stack().child(row(this, cx)).children(ended).children(board_row(this, cx)).child(div().flex().flex_row().pt_3().child(link));
    vec![section("Forge providers", body), saved_to(&forge::path())]
}

fn board_row(this: &Dirigent, cx: &mut Context<Dirigent>) -> Option<Div> {
    let t = theme::t();
    let player = this.leaderboard.active(&this.forge.status)?;
    let leave = settings::text_button("board-leave", "Leave and delete my usage")
        .text_color(rgba(t.error))
        .on_click(cx.listener(|this, _, _, cx| this.leave_board(cx)));
    Some(
        line(NO_DOT)
            .h(u(38.))
            .pr_2()
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
    line(dot)
        .h(u(38.))
        .pr_2()
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

fn line(dot: u32) -> Div {
    div().flex().flex_row().items_center().gap_3().pl_2().child(theme::dot(dot))
}
