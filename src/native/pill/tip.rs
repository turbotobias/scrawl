//! Small AppKit labels beside the capsule's Metal panel: a capsule button's tooltip (what it
//! does and the keys that do the same, `model::tip_of`) under the capsule, and the banner that
//! names what a recording records as it starts, on that window, area or screen. They're AppKit
//! panels of their own so they can sit outside the capsule, and because AppKit's own tooltips
//! never show while Scrawl isn't the active app, which it never is while recording. Like the
//! capsule they stay ordered in, invisible when not showing, so the capture's filter always has
//! them to leave out.

use std::time::Instant;

use objc2::MainThreadMarker;
use objc2::rc::Retained;
use objc2_app_kit::{
    NSBackingStoreType, NSBox, NSBoxType, NSColor, NSFont, NSFontWeight, NSFontWeightMedium,
    NSFontWeightRegular, NSFontWeightSemibold, NSPanel, NSScreen, NSStatusWindowLevel, NSTextField,
    NSTitlePosition, NSView, NSWindowCollectionBehavior, NSWindowStyleMask,
};
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};

use super::model::ElemId;

/// How long the pointer rests on a button before its tooltip shows, and for how long after one
/// hides the next button's shows at once, as macOS's own do.
const DELAY_S: f64 = 0.5;
const HANDOFF_S: f64 = 0.4;
const FADE_IN_S: f32 = 0.10;
const FADE_OUT_S: f32 = 0.08;
/// The tooltip's distance from the capsule and from the screen's edges.
const GAP: f64 = 6.0;
const MARGIN: f64 = 4.0;
/// How long the banner shows, and how long it takes to go.
pub(super) const BANNER_S: f64 = 2.4;
const BANNER_OUT_S: f32 = 0.35;
/// The banner's distance from the top of what's recorded (a screen's leaves room for the
/// capsule).
const BANNER_INSET: f64 = 16.0;
const BANNER_SCREEN_INSET: f64 = 120.0;

/// Where a tooltip goes, in global points (top-left origin).
#[derive(Clone, Copy, Debug)]
pub(super) struct Anchor {
    /// The button's horizontal centre.
    pub x: f64,
    /// The capsule's top and bottom edges.
    pub top: f64,
    pub bottom: f64,
    /// The display's visible frame.
    pub visible: [f64; 4],
}

/// What a tooltip says: the button's name and its keys (empty: none).
pub(super) type Text = (&'static str, String);

/// How a label looks.
struct Style {
    height: f64,
    font: f64,
    /// The name's weight, and the second part's.
    weights: [NSFontWeight; 2],
    /// The second part's brightness.
    dim: f64,
    inset: f64,
    gap: f64,
    radius: f64,
    /// A red dot before the name (the banner's "recording").
    dot: bool,
}

/// A dark rounded label with a name and a dimmer second part, in a click-through panel.
struct Bubble {
    window: Retained<NSPanel>,
    back: Retained<NSBox>,
    /// The banner's red dot (kept with the label it's in).
    _dot: Option<Retained<NSBox>>,
    name: Retained<NSTextField>,
    detail: Retained<NSTextField>,
    style: Style,
    alpha: f32,
    frame: [f64; 4],
}

const DOT: f64 = 8.0;

impl Bubble {
    fn open(mtm: MainThreadMarker, style: Style) -> Self {
        let rect = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(80.0, style.height));
        let window = NSPanel::initWithContentRect_styleMask_backing_defer(
            mtm.alloc(),
            rect,
            NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel,
            NSBackingStoreType::Buffered,
            false,
        );
        unsafe { window.setReleasedWhenClosed(false) };
        window.setBackgroundColor(Some(&NSColor::clearColor()));
        window.setOpaque(false);
        window.setHasShadow(true);
        window.setIgnoresMouseEvents(true);
        window.setHidesOnDeactivate(false);
        window.setCanHide(false);
        window.setLevel(NSStatusWindowLevel + 6);
        window.setCollectionBehavior(
            NSWindowCollectionBehavior::CanJoinAllSpaces
                | NSWindowCollectionBehavior::CanJoinAllApplications
                | NSWindowCollectionBehavior::FullScreenAuxiliary
                | NSWindowCollectionBehavior::Stationary
                | NSWindowCollectionBehavior::IgnoresCycle,
        );
        window.setAlphaValue(0.0);

        let root = NSView::initWithFrame(mtm.alloc(), rect);
        let rounded = |radius: f64, fill: &NSColor| {
            let view = NSBox::initWithFrame(mtm.alloc(), rect);
            view.setBoxType(NSBoxType::Custom);
            view.setTitlePosition(NSTitlePosition::NoTitle);
            view.setFillColor(fill);
            view.setBorderWidth(0.0);
            view.setCornerRadius(radius);
            root.addSubview(&view);
            view
        };
        let back = rounded(style.radius, &NSColor::colorWithWhite_alpha(0.07, 0.94));
        back.setBorderColor(&NSColor::colorWithWhite_alpha(1.0, 0.16));
        back.setBorderWidth(0.5);
        let dot = style.dot.then(|| {
            let red = NSColor::colorWithSRGBRed_green_blue_alpha(1.0, 0.23, 0.19, 1.0);
            let dot = rounded(DOT * 0.5, &red);
            dot.setFrame(NSRect::new(
                NSPoint::new(style.inset, ((style.height - DOT) * 0.5).round()),
                NSSize::new(DOT, DOT),
            ));
            dot
        });
        let label = |weight, white: f64| {
            let field = NSTextField::labelWithString(&NSString::from_str(""), mtm);
            field.setFont(Some(&NSFont::systemFontOfSize_weight(style.font, weight)));
            field.setTextColor(Some(&NSColor::colorWithWhite_alpha(1.0, white)));
            root.addSubview(&field);
            field
        };
        let name = label(style.weights[0], 0.96);
        let detail = label(style.weights[1], style.dim);
        window.setContentView(Some(&root));
        window.orderFrontRegardless();
        Self {
            window,
            back,
            _dot: dot,
            name,
            detail,
            style,
            alpha: 0.0,
            frame: [0.0; 4],
        }
    }

    fn window_number(&self) -> u32 {
        self.window.windowNumber().max(0) as u32
    }

    fn close(&self) {
        self.window.orderOut(None);
        self.window.close();
    }

    fn order_front(&self) {
        self.window.setLevel(NSStatusWindowLevel + 6);
        self.window.orderFrontRegardless();
    }

    fn width(&self) -> f64 {
        self.back.frame().size.width
    }

    fn set_text(&mut self, name: &str, detail: &str) {
        let s = &self.style;
        self.name.setStringValue(&NSString::from_str(name));
        self.detail.setStringValue(&NSString::from_str(detail));
        self.name.sizeToFit();
        self.detail.sizeToFit();
        let name_size = self.name.frame().size;
        let detail_size = self.detail.frame().size;
        let start = s.inset + if s.dot { DOT + 7.0 } else { 0.0 };
        let detail_w = if detail.is_empty() {
            0.0
        } else {
            s.gap + detail_size.width
        };
        let w = (start + name_size.width + detail_w + s.inset).round();
        let centred = |h: f64| ((s.height - h) * 0.5).round();
        self.name
            .setFrameOrigin(NSPoint::new(start, centred(name_size.height)));
        self.detail.setFrameOrigin(NSPoint::new(
            start + name_size.width + s.gap,
            centred(detail_size.height),
        ));
        self.detail.setHidden(detail.is_empty());
        self.back.setFrame(NSRect::new(
            NSPoint::new(0.0, 0.0),
            NSSize::new(w, s.height),
        ));
        self.window.setContentSize(NSSize::new(w, s.height));
    }

    /// Puts its top-left at `[x, y]` (global points).
    fn place(&mut self, mtm: MainThreadMarker, [x, y]: [f64; 2]) {
        let frame = [x.round(), y.round(), self.width(), self.style.height];
        if frame == self.frame {
            return;
        }
        self.frame = frame;
        let primary_h = NSScreen::screens(mtm)
            .firstObject()
            .map_or(900.0, |s| s.frame().size.height);
        let [ax, ay, aw, ah] = crate::screens::flip(frame, primary_h);
        self.window
            .setFrame_display(NSRect::new(NSPoint::new(ax, ay), NSSize::new(aw, ah)), true);
    }

    /// Steps its opacity towards shown or hidden.
    fn fade(&mut self, shown: bool, dt: f32, out_s: f32) {
        let next = if shown {
            (self.alpha + dt / FADE_IN_S).min(1.0)
        } else {
            (self.alpha - dt / out_s).max(0.0)
        };
        if (next - self.alpha).abs() > 1e-4 {
            self.alpha = next;
            self.window.setAlphaValue(f64::from(next));
        }
    }
}

pub(super) struct Tip {
    bubble: Bubble,
    clock: Instant,
    /// The button under the pointer, and since when.
    hover: Option<(ElemId, f64)>,
    /// What the tooltip shows now.
    shown: Option<(ElemId, Text)>,
    hidden_at: f64,
    /// The button just pressed: its tooltip stays away until the pointer leaves it.
    muted: Option<ElemId>,
    last_update: f64,
}

impl Tip {
    pub fn open(mtm: MainThreadMarker) -> Self {
        let style = Style {
            height: 24.0,
            font: 12.0,
            weights: unsafe { [NSFontWeightMedium, NSFontWeightRegular] },
            dim: 0.58,
            inset: 9.0,
            gap: 6.0,
            radius: 7.0,
            dot: false,
        };
        Self {
            bubble: Bubble::open(mtm, style),
            clock: Instant::now(),
            hover: None,
            shown: None,
            hidden_at: f64::NEG_INFINITY,
            muted: None,
            last_update: 0.0,
        }
    }

    pub fn window_number(&self) -> u32 {
        self.bubble.window_number()
    }

    /// With the capsule's (on the main thread).
    pub fn close(&self) {
        self.bubble.close();
    }

    /// Back in front with the capsule when it shows again.
    pub fn order_front(&self) {
        self.bubble.order_front();
    }

    /// Called on every tick with the button under the pointer, its tooltip and where it goes
    /// (`None`: no button, or one without a tooltip), and whether the mouse button is down.
    pub fn update(
        &mut self,
        mtm: MainThreadMarker,
        want: Option<(ElemId, Text, Anchor)>,
        pressed: bool,
    ) {
        let now = self.clock.elapsed().as_secs_f64();
        let dt = (now - self.last_update).clamp(0.0, 0.1) as f32;
        self.last_update = now;
        let on = want.as_ref().map(|(id, ..)| *id);
        if pressed {
            self.muted = on.or(self.muted);
        } else if self.muted != on {
            self.muted = None;
        }
        let want = want.filter(|(id, ..)| !pressed && self.muted != Some(*id));
        let Some((id, text, anchor)) = want else {
            self.hover = None;
            self.hide(now);
            self.bubble.fade(false, dt, FADE_OUT_S);
            return;
        };
        if self.hover.is_none_or(|(on, _)| on != id) {
            self.hide(now);
            self.hover = Some((id, now));
        }
        let since = self.hover.map_or(now, |(_, t)| t);
        let due = now - since >= DELAY_S || now - self.hidden_at < HANDOFF_S;
        if due && self.shown.as_ref().is_none_or(|(_, shown)| *shown != text) {
            self.bubble.set_text(text.0, &text.1);
            self.shown = Some((id, text));
        }
        if self.shown.is_some() {
            let at = self.spot(anchor);
            self.bubble.place(mtm, at);
        }
        self.bubble.fade(self.shown.is_some(), dt, FADE_OUT_S);
    }

    fn hide(&mut self, now: f64) {
        if self.shown.take().is_some() {
            self.hidden_at = now;
        }
    }

    /// Under the capsule, centred on the button and kept on screen; above it when there's no
    /// room below.
    fn spot(&self, anchor: Anchor) -> [f64; 2] {
        let w = self.bubble.width();
        let h = self.bubble.style.height;
        let [vx, vy, vw, vh] = anchor.visible;
        let x = (anchor.x - w * 0.5)
            .min(vx + vw - MARGIN - w)
            .max(vx + MARGIN);
        let below = anchor.bottom + GAP;
        let y = if below + h > vy + vh - MARGIN {
            anchor.top - GAP - h
        } else {
            below
        };
        [x, y]
    }
}

/// Where a banner goes: what's recorded (global points), whether that's a whole screen, and the
/// visible frame of its display.
#[derive(Clone, Copy, Debug)]
pub(super) struct Place {
    pub rect: [f64; 4],
    pub screen: bool,
    pub visible: [f64; 4],
}

/// "● Recording  Safari — GitHub" for a moment as a recording starts, at the top of what's
/// recorded, so it's plain which window, area or screen it is.
pub(super) struct Banner {
    bubble: Bubble,
    clock: Instant,
    /// Until when it shows (`clock` seconds).
    until: f64,
    last_update: f64,
}

impl Banner {
    pub fn open(mtm: MainThreadMarker) -> Self {
        let style = Style {
            height: 32.0,
            font: 13.0,
            weights: unsafe { [NSFontWeightSemibold, NSFontWeightRegular] },
            dim: 0.74,
            inset: 12.0,
            gap: 7.0,
            radius: 9.0,
            dot: true,
        };
        Self {
            bubble: Bubble::open(mtm, style),
            clock: Instant::now(),
            until: f64::NEG_INFINITY,
            last_update: 0.0,
        }
    }

    pub fn window_number(&self) -> u32 {
        self.bubble.window_number()
    }

    pub fn close(&self) {
        self.bubble.close();
    }

    pub fn order_front(&self) {
        self.bubble.order_front();
    }

    /// Shows `name` and `detail` over `place` for `seconds` before it fades.
    pub fn show(
        &mut self,
        mtm: MainThreadMarker,
        name: &str,
        detail: &str,
        place: Place,
        seconds: f64,
    ) {
        self.bubble.set_text(name, detail);
        let w = self.bubble.width();
        let h = self.bubble.style.height;
        let [x, y, width, _] = place.rect;
        let [vx, vy, vw, vh] = place.visible;
        let inset = if place.screen {
            BANNER_SCREEN_INSET
        } else {
            BANNER_INSET
        };
        let at = [
            (x + (width - w) * 0.5)
                .min(vx + vw - MARGIN - w)
                .max(vx + MARGIN),
            (y + inset).min(vy + vh - MARGIN - h).max(vy + MARGIN),
        ];
        self.bubble.place(mtm, at);
        self.bubble.order_front();
        self.until = self.clock.elapsed().as_secs_f64() + seconds;
    }

    pub fn hide(&mut self) {
        self.until = f64::NEG_INFINITY;
    }

    /// Called on every tick.
    pub fn update(&mut self) {
        let now = self.clock.elapsed().as_secs_f64();
        let dt = (now - self.last_update).clamp(0.0, 0.1) as f32;
        self.last_update = now;
        self.bubble.fade(now < self.until, dt, BANNER_OUT_S);
    }
}
