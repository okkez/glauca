//! View-building free functions: detail fields, chips, state/review icons,
//! avatars, and title highlighting. Pure presentation helpers with no app state.

use gpui::*;
use gpui_component::avatar::Avatar;
use gpui_component::{ActiveTheme, Sizable, StyledExt, Theme, h_flex};

use glauca_core::logic::ReviewState;
use glauca_core::types::{ActorKind, ItemEntry, UserRef};

/// Overlay GitHub Dark (Primer "dark default") colors on gpui-component's stock
/// dark theme, which is near-black and felt too dark. Only the fields the app
/// actually reads via `cx.theme()` are overridden. Must run *after* the theme
/// is switched to dark: `Theme::change` / `apply_config` rebuild `colors` from
/// the base config and would otherwise discard these.
pub(crate) fn apply_github_dark_overlay(cx: &mut App) {
    let c = &mut Theme::global_mut(cx).colors;
    c.background = rgb(0x0d1117).into(); // canvas.default
    c.foreground = rgb(0xe6edf3).into(); // fg.default
    c.border = rgb(0x30363d).into(); // border.default
    c.sidebar = rgb(0x161b22).into(); // canvas.subtle (left pane)
    c.sidebar_foreground = rgb(0xe6edf3).into();
    // `accent` is gpui-component's inline-code background and our unread-badge /
    // row-tint color. A bright blue there is hard to read, so use a neutral grey
    // (GitHub neutral.muted) — links and the filter-match highlight use `link`
    // instead so they stay blue.
    c.accent = rgb(0x373e47).into();
    c.accent_foreground = rgb(0xe6edf3).into();
    c.link = rgb(0x2f81f7).into(); // accent.fg — links + filter-match highlight
    c.primary = rgb(0x2f81f7).into();
    c.muted_foreground = rgb(0x8b949e).into(); // fg.muted
    c.list_active = rgb(0x21262d).into(); // selected row
    c.list_hover = rgb(0x161b22).into(); // hovered row
    c.green = rgb(0x3fb950).into(); // success (open)
    c.red = rgb(0xf85149).into(); // danger (closed)
    c.magenta = rgb(0xa371f7).into(); // done/purple (merged)
    c.yellow = rgb(0xd29922).into(); // attention (pending review)
}

/// A `label: value` row in the detail pane.
pub(crate) fn detail_field(label: &str, value: &str, cx: &App) -> impl IntoElement {
    h_flex()
        .w_full()
        .gap_2()
        .text_sm()
        .child(
            div()
                .flex_shrink_0()
                .w(px(96.))
                .text_color(cx.theme().muted_foreground)
                .child(SharedString::from(label.to_string())),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_color(cx.theme().foreground)
                .child(SharedString::from(value.to_string())),
        )
}

/// A `label: <chips>` row in the detail pane, where the value is a wrapping row
/// of people chips (avatar + login). Used for author / assignees / reviewers.
pub(crate) fn detail_people_field(
    label: &str,
    chips: impl IntoIterator<Item = impl IntoElement>,
    cx: &App,
) -> impl IntoElement {
    h_flex()
        .w_full()
        .gap_2()
        .text_sm()
        .items_start()
        .child(
            div()
                .flex_shrink_0()
                .w(px(96.))
                .text_color(cx.theme().muted_foreground)
                .child(SharedString::from(label.to_string())),
        )
        .child(
            h_flex()
                .flex_1()
                .min_w_0()
                .flex_wrap()
                .gap_2()
                .children(chips),
        )
}

/// A people chip: avatar + login text, shown inline in the detail header.
pub(crate) fn user_chip(user: UserRef, _cx: &App) -> impl IntoElement {
    let login = SharedString::from(user.login.clone());
    h_flex()
        .gap_1()
        .items_center()
        .child(user_avatar(&user))
        .child(login)
}

/// A reviewer chip: avatar with review-state overlay + login text.
pub(crate) fn reviewer_chip(user: UserRef, state: ReviewState, cx: &App) -> impl IntoElement {
    let login = SharedString::from(user.login.clone());
    h_flex()
        .gap_1()
        .items_center()
        .child(reviewer_avatar(&user, state, cx))
        .child(login)
}

/// Status glyph for a list row: a GitHub-style octicon (vendored under
/// `assets/octicons`, served by [`assets::Assets`]) whose shape encodes
/// issue-vs-PR and whose color encodes the state (open=green, merged=magenta,
/// closed=red, draft=muted). gpui paints the SVG as a mask tinted by
/// `text_color`.
pub(crate) fn item_state_icon(item: &ItemEntry, cx: &App) -> impl IntoElement {
    let (path, color) = item_state_icon_info(item, cx);
    svg()
        .path(path)
        .size_4()
        .flex_shrink_0()
        // Nudge down so the icon centers on the first title line.
        .mt(px(2.))
        .text_color(color)
}

/// Octicon path + color for an item's state, shared by the list-row status icon
/// and the detail-header state pill.
pub(crate) fn item_state_icon_info(item: &ItemEntry, cx: &App) -> (&'static str, Hsla) {
    let theme = cx.theme();
    if item.kind == "pull_request" {
        if item.is_draft {
            (
                "octicons/git-pull-request-draft.svg",
                theme.muted_foreground,
            )
        } else {
            match item.state.as_str() {
                "merged" => ("octicons/git-merge.svg", theme.magenta),
                "closed" => ("octicons/git-pull-request-closed.svg", theme.red),
                _ => ("octicons/git-pull-request.svg", theme.green),
            }
        }
    } else {
        match item.state.as_str() {
            "closed" => ("octicons/issue-closed.svg", theme.red),
            _ => ("octicons/issue-opened.svg", theme.green),
        }
    }
}

/// GitHub-style state label for the detail-header state pill.
pub(crate) fn state_label(item: &ItemEntry) -> &'static str {
    if item.kind == "pull_request" {
        if item.is_draft {
            "Draft"
        } else {
            match item.state.as_str() {
                "merged" => "Merged",
                "closed" => "Closed",
                _ => "Open",
            }
        }
    } else {
        match item.state.as_str() {
            "closed" => "Closed",
            _ => "Open",
        }
    }
}

/// Side length of the participant avatars in the item list.
pub(crate) const AVATAR_PX: f32 = 24.;

/// Larger avatar shown in the left-pane header (current user).
pub(crate) const HEADER_AVATAR_PX: f32 = 36.;

/// Side length of the review-state badge overlaid on a reviewer avatar.
pub(crate) const BADGE_PX: f32 = 14.;

/// Corner radius for a team avatar. GitHub renders non-human actors (teams, orgs,
/// bots) as a rounded square rather than a circle; Primer's 24px `.avatar-3` uses
/// `--borderRadius-medium`, which is 6px.
pub(crate) const TEAM_AVATAR_RADIUS_PX: f32 = 6.;

/// Side length of the fallback icon inside a team avatar that has no image.
pub(crate) const TEAM_ICON_PX: f32 = 14.;

/// Max avatars shown per group (assignees / reviewers) before a `+N` overflow.
pub(crate) const AVATAR_LIMIT: usize = 5;

/// GitHub serves a 460px avatar PNG by default; downscaling that to the small
/// list avatar aliases badly (looks grainy). Ask GitHub to resize server-side
/// to roughly the displayed size (2× for HiDPI sharpness) via the `s=` param.
pub(crate) fn sized_avatar_url(url: &str, target_px: f32) -> String {
    let px = (target_px * 2.0) as u32;
    let sep = if url.contains('?') { '&' } else { '?' };
    format!("{url}{sep}s={px}")
}

/// One participant avatar: the user's GitHub avatar image, falling back to the
/// login's initials placeholder when there is no avatar URL (older cache rows).
/// `name` also drives the alt/initials text. Teams go through [`team_avatar`].
pub(crate) fn user_avatar(user: &UserRef) -> Avatar {
    let mut a = Avatar::new()
        .name(user.login.clone())
        .with_size(px(AVATAR_PX));
    if let Some(url) = &user.avatar_url {
        a = a.src(sized_avatar_url(url, AVATAR_PX));
    }
    a
}

/// A team's avatar. GitHub renders non-human actors as a rounded square instead of
/// a circle, which is the *only* cue at this size — a team with no image and a user
/// with no image both fall back to a glyph. Falls back to the `people` octicon when
/// the team (and its org) has no avatar.
pub(crate) fn team_avatar(user: &UserRef, cx: &App) -> impl IntoElement {
    let frame = div()
        .size(px(AVATAR_PX))
        .flex_shrink_0()
        .rounded(px(TEAM_AVATAR_RADIUS_PX))
        .overflow_hidden()
        .flex()
        .items_center()
        .justify_center()
        .bg(cx.theme().accent);
    match &user.avatar_url {
        Some(url) => frame.child(
            img(SharedString::from(sized_avatar_url(url, AVATAR_PX)))
                .size(px(AVATAR_PX))
                .rounded(px(TEAM_AVATAR_RADIUS_PX)),
        ),
        None => frame.child(
            svg()
                .path("octicons/people.svg")
                .size(px(TEAM_ICON_PX))
                .text_color(cx.theme().muted_foreground),
        ),
    }
}

/// Octicon, color, and tooltip text for a PR's `reviewDecision` (the raw GitHub
/// value), shown as an icon in the detail header.
pub(crate) fn review_decision_icon(decision: &str, cx: &App) -> (&'static str, Hsla, &'static str) {
    let t = cx.theme();
    match decision {
        "APPROVED" => ("octicons/check-circle-fill.svg", t.green, "Approved"),
        "CHANGES_REQUESTED" => ("octicons/x-circle-fill.svg", t.red, "Changes requested"),
        "REVIEW_REQUIRED" => ("octicons/clock.svg", t.yellow, "Review required"),
        _ => ("octicons/comment.svg", t.muted_foreground, "Review"),
    }
}

/// Octicon, icon color, and badge background for a reviewer's [`ReviewState`],
/// shown as a small badge overlaid on the reviewer avatar. gpui tints the SVG
/// mask with `text_color`; the badge background shows through the icon's
/// knockout — white for the filled check/x (GitHub-style), otherwise the
/// neutral background as a plain ring.
pub(crate) fn review_state_icon(state: ReviewState, cx: &App) -> (&'static str, Hsla, Hsla) {
    let theme = cx.theme();
    match state {
        ReviewState::Approved => ("octicons/check-circle-fill.svg", theme.green, white()),
        ReviewState::ChangesRequested => ("octicons/x-circle-fill.svg", theme.red, white()),
        ReviewState::Commented | ReviewState::Dismissed => (
            "octicons/comment.svg",
            theme.muted_foreground,
            theme.background,
        ),
        ReviewState::Pending => ("octicons/clock.svg", theme.yellow, theme.background),
    }
}

/// A `+N` label for participants beyond [`AVATAR_LIMIT`].
pub(crate) fn avatar_overflow(n: usize, cx: &App) -> impl IntoElement {
    div()
        .flex_shrink_0()
        .text_xs()
        .text_color(cx.theme().muted_foreground)
        .child(SharedString::from(format!("+{n}")))
}

/// A reviewer avatar with its review-state octicon overlaid bottom-right
/// (relative+absolute, mirroring gpui-component's Badge pattern).
pub(crate) fn reviewer_avatar(user: &UserRef, state: ReviewState, cx: &App) -> impl IntoElement {
    let (icon, color, badge_bg) = review_state_icon(state, cx);
    div()
        .relative()
        .flex_shrink_0()
        .size(px(AVATAR_PX))
        .child(match user.kind {
            ActorKind::Team => team_avatar(user, cx).into_any_element(),
            ActorKind::User => user_avatar(user).into_any_element(),
        })
        .child(
            svg()
                .path(icon)
                .size(px(BADGE_PX))
                .absolute()
                .bottom(px(-3.))
                .right(px(-3.))
                .text_color(color)
                // Fills the icon's knockout and rings the badge against the
                // avatar behind it.
                .bg(badge_bg)
                .rounded_full(),
        )
}

/// Render an item title, emphasising the inline-filter match range if any.
///
/// Uses `StyledText` (not flex spans) so the title wraps across lines and the
/// row grows to fit — the highlight is an overlaid style range on the wrapping
/// text rather than a separate box that can't break mid-word.
pub(crate) fn highlight_title(
    title: &str,
    ranges: Vec<(usize, usize)>,
    cx: &App,
) -> impl IntoElement {
    let theme = cx.theme();
    let mut text = StyledText::new(SharedString::from(title.to_string()));
    let highlights: Vec<_> = ranges
        .into_iter()
        .filter(|&(start, end)| start < end && end <= title.len())
        .map(|(start, end)| {
            (
                start..end,
                HighlightStyle {
                    // `link` (not `accent`) so the match stays a visible blue —
                    // `accent` is the muted grey used for inline code / badges.
                    background_color: Some(theme.link),
                    color: Some(theme.accent_foreground),
                    ..Default::default()
                },
            )
        })
        .collect();
    if !highlights.is_empty() {
        text = text.with_highlights(highlights);
    }
    div()
        .flex_1()
        .min_w_0()
        .font_bold()
        .text_color(theme.foreground)
        .child(text)
}
