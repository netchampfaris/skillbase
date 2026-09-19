//! Tooltips that carry text, capped at a width they can be read at.
//!
//! Every tooltip in the application is built here, so that none of them can
//! draw a line wider than the window.

use gpui_kit::component::button::Button;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::{
    AnyView, App, InteractiveElement, ParentElement as _, SharedString, Styled as _, Window, div,
    px,
};

/// How wide a tooltip's text is allowed to get.
///
/// A tooltip is read in one glance, so the line has to be short enough that the
/// eye finds the start of the next one — around fifty characters here. It is
/// also the width of the list column, which keeps a tooltip from covering the
/// list it explains.
pub(crate) const TOOLTIP_WIDTH: f32 = 320.;

/// A tooltip carrying a sentence, capped at [`TOOLTIP_WIDTH`].
///
/// `Tooltip::new` lays its text out on a single line however long the text is,
/// so a description of several sentences drew a box wider than the window.
///
/// The cap has to sit on an element of our own rather than on the tooltip: the
/// tooltip's box is a row that takes whatever width its content asks for, and
/// it is the block the text is laid out in that decides where the lines break.
/// A block with a width also breaks a run that has no spaces in it, so a
/// description written as one long word wraps rather than spilling out.
pub(crate) fn text_tooltip(
    text: impl Into<SharedString>,
) -> impl Fn(&mut Window, &mut App) -> AnyView {
    let text = text.into();
    move |window, cx| {
        let text = text.clone();
        Tooltip::element(move |_, _| {
            div()
                .max_w(px(TOOLTIP_WIDTH))
                .debug_selector(|| "text-tooltip".into())
                .child(text.clone())
        })
        .build(window, cx)
    }
}

/// A [`text_tooltip`] on a component whose own `tooltip` takes a string.
///
/// `Button::tooltip` builds `Tooltip::new` from its string and takes no
/// builder, so its tooltip cannot be capped. This puts a capped one on the
/// button's own element instead. It is the tooltip every other element in the
/// window uses, so it shows at the pointer rather than above the button.
///
/// `Switch` has no element of its own to reach, so a switch takes its tooltip
/// from a `div` wrapped around it.
pub(crate) trait TextTooltipExt: InteractiveElement + Sized {
    fn text_tooltip(mut self, text: impl Into<SharedString>) -> Self {
        let text: SharedString = text.into();
        self.interactivity().tooltip(text_tooltip(text));
        self
    }
}

impl TextTooltipExt for Button {}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::rc::Rc;
    use std::time::Duration;

    use gpui_kit::base::ElementExt as _;
    use gpui_kit::component::Root;
    use gpui_kit::component::button::Button;
    use gpui_kit::{
        AppContext as _, AvailableSpace, Context, InteractiveElement as _, IntoElement, Modifiers,
        ParentElement as _, Pixels, Render, Size, Styled as _, TestAppContext, VisualTestContext,
        Window, div, point, px,
    };

    use super::{TOOLTIP_WIDTH, TextTooltipExt as _, text_tooltip};
    use crate::ui::list::dialog_probe::window;

    /// The margin, padding and border the tooltip draws around its text. The
    /// cap is on the text, so the box is that much wider than the cap.
    const CHROME: Pixels = px(48.);

    /// How large a tooltip built by [`text_tooltip`] draws.
    ///
    /// The window lays a tooltip out against its minimum size, which is what
    /// leaves the box free to be as wide as its one line of text — so the
    /// measurement has to be taken the same way for it to say anything.
    fn tooltip_size(cx: &mut VisualTestContext, text: &str) -> Size<Pixels> {
        let measured = Rc::new(Cell::new(Size::default()));
        let build = text_tooltip(text.to_string());
        let out = measured.clone();
        cx.draw(
            point(px(0.), px(0.)),
            AvailableSpace::min_size(),
            move |window, cx| {
                let tooltip = build(window, cx);
                div()
                    .on_prepaint(move |bounds, _, _| out.set(bounds.size))
                    .child(tooltip)
            },
        );
        measured.get()
    }

    /// A tooltip used to be laid out on one line however long its text was, so
    /// the full description offered on a truncated row arrived as a line wider
    /// than the window — worse than the row it was explaining.
    #[gpui_kit::test]
    fn a_tooltip_holding_a_sentence_wraps_inside_a_reading_width(cx: &mut TestAppContext) {
        let (mut cx, _) = window(cx);

        let label = tooltip_size(&mut cx, "Short.");
        // A short label still hugs its text: the cap is a maximum, not a width.
        assert!(
            label.width < px(TOOLTIP_WIDTH),
            "a one-word tooltip drew {:?} wide",
            label.width
        );

        let sentence = tooltip_size(
            &mut cx,
            "Use this skill whenever the user works with PDF files.",
        );
        let paragraph = tooltip_size(
            &mut cx,
            "Use this skill whenever the user works with PDF files: reading one, filling in a \
             form, splitting one apart, or putting several together. It reads the pages and \
             does not change them.",
        );
        // Nothing in this one is a place to break a line, so the break has to
        // fall mid-word. Unwrapped, it was the description that ran off the
        // screen.
        let unbroken = tooltip_size(&mut cx, &"unbrokenrun".repeat(20));

        for (what, size) in [
            ("a sentence", sentence),
            ("a paragraph", paragraph),
            ("one long word", unbroken),
        ] {
            assert!(
                size.width <= px(TOOLTIP_WIDTH) + CHROME,
                "{what} drew {:?} wide, past the {TOOLTIP_WIDTH}pt cap",
                size.width
            );
            assert!(
                size.height > label.height,
                "{what} is wider than the cap, so it has to take more than the one line \
                 a label takes: {:?} against {:?}",
                size.height,
                label.height
            );
        }

        // The lines pile up rather than the box being cut off at some height:
        // three sentences take more of them than one.
        assert!(
            paragraph.height > sentence.height,
            "a paragraph drew {:?} against a sentence's {:?}",
            paragraph.height,
            sentence.height
        );
    }

    const LONG: &str = "Links Claude Code, Codex, Cursor, Gemini CLI, GitHub Copilot and \
                        OpenCode, in one write. The agents reached through Shared are left alone.";

    /// A window holding one button with a long tooltip, at the right edge.
    struct ButtonProbe;

    impl Render for ButtonProbe {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().size_full().flex().justify_end().child(
                Button::new("probe")
                    .label("Link all")
                    .debug_selector(|| "probe".into())
                    .text_tooltip(LONG),
            )
        }
    }

    /// A button's own `tooltip` could not be capped, so its long sentences
    /// drew as one line that ran out of the window. The button now takes the
    /// capped tooltip, and it stays inside the window at the window's edge.
    #[gpui_kit::test]
    fn a_button_tooltip_wraps_inside_the_window(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let handle = cx.add_window(|window, cx| {
            let view = cx.new(|_| ButtonProbe);
            Root::new(view, window, cx)
        });
        cx.run_until_parked();
        let mut cx = VisualTestContext::from_window(handle.into(), cx);

        let button = cx.debug_bounds("probe").expect("the button drew");
        cx.simulate_mouse_move(button.center(), None, Modifiers::none());
        cx.executor().advance_clock(Duration::from_secs(1));
        cx.run_until_parked();

        let tooltip = cx
            .debug_bounds("text-tooltip")
            .expect("hovering the button showed its tooltip");
        let viewport = cx.update(|window, _| window.viewport_size());
        assert!(
            tooltip.size.width <= px(TOOLTIP_WIDTH),
            "the tooltip's text drew {:?} wide, past the {TOOLTIP_WIDTH}pt cap",
            tooltip.size.width
        );
        assert!(
            tooltip.origin.x >= px(0.) && tooltip.right() <= viewport.width,
            "the tooltip drew at {tooltip:?}, outside a {viewport:?} window"
        );
    }
}
