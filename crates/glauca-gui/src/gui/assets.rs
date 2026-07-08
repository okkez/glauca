//! Embedded SVG assets served to gpui via [`AssetSource`].
//!
//! gpui resolves `svg().path("…")` through the single app-level `AssetSource`
//! (set with `Application::with_assets`). We bundle the GitHub Octicons used for
//! the item-list status glyphs (`octicons/*.svg`, MIT — see `assets/octicons/NOTICE`).
//! gpui paints SVGs as a monochrome mask tinted by the element's `text_color`,
//! so the icon color is chosen at the call site, not in the file.

use std::borrow::Cow;

use gpui::{AssetSource, Result, SharedString};

/// App asset source. Only the bundled octicons are served; any other path
/// resolves to `None` (gpui-component currently needs no app-served assets).
pub struct Assets;

macro_rules! octicons {
    ($($path:literal => $file:literal),* $(,)?) => {
        fn octicon_bytes(path: &str) -> Option<&'static [u8]> {
            match path {
                $($path => Some(include_bytes!($file)),)*
                _ => None,
            }
        }
    };
}

octicons! {
    "octicons/issue-opened.svg" => "../../assets/octicons/issue-opened.svg",
    "octicons/issue-closed.svg" => "../../assets/octicons/issue-closed.svg",
    "octicons/git-pull-request.svg" => "../../assets/octicons/git-pull-request.svg",
    "octicons/git-merge.svg" => "../../assets/octicons/git-merge.svg",
    "octicons/git-pull-request-closed.svg" => "../../assets/octicons/git-pull-request-closed.svg",
    "octicons/git-pull-request-draft.svg" => "../../assets/octicons/git-pull-request-draft.svg",
    "octicons/lock.svg" => "../../assets/octicons/lock.svg",
    "octicons/check-circle-fill.svg" => "../../assets/octicons/check-circle-fill.svg",
    "octicons/x-circle-fill.svg" => "../../assets/octicons/x-circle-fill.svg",
    "octicons/comment.svg" => "../../assets/octicons/comment.svg",
    "octicons/clock.svg" => "../../assets/octicons/clock.svg",
}

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        Ok(octicon_bytes(path).map(Cow::Borrowed))
    }

    fn list(&self, _path: &str) -> Result<Vec<SharedString>> {
        Ok(Vec::new())
    }
}
